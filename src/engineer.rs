//! The race engineer: what the pilot says on the radio (transcribed) goes to the language model,
//! which answers from the running simulator's telemetry with the `engineer` tool, and the answer
//! is spoken back over the radio as the model writes it.

use crate::{
    arrstr,
    config::{ArrayStr, Config, RawConfig},
    language::LanguageDetector,
    model::{Model, Reply, Settings, StopReason, Tools, claude},
    sim::Sim,
    sqlite::pool::Pool,
    synthesizer::{SpeechInput, SpeechOutput, Synthesizer},
    task::Task,
};
use sdl3::audio::{AudioDevice, AudioStreamOwner};
use std::{
    path::{Path, PathBuf},
    sync::Arc,
    task::Poll,
};
use tokio::{
    runtime::Handle,
    sync::{Mutex, mpsc},
};
use tool::{Engineer as EngineerTool, SCHEMA_PATH, SKILL_PATH, instructions};

mod tool;

/// Said when there's no telemetry to answer from.
const NO_TELEMETRY: &str = "No telemetry yet, get in the car.";
/// Said when the model declines to answer.
const SAY_AGAIN: &str = "Say again?";

/// One line of the conversation, as the chat shows it.
pub struct ChatEntry {
    pub speaker: Speaker,
    pub text: String,
    /// Small print under it (e.g. the tokens an answer took)
    pub detail: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Speaker {
    Pilot,
    Engineer,
    /// The app: sessions starting, errors, cancelled questions
    Notice,
}

pub struct Engineer {
    client: reqwest::Client,
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
}

struct Session {
    /// The database the tool reads
    db: PathBuf,
    state: Arc<Mutex<SessionState>>,
}

struct SessionState {
    conversation: Box<dyn Model>,
    pool: Pool,
    /// The game session the conversation is about (track, session type and number), for
    /// `auto_session`
    game_session: Option<String>,
}

impl Engineer {
    pub async fn new(
        assets_path: &Path,
        config: &RawConfig,
        client: reqwest::Client,
    ) -> anyhow::Result<Self> {
        let (skill, schema) = tokio::try_join!(
            tokio::fs::read_to_string(assets_path.join(&config.skill_path).join(SKILL_PATH)),
            tokio::fs::read_to_string(assets_path.join(&config.skill_path).join(SCHEMA_PATH))
        )?;

        Ok(Self {
            client,
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

    /// Starts a new conversation with the next question: the engineer forgets what was said.
    pub fn new_session(&mut self) {
        self.stop();
        self.new_session = true;
        self.notice("New session: the engineer starts over");
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

        let Some(pool) = sim.and_then(|sim| sim.database()).cloned() else {
            self.reply(NO_TELEMETRY.into(), Some("no telemetry".into()));
            self.say(rt, config, synth, NO_TELEMETRY, arrstr!("en"));
            return;
        };

        let db = pool.database().path().to_owned();
        if self.new_session || self.session.as_ref().is_none_or(|s| s.db != db) {
            match self.start(config, pool) {
                Ok(session) => self.session = Some(session),
                Err(e) => {
                    self.notice(format!("Error starting the race engineer: {e:#}"));
                    return;
                }
            }
            self.new_session = false;
        }

        let Some(session) = &self.session else {
            return;
        };
        let state = session.state.clone();
        let auto_session = config.get().auto_session;

        // What the model writes goes straight to the synthesizer, and to the chat
        let (lang, speech) = self.speak(rt, config, synth, lang);
        let (chat, text) = mpsc::unbounded_channel();
        let task = Task::spawn(async move {
            let mut state = state.lock().await;
            if auto_session {
                state.follow_game_session().await;
            }
            let mut on_text = |text: &str| {
                speech.push(text);
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
            self.turn = None;
            self.session = None;
            self.notice("The simulator stopped: the session ended");
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
                            self.chat[entry].detail = Some("cut off".into());
                        }
                        self.notice(format!("Error asking the race engineer: {e:#}"));
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
        let mut detail = format!(
            "{} | {} in, {} cached, {} out",
            reply.model,
            usage.input_tokens + usage.cache_write_tokens,
            usage.cache_read_tokens,
            usage.output_tokens
        );

        let text = match reply.stop {
            // The exchange was forgotten, and whatever was said with it: the pilot can ask again
            StopReason::Refusal => {
                detail.push_str(" | declined");
                self.say(rt, config, synth, SAY_AGAIN, turn.lang);
                SAY_AGAIN.to_owned()
            }
            StopReason::MaxTokens => {
                detail.push_str(" | cut off");
                reply.text
            }
            // Every later question would end the same way: the next one starts over
            StopReason::ContextWindowExceeded => {
                self.new_session = true;
                detail.push_str(" | conversation full, starting over");
                reply.text
            }
            StopReason::EndTurn | StopReason::StopSequence => reply.text,
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

    /// Speaks `text` in `lang`, or in English without a voice for it.
    fn say(
        &mut self,
        rt: &Handle,
        config: &Config,
        synth: &mut Synthesizer,
        text: &str,
        lang: ArrayStr<2>,
    ) {
        let (_, speech) = self.speak(rt, config, synth, lang);
        speech.push(text);
    }

    /// Starts speaking what's pushed on the returned input (in `lang`, or in English without a
    /// voice for it, as returned), cutting off whatever was being said.
    fn speak(
        &mut self,
        rt: &Handle,
        config: &Config,
        synth: &mut Synthesizer,
        lang: ArrayStr<2>,
    ) -> (ArrayStr<2>, SpeechInput) {
        let (lang, effects) = {
            let config = config.get();
            let lang = match config.voices().contains_key(&lang) {
                true => lang,
                false => arrstr!("en"),
            };
            (lang, config.synth_effects())
        };

        let (input, speech) = synth.stream(rt, lang, effects);
        self.playback = Some(Playback {
            speech,
            stream: None,
            done: false,
        });
        (lang, input)
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
                    self.notice(format!("Error synthesizing the answer: {e:#}"));
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
                config.master_volume * config.engineer_volume
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
                    self.notice("No speaker selected: the answer isn't played");
                    return;
                }
            };

            if let Err(e) = played {
                self.playback = None;
                self.notice(format!("Error playing the answer: {e}"));
                return;
            }
        }

        // Once SDL has played all of it, the stream can go
        if playback.done
            && playback
                .stream
                .as_ref()
                .is_none_or(|stream| stream.queued_bytes().is_ok_and(|queued| queued == 0))
        {
            self.playback = None;
        }
    }

    /// Drops the answer on its way and stops the one being spoken.
    fn stop(&mut self) {
        if let Some(turn) = self.turn.take() {
            if let Some(entry) = turn.entry {
                self.chat[entry].detail = Some("cut off".into());
            }
            self.notice("Cancelled: the pilot talked over it");
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

    fn start(&self, config: &Config, pool: Pool) -> anyhow::Result<Session> {
        let tool = EngineerTool::new(pool.clone(), self.schema.clone());
        let mut tools = Tools::default();
        tools.insert(
            EngineerTool::NAME,
            Some(EngineerTool::description()),
            EngineerTool::input_schema(),
            tool.clone(),
        );
        let settings = {
            let config = config.get();
            Settings {
                model: config.claude_model.clone(),
                effort: config.claude_effort.clone(),
                system: self.skill.clone(),
                tools,
            }
        };
        let conversation = Box::new(claude::Conversation::new(settings, self.client.clone())?);

        // The history cache is built now, so the first history question doesn't wait for it
        tokio::spawn(async move {
            if let Err(e) = tool.warm().await {
                log::debug!("Not warming the engineer's history yet: {e:#}");
            }
        });

        let db = pool.database().path().to_owned();
        log::info!("Engineer session started on {}", db.display());
        Ok(Session {
            db,
            state: Arc::new(Mutex::new(SessionState {
                conversation,
                pool,
                game_session: None,
            })),
        })
    }
}

impl SessionState {
    /// A new game session (another track, or the next practice, qualifying or race) starts the
    /// conversation over.
    async fn follow_game_session(&mut self) {
        let current = match game_session(&self.pool).await {
            Ok(current) => current,
            Err(e) => {
                log::warn!("Error reading the game session: {e:#}");
                return;
            }
        };

        if current.is_some() && current != self.game_session {
            if self.game_session.is_some() {
                log::info!("New game session: the engineer starts over");
                self.conversation.reset();
            }
            self.game_session = current;
        }
    }
}

/// The game session in the database: its track, type and number.
async fn game_session(pool: &Pool) -> anyhow::Result<Option<String>> {
    let conn = pool.get().await?;
    Ok(conn
        .run(|conn| {
            use rusqlite::OptionalExtension;
            conn.query_row(
                "SELECT track, session_type, session_number FROM session ORDER BY id DESC LIMIT 1",
                [],
                |row| {
                    Ok(format!(
                        "{}|{}|{}",
                        row.get::<_, Option<String>>(0)?.unwrap_or_default(),
                        row.get::<_, Option<String>>(1)?.unwrap_or_default(),
                        row.get::<_, Option<i64>>(2)?.unwrap_or_default()
                    ))
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
    use lambiase_sim::{
        recorder::{DATABASE, Recorder},
        sys::lambiase_session_info,
    };
    use std::num::NonZeroUsize;

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
            .block_on(Engineer::new(Path::new("assets"), &config.get(), client))
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

    fn session(kind: &str, number: i64) -> lambiase_session_info {
        let mut session = lambiase_session_info::default();
        session.track = Some("Circuit de la Sarthe".to_owned()).into();
        session.session_type = Some(kind.to_owned()).into();
        session.session_number = Some(number).into();
        session
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn follows_the_game_session() {
        let dir = tempfile::tempdir().unwrap();
        let recorder = Recorder::new(dir.path(), None).unwrap();
        recorder.push_scoring(session("practice", 1), Vec::new());
        // Written by the recorder's thread, soon
        let path = dir.path().join(DATABASE);
        let one = NonZeroUsize::new(1).unwrap();
        let pool = Pool::new(Database::read_only(Arc::new(path)), one, one);

        let mut builder = claude::ConversationBuilder::new();
        builder.api_key("test");
        let mut state = SessionState {
            conversation: Box::new(builder.build(reqwest::Client::new()).unwrap()),
            pool,
            game_session: None,
        };

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while state.game_session.is_none() {
            assert!(std::time::Instant::now() < deadline, "no session recorded");
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            state.follow_game_session().await;
        }
        assert_eq!(
            state.game_session.as_deref(),
            Some("Circuit de la Sarthe|practice|1")
        );

        recorder.push_scoring(session("race", 1), Vec::new());
        drop(recorder);
        state.follow_game_session().await;
        assert_eq!(
            state.game_session.as_deref(),
            Some("Circuit de la Sarthe|race|1")
        );
    }
}
