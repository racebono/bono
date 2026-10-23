//! The race engineer: what the pilot says on the radio (transcribed) goes to the language model,
//! which answers from the running simulator's telemetry with the `engineer` tool, and the answer
//! is spoken back over the radio as the model writes it.

use crate::{
    account::{Account, Status},
    arrstr,
    config::{ArrayStr, Config, ModelConfig, ModelProvider, RawConfig},
    language::{self, LanguageDetector},
    provider::{
        self, Model, Reply, StopReason, Tools,
        bono_cloud::{GameSession, Lease},
    },
    sim::Sim,
    sqlite::pool::Pool,
    synthesizer::{Drain, Drained, Speech, SpeechInput, SpeechOutput, Synthesizer, plays_on},
    task::Task,
};
use bono_i18n::t;
use sdl3::audio::{AudioDevice, AudioStreamOwner};
use std::{
    path::{Path, PathBuf},
    sync::Arc,
    task::Poll,
    time::{Duration, Instant},
};
use tokio::{
    runtime::Handle,
    sync::{Mutex, mpsc},
};
use tool::{Engineer as EngineerTool, SCHEMA_PATH, SKILL_PATH, instructions};

mod tool;

/// One line of the conversation, as the chat shows it.
pub struct ChatEntry {
    pub speaker: Speaker,
    pub text: String,
    /// Small print under it (e.g. the tokens an answer took)
    pub detail: Option<String>,
}

/// What the pilot said on the radio, as it was heard.
pub enum Heard {
    /// A question, in a language with a voice to answer in
    Question { text: String, lang: ArrayStr<2> },
    /// Something said in a language without a voice
    Unvoiced { text: String, lang: ArrayStr<2> },
    /// Nothing understood: noise, silence, or a language too unclear
    Nothing,
    /// There's no voice to answer with: none at all (`None`), or none for the only language the
    /// speech to text model understands
    NoVoice(Option<ArrayStr<2>>),
    /// The talk button was let go too soon to have said anything: likely pressed by accident
    TooShort,
    /// The talk button was pressed, but nothing can listen
    NotListening(NotListening),
    /// Recording or transcribing failed
    Failed(anyhow::Error),
}

/// Why nothing can listen to the radio.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NotListening {
    NoMicrophone,
    /// No speech to text model is picked, or it couldn't be loaded
    NoModel,
    /// The speech to text model is still loading
    Loading,
}

/// How many times in a row nothing can be understood before the pilot is told what to check.
const MISHEARD_HINT: u32 = 3;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Speaker {
    Pilot,
    Engineer,
    /// The app: sessions starting, errors, cancelled questions
    Notice,
}

pub struct Engineer {
    client: reqwest::Client,
    /// Who Bono Cloud is used as
    account: Option<Account>,
    skill: Arc<str>,
    schema: Arc<str>,
    /// Everything said, for the chat
    chat: Vec<ChatEntry>,
    /// The language the pilot last asked in: typed questions whose language is unclear are
    /// answered in it
    last_lang: ArrayStr<2>,
    /// Which language typed questions are in (the model answers in the pilot's language)
    languages: LanguageDetector,
    /// The conversation for the database the tool reads. `None` until the first question, and
    /// again once the simulator stops, letting go of its database
    session: Option<Session>,
    /// A new session was asked for: the next question starts one
    new_session: bool,
    /// The question being answered
    turn: Option<Turn>,
    /// The answer being spoken
    playback: Option<Playback>,
    /// The spotter is speaking: the answer waits, and carries on from there after
    held: bool,
    /// Times in a row nothing was understood on the radio
    misheard: u32,
}

/// A question being answered.
struct Turn {
    task: Task<anyhow::Result<Reply>>,
    /// The language it's answered in
    lang: ArrayStr<2>,
    /// The answer as the model writes it, for the chat
    text: mpsc::UnboundedReceiver<String>,
    /// The chat entry it's written into, once it says something
    entry: Option<usize>,
}

/// Speech played as it's synthesized.
struct Playback {
    speech: SpeechOutput,
    /// Opened with the first piece of speech. Dropping it stops playback
    stream: Option<AudioStreamOwner>,
    /// All the speech is on the stream
    done: bool,
    /// Whether SDL played all of it, once it's all on the stream
    drain: Drain,
}

struct Session {
    /// The database the tool reads
    db: PathBuf,
    /// The model settings it was started with: changing them starts over
    model: ModelConfig,
    /// The account's user id, when the conversation is the account's (Bono Cloud's): it ends
    /// once that account signs out
    user: Option<String>,
    /// Bono Cloud's session lease, ended without waiting for the conversation
    lease: Option<Lease>,
    state: Arc<Mutex<SessionState>>,
}

struct SessionState {
    conversation: Box<dyn Model>,
    pool: Pool,
    /// The simulator writing the database, e.g. `lmu`
    sim: &'static str,
    tool: EngineerTool,
    /// The game session the conversation is about (track, session type and number), for
    /// `auto_session`
    game_session: Option<String>,
}

impl Engineer {
    pub async fn new(
        assets_path: &Path,
        config: &RawConfig,
        client: reqwest::Client,
        account: Option<Account>,
    ) -> anyhow::Result<Self> {
        let (skill, schema) = tokio::try_join!(
            tokio::fs::read_to_string(assets_path.join(config.skill_path()).join(SKILL_PATH)),
            tokio::fs::read_to_string(assets_path.join(config.skill_path()).join(SCHEMA_PATH))
        )?;

        Ok(Self {
            client,
            account,
            skill: instructions(skill).into(),
            schema: schema.into(),
            chat: Vec::new(),
            last_lang: arrstr!("en"),
            languages: LanguageDetector::new(),
            session: None,
            new_session: false,
            turn: None,
            playback: None,
            held: false,
            misheard: 0,
        })
    }

    /// Everything said so far, oldest first.
    #[inline]
    pub fn chat(&self) -> &[ChatEntry] {
        &self.chat
    }

    /// Whether a question is being answered.
    #[inline]
    pub fn is_thinking(&self) -> bool {
        self.turn.is_some()
    }

    /// Whether an answer is being spoken, or is about to be.
    #[inline]
    pub fn is_speaking(&self) -> bool {
        self.playback.is_some()
    }

    /// The language the pilot last asked in.
    #[inline]
    pub fn language(&self) -> ArrayStr<2> {
        self.last_lang
    }

    /// Pauses the answer being spoken (and holds one about to start) while the spotter speaks;
    /// releasing it carries on from where it stopped.
    pub fn hold(&mut self, held: bool) {
        if std::mem::replace(&mut self.held, held) == held {
            return;
        }

        if let Some(stream) = self
            .playback
            .as_ref()
            .and_then(|playback| playback.stream.as_ref())
        {
            let result = match held {
                true => stream.pause(),
                false => stream.resume(),
            };
            if let Err(e) = result {
                log::warn!("Error holding the answer for the spotter: {e}");
            }
        }
    }

    /// The speaker `which` is gone (unplugged): the answer being spoken on it stops. `picked`:
    /// whether it's the speaker picked, which the pilot is told about either way.
    pub fn speaker_removed(&mut self, which: u32, picked: bool) {
        let on_it = (self.playback.as_ref())
            .and_then(|playback| playback.stream.as_ref())
            .is_some_and(|stream| plays_on(stream, which));
        if on_it {
            self.playback = None;
        }
        if on_it || picked {
            self.notice(t!("engineer.speaker_removed"));
        }
    }

    /// Starts a new conversation with the next question: the engineer forgets what was said.
    pub fn new_session(&mut self) {
        self.stop();
        if let Some(lease) = self.session.as_ref().and_then(|s| s.lease.as_ref()) {
            lease.end();
        }
        self.new_session = true;
        self.notice(t!("engineer.new_session"));
    }

    /// What the pilot said on the radio: a question is asked, and anything else is answered
    /// with why it can't be, cutting off whatever was being said (the pilot talked over it).
    pub fn hear(
        &mut self,
        rt: &Handle,
        sim: Option<&mut Sim>,
        config: &Config,
        synth: &mut Synthesizer,
        heard: Heard,
    ) {
        match heard {
            Heard::Question { text, lang } => {
                self.misheard = 0;
                self.ask(rt, sim, config, synth, text, lang);
            }
            Heard::TooShort => {}
            Heard::Unvoiced { text, lang } => {
                self.stop();
                let language = language::ui_name(lang);
                self.notice(t!(
                    "engineer.no_voice_for",
                    language = language,
                    text = text
                ));
                self.say_again(rt, config, synth);
            }
            Heard::Nothing => {
                self.stop();
                self.misheard += 1;
                match self.misheard >= MISHEARD_HINT {
                    true => {
                        self.misheard = 0;
                        self.notice(t!("engineer.still_not_understood"));
                    }
                    false => self.notice(t!("engineer.not_understood")),
                }
                self.say_again(rt, config, synth);
            }
            Heard::Failed(e) => {
                self.stop();
                self.notice(t!("engineer.error_hearing", error = format!("{e:#}")));
                self.say_again(rt, config, synth);
            }
            // The voice may not be there either: the tone says it
            Heard::NotListening(why) => {
                self.stop();
                self.notice(match why {
                    NotListening::NoMicrophone => t!("engineer.no_microphone"),
                    NotListening::NoModel => t!("engineer.no_stt_model"),
                    NotListening::Loading => t!("engineer.stt_loading"),
                });
                self.no_beep();
            }
            Heard::NoVoice(lang) => {
                self.stop();
                self.notice(match lang {
                    Some(lang) => {
                        t!(
                            "engineer.no_voice_for_model",
                            language = language::ui_name(lang)
                        )
                    }
                    None => t!("engineer.no_voices").to_owned(),
                });
                self.no_beep();
            }
        }
    }

    /// Asks the pilot to say it again, in the language they last asked in (see [`Self::voice`]).
    /// Without any voice, the radio's low tone says it can't answer.
    fn say_again(&mut self, rt: &Handle, config: &Config, synth: &mut Synthesizer) {
        match self.voice(config, self.last_lang) {
            Some(lang) => self.say(rt, config, synth, t!("engineer.say_again": &lang), lang),
            None => self.no_beep(),
        }
    }

    /// The radio's low tone, as loud as the engineer: the request couldn't be answered.
    fn no_beep(&mut self) {
        self.playback = Some(Playback {
            speech: SpeechOutput::ready(Speech::no_beep(1.0)),
            stream: None,
            done: false,
            drain: Drain::new(Instant::now()),
        });
    }

    /// A question typed in the chat, answered like one asked on the radio: spoken in the
    /// question's language, or the last one the pilot used if that's unclear.
    pub fn ask_text(
        &mut self,
        rt: &Handle,
        sim: Option<&mut Sim>,
        config: &Config,
        synth: &mut Synthesizer,
        question: String,
    ) {
        let lang = match self.languages.detect(&question) {
            Some(lang) => lang,
            None => self.last_lang,
        };
        self.ask(rt, sim, config, synth, question, lang);
    }

    /// The pilot asked something on the radio: it's sent to the model, with the running
    /// simulator's telemetry, and the answer is spoken as the model writes it. An answer still on its way, or
    /// being spoken, is dropped: the pilot talked over it.
    pub fn ask(
        &mut self,
        rt: &Handle,
        sim: Option<&mut Sim>,
        config: &Config,
        synth: &mut Synthesizer,
        question: String,
        lang: ArrayStr<2>,
    ) {
        self.stop();
        self.last_lang = lang;
        self.chat.push(ChatEntry {
            speaker: Speaker::Pilot,
            text: question.clone(),
            detail: None,
        });

        let database = sim.and_then(|sim| Some((sim.id(), sim.database()?.clone())));
        let Some((sim, pool)) = database else {
            // Said in the pilot's language, when there's a voice for it
            let lang = self.voice(config, lang).unwrap_or(lang);
            let text = t!("engineer.no_telemetry": &lang);
            self.reply(text.into(), Some(t!("engineer.detail_no_telemetry").into()));
            self.say(rt, config, synth, text, lang);
            return;
        };

        let db = pool.database().path().to_owned();
        let model_changed = self
            .session
            .as_ref()
            .is_some_and(|s| s.model != config.get().model);
        if model_changed {
            self.notice(t!("engineer.new_model"));
        }
        if self.new_session || model_changed || self.session.as_ref().is_none_or(|s| s.db != db) {
            match self.start(config, sim, pool) {
                Ok(session) => self.session = Some(session),
                Err(e) => {
                    self.notice(t!("engineer.error_starting", error = format!("{e:#}")));
                    self.no_beep();
                    return;
                }
            }
            self.new_session = false;
        }

        let Some(session) = &self.session else {
            return;
        };
        let state = session.state.clone();
        let auto_session = *config.get().auto_session();

        // What the model writes goes straight to the synthesizer, and to the chat
        let speech = self.speak(rt, config, synth, lang);
        let lang = speech.as_ref().map_or(lang, |(lang, _)| *lang);
        let speech = speech.map(|(_, speech)| speech);
        let (chat, text) = mpsc::unbounded_channel();
        let task = Task::spawn(async move {
            let mut state = state.lock().await;
            state.follow_game_session(auto_session).await;
            // Only the new user message gets a snapshot. Older messages and thinking
            // blocks remain untouched, preserving the conversation's cached prefix.
            let question = state.tool.question_with_snapshot(question).await;
            let mut on_text = |text: &str| {
                if let Some(speech) = &speech {
                    speech.push(text);
                }
                let _ = chat.send(text.to_owned());
            };
            state
                .conversation
                .send_streaming(question, &mut on_text)
                .await
            // The speech's text ends here, with the turn
        });

        self.turn = Some(Turn {
            task,
            lang,
            text,
            entry: None,
        });
    }

    /// Called every frame: answers come in, are synthesized and played. `sim_running`: whether a
    /// simulator is running at all.
    pub fn update(
        &mut self,
        rt: &Handle,
        sim_running: bool,
        config: &Config,
        synth: &mut Synthesizer,
        speaker: Option<&AudioDevice>,
    ) {
        self.languages.update(rt, config.get().voices());
        // The game stopped: the conversation lets go of the database, so the plugin can delete it
        if !sim_running && self.session.is_some() {
            let interrupted = self.turn.take().is_some();
            self.session = None;
            self.notice(t!("engineer.sim_stopped"));
            if interrupted {
                self.no_beep();
            }
        }
        // Bono Cloud's no longer picked: its lease isn't needed until it is again (the conversation
        // goes with the next question)
        if let Some(session) = &self.session
            && let Some(lease) = &session.lease
            && session.model != config.get().model
        {
            lease.end();
        }
        // The account the conversation is had as signed out (or another signed in): it ends,
        // answer on its way included
        if self.session.as_ref().is_some_and(|s| !self.signed_in_as(s)) {
            let interrupted = self.turn.is_some();
            self.stop();
            self.session = None;
            self.notice(t!("engineer.signed_out"));
            if interrupted {
                self.no_beep();
            }
        }

        if let Some(turn) = &mut self.turn {
            // The answer so far
            while let Ok(text) = turn.text.try_recv() {
                match turn.entry {
                    Some(entry) => self.chat[entry].text.push_str(&text),
                    None => {
                        turn.entry = Some(self.chat.len());
                        self.chat.push(ChatEntry {
                            speaker: Speaker::Engineer,
                            text,
                            detail: None,
                        });
                    }
                }
            }

            if let Some(result) = turn.task.update(rt)
                && let Some(turn) = self.turn.take()
            {
                match result {
                    Ok(reply) => self.answer(rt, config, synth, reply, turn),
                    Err(e) => {
                        if let Some(entry) = turn.entry {
                            self.chat[entry].detail = Some(t!("engineer.detail_cut_off").into());
                        }
                        self.notice(t!("engineer.error_asking", error = format!("{e:#}")));
                        self.no_beep();
                    }
                }
            }
        }

        self.play(config, speaker);
    }

    fn answer(
        &mut self,
        rt: &Handle,
        config: &Config,
        synth: &mut Synthesizer,
        reply: Reply,
        turn: Turn,
    ) {
        log::info!(
            "Engineer ({}, {:?}; {} in, {} cached, {} out): {}",
            reply.model,
            reply.stop,
            reply.usage.input_tokens + reply.usage.cache_write_tokens,
            reply.usage.cache_read_tokens,
            reply.usage.output_tokens,
            reply.text
        );

        let usage = &reply.usage;
        let mut detail = t!(
            "engineer.detail_tokens",
            model = reply.model,
            input = usage.input_tokens + usage.cache_write_tokens,
            cached = usage.cache_read_tokens,
            output = usage.output_tokens
        );

        let text = match reply.stop {
            // The exchange was forgotten, and whatever was said with it: the pilot can ask again
            StopReason::Refusal => {
                detail = format!("{detail} | {}", t!("engineer.detail_declined"));
                let say_again = t!("engineer.say_again": &turn.lang);
                self.say(rt, config, synth, say_again, turn.lang);
                if self.voice(config, turn.lang).is_none() {
                    self.no_beep();
                }
                say_again.to_owned()
            }
            StopReason::MaxTokens => {
                detail = format!("{detail} | {}", t!("engineer.detail_cut_off"));
                self.no_beep();
                reply.text
            }
            // Every later question would end the same way: the next one starts over
            StopReason::ContextWindowExceeded => {
                self.new_session = true;
                detail = format!("{detail} | {}", t!("engineer.detail_full"));
                self.no_beep();
                reply.text
            }
            StopReason::EndTurn | StopReason::StopSequence => {
                if reply.text.trim().is_empty() {
                    self.no_beep();
                }
                reply.text
            }
        };

        match turn.entry {
            Some(entry) => {
                self.chat[entry].text = text;
                self.chat[entry].detail = Some(detail);
            }
            None if !text.trim().is_empty() => self.reply(text, Some(detail)),
            None => {}
        }
    }

    /// Speaks `text` in `lang` (see [`Self::voice`]): nothing without any voice.
    fn say(
        &mut self,
        rt: &Handle,
        config: &Config,
        synth: &mut Synthesizer,
        text: &str,
        lang: ArrayStr<2>,
    ) {
        if let Some((_, speech)) = self.speak(rt, config, synth, lang) {
            speech.push(text);
        }
    }

    /// The language `lang` is spoken in: itself with a voice for it, or else the one the pilot
    /// last asked in, the app's, or any with a voice. `None` without any voice.
    pub fn voice(&self, config: &Config, lang: ArrayStr<2>) -> Option<ArrayStr<2>> {
        let config = config.get();
        let voices = config.voices();
        let app = ArrayStr::from_str(config.locale().split('-').next().unwrap_or_default());
        [Some(lang), Some(self.last_lang), app]
            .into_iter()
            .flatten()
            .find(|lang| voices.contains_key(lang))
            .or_else(|| voices.keys().next().copied())
    }

    /// Starts speaking what's pushed on the returned input (in `lang`, see [`Self::voice`], as
    /// returned), cutting off whatever was being said. `None` without any voice.
    fn speak(
        &mut self,
        rt: &Handle,
        config: &Config,
        synth: &mut Synthesizer,
        lang: ArrayStr<2>,
    ) -> Option<(ArrayStr<2>, SpeechInput)> {
        let lang = self.voice(config, lang)?;
        let effects = config.get().synth_effects();

        let (input, speech) = synth.stream(rt, lang, effects);
        self.playback = Some(Playback {
            speech,
            stream: None,
            done: false,
            drain: Drain::new(Instant::now()),
        });
        Some((lang, input))
    }

    /// Plays the speech synthesized so far.
    fn play(&mut self, config: &Config, speaker: Option<&AudioDevice>) {
        let held = self.held;
        let Some(playback) = &mut self.playback else {
            return;
        };

        while !playback.done {
            let mut speech = match playback.speech.try_next() {
                Poll::Pending => break,
                Poll::Ready(Some(Ok(speech))) => speech,
                Poll::Ready(Some(Err(e))) => {
                    self.playback = None;
                    self.notice(t!("engineer.error_synthesizing", error = format!("{e:#}")));
                    self.no_beep();
                    return;
                }
                // SDL plays what's left
                Poll::Ready(None) => {
                    playback.done = true;
                    if let Some(stream) = &playback.stream
                        && let Err(e) = stream.flush()
                    {
                        log::warn!("Error flushing the answer's audio: {e}");
                    }
                    break;
                }
            };

            let gain = {
                let config = config.get();
                config.master_volume() * config.engineer_volume()
            };
            for sample in &mut speech.samples {
                *sample *= gain;
            }

            let played = match (&playback.stream, speaker) {
                (Some(stream), _) => stream.put_data_f32(&speech.samples),
                (None, Some(speaker)) => speech.start(speaker).and_then(|stream| {
                    // Starts held while the spotter speaks
                    if held {
                        stream.pause()?;
                    }
                    playback.stream = Some(stream);
                    Ok(())
                }),
                (None, None) => {
                    self.playback = None;
                    self.notice(t!("engineer.no_speaker"));
                    return;
                }
            };

            if let Err(e) = played {
                self.playback = None;
                self.notice(t!("engineer.error_playing", error = e));
                return;
            }
        }

        // Once SDL has played all of it, the stream can go. A speaker that stops taking audio
        // without SDL saying it was removed doesn't keep it forever, holding up the spotter's calls
        if !playback.done {
            return;
        }
        let drained = match &playback.stream {
            Some(stream) => {
                let queued = stream.queued_bytes().ok();
                playback.drain.check(queued, held, Instant::now())
            }
            None => Drained::Done,
        };
        match drained {
            Drained::Playing => {}
            Drained::Done => self.playback = None,
            Drained::Stalled => {
                self.playback = None;
                self.notice(t!("engineer.speaker_stalled"));
            }
        }
    }

    /// Whether the account `session` is had as, if any, is still signed in.
    fn signed_in_as(&self, session: &Session) -> bool {
        let Some(user) = &session.user else {
            return true;
        };
        self.account
            .as_ref()
            .is_some_and(|account| match account.status() {
                Status::SignedIn(signed_in) => signed_in.id == *user,
                Status::SignedOut => false,
            })
    }

    /// Drops the answer on its way and stops the one being spoken.
    fn stop(&mut self) {
        if let Some(turn) = self.turn.take() {
            if let Some(entry) = turn.entry {
                self.chat[entry].detail = Some(t!("engineer.detail_cut_off").into());
            }
            self.notice(t!("engineer.cancelled"));
        }
        self.playback = None;
    }

    fn reply(&mut self, text: String, detail: Option<String>) {
        self.chat.push(ChatEntry {
            speaker: Speaker::Engineer,
            text,
            detail,
        });
    }

    fn notice(&mut self, text: impl Into<String>) {
        let text = text.into();
        log::info!("{text}");
        self.chat.push(ChatEntry {
            speaker: Speaker::Notice,
            text,
            detail: None,
        });
    }

    fn start(&self, config: &Config, sim: &'static str, pool: Pool) -> anyhow::Result<Session> {
        let tool = EngineerTool::new(pool.clone(), self.schema.clone());
        let mut tools = Tools::default();
        tools.insert(
            EngineerTool::NAME,
            Some(EngineerTool::description()),
            EngineerTool::input_schema(),
            tool.clone(),
        );
        let model = config.get().model.clone();
        // Taken before the conversation: if the account signs out meanwhile, it ends next frame
        let user = match model.provider() {
            ModelProvider::BonoCloud => match self.account.as_ref().map(Account::status) {
                Some(Status::SignedIn(user)) => Some(user.id),
                Some(Status::SignedOut) | None => anyhow::bail!(t!("provider.bono_cloud_sign_in")),
            },
            ModelProvider::Claude | ModelProvider::Ollama | ModelProvider::OpenAi => None,
        };
        let conversation = provider::conversation(
            &model,
            self.account.as_ref(),
            self.skill.clone(),
            tools,
            self.client.clone(),
        )?;

        // The history cache is built now, so the first history question doesn't wait for it
        let warming_tool = tool.clone();
        tokio::spawn(async move {
            if let Err(e) = warming_tool.warm().await {
                log::debug!("Not warming the engineer's history yet: {e:#}");
            }
        });

        let db = pool.database().path().to_owned();
        log::info!("Engineer session started on {}", db.display());
        Ok(Session {
            db,
            model,
            user,
            lease: conversation.lease(),
            state: Arc::new(Mutex::new(SessionState {
                conversation,
                tool,
                pool,
                sim,
                game_session: None,
            })),
        })
    }

    /// The app is quitting: Bono Cloud's lease is ended, waiting for the website a moment, as
    /// nothing will be left to after.
    pub fn quit(&mut self, rt: &Handle) {
        /// The longest the app waits on the website to quit
        const WAIT: Duration = Duration::from_secs(3);

        self.stop();
        // Before the conversation goes: it would end the lease without waiting
        if let Some(lease) = self.session.as_ref().and_then(|s| s.lease.as_ref())
            && rt
                .block_on(async { tokio::time::timeout(WAIT, lease.end_and_wait()).await })
                .is_err()
        {
            log::warn!("Gave up ending the Cloud session lease: it expires on its own");
        }
        self.session = None;
    }
}

impl SessionState {
    /// Tells the conversation which game session it's about (Bono Cloud leases it). With
    /// `auto_session`, a new one (another track, or the next practice, qualifying or race) starts
    /// the conversation over.
    async fn follow_game_session(&mut self, auto_session: bool) {
        let current = match game_session(&self.pool, self.sim).await {
            Ok(Some(current)) => current,
            Ok(None) => return,
            Err(e) => {
                log::warn!("Error reading the game session: {e:#}");
                return;
            }
        };

        if self.game_session.as_ref() != Some(&current.key) {
            if auto_session && self.game_session.is_some() {
                log::info!("New game session: the engineer starts over");
                self.conversation.reset();
            }
            self.game_session = Some(current.key.clone());
        }
        self.conversation.set_game_session(&current);
    }
}

/// The game session in the database: its track, type and number, and how long it's scheduled
/// to last.
async fn game_session(pool: &Pool, sim: &'static str) -> anyhow::Result<Option<GameSession>> {
    let conn = pool.get().await?;
    Ok(conn
        .run(move |conn| {
            use rusqlite::OptionalExtension;
            conn.query_row(
                "SELECT track, session_type, session_number, session_end_time FROM session \
                 ORDER BY id DESC LIMIT 1",
                [],
                |row| {
                    let session_type = row.get::<_, Option<String>>(1)?.unwrap_or_default();
                    let key = format!(
                        "{}|{}|{}",
                        row.get::<_, Option<String>>(0)?.unwrap_or_default(),
                        session_type,
                        row.get::<_, Option<i64>>(2)?.unwrap_or_default()
                    );
                    let scheduled = row.get::<_, Option<f64>>(3)?;
                    Ok(GameSession::new(key, sim, &session_type, scheduled))
                },
            )
            .optional()
        })
        .await?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sqlite::Database;
    use bono_sim::{
        recorder::{DATABASE, Recorder},
        sys::bono_session_info,
    };
    use std::num::NonZeroUsize;

    #[test]
    fn incomplete_answers_play_a_tone_without_a_voice() {
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let _entered = runtime.enter();
        let rt = runtime.handle();
        let dir = tempfile::tempdir().unwrap();
        let config = rt
            .block_on(Config::new(dir.path().join("config.json")))
            .unwrap();
        let mut synth = Synthesizer::new(rt, &config.get());
        let mut engineer = rt
            .block_on(Engineer::new(
                Path::new("assets"),
                &config.get(),
                reqwest::Client::new(),
                None,
            ))
            .unwrap();

        for (stop, text, tone) in [
            (StopReason::MaxTokens, "Partial answer", true),
            (StopReason::ContextWindowExceeded, "Partial answer", true),
            (StopReason::Refusal, "", true),
            (StopReason::EndTurn, "", true),
            (StopReason::EndTurn, "Complete answer", false),
            (StopReason::StopSequence, "Complete answer", false),
        ] {
            engineer.playback = None;
            let (_, receiver) = mpsc::unbounded_channel();
            let turn = Turn {
                task: Task::spawn(std::future::pending()),
                lang: arrstr!("en"),
                text: receiver,
                entry: None,
            };
            engineer.answer(
                rt,
                &config,
                &mut synth,
                Reply {
                    text: text.into(),
                    stop,
                    usage: Default::default(),
                    model: "test".into(),
                },
                turn,
            );
            assert_eq!(engineer.playback.is_some(), tone, "{stop:?}");
            if let Some(playback) = &mut engineer.playback {
                let Poll::Ready(Some(Ok(speech))) = playback.speech.try_next() else {
                    panic!("failure tone must be ready without speech synthesis");
                };
                assert!(speech.samples.iter().any(|sample| *sample != 0.0));
            }
        }
        assert!(engineer.new_session);
    }

    /// A typed question answered the way the app does it: `ask_text`, then `update` every frame
    /// until the answer is in. Needs `ANTHROPIC_API_KEY` and `EVAL_DB` (a recorded race).
    #[test]
    #[ignore]
    fn answers_like_the_app() {
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let _entered = runtime.enter();
        let rt = runtime.handle();

        let db = PathBuf::from(std::env::var("EVAL_DB").expect("EVAL_DB isn't set"));
        // As the LMU simulator opens it
        let pool = Pool::new(
            Database::read_only(Arc::new(db)),
            NonZeroUsize::new(2).unwrap(),
            NonZeroUsize::new(4).unwrap(),
        );
        let mut sim = crate::sim::recording(pool);

        let dir = tempfile::tempdir().unwrap();
        let config = rt
            .block_on(Config::new(dir.path().join("config.json")))
            .unwrap();
        let mut synth = Synthesizer::new(rt, &config.get());
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(30))
            .build()
            .unwrap();
        let mut engineer = rt
            .block_on(Engineer::new(
                Path::new("assets"),
                &config.get(),
                client,
                None,
            ))
            .unwrap();

        let questions = std::env::var("EVAL_QUESTIONS")
            .unwrap_or_else(|_| "how are the tyres?|where am I losing time?".into());
        for question in questions.split('|') {
            let start = std::time::Instant::now();
            engineer.ask_text(rt, Some(&mut sim), &config, &mut synth, question.to_owned());
            while engineer.is_thinking() {
                assert!(
                    start.elapsed() < std::time::Duration::from_secs(120),
                    "stuck on {question:?}"
                );
                engineer.update(rt, true, &config, &mut synth, None);
                std::thread::sleep(std::time::Duration::from_millis(16));
            }
            println!("{question:?} answered in {:.1?}", start.elapsed());
        }

        for entry in engineer.chat() {
            println!("{:?}: {} {:?}", entry.speaker, entry.text, entry.detail);
        }
    }

    fn session(kind: &str, number: i64) -> bono_session_info {
        let mut session = bono_session_info::default();
        session.track = Some("Circuit de la Sarthe".to_owned()).into();
        session.session_type = Some(kind.to_owned()).into();
        session.session_number = Some(number).into();
        session
    }

    /// A conversation that only records what it's told about game sessions.
    #[derive(Default)]
    struct Recorded {
        games: Vec<GameSession>,
        resets: usize,
    }

    struct RecordingModel(Arc<std::sync::Mutex<Recorded>>);

    impl Model for RecordingModel {
        fn send_streaming<'a>(
            &'a mut self,
            _: String,
            _: &'a mut (dyn FnMut(&str) + Send),
        ) -> futures::future::BoxFuture<'a, anyhow::Result<Reply>> {
            unimplemented!()
        }

        fn reset(&mut self) {
            self.0.lock().unwrap().resets += 1;
        }

        fn usage(&self) -> &std::collections::BTreeMap<String, provider::Usage> {
            unimplemented!()
        }

        fn set_game_session(&mut self, game: &GameSession) {
            self.0.lock().unwrap().games.push(game.clone());
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn follows_the_game_session() {
        use provider::bono_cloud::SessionType;

        for auto_session in [true, false] {
            let dir = tempfile::tempdir().unwrap();
            let recorder = Recorder::new(dir.path(), None).unwrap();
            recorder.push_scoring(session("practice", 1), Vec::new());
            // Written by the recorder's thread, soon
            let path = dir.path().join(DATABASE);
            let one = NonZeroUsize::new(1).unwrap();
            let pool = Pool::new(Database::read_only(Arc::new(path)), one, one);

            let recorded = Arc::new(std::sync::Mutex::new(Recorded::default()));
            let mut state = SessionState {
                conversation: Box::new(RecordingModel(recorded.clone())),
                tool: EngineerTool::new(pool.clone(), "".into()),
                pool,
                sim: "lmu",
                game_session: None,
            };

            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
            while state.game_session.is_none() {
                assert!(std::time::Instant::now() < deadline, "no session recorded");
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
                state.follow_game_session(auto_session).await;
            }
            assert_eq!(
                state.game_session.as_deref(),
                Some("Circuit de la Sarthe|practice|1")
            );
            state.follow_game_session(auto_session).await;

            let mut race = session("race", 1);
            race.session_end_time = Some(5400.0).into();
            recorder.push_scoring(race, Vec::new());
            drop(recorder);
            state.follow_game_session(auto_session).await;
            assert_eq!(
                state.game_session.as_deref(),
                Some("Circuit de la Sarthe|race|1")
            );

            let recorded = recorded.lock().unwrap();
            // Told every turn, so a lease ended meanwhile is started again
            let games = &recorded.games;
            assert_eq!(games.len(), 3);
            assert_eq!(games[0], games[1]);
            assert_eq!(games[0].key, "Circuit de la Sarthe|practice|1");
            assert_eq!(games[0].sim, "lmu");
            assert_eq!(games[0].session_type, SessionType::Practice);
            // Not scheduled: the type's default
            assert_eq!(games[0].expected_minutes, 60);
            assert_eq!(games[2].key, "Circuit de la Sarthe|race|1");
            assert_eq!(games[2].session_type, SessionType::Race);
            assert_eq!(games[2].expected_minutes, 90);
            // Only auto sessions start the conversation over
            assert_eq!(recorded.resets, usize::from(auto_session));
        }
    }
}
