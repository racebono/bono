//! The race engineer: what the pilot says on the radio (transcribed) goes to the language model,
//! which answers from the running simulator's telemetry with the `engineer` tool, and the answer
//! is spoken back over the radio as the model writes it.

use crate::{
    account::{Account, Status},
    arrstr,
    config::{ArrayStr, Config, ModelConfig, ModelProvider},
    language::{self, LanguageDetector},
    provider::{
        self, Model, Reply, Steer, StopReason, Tools,
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
    collections::VecDeque,
    path::PathBuf,
    sync::Arc,
    task::Poll,
    time::{Duration, Instant},
};
use timeline::Timeline;
use tokio::{
    runtime::Handle,
    sync::{Mutex, mpsc},
};
use tool::{Engineer as EngineerTool, SKILL, instructions};

mod schedule;
mod timeline;
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
    /// A question in the pilot's language, answered with the selected or an available voice.
    /// `released`: when the talk button was let go.
    Question {
        text: String,
        lang: ArrayStr<2>,
        released: Instant,
    },
    /// Something said in a language without a voice
    Unvoiced { text: String, lang: ArrayStr<2> },
    /// Nothing understood: noise, silence, or a language too unclear
    Nothing,
    /// There's no voice to answer with: none at all (`None`), or none for the only language the
    /// speech to text model understands
    NoVoice(Option<ArrayStr<2>>),
    /// A fixed input language requires a multilingual model, but the loaded one is English-only.
    UnsupportedLanguage(ArrayStr<2>),
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
    /// Everything said, for the chat
    chat: Vec<ChatEntry>,
    /// The language the pilot last asked in: typed questions whose language is unclear are
    /// answered in it
    last_lang: ArrayStr<2>,
    /// Which language typed questions are in (the model answers in the pilot's language)
    languages: LanguageDetector,
    /// The tool for the recording the running simulator has, from the moment it has one: its
    /// tables are kept up to date in the background, so the first question finds them ready
    tool: Option<EngineerTool>,
    /// The conversation for the database the tool reads. `None` until the first question, and
    /// again once the simulator stops, letting go of its database
    session: Option<Session>,
    /// A new session was asked for: the next question starts one
    new_session: bool,
    /// The question being answered
    turn: Option<Turn>,
    /// Answers waiting for their first piece of speech, in the order they were
    /// [queued](Self::queue): each joins the back of `line` once it has one, so an answer still
    /// being thought about never holds up one that's ready
    incoming: Vec<Playback>,
    /// The line of answers, in the order they're played: the front one plays, the others wait
    /// with their speech buffered. Only [`Self::fill_gap`] moves one ahead
    line: VecDeque<Playback>,
    /// The spotter is speaking: the front answer waits, and carries on from there after
    spotter_held: bool,
    /// The talk button is held: the front answer waits, as for the spotter
    talking: bool,
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
    /// The pilot's questions asked while it runs, for it to take at its next tool hand-back.
    /// What it leaves there is asked next
    steer: Steer,
    /// The latest of those questions' language and the first one's release, as given to
    /// [`Engineer::ask`]: what it leaves is asked with them. `None` until one is asked
    steered: Option<(Option<ArrayStr<2>>, Option<Instant>)>,
}

/// An answer in the engineer's line: speech played as it's synthesized.
pub struct Playback {
    speech: SpeechOutput,
    /// Opened with the first piece of speech. Dropping it stops playback
    stream: Option<AudioStreamOwner>,
    /// All the speech is on the stream
    done: bool,
    /// Whether SDL played all of it, once it's all on the stream
    drain: Drain,
    /// The question it answers, if it's an answer
    timeline: Option<Arc<Timeline>>,
    /// Play the incomplete-answer tone once this speech has drained.
    tone_after_drain: bool,
    sentences: SentenceQueue,
    /// Made in one piece (a tone, "say again"): it may play in the gap of an answer
    /// [waiting on its turn](Self::waiting_on_its_turn)
    complete: bool,
    /// Its pilot turn is still running: more of it may come
    writing: bool,
    /// Paused: whether [`Self::set_held`]`(true)` is in effect
    held: bool,
    /// Its stream was flushed since its last piece went on, so all of it plays out: a stream
    /// that resamples holds some of its input back until then
    flushed: bool,
    /// Since when it's been [waiting on its turn](Self::waiting_on_its_turn), with nothing
    /// submitted since: [`Engineer::fill_gap`] waits [`GAP_AFTER`]
    waiting_since: Option<Instant>,
}

/// How long the answer at the front waits on its turn, with nothing to play, before another
/// plays in its gap. The speaker may still be playing what SDL has taken from the stream (its
/// queued bytes count only what SDL hasn't), and a short pause sounds natural on the radio.
const GAP_AFTER: Duration = Duration::from_millis(300);

/// How far behind where it was interrupted an answer resumes, in seconds.
const RESUME_BEHIND: f32 = 0.4;

/// How much later than that it may resume, in seconds, to start at a zero crossing: starting
/// halfway through a wave would click.
const RESUME_SNAP: f32 = 0.005;

/// Where to resume `speech`, a sentence, when it was interrupted at the sample `interrupted`: the
/// sentence's start if it began less than [`RESUME_BEHIND`] before that. A later start may still
/// fall in the silence before the sentence's voice, which then replays whole too.
fn resume_point(speech: &Speech, interrupted: usize) -> usize {
    let rate = speech.sample_rate as f32;
    let start = interrupted.saturating_sub((RESUME_BEHIND * rate).round() as usize);
    if start == 0 {
        return 0;
    }
    let snap = (RESUME_SNAP * rate).ceil() as usize;
    let crossing = speech.samples[start..]
        .windows(2)
        .take(snap)
        .position(|pair| (pair[0] < 0.0) != (pair[1] < 0.0));
    return start + crossing.map_or(0, |i| i + 1);
}

/// Input is always mono f32 at a fixed rate. SDL's queued bytes describe input
/// consumption, not the physical speaker position (which includes device buffering).
///
/// Each piece is a sentence, except the end of the transmission (the roger beep and the squelch
/// closing).
#[derive(Default)]
struct SentenceQueue {
    pieces: VecDeque<Speech>,
    submitted: usize,
    /// How many samples of the submitted pieces went on the stream
    samples: usize,
    /// Where the front piece resumes from: its samples before that aren't submitted
    resume: usize,
}

impl SentenceQueue {
    /// How many of the submitted samples were played, given what's `queued` on the stream.
    fn played(&self, queued: Option<i32>) -> Option<usize> {
        let queued = queued.and_then(|n| usize::try_from(n).ok())?;
        Some(
            self.samples
                .saturating_sub(queued.div_ceil(size_of::<f32>())),
        )
    }

    fn consumed(&mut self, queued: Option<i32>) {
        let Some(mut consumed) = self.played(queued) else {
            return; // Unknown position: replay conservatively from the oldest retained sentence.
        };
        while self.submitted > 0 {
            let len = self.pieces.front().unwrap().samples.len() - self.resume;
            if len > consumed {
                break;
            }
            consumed -= len;
            self.samples -= len;
            self.submitted -= 1;
            self.pieces.pop_front();
            self.resume = 0;
        }
    }

    /// The stream is about to be cleared, with `queued` left on it: everything unplayed will be
    /// submitted again, from where to resume.
    fn rewind(&mut self, queued: Option<i32>) {
        self.consumed(queued);
        self.resume = match (self.played(queued), self.pieces.front()) {
            // Nothing was played since the last resume: it's still where to resume from
            (Some(0), _) => self.resume,
            (Some(played), Some(front)) => resume_point(front, self.resume + played),
            _ => 0,
        };
        self.submitted = 0;
        self.samples = 0;
    }

    fn pending(&self) -> Option<&Speech> {
        self.pieces.get(self.submitted)
    }

    /// How many samples of the [pending](Self::pending) piece to skip.
    fn skip(&self) -> usize {
        match self.submitted {
            0 => self.resume,
            _ => 0,
        }
    }

    fn submitted(&mut self) {
        self.samples += self.pending().unwrap().samples.len() - self.skip();
        self.submitted += 1;
    }
}

impl Playback {
    /// `speech`, to play once it's at the front of the line. `complete`: it's made in one piece.
    pub fn new(speech: SpeechOutput, complete: bool) -> Self {
        Self {
            speech,
            stream: None,
            done: false,
            drain: Drain::new(Instant::now()),
            timeline: None,
            tone_after_drain: false,
            sentences: SentenceQueue::default(),
            complete,
            writing: false,
            held: false,
            flushed: false,
            waiting_since: None,
        }
    }

    /// The radio's low tone, as loud as the engineer: a request couldn't be answered.
    pub fn tone() -> Self {
        Self::new(SpeechOutput::ready(Speech::no_beep(1.0)), true)
    }

    /// Takes the next piece of speech, if it's ready, at `gain`: it's pending, to play. At the
    /// end (`Ready(None)`), it's [`done`](Self::done), and SDL plays what's left.
    fn pull(&mut self, gain: f32) -> Poll<Option<anyhow::Result<()>>> {
        match self.speech.try_next() {
            Poll::Pending => Poll::Pending,
            Poll::Ready(Some(Ok(mut speech))) => {
                for sample in &mut speech.samples {
                    *sample *= gain;
                }
                self.sentences.pieces.push_back(speech);
                Poll::Ready(Some(Ok(())))
            }
            Poll::Ready(Some(Err(e))) => Poll::Ready(Some(Err(e))),
            Poll::Ready(None) => {
                self.done = true;
                if let Some(stream) = &self.stream
                    && let Err(e) = stream.flush()
                {
                    log::warn!("Error flushing the answer's audio: {e}");
                }
                Poll::Ready(None)
            }
        }
    }

    /// It's caught up with what its pilot turn has written so far: what's on its stream plays out
    /// in full, once for each such stretch, so it can be seen to have played it all. Flushing
    /// after every piece instead could leave gaps between them.
    fn caught_up(&mut self) {
        if let Some(stream) = &self.stream
            && !std::mem::replace(&mut self.flushed, true)
            && let Err(e) = stream.flush()
        {
            log::warn!("Error flushing the answer's audio: {e}");
        }
    }

    /// Whether it's played all it has so far and waits on its pilot turn for more (e.g. "Copy,
    /// checking." said, the lookups still running): nothing is playing meanwhile, and its next
    /// piece isn't synthesized yet.
    fn waiting_on_its_turn(&self) -> bool {
        self.writing
            && self.sentences.pending().is_none()
            && self.speech.is_empty()
            && (self.stream.as_ref()).is_none_or(|stream| matches!(stream.queued_bytes(), Ok(0)))
    }

    /// Lets go of its stream: what it hasn't played goes out again, on whichever speaker is
    /// picked once it's next at the front. An answer [waiting on its
    /// turn](Self::waiting_on_its_turn) needs none, and holds none behind another.
    fn close_stream(&mut self) {
        if let Some(stream) = self.stream.take() {
            self.sentences.rewind(stream.queued_bytes().ok());
        }
        self.flushed = false;
        self.waiting_since = None;
        self.drain = Drain::new(Instant::now());
    }

    /// Pauses it, ready to resume [`RESUME_BEHIND`] before where it stopped, or resumes it.
    /// Recorded even without a stream to pause, and nothing is done if it's already so.
    fn set_held(&mut self, held: bool) -> Result<(), sdl3::Error> {
        if std::mem::replace(&mut self.held, held) == held {
            return Ok(());
        }
        let Some(stream) = &self.stream else {
            return Ok(());
        };
        if held {
            stream.pause()?;
            self.sentences.rewind(stream.queued_bytes().ok());
            stream.clear()?;
        } else {
            stream.resume()?;
        }
        self.drain = Drain::new(Instant::now());
        Ok(())
    }
}

struct Session {
    /// The database the tool reads
    db: PathBuf,
    /// The model settings it was started with: changing them starts over
    model: ModelConfig,
    /// Whether corners are called by their names: the system prompt says how, so changing it
    /// starts over too
    corner_names: bool,
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
    pub fn new(client: reqwest::Client, account: Option<Account>) -> Self {
        Self {
            client,
            account,
            skill: instructions(SKILL.to_owned()).into(),
            chat: Vec::new(),
            last_lang: arrstr!("en"),
            languages: LanguageDetector::new(),
            tool: None,
            session: None,
            new_session: false,
            turn: None,
            incoming: Vec::new(),
            line: VecDeque::new(),
            spotter_held: false,
            talking: false,
            misheard: 0,
        }
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

    /// Whether an answer is being spoken, or is about to be: anything in the line, or waiting
    /// to join it.
    #[inline]
    pub fn is_speaking(&self) -> bool {
        !self.line.is_empty() || !self.incoming.is_empty()
    }

    /// Adds `playback` to the line: how any answer joins it, a pilot turn's or one made
    /// elsewhere (e.g. scheduled). It waits in `incoming` until its first piece of speech is
    /// ready, then joins the back of the line (see [`Self::admit`]).
    pub fn queue(&mut self, playback: Playback) {
        self.incoming.push(playback);
    }

    /// The configured input language, or the language the pilot last asked in.
    #[inline]
    pub fn language(&self, config: &Config) -> ArrayStr<2> {
        config
            .get()
            .heard_language()
            .map(language::voice_code)
            .unwrap_or(self.last_lang)
    }

    /// Pauses the answer at the front of the line (and holds one about to start) while the
    /// spotter speaks; releasing it resumes `RESUME_BEHIND` before where it stopped, or from the
    /// sentence's start if that sentence began since then.
    pub fn hold(&mut self, held: bool) {
        self.spotter_held = held;
        self.hold_front();
    }

    /// Pauses the answer at the front of the line (and holds one about to start) while the
    /// talk button is held, as [`Self::hold`] does for the spotter, so it doesn't play over the
    /// pilot. It stays paused until both let go.
    pub fn talk(&mut self, talking: bool) {
        self.talking = talking;
        self.hold_front();
    }

    /// Whether the front of the line waits: the spotter is speaking or the pilot is talking.
    fn held(&self) -> bool {
        self.spotter_held || self.talking
    }

    /// Holds the front of the line as [`Self::held`] says, whichever answer is there. Nothing
    /// is done when it's already so (see [`Playback::set_held`]).
    fn hold_front(&mut self) {
        let held = self.held();
        let Some(playback) = self.line.front_mut() else {
            return;
        };
        if let Err(e) = playback.set_held(held) {
            log::warn!("Error holding the answer: {e}");
            self.line.pop_front();
        }
    }

    pub fn microphone_removed(&mut self) {
        self.notice(t!("engineer.microphone_removed"));
    }

    /// The speaker `which` is gone (unplugged): the answer being spoken on it stops. Only the
    /// front of the line holds a stream (see [`Self::fill_gap`]), and one only waiting on its
    /// turn there keeps its place: the rest of it plays on whichever speaker is picked, as the
    /// others do. `picked`: whether it's the speaker picked, which the pilot is told about
    /// either way.
    pub fn speaker_removed(&mut self, which: u32, picked: bool) {
        let on_it = (self.line.front())
            .and_then(|playback| playback.stream.as_ref())
            .is_some_and(|stream| plays_on(stream, which));
        if on_it {
            if self.line[0].waiting_on_its_turn() {
                self.line[0].close_stream();
            } else {
                self.line.pop_front();
            }
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
    /// with why it can't be, after whatever is being said: nothing heard stops the engineer.
    pub fn hear(
        &mut self,
        rt: &Handle,
        sim: Option<&mut Sim>,
        config: &Config,
        synth: &mut Synthesizer,
        heard: Heard,
    ) {
        match heard {
            Heard::Question {
                text,
                lang,
                released,
            } => {
                self.misheard = 0;
                self.ask(rt, sim, config, synth, text, Some(lang), Some(released));
            }
            Heard::TooShort => {}
            Heard::Unvoiced { text, lang } => {
                let language = language::ui_name(lang);
                self.notice(t!(
                    "engineer.no_voice_for",
                    language = language,
                    text = text
                ));
                self.say_again(rt, config, synth);
            }
            Heard::Nothing => {
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
                self.notice(t!("engineer.error_hearing", error = format!("{e:#}")));
                self.say_again(rt, config, synth);
            }
            // The voice may not be there either: the tone says it
            Heard::NotListening(why) => {
                self.notice(match why {
                    NotListening::NoMicrophone => t!("engineer.no_microphone"),
                    NotListening::NoModel => t!("engineer.no_stt_model"),
                    NotListening::Loading => t!("engineer.stt_loading"),
                });
                self.no_beep();
            }
            Heard::NoVoice(lang) => {
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
            Heard::UnsupportedLanguage(lang) => {
                self.notice(t!(
                    "engineer.stt_unsupported_language",
                    language = language::ui_name(lang)
                ));
                self.no_beep();
            }
        }
    }

    /// Asks the pilot to say it again in the selected reply language, or their last language.
    /// Without any voice, the radio's low tone says it can't answer.
    fn say_again(&mut self, rt: &Handle, config: &Config, synth: &mut Synthesizer) {
        let spoken = config.get().engineer_reply_language();
        let lang = spoken.unwrap_or_else(|| self.language(config));
        match self.voice(config, lang) {
            Some(lang) => {
                let text = t!("engineer.say_again": &lang);
                self.say(rt, config, synth, text, lang, None);
            }
            None => self.no_beep(),
        }
    }

    /// The radio's low tone, as loud as the engineer, at the back of the line: the request
    /// couldn't be answered.
    fn no_beep(&mut self) {
        self.queue(Playback::tone());
    }

    /// Preserve the answer still being synthesized or played before sounding its failure tone:
    /// it follows the answer of the pilot turn still running, if it has one.
    fn incomplete_beep(&mut self) {
        let writing = (self.incoming.iter_mut().chain(&mut self.line)).find(|p| p.writing);
        match writing {
            Some(playback) => playback.tone_after_drain = true,
            None => self.no_beep(),
        }
    }

    /// Follows the running simulator's recording, as soon as it has one and however far into a
    /// session that is. Called every frame: asking the simulator for its recording doesn't block,
    /// and starting or dropping the tool does its work on other threads.
    pub fn follow_recording(&mut self, rt: &Handle, sim: Option<&mut Sim>) {
        let pool = sim.and_then(|sim| sim.database());
        // Another simulator may still be running, or this recording may disappear and return
        // at the same path. Release its conversation now rather than at the next question.
        if self
            .session
            .as_ref()
            .is_some_and(|session| pool.is_none_or(|pool| session.db != pool.database().path()))
        {
            let interrupted = self.turn.is_some();
            self.stop();
            self.drop_session(rt);
            self.notice(t!("engineer.sim_stopped"));
            if interrupted {
                self.no_beep();
            }
        }
        match pool {
            Some(pool) => {
                self.tool_for(rt, pool);
            }
            None => self.drop_tool(rt),
        }
    }

    /// The tool for `pool`'s recording, started if it isn't the one being followed.
    fn tool_for(&mut self, rt: &Handle, pool: &Pool) -> EngineerTool {
        if let Some(tool) = &self.tool
            && tool.path() == pool.database().path()
        {
            return tool.clone();
        }
        self.drop_tool(rt);
        self.tool.insert(EngineerTool::new(pool.clone())).clone()
    }

    /// Lets go of the recording. Closing the last of its pool's connections joins their threads:
    /// not on the UI thread.
    fn drop_tool(&mut self, rt: &Handle) {
        if let Some(tool) = self.tool.take() {
            rt.spawn_blocking(move || drop(tool));
        }
    }

    /// A conversation may hold the last pool reference once its recording stops being followed.
    fn drop_session(&mut self, rt: &Handle) {
        if let Some(session) = self.session.take() {
            rt.spawn_blocking(move || drop(session));
        }
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
        let heard = *config.get().heard_language();
        let lang = match heard.or_else(|| self.languages.detect(&question)) {
            Some(lang) => Some(lang),
            // Too short or ambiguous for any language in particular ("Positions."): the one the
            // pilot was speaking
            None if self.languages.is_ready() => Some(self.last_lang),
            // Nothing to tell languages apart with: the model goes by the question
            None => None,
        };
        self.ask(rt, sim, config, synth, question, lang, None);
    }

    /// The pilot asked something on the radio: it's sent to the model, with the running
    /// simulator's telemetry, and the answer is spoken as the model writes it. Asked while a
    /// question is being answered, it steers that turn instead (see [`Turn::steer`]); what's
    /// being said carries on.
    ///
    /// `lang`: the language it was asked in; `None` if it isn't known. Unless a reply language
    /// is selected, the model is told to answer in this language, or goes by the question if
    /// unknown, spoken in the last language the pilot used.
    ///
    /// `released`: when the talk button was let go, if it was asked on the radio.
    #[allow(clippy::too_many_arguments)]
    pub fn ask(
        &mut self,
        rt: &Handle,
        sim: Option<&mut Sim>,
        config: &Config,
        synth: &mut Synthesizer,
        question: String,
        lang: Option<ArrayStr<2>>,
        released: Option<Instant>,
    ) {
        if let Some(lang) = lang {
            self.last_lang = language::voice_code(lang);
        }
        self.chat.push(ChatEntry {
            speaker: Speaker::Pilot,
            text: question.clone(),
            detail: None,
        });
        // The turn hears it at its next tool hand-back, or answers it next
        if let Some(turn) = &mut self.turn {
            turn.steer.push(question);
            let first = turn.steered.map_or(released, |(_, first)| first);
            turn.steered = Some((lang, first));
            return;
        }
        self.send_question(rt, sim, config, synth, question, lang, released);
    }

    /// Sends `question` to the model on the running simulator's recording, starting the
    /// conversation over first if it must be (a new session was asked for, the model settings
    /// changed, another recording), or answers why it can't be. `lang` and `released`: as given
    /// to [`Self::ask`], which has taken in `lang`.
    #[allow(clippy::too_many_arguments)]
    fn send_question(
        &mut self,
        rt: &Handle,
        sim: Option<&mut Sim>,
        config: &Config,
        synth: &mut Synthesizer,
        question: String,
        lang: Option<ArrayStr<2>>,
        released: Option<Instant>,
    ) {
        // Whisper distinguishes Nynorsk (`nn`), but both Norwegian forms use the `no` voice.
        let lang = lang.map(language::voice_code);
        let spoken = config.get().engineer_reply_language();
        let known = lang.is_some() || spoken.is_some();
        let lang = spoken.unwrap_or(lang.unwrap_or(self.last_lang));
        let timeline = Arc::new(Timeline::new(released, Instant::now()));
        let mut sim = sim;
        let database = sim
            .as_deref_mut()
            .and_then(|sim| Some((sim.id(), sim.database()?.clone())));
        // After `database`, which takes in the latest look at the plugin
        let outdated = sim.as_deref().is_some_and(|sim| sim.plugin_outdated());
        let Some((sim, pool)) = database else {
            // Said in the pilot's language, when there's a voice for it
            let lang = self.voice(config, lang).unwrap_or(lang);
            let (text, detail) = match outdated {
                true => (
                    t!("engineer.plugin_outdated": &lang),
                    t!("engineer.detail_plugin_outdated"),
                ),
                false => (
                    t!("engineer.no_telemetry": &lang),
                    t!("engineer.detail_no_telemetry"),
                ),
            };
            self.reply(text.into(), Some(detail.into()));
            self.say(rt, config, synth, text, lang, Some(timeline));
            return;
        };

        let db = pool.database().path().to_owned();
        let model_changed = self
            .session
            .as_ref()
            .is_some_and(|s| s.model != config.get().model);
        let corners_changed = self
            .session
            .as_ref()
            .is_some_and(|s| s.corner_names != *config.get().corner_names());
        if model_changed {
            self.notice(t!("engineer.new_model"));
        }
        if self.new_session
            || model_changed
            || corners_changed
            || self.session.as_ref().is_none_or(|s| s.db != db)
        {
            let tool = self.tool_for(rt, &pool);
            match self.start(config, sim, pool, tool) {
                Ok(session) => {
                    self.drop_session(rt);
                    self.session = Some(session);
                }
                Err(e) => {
                    self.notice(t!("engineer.error_starting", error = format!("{e:#}")));
                    self.no_beep();
                    return;
                }
            }
            self.new_session = false;
        }
        self.start_turn(rt, config, synth, question, lang, known, timeline);
    }

    /// Starts a turn on the current session for `question`, answered in `lang` (`known`: told
    /// to the model, see [`Self::ask`]).
    #[allow(clippy::too_many_arguments)]
    fn start_turn(
        &mut self,
        rt: &Handle,
        config: &Config,
        synth: &mut Synthesizer,
        question: String,
        lang: ArrayStr<2>,
        known: bool,
        timeline: Arc<Timeline>,
    ) {
        let Some(session) = &self.session else {
            return;
        };
        let state = session.state.clone();
        let auto_session = *config.get().auto_session();

        // What the model writes goes straight to the synthesizer, and to the chat
        let speech = self.speak(rt, config, synth, lang, Some(timeline.clone()));
        let lang = speech.as_ref().map_or(lang, |(lang, _)| *lang);
        let speech = speech.map(|(_, speech)| speech);
        // Told last, after the question (and its snapshot) and after each tool result: by the
        // answer, the skill, the telemetry and often earlier answers are all in English
        let note: Option<Arc<str>> = known.then(|| language::reply_note(lang).into());
        let (chat, text) = mpsc::unbounded_channel();
        let steer = Steer::default();
        let slot = steer.clone();
        let task = Task::spawn(async move {
            let mut state = state.lock().await;
            state.follow_game_session(auto_session).await;
            let question = state.tool.question_with_snapshot(question).await;
            let question = match &note {
                Some(note) => format!("{question}\n\n{note}"),
                None => question,
            };
            state.conversation.set_tool_result_note(note);
            state.conversation.set_steer(Some(slot));
            let mut on_text = |text: &str| {
                timeline.first_text();
                if let Some(speech) = &speech {
                    speech.push(text);
                }
                let _ = chat.send(text.to_owned());
            };
            let reply = state
                .conversation
                .send_streaming(question, &mut on_text)
                .await;
            if reply.is_ok() {
                timeline.answered();
            }
            reply
            // The speech's text ends here, with the turn
        });

        self.turn = Some(Turn {
            task,
            lang,
            text,
            entry: None,
            steer,
            steered: None,
        });
    }

    /// Called every frame: answers come in, are synthesized and played. `sim`: the running
    /// simulator, if any, for the questions a turn left over.
    pub fn update(
        &mut self,
        rt: &Handle,
        sim: Option<&mut Sim>,
        config: &Config,
        synth: &mut Synthesizer,
        speaker: Option<&AudioDevice>,
    ) {
        self.languages.update(rt, config.get().voices());
        // The game stopped: the conversation lets go of the database, so the plugin can delete it
        if sim.is_none() && self.session.is_some() {
            let interrupted = self.turn.take().is_some();
            self.drop_session(rt);
            self.notice(t!("engineer.sim_stopped"));
            if interrupted {
                // What it was saying goes with the turn, which the conversation forgot
                self.line.clear();
                self.incoming.clear();
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
            self.drop_session(rt);
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
                // Questions the turn missed (it was writing its final text) are asked next, as
                // any question is. A failed or refused turn was forgotten, and they go with it:
                // they may build on its question
                let missed = match &result {
                    Ok(reply) if reply.stop != StopReason::Refusal => turn.steer.take(),
                    Ok(_) | Err(_) => Vec::new(),
                };
                let (lang, released) = turn.steered.unwrap_or_default();
                match result {
                    Ok(reply) => self.answer(rt, config, synth, reply, turn),
                    Err(e) => {
                        if let Some(entry) = turn.entry {
                            self.chat[entry].detail = Some(t!("engineer.detail_cut_off").into());
                        }
                        self.notice(t!("engineer.error_asking", error = format!("{e:#}")));
                        // What it said before failing plays out first
                        self.incomplete_beep();
                    }
                }
                // Its answer has all it will have
                for playback in self.incoming.iter_mut().chain(&mut self.line) {
                    playback.writing = false;
                }
                if !missed.is_empty() {
                    let question = missed.join("\n");
                    self.send_question(rt, sim, config, synth, question, lang, released);
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
                // Its answer stops, unheard or not, as the chat only shows "say again"
                self.incoming.retain(|playback| !playback.writing);
                self.line.retain(|playback| !playback.writing);
                let say_again = t!("engineer.say_again": &turn.lang);
                self.say(rt, config, synth, say_again, turn.lang, None);
                if self.voice(config, turn.lang).is_none() {
                    self.no_beep();
                }
                say_again.to_owned()
            }
            StopReason::MaxTokens => {
                detail = format!("{detail} | {}", t!("engineer.detail_cut_off"));
                self.incomplete_beep();
                reply.text
            }
            // Every later question would end the same way: the next one starts over
            StopReason::ContextWindowExceeded => {
                self.new_session = true;
                detail = format!("{detail} | {}", t!("engineer.detail_full"));
                self.incomplete_beep();
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

    /// Speaks `text` in `lang` (see [`Self::voice`]), synthesized in one piece and added to the
    /// line: nothing without any voice. `timeline`: the question it answers, if any.
    fn say(
        &mut self,
        rt: &Handle,
        config: &Config,
        synth: &mut Synthesizer,
        text: &str,
        lang: ArrayStr<2>,
        timeline: Option<Arc<Timeline>>,
    ) {
        let Some(lang) = self.voice(config, lang) else {
            return;
        };
        let effects = config.get().synth_effects();
        let speech = synth.synthesize_output(rt, lang, text.to_owned(), effects);
        let mut playback = Playback::new(speech, true);
        playback.timeline = timeline;
        self.queue(playback);
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

    /// Starts an answer to speak what's pushed on the returned input (in `lang`, see
    /// [`Self::voice`], as returned), written by the pilot turn starting: it joins the line once
    /// its first sentence is synthesized. `timeline`: the question it answers. `None` without
    /// any voice.
    fn speak(
        &mut self,
        rt: &Handle,
        config: &Config,
        synth: &mut Synthesizer,
        lang: ArrayStr<2>,
        timeline: Option<Arc<Timeline>>,
    ) -> Option<(ArrayStr<2>, SpeechInput)> {
        let lang = self.voice(config, lang)?;
        let effects = config.get().synth_effects();

        let (input, speech) = synth.stream(rt, lang, effects);
        let mut playback = Playback::new(speech, false);
        playback.writing = true;
        playback.timeline = timeline;
        self.queue(playback);
        Some((lang, input))
    }

    /// Answers whose first piece of speech is ready join the back of the line, in the order
    /// they were queued. One that ends without saying anything never joins it.
    fn admit(&mut self, gain: f32) {
        let mut i = 0;
        while i < self.incoming.len() {
            match self.incoming[i].pull(gain) {
                Poll::Pending => i += 1,
                Poll::Ready(Some(Ok(()))) => {
                    let playback = self.incoming.remove(i);
                    self.line.push_back(playback);
                }
                Poll::Ready(Some(Err(e))) => {
                    self.incoming.remove(i);
                    self.notice(t!("engineer.error_synthesizing", error = format!("{e:#}")));
                    self.no_beep();
                }
                // It said nothing
                Poll::Ready(None) => {
                    if self.incoming.remove(i).tone_after_drain {
                        self.no_beep();
                    }
                }
            }
        }
    }

    /// The gap: once the answer at the front has played all it has and waited on its pilot turn
    /// for more for [`GAP_AFTER`], the first complete answer behind it plays. The waiting one
    /// stays next, giving up its stream, and carries on once that's played. This is the one
    /// place an answer is moved ahead in the line, and only complete ones are, so a gap never
    /// opens another, and only the front holds a stream.
    fn fill_gap(&mut self, now: Instant) {
        let Some(front) = self.line.front_mut() else {
            return;
        };
        if !front.waiting_on_its_turn() {
            front.waiting_since = None;
            return;
        }
        if now.duration_since(*front.waiting_since.get_or_insert(now)) < GAP_AFTER {
            return;
        }
        // An answer starts playing at the front, or here: a check that drops one gone stale
        // before it starts (scheduled requests) goes in both places
        if let Some(complete) = self.line.iter().position(|playback| playback.complete) {
            self.line[0].close_stream();
            let playback = self.line.remove(complete).expect("just found");
            self.line.push_front(playback);
        }
    }

    /// Plays the line: what the answer at its front has synthesized so far.
    fn play(&mut self, config: &Config, speaker: Option<&AudioDevice>) {
        let gain = {
            let config = config.get();
            config.master_volume() * config.engineer_volume()
        };
        self.admit(gain);
        // Whichever answer is at the front waits while the spotter speaks or the pilot talks
        self.hold_front();
        let held = self.held();
        let Some(playback) = self.line.front_mut() else {
            return;
        };

        if held {
            return;
        }
        if let Some(stream) = &playback.stream {
            playback.sentences.consumed(stream.queued_bytes().ok());
        }

        // Submit at most one sentence per frame, including when replaying. Synthesis
        // stays on its worker and a long retained answer cannot monopolize the UI.
        if !playback.done && playback.sentences.pending().is_none() {
            match playback.pull(gain) {
                Poll::Ready(Some(Err(e))) => {
                    self.line.pop_front();
                    self.notice(t!("engineer.error_synthesizing", error = format!("{e:#}")));
                    self.no_beep();
                    return;
                }
                // Nothing more yet: what it said plays out while its turn works on the rest
                Poll::Pending if playback.writing => playback.caught_up(),
                Poll::Pending | Poll::Ready(_) => {}
            }
        }

        if let Some(speech) = playback.sentences.pending() {
            let skip = playback.sentences.skip();
            let played = match (&playback.stream, speaker) {
                (Some(stream), _) => stream.put_data_f32(&speech.samples[skip..]),
                // On whichever speaker is picked, from where it resumes
                (None, Some(speaker)) => speech.start(speaker, skip).and_then(|stream| {
                    playback.stream = Some(stream);
                    Ok(())
                }),
                (None, None) => {
                    self.line.pop_front();
                    self.notice(t!("engineer.no_speaker"));
                    return;
                }
            };

            if let Err(e) = played {
                self.line.pop_front();
                self.notice(t!("engineer.error_playing", error = e));
                return;
            }
            playback.sentences.submitted();
            playback.flushed = false;
            playback.waiting_since = None;
            if let Some(timeline) = &playback.timeline {
                timeline.first_audio();
            }
            if playback.done && playback.sentences.pending().is_none() {
                if let Some(stream) = &playback.stream
                    && let Err(e) = stream.flush()
                {
                    log::warn!("Error flushing the resumed answer's audio: {e}");
                }
            }
        }

        // Once SDL has played all of it, the stream can go. A speaker that stops taking audio
        // without SDL saying it was removed doesn't keep it forever, holding up the spotter's calls
        if !playback.done || playback.sentences.pending().is_some() {
            // More to come: something else may play while it waits for it
            self.fill_gap(Instant::now());
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
            Drained::Done => {
                // Not marked if none of it reached the speaker (see `Timeline::played`)
                if let Some(timeline) = &playback.timeline {
                    timeline.played();
                }
                let tone = playback.tone_after_drain;
                self.line.pop_front();
                // Right after the answer it belongs to; the next one starts on a later frame
                if tone {
                    self.line.push_front(Playback::tone());
                }
            }
            Drained::Stalled => {
                self.line.pop_front();
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

    /// Drops the answer on its way, with the questions steering it, and the whole line,
    /// stopping the one being spoken.
    fn stop(&mut self) {
        if let Some(turn) = self.turn.take() {
            if let Some(entry) = turn.entry {
                self.chat[entry].detail = Some(t!("engineer.detail_cut_off").into());
            }
            self.notice(t!("engineer.cancelled"));
        }
        self.incoming.clear();
        self.line.clear();
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

    fn start(
        &self,
        config: &Config,
        sim: &'static str,
        pool: Pool,
        tool: EngineerTool,
    ) -> anyhow::Result<Session> {
        let mut tools = Tools::default();
        tools.insert(
            EngineerTool::NAME,
            Some(EngineerTool::description()),
            EngineerTool::input_schema(),
            tool.clone(),
        );
        let model = config.get().model.clone();
        let corner_names = *config.get().corner_names();
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
            system_prompt(&self.skill, corner_names),
            tools,
            self.client.clone(),
        )?;

        let db = pool.database().path().to_owned();
        log::info!("Engineer session started on {}", db.display());
        Ok(Session {
            db,
            model,
            corner_names,
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
    pub fn quit(&mut self, rt: &Handle) -> tokio::task::JoinHandle<()> {
        /// The longest the app waits on the website to quit
        const WAIT: Duration = Duration::from_secs(3);

        self.stop();
        let session = self.session.take();
        let tool = self.tool.take();
        rt.spawn(async move {
            if let Some(lease) = session.as_ref().and_then(|s| s.lease.as_ref())
                && tokio::time::timeout(WAIT, lease.end_and_wait())
                    .await
                    .is_err()
            {
                log::warn!("Gave up ending the Cloud session lease: it expires on its own");
            }
            // Closing the final pool reference joins SQLite workers, including an interrupted
            // snapshot's job. Keep this off both the UI and the async executor's workers.
            let _ = tokio::task::spawn_blocking(move || drop((session, tool))).await;
        })
    }
}

/// How the engineer calls corners: by number unless the pilot asked for their names
const CORNERS_BY_NUMBER: &str = "Call corners by their number (\"turn 5\"), not their name, even \
     when the track data has one, unless the pilot named the corner first. If the pilot asks \
     what a corner is called, tell them its name.";
const CORNERS_BY_NAME: &str = "Call corners by their name (\"Tertre Rouge\") when the track data \
     has one, and by their number (\"turn 5\") only when it doesn't.";

/// The skill's instructions with how to call corners.
fn system_prompt(skill: &str, corner_names: bool) -> Arc<str> {
    let corners = match corner_names {
        true => CORNERS_BY_NAME,
        false => CORNERS_BY_NUMBER,
    };
    format!("{skill}\n\n## Corners\n\n{corners}\n").into()
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
    fn selected_reply_language_keeps_the_pilots_language_independent() {
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let _entered = runtime.enter();
        let rt = runtime.handle();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        // Voice paths only stand in for installed choices; no model or audio device is needed.
        std::fs::write(
            &path,
            r#"{
            "voices": {"en": "en.onnx", "es": "es.onnx"},
            "spoken_language": "en",
            "heard_language": "es"
        }"#,
        )
        .unwrap();
        let config = runtime.block_on(Config::new(&path)).unwrap();
        let mut synth = Synthesizer::new(rt, &crate::config::RawConfig::default());
        let mut engineer = Engineer::new(reqwest::Client::new(), None);
        engineer.ask_text(
            rt,
            None,
            &config,
            &mut synth,
            "¿Cuánto combustible me queda?".into(),
        );
        assert_eq!(engineer.language(&config), crate::arrstr!("es"));
        assert_eq!(
            engineer.chat.last().unwrap().text,
            t!("engineer.no_telemetry": "en")
        );
        assert!(synth.inner.contains_key(&crate::arrstr!("en")));
        assert!(!synth.inner.contains_key(&crate::arrstr!("es")));

        // The engineer's output preference must not override the spotter's voice selection.
        assert_eq!(
            engineer.voice(&config, crate::arrstr!("es")),
            Some(crate::arrstr!("es"))
        );
        engineer.stop();
    }

    #[test]
    fn fixed_input_language_applies_to_say_again_before_a_question_succeeds() {
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let _entered = runtime.enter();
        let rt = runtime.handle();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        std::fs::write(
            &path,
            r#"{
            "voices": {"en": "en.onnx", "es": "es.onnx"},
            "heard_language": "es"
        }"#,
        )
        .unwrap();
        let config = runtime.block_on(Config::new(&path)).unwrap();
        let mut synth = Synthesizer::new(rt, &crate::config::RawConfig::default());
        let mut engineer = Engineer::new(reqwest::Client::new(), None);
        assert_eq!(engineer.last_lang, arrstr!("en"));
        engineer.hear(rt, None, &config, &mut synth, Heard::Nothing);
        assert_eq!(engineer.language(&config), arrstr!("es"));
        assert!(synth.inner.contains_key(&arrstr!("es")));
        assert!(!synth.inner.contains_key(&arrstr!("en")));
        config.update(|raw| raw.heard_language.set(Some(arrstr!("en"))));
        assert_eq!(engineer.language(&config), arrstr!("en"));
        engineer.stop();
    }

    #[test]
    fn nynorsk_input_uses_the_norwegian_voice_for_replies_and_the_spotter() {
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let _entered = runtime.enter();
        let rt = runtime.handle();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        std::fs::write(
            &path,
            r#"{
            "voices": {"en": "en.onnx", "no": "no.onnx"},
            "heard_language": "nn"
        }"#,
        )
        .unwrap();
        let config = runtime.block_on(Config::new(&path)).unwrap();
        let mut synth = Synthesizer::new(rt, &crate::config::RawConfig::default());
        let mut engineer = Engineer::new(reqwest::Client::new(), None);
        for radio in [false, true] {
            let text = "Kor mykje drivstoff har eg att?".to_owned();
            if radio {
                engineer.hear(
                    rt,
                    None,
                    &config,
                    &mut synth,
                    Heard::Question {
                        text,
                        lang: arrstr!("nn"),
                        released: Instant::now(),
                    },
                );
            } else {
                engineer.ask_text(rt, None, &config, &mut synth, text);
            }
            assert_eq!(engineer.language(&config), arrstr!("no"));
            assert_eq!(
                engineer.voice(&config, engineer.language(&config)),
                Some(arrstr!("no"))
            );
            assert!(synth.inner.contains_key(&arrstr!("no")));
            assert!(!synth.inner.contains_key(&arrstr!("en")));
        }
        engineer.stop();
    }

    fn sentences(lengths: &[usize]) -> SentenceQueue {
        let mut queue = SentenceQueue::default();
        for (index, &len) in lengths.iter().enumerate() {
            queue.pieces.push_back(Speech {
                samples: vec![index as f32; len],
                sample_rate: 22050,
            });
            queue.submitted();
        }
        queue
    }

    #[test]
    fn resumes_first_unfinished_sentence() {
        for (queued, remaining) in [(120, 3), (84, 3), (80, 2), (44, 2), (40, 1), (0, 0)] {
            let mut queue = sentences(&[10, 10, 10]);
            queue.rewind(Some(queued));
            assert_eq!(queue.pieces.len(), remaining, "queued bytes: {queued}");
            assert_eq!(queue.skip(), 0, "queued bytes: {queued}");
            if remaining > 0 {
                assert_eq!(queue.pending().unwrap().samples.len(), 10);
                assert_eq!(queue.pending().unwrap().samples[0], (3 - remaining) as f32);
            }
        }
    }

    /// What's left on the stream, in bytes, once `played` of the `total` samples on it were.
    fn left(total: usize, played: usize) -> Option<i32> {
        Some(((total - played) * size_of::<f32>()) as i32)
    }

    /// [`RESUME_BEHIND`] at the sample rate of [`sentences`].
    const BEHIND: usize = 8820;

    #[test]
    fn resumes_behind_where_it_was_interrupted() {
        let mut queue = sentences(&[22050, 22050]);
        queue.rewind(left(44100, 11025));
        assert_eq!(queue.pieces.len(), 2);
        assert_eq!(queue.submitted, 0);
        assert_eq!(queue.skip(), 11025 - BEHIND);
        assert_eq!(queue.pending().unwrap().samples[0], 0.0);

        // Just far enough into the sentence for its start to be out of the window
        let mut queue = sentences(&[22050]);
        queue.rewind(left(22050, BEHIND + 1));
        assert_eq!(queue.skip(), 1);
    }

    #[test]
    fn restarts_a_sentence_that_began_in_the_window() {
        for played in [1, BEHIND - 1, BEHIND] {
            let mut queue = sentences(&[22050, 22050]);
            queue.rewind(left(44100, 22050 + played));
            assert_eq!(queue.pieces.len(), 1, "played: {played}");
            assert_eq!(queue.skip(), 0, "played: {played}");
            assert_eq!(queue.pending().unwrap().samples[0], 1.0);
        }
    }

    #[test]
    fn a_resumed_sentence_tracks_its_position() {
        let mut queue = sentences(&[22050, 22050]);
        queue.rewind(left(44100, 11025));
        let resume = 11025 - BEHIND;
        assert_eq!(queue.skip(), resume);

        // Nothing played since: not further back, whether or not it was resubmitted
        queue.rewind(Some(0));
        assert_eq!(queue.skip(), resume);
        queue.submitted();
        assert_eq!(queue.samples, 22050 - resume);
        queue.rewind(left(22050 - resume, 0));
        assert_eq!(queue.skip(), resume);

        // Measured from where it resumed, not the sentence's start
        queue.submitted();
        queue.rewind(left(22050 - resume, 13230));
        assert_eq!(queue.skip(), resume + 13230 - BEHIND);

        // Played shortly after resuming: back to its start at most
        let total = 22050 - queue.skip();
        queue.submitted();
        queue.rewind(left(total, 1));
        assert_eq!(queue.skip(), 0);
        assert_eq!(queue.pieces.len(), 2);

        // The next sentence starts in full once this one is played out
        let mut queue = sentences(&[22050]);
        queue.rewind(left(22050, 11025));
        queue.pieces.push_back(Speech {
            samples: vec![1.0; 100],
            sample_rate: 22050,
        });
        let total = 22050 - queue.skip();
        queue.submitted();
        queue.submitted();
        queue.consumed(left(total + 100, total));
        assert_eq!(queue.pieces.len(), 1);
        assert_eq!(queue.samples, 100);
        assert_eq!(queue.resume, 0);
    }

    #[test]
    fn resumes_at_a_zero_crossing_nearby() {
        let mut samples = vec![0.5; 22050];
        samples[BEHIND + 50..].fill(-0.5);
        let speech = Speech {
            samples,
            sample_rate: 22050,
        };
        // The crossing is within reach
        assert_eq!(resume_point(&speech, 2 * BEHIND), BEHIND + 50);
        // Past it: the next is too far to wait for
        assert_eq!(resume_point(&speech, 2 * BEHIND + 100), BEHIND + 100);
        // Before it, and too far
        assert_eq!(resume_point(&speech, BEHIND + 1), 1);
    }

    #[test]
    fn repeated_interruptions_preserve_unsubmitted_sentences() {
        let mut queue = sentences(&[10, 20, 30]);
        queue.rewind(Some(160)); // first sentence finished; second half spoken
        queue.submitted(); // only the second sentence has been replayed so far
        queue.rewind(Some(40));
        assert_eq!(queue.pending().unwrap().samples, vec![1.0; 20]);
        queue.submitted();
        queue.rewind(Some(0));
        assert_eq!(queue.pending().unwrap().samples, vec![2.0; 30]);
    }

    #[test]
    fn unknown_positions_retain_audio_and_empty_pieces_do_not_stick() {
        for queued in [None, Some(-1)] {
            let mut queue = sentences(&[10, 20]);
            queue.rewind(queued);
            assert_eq!(queue.pieces.len(), 2);
            assert_eq!(queue.skip(), 0);
        }

        // An unknown position restarts a sentence that was being resumed
        let mut queue = sentences(&[22050]);
        queue.rewind(left(22050, 11025));
        assert_ne!(queue.skip(), 0);
        queue.submitted();
        queue.rewind(None);
        assert_eq!(queue.skip(), 0);

        let mut queue = sentences(&[0, 10, 0]);
        queue.consumed(Some(40));
        assert_eq!(queue.pieces.len(), 2);
        queue.consumed(Some(0));
        assert!(queue.pieces.is_empty());
    }

    #[test]
    #[ignore = "run alone with SDL_AUDIO_DRIVER=dummy"]
    fn completed_speech_replays_after_spotter_hold() {
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let _entered = runtime.enter();
        let dir = tempfile::tempdir().unwrap();
        let config = runtime
            .block_on(Config::new(dir.path().join("config.json")))
            .unwrap();
        let sdl = sdl3::init().unwrap();
        let audio = sdl.audio().unwrap();
        let mut sentences = sentences(&[22050, 22050]);
        let device = audio
            .open_playback_device(&sentences.pieces[0].spec())
            .unwrap();
        let stream = device
            .clone()
            .open_device_stream(Some(&sentences.pieces[0].spec()))
            .unwrap();
        stream.pause().unwrap();
        for speech in &sentences.pieces {
            stream.put_data_f32(&speech.samples).unwrap();
        }
        stream.flush().unwrap();
        // Simulate the first sentence and half of the second having drained.
        stream.clear().unwrap();
        stream
            .put_data_f32(&sentences.pieces[1].samples[11025..])
            .unwrap();
        sentences.consumed(stream.queued_bytes().ok());
        assert_eq!(sentences.pieces.len(), 1);

        let timeline = Arc::new(Timeline::new(None, Instant::now()));
        let mut engineer = Engineer::new(reqwest::Client::new(), None);
        let mut playback = Playback::new(SpeechOutput::ready(Speech::no_beep(1.0)), false);
        playback.stream = Some(stream);
        playback.done = true;
        playback.timeline = Some(timeline.clone());
        playback.sentences = sentences;
        engineer.line.push_back(playback);
        engineer.hold(true);
        engineer.hold(true);
        engineer.play(&config, Some(&device));
        let playback = engineer.line.front().unwrap();
        assert!(playback.held);
        assert_eq!(playback.sentences.submitted, 0);
        let resume = 11025 - BEHIND;
        assert_eq!(playback.sentences.skip(), resume);
        assert_eq!(playback.stream.as_ref().unwrap().queued_bytes().unwrap(), 0);

        engineer.hold(false);
        assert!(!engineer.line[0].held);
        // Keep the dummy device paused while submitting, so the exact input byte count is stable.
        engineer.line[0].stream.as_ref().unwrap().pause().unwrap();
        engineer.play(&config, Some(&device));
        let playback = engineer.line.front().unwrap();
        assert_eq!(playback.sentences.submitted, 1);
        assert_eq!(
            playback.stream.as_ref().unwrap().queued_bytes().unwrap() as usize,
            (22050 - resume) * size_of::<f32>()
        );
        assert_eq!(playback.sentences.pieces[0].samples[0], 1.0);
        // A second interruption must retain the same unfinished sentence.
        engineer.hold(true);
        assert_eq!(engineer.line[0].sentences.skip(), resume);
        let pending = engineer.line[0].sentences.pending().unwrap();
        assert_eq!(pending.samples[0], 1.0);
        engineer.stop();
        assert!(!engineer.is_speaking());
        let line = timeline.to_string();
        assert!(!line.contains("first audio never"), "{line}");
        assert!(line.ends_with("played never"), "{line}");
    }

    #[test]
    #[ignore = "run alone with SDL_AUDIO_DRIVER=dummy"]
    fn a_drained_answer_is_marked_played() {
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let _entered = runtime.enter();
        let dir = tempfile::tempdir().unwrap();
        let config = runtime
            .block_on(Config::new(dir.path().join("config.json")))
            .unwrap();
        let sdl = sdl3::init().unwrap();
        let audio = sdl.audio().unwrap();
        let spec = Speech::no_beep(1.0).spec();
        let device = audio.open_playback_device(&spec).unwrap();
        let stream = device.clone().open_device_stream(Some(&spec)).unwrap();

        let timeline = Arc::new(Timeline::new(None, Instant::now()));
        // Its speech already reached the stream
        timeline.first_audio();
        let mut engineer = Engineer::new(reqwest::Client::new(), None);
        let mut playback = Playback::new(SpeechOutput::ready(Speech::no_beep(1.0)), false);
        playback.stream = Some(stream);
        playback.done = true;
        playback.timeline = Some(timeline.clone());
        engineer.line.push_back(playback);
        engineer.play(&config, Some(&device));
        assert!(!engineer.is_speaking());
        let line = timeline.to_string();
        assert!(!line.ends_with("played never"), "{line}");
    }

    #[test]
    #[ignore = "run alone with SDL_AUDIO_DRIVER=dummy"]
    fn an_answer_caught_up_with_its_turn_plays_out_and_opens_the_gap() {
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let _entered = runtime.enter();
        let dir = tempfile::tempdir().unwrap();
        let config = runtime
            .block_on(Config::new(dir.path().join("config.json")))
            .unwrap();
        let sdl = sdl3::init().unwrap();
        let audio = sdl.audio().unwrap();
        // At another rate than the voice's: the stream resamples, holding some of its input back
        // until it's flushed
        let spec = sdl3::audio::AudioSpec {
            freq: Some(48000),
            channels: Some(1),
            format: Some(sdl3::audio::AudioFormat::f32_sys()),
        };
        let device = audio.open_playback_device(&spec).unwrap();
        let sentence = || Speech {
            samples: vec![0.0; 2205],
            sample_rate: 22050,
        };

        // It said its first sentence, and its turn is still working on the next
        let mut engineer = Engineer::new(reqwest::Client::new(), None);
        let (send, mut answer) = being_written();
        answer.sentences.pieces.push_back(sentence());
        engineer.line.push_back(answer);
        let play_until = |engineer: &mut Engineer, done: fn(&Engineer) -> bool| {
            let deadline = Instant::now() + Duration::from_secs(5);
            while !done(engineer) {
                assert!(Instant::now() < deadline, "it never played out");
                engineer.play(&config, Some(&device));
                std::thread::sleep(Duration::from_millis(10));
            }
        };
        play_until(&mut engineer, |e| e.line[0].waiting_on_its_turn());
        assert!(engineer.line[0].flushed);

        // Its next sentence starts another stretch
        send.send(Ok(sentence())).unwrap();
        engineer.play(&config, Some(&device));
        assert!(!engineer.line[0].flushed);
        assert!(!engineer.line[0].waiting_on_its_turn());

        // Caught up again, it leaves the radio to a complete answer, and its stream with it
        engineer.line.push_back(Playback::tone());
        play_until(&mut engineer, |engineer| engineer.line[0].complete);
        assert!(engineer.line[1].writing);
        assert!(engineer.line[1].stream.is_none());

        // Back at the front, its next sentence opens another
        play_until(&mut engineer, |engineer| engineer.line.len() == 1);
        send.send(Ok(sentence())).unwrap();
        engineer.play(&config, Some(&device));
        assert!(engineer.line[0].stream.is_some());
        assert_eq!(engineer.line[0].sentences.submitted, 1);
    }

    #[test]
    #[ignore = "run alone with SDL_AUDIO_DRIVER=dummy"]
    fn a_removed_speaker_stops_only_the_answer_playing_on_it() {
        let sdl = sdl3::init().unwrap();
        let audio = sdl.audio().unwrap();
        let spec = piece().spec();
        let device = audio.open_playback_device(&spec).unwrap();
        let stream = || device.clone().open_device_stream(Some(&spec)).unwrap();
        let on = |playback: &Playback| {
            let stream = playback.stream.as_ref().unwrap();
            stream.device_id().unwrap().id().0
        };
        let notices = |engineer: &Engineer| {
            (engineer.chat.iter())
                .filter(|e| {
                    e.speaker == Speaker::Notice && e.text == t!("engineer.speaker_removed")
                })
                .count()
        };
        // An answer that said its first sentence and waits on its turn for more, with an answer
        // ready behind it (on a stream of its own: SDL removes each logical device)
        let line = || {
            let (send, mut waiting) = being_written();
            waiting.stream = Some(stream());
            let mut behind = Playback::tone();
            behind.stream = Some(stream());
            let ids = (on(&waiting), on(&behind));
            let mut engineer = Engineer::new(reqwest::Client::new(), None);
            engineer.line.extend([waiting, behind]);
            (send, engineer, ids)
        };

        // Nothing is playing on the speaker: the waiting answer keeps its place
        let (_send, mut engineer, (waiting_on, _)) = line();
        engineer.speaker_removed(waiting_on, false);
        assert_eq!(engineer.line.len(), 2);
        assert!(engineer.line[0].writing);
        assert!(engineer.line[0].stream.is_none());
        assert_eq!(notices(&engineer), 1);

        // The answer behind plays in its gap: whichever goes first, only that one stops
        for gap_first in [true, false] {
            let (_send, mut engineer, (waiting_on, gap_on)) = line();
            let now = Instant::now();
            engineer.fill_gap(now);
            engineer.fill_gap(now + GAP_AFTER);
            assert!(engineer.line[0].complete);
            assert!(engineer.line[1].stream.is_none());
            let order = match gap_first {
                true => [gap_on, waiting_on],
                false => [waiting_on, gap_on],
            };
            for which in order {
                engineer.speaker_removed(which, false);
            }
            assert_eq!(engineer.line.len(), 1, "gap first: {gap_first}");
            assert!(engineer.line[0].writing, "gap first: {gap_first}");
            assert_eq!(notices(&engineer), 1, "gap first: {gap_first}");
        }
    }

    #[test]
    fn a_refused_answer_is_replaced_by_say_again() {
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let _entered = runtime.enter();
        let rt = runtime.handle();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        // The voice's path only stands in for an installed one: nothing is synthesized
        std::fs::write(&path, r#"{"voices": {"en": "en.onnx"}}"#).unwrap();
        let config = runtime.block_on(Config::new(&path)).unwrap();
        let mut synth = Synthesizer::new(rt, &config.get());
        let mut engineer = Engineer::new(reqwest::Client::new(), None);

        // Whether it has spoken yet or not
        for spoken in [true, false] {
            engineer.stop();
            let (_send, answer) = being_written();
            match spoken {
                true => engineer.line.push_back(answer),
                false => engineer.queue(answer),
            }
            let (_, text) = mpsc::unbounded_channel();
            engineer.answer(
                rt,
                &config,
                &mut synth,
                Reply {
                    text: "Partial".into(),
                    stop: StopReason::Refusal,
                    usage: Default::default(),
                    model: "test".into(),
                },
                Turn {
                    task: Task::spawn(std::future::pending()),
                    lang: arrstr!("en"),
                    text,
                    entry: None,
                    steer: Steer::default(),
                    steered: None,
                },
            );
            assert!(engineer.line.is_empty(), "spoken: {spoken}");
            assert_eq!(engineer.incoming.len(), 1, "spoken: {spoken}");
            assert!(engineer.incoming[0].complete, "spoken: {spoken}");
            let reply = engineer.chat.last().unwrap();
            assert_eq!(reply.text, t!("engineer.say_again": "en"));
        }
    }

    #[test]
    fn speech_that_said_nothing_is_not_marked_played() {
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let _entered = runtime.enter();
        let dir = tempfile::tempdir().unwrap();
        let config = runtime
            .block_on(Config::new(dir.path().join("config.json")))
            .unwrap();

        // At the front, with nothing left to play
        let timeline = Arc::new(Timeline::new(None, Instant::now()));
        let mut engineer = Engineer::new(reqwest::Client::new(), None);
        let mut playback = Playback::new(SpeechOutput::ready(Speech::no_beep(1.0)), false);
        playback.done = true;
        playback.timeline = Some(timeline.clone());
        engineer.line.push_back(playback);
        engineer.play(&config, None);
        assert!(!engineer.is_speaking());
        let line = timeline.to_string();
        assert!(line.ends_with("played never"), "{line}");

        // Ended before its first piece: it never joins the line
        let timeline = Arc::new(Timeline::new(None, Instant::now()));
        let (send, speech) = SpeechOutput::channel();
        drop(send);
        let mut playback = Playback::new(speech, false);
        playback.timeline = Some(timeline.clone());
        engineer.queue(playback);
        engineer.play(&config, None);
        assert!(!engineer.is_speaking());
        let line = timeline.to_string();
        assert!(line.ends_with("played never"), "{line}");
    }

    #[test]
    fn an_answer_that_gave_up_its_stream_is_still_marked_played() {
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let _entered = runtime.enter();
        let dir = tempfile::tempdir().unwrap();
        let config = runtime
            .block_on(Config::new(dir.path().join("config.json")))
            .unwrap();

        // It spoke, then closed its stream giving way in a gap, and ended with nothing more
        let timeline = Arc::new(Timeline::new(None, Instant::now()));
        timeline.first_audio();
        let mut engineer = Engineer::new(reqwest::Client::new(), None);
        let mut playback = Playback::new(SpeechOutput::ready(Speech::no_beep(1.0)), false);
        playback.done = true;
        playback.timeline = Some(timeline.clone());
        engineer.line.push_back(playback);
        engineer.play(&config, None);
        assert!(!engineer.is_speaking());
        let line = timeline.to_string();
        assert!(!line.ends_with("played never"), "{line}");
    }

    /// A piece of speech, standing in for a synthesized sentence.
    fn piece() -> Speech {
        Speech {
            samples: vec![0.25, -0.25],
            sample_rate: 22050,
        }
    }

    /// A pilot answer whose turn is still running and that has nothing to play yet: the sender
    /// keeps its speech open.
    fn being_written() -> (mpsc::UnboundedSender<anyhow::Result<Speech>>, Playback) {
        let (send, speech) = SpeechOutput::channel();
        let mut playback = Playback::new(speech, false);
        playback.writing = true;
        (send, playback)
    }

    /// Whether `front` is still at the front of the line `[front, behind]` once a gap is looked
    /// for in a frame, and again `after` that.
    fn stays_ahead(mut front: Playback, behind: Playback, after: Duration) -> bool {
        let marker = Arc::new(Timeline::new(None, Instant::now()));
        front.timeline = Some(marker.clone());
        let mut engineer = Engineer::new(reqwest::Client::new(), None);
        engineer.line.extend([front, behind]);
        let now = Instant::now();
        engineer.fill_gap(now);
        engineer.fill_gap(now + after);
        assert_eq!(engineer.line.len(), 2);
        (engineer.line[0].timeline.as_ref()).is_some_and(|first| Arc::ptr_eq(first, &marker))
    }

    #[test]
    fn answers_join_the_line_when_their_first_piece_is_ready() {
        let mut engineer = Engineer::new(reqwest::Client::new(), None);
        let (send, speech) = SpeechOutput::channel();
        engineer.queue(Playback::new(speech, false));
        engineer.queue(Playback::tone());
        engineer.admit(1.0);
        // The tone was ready: it doesn't wait behind an answer still being thought about
        assert_eq!(engineer.line.len(), 1);
        assert!(engineer.line[0].complete);
        assert_eq!(engineer.incoming.len(), 1);
        assert!(engineer.is_speaking());

        send.send(Ok(piece())).unwrap();
        engineer.admit(1.0);
        assert!(engineer.incoming.is_empty());
        assert_eq!(engineer.line.len(), 2);
        let answer = &engineer.line[1];
        assert!(!answer.complete);
        // Its first piece is kept, to play
        assert_eq!(answer.sentences.pending().unwrap().samples, piece().samples);
    }

    #[test]
    fn a_complete_answer_plays_in_the_gap_of_an_answer_being_written() {
        let (_send, front) = being_written();
        assert!(!stays_ahead(front, Playback::tone(), GAP_AFTER));
    }

    #[test]
    fn the_gap_opens_only_after_a_pause() {
        let (_send, front) = being_written();
        let almost = GAP_AFTER - Duration::from_millis(1);
        assert!(stays_ahead(front, Playback::tone(), almost));
    }

    #[test]
    fn nothing_jumps_ahead_of_an_answer_with_speech_to_play() {
        let (_send, mut front) = being_written();
        front.sentences.pieces.push_back(piece());
        assert!(stays_ahead(front, Playback::tone(), GAP_AFTER));
    }

    #[test]
    fn nothing_jumps_ahead_of_an_answer_with_its_next_piece_ready() {
        // Synthesized, not yet taken from its speech
        let (send, front) = being_written();
        send.send(Ok(piece())).unwrap();
        assert!(stays_ahead(front, Playback::tone(), GAP_AFTER));
    }

    #[test]
    fn nothing_jumps_ahead_of_a_finished_turn() {
        let (_send, mut front) = being_written();
        front.writing = false;
        assert!(stays_ahead(front, Playback::tone(), GAP_AFTER));
    }

    #[test]
    fn only_complete_answers_use_the_gap() {
        let (_send, front) = being_written();
        let behind = Playback::new(SpeechOutput::ready(piece()), false);
        assert!(stays_ahead(front, behind, GAP_AFTER));
    }

    #[test]
    fn talking_and_the_spotter_hold_overlap() {
        let mut engineer = Engineer::new(reqwest::Client::new(), None);
        engineer.line.push_back(Playback::tone());
        engineer.talk(true);
        assert!(engineer.line[0].held);
        engineer.hold(true);
        // Letting go of the talk button while the spotter speaks keeps it paused
        engineer.talk(false);
        assert!(engineer.line[0].held);
        engineer.hold(false);
        assert!(!engineer.line[0].held);
    }

    #[test]
    fn an_answer_ready_while_talking_waits_for_release() {
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let _entered = runtime.enter();
        let dir = tempfile::tempdir().unwrap();
        let config = runtime
            .block_on(Config::new(dir.path().join("config.json")))
            .unwrap();
        let no_speaker = |engineer: &Engineer| {
            (engineer.chat.iter())
                .any(|e| e.speaker == Speaker::Notice && e.text == t!("engineer.no_speaker"))
        };

        // The answer is ready while the pilot talks: it doesn't start over them
        let mut engineer = Engineer::new(reqwest::Client::new(), None);
        engineer.talk(true);
        engineer.queue(Playback::tone());
        engineer.play(&config, None);
        engineer.play(&config, None);
        assert_eq!(engineer.line.len(), 1);
        assert!(!no_speaker(&engineer));

        // Released, it plays (and finds no speaker to play on)
        engineer.talk(false);
        engineer.play(&config, None);
        assert!(no_speaker(&engineer));
    }

    #[test]
    fn an_error_lets_the_answer_finish_before_the_tone() {
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let _entered = runtime.enter();
        let rt = runtime.handle();
        let dir = tempfile::tempdir().unwrap();
        let config = rt
            .block_on(Config::new(dir.path().join("config.json")))
            .unwrap();
        let mut synth = Synthesizer::new(rt, &config.get());
        let mut engineer = Engineer::new(reqwest::Client::new(), None);

        // The turn has spoken, and fails before it's done
        let (send, answer) = being_written();
        engineer.line.push_back(answer);
        let (_chat, text) = mpsc::unbounded_channel();
        engineer.turn = Some(Turn {
            task: Task::spawn(async { Err(anyhow::anyhow!("connection lost")) }),
            lang: arrstr!("en"),
            text,
            entry: None,
            steer: Steer::default(),
            steered: None,
        });
        let deadline = Instant::now() + Duration::from_secs(10);
        while engineer.is_thinking() {
            assert!(Instant::now() < deadline, "the turn never ended");
            std::thread::sleep(Duration::from_millis(1));
            engineer.update(rt, None, &config, &mut synth, None);
        }
        assert_eq!(engineer.line.len(), 1);
        assert!(!engineer.line[0].writing);
        assert!(engineer.incoming.is_empty());
        let notices = (engineer.chat.iter())
            .filter(|e| e.speaker == Speaker::Notice)
            .map(|e| e.text.as_str())
            .collect::<Vec<_>>();
        assert!(notices.iter().any(|text| text.contains("connection lost")));
        assert!(!notices.contains(&t!("engineer.cancelled")));

        // What it said plays out, then the tone
        drop(send);
        engineer.play(&config, None);
        assert_eq!(engineer.line.len(), 1);
        let tone = &mut engineer.line[0];
        assert!(tone.complete);
        let Poll::Ready(Some(Ok(tone))) = tone.speech.try_next() else {
            panic!("the tone must follow the answer");
        };
        assert_eq!(tone.samples, Speech::no_beep(1.0).samples);
    }

    #[test]
    fn incomplete_answers_preserve_speech_before_the_tone() {
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let _entered = runtime.enter();
        let rt = runtime.handle();
        let dir = tempfile::tempdir().unwrap();
        let config = rt
            .block_on(Config::new(dir.path().join("config.json")))
            .unwrap();
        let mut synth = Synthesizer::new(rt, &config.get());
        let mut engineer = Engineer::new(reqwest::Client::new(), None);

        for stop in [StopReason::MaxTokens, StopReason::ContextWindowExceeded] {
            // Stand in for synthesized speech without loading a voice or opening a device.
            let mut answer = Playback::new(SpeechOutput::ready(piece()), false);
            answer.writing = true;
            engineer.line.push_back(answer);
            let (_, text) = mpsc::unbounded_channel();
            engineer.answer(
                rt,
                &config,
                &mut synth,
                Reply {
                    text: "Partial answer".into(),
                    stop,
                    usage: Default::default(),
                    model: "test".into(),
                },
                Turn {
                    task: Task::spawn(std::future::pending()),
                    lang: arrstr!("en"),
                    text,
                    entry: None,
                    steer: Steer::default(),
                    steered: None,
                },
            );
            assert_eq!(engineer.line.len(), 1, "{stop:?}");
            assert!(engineer.incoming.is_empty(), "{stop:?}");
            let playback = &mut engineer.line[0];
            assert!(playback.tone_after_drain, "{stop:?}");
            assert!(!playback.done);
            let Poll::Ready(Some(Ok(speech))) = playback.speech.try_next() else {
                panic!("the answer's speech must be preserved");
            };
            assert_eq!(speech.samples, [0.25, -0.25]);
            assert_eq!(speech.sample_rate, 22050);

            // With the speech consumed and no device queue left, playback has drained.
            engineer.play(&config, None);
            assert_eq!(engineer.line.len(), 1, "tone follows the answer");
            let playback = &mut engineer.line[0];
            assert!(playback.complete);
            assert!(!playback.tone_after_drain);
            let Poll::Ready(Some(Ok(tone))) = playback.speech.try_next() else {
                panic!("failure tone must be ready after the answer drains");
            };
            assert_eq!(tone.samples, Speech::no_beep(1.0).samples);
            engineer.play(&config, None);
            assert!(!engineer.is_speaking(), "tone must only play once");
        }
    }

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
        let mut engineer = Engineer::new(reqwest::Client::new(), None);

        for (stop, text, tone) in [
            (StopReason::MaxTokens, "Partial answer", true),
            (StopReason::ContextWindowExceeded, "Partial answer", true),
            (StopReason::Refusal, "", true),
            (StopReason::EndTurn, "", true),
            (StopReason::EndTurn, "Complete answer", false),
            (StopReason::StopSequence, "Complete answer", false),
        ] {
            engineer.stop();
            let (_, receiver) = mpsc::unbounded_channel();
            let turn = Turn {
                task: Task::spawn(std::future::pending()),
                lang: arrstr!("en"),
                text: receiver,
                entry: None,
                steer: Steer::default(),
                steered: None,
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
            assert!(engineer.line.is_empty(), "{stop:?}");
            assert_eq!(engineer.incoming.len(), usize::from(tone), "{stop:?}");
            if let Some(playback) = engineer.incoming.first_mut() {
                assert!(playback.complete);
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
    /// `EVAL_LANG` (e.g. `it`) asks them as if heard on the radio in that language instead: with
    /// no voices installed, typed questions aren't told apart.
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
        let mut engineer = Engineer::new(client, None);

        let questions = std::env::var("EVAL_QUESTIONS")
            .unwrap_or_else(|_| "how are the tyres?|where am I losing time?".into());
        let lang = std::env::var("EVAL_LANG")
            .ok()
            .map(|lang| ArrayStr::from_str(&lang).expect("EVAL_LANG isn't a language code"));
        for question in questions.split('|') {
            let start = std::time::Instant::now();
            let (running, asked) = (Some(&mut *sim), question.to_owned());
            match lang {
                Some(lang) => {
                    engineer.ask(rt, running, &config, &mut synth, asked, Some(lang), None)
                }
                None => engineer.ask_text(rt, running, &config, &mut synth, asked),
            }
            while engineer.is_thinking() {
                assert!(
                    start.elapsed() < std::time::Duration::from_secs(120),
                    "stuck on {question:?}"
                );
                engineer.update(rt, Some(&mut *sim), &config, &mut synth, None);
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
    async fn follows_the_recording_before_any_question() {
        let rt = tokio::runtime::Handle::current();
        let dir = tempfile::tempdir().unwrap();
        let one = NonZeroUsize::new(1).unwrap();
        let pool = |name: &str| {
            Pool::new(
                Database::read_only(Arc::new(dir.path().join(name))),
                one,
                one,
            )
        };
        let mut engineer = Engineer::new(reqwest::Client::new(), None);
        assert!(engineer.tool.is_none());

        // No simulator yet
        engineer.follow_recording(&rt, None);
        assert!(engineer.tool.is_none());

        // Opened mid-session, or the game's just started: followed with no question asked
        let mut sim = crate::sim::recording(pool("a.db"));
        engineer.follow_recording(&rt, Some(&mut *sim));
        let first = engineer.tool.clone().expect("following the recording");
        assert!(engineer.session.is_none());

        // Frame after frame, and for a conversation, it's the same one: what it's read is kept
        engineer.follow_recording(&rt, Some(&mut *sim));
        assert!(engineer.tool.as_ref().unwrap().is_same(&first));
        let same = pool("a.db");
        assert!(engineer.tool_for(&rt, &same).is_same(&first));

        // Another recording: another tool
        let other = engineer.tool_for(&rt, &pool("b.db"));
        assert!(!other.is_same(&first));

        // The simulator gone: it lets go
        engineer.follow_recording(&rt, None);
        assert!(engineer.tool.is_none());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_conversation_ends_when_its_recording_is_lost_or_switched() {
        let rt = tokio::runtime::Handle::current();
        for switch in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            let one = NonZeroUsize::new(1).unwrap();
            let pool = |name: &str| {
                Pool::new(
                    Database::read_only(Arc::new(dir.path().join(name))),
                    one,
                    one,
                )
            };
            let original = pool("a.db");
            let mut sim = crate::sim::recording(original.clone());
            let mut engineer = Engineer::new(reqwest::Client::new(), None);
            engineer.follow_recording(&rt, Some(&mut *sim));
            let first = engineer.tool.clone().unwrap();
            engineer.session = Some(Session {
                db: original.database().path().to_owned(),
                model: ModelConfig::default(),
                corner_names: false,
                user: None,
                lease: None,
                state: Arc::new(Mutex::new(SessionState {
                    conversation: Box::new(RecordingModel(Arc::new(std::sync::Mutex::new(
                        Recorded::default(),
                    )))),
                    pool: original,
                    tool: first.clone(),
                    sim: "lmu",
                    game_session: None,
                })),
            });
            engineer.follow_recording(&rt, Some(&mut *sim));
            assert!(engineer.session.is_some());
            if switch {
                let mut other = crate::sim::recording(pool("b.db"));
                engineer.follow_recording(&rt, Some(&mut *other));
            } else {
                engineer.follow_recording(&rt, None);
                assert!(engineer.tool.is_none());
            }
            assert!(engineer.session.is_none());
            engineer.follow_recording(&rt, Some(&mut *sim));
            assert!(!engineer.tool.as_ref().unwrap().is_same(&first));
            engineer.quit(&rt).await.unwrap();
        }
    }

    /// How a [`ScriptedModel`]'s turn ends, once the test says.
    enum Act {
        /// It takes the questions steering it, then replies
        Steered,
        /// It replies without looking at them
        Reply,
        Refuse,
        /// It replies with the conversation full
        Full,
        Fail,
    }

    /// What a [`ScriptedModel`] was asked.
    #[derive(Default)]
    struct Asked {
        /// Each turn's question, in order
        questions: Vec<String>,
        /// What each [`Act::Steered`] took from its turn's steer
        steered: Vec<Vec<String>>,
    }

    /// A conversation whose turns each wait for an [`Act`] from the test.
    struct ScriptedModel {
        asked: Arc<std::sync::Mutex<Asked>>,
        acts: mpsc::UnboundedReceiver<Act>,
        steer: Option<Steer>,
    }

    impl Model for ScriptedModel {
        fn send_streaming<'a>(
            &'a mut self,
            text: String,
            _: &'a mut (dyn FnMut(&str) + Send),
        ) -> futures::future::BoxFuture<'a, anyhow::Result<Reply>> {
            self.asked.lock().unwrap().questions.push(text);
            Box::pin(async move {
                let stop = match self.acts.recv().await {
                    Some(Act::Steered) => {
                        let taken = self.steer.as_ref().map(Steer::take).unwrap_or_default();
                        self.asked.lock().unwrap().steered.push(taken);
                        StopReason::EndTurn
                    }
                    Some(Act::Reply) => StopReason::EndTurn,
                    Some(Act::Refuse) => StopReason::Refusal,
                    Some(Act::Full) => StopReason::ContextWindowExceeded,
                    Some(Act::Fail) | None => anyhow::bail!("scripted failure"),
                };
                Ok(Reply {
                    text: "Copy.".into(),
                    stop,
                    usage: Default::default(),
                    model: "scripted".into(),
                })
            })
        }

        fn reset(&mut self) {}

        fn usage(&self) -> &std::collections::BTreeMap<String, provider::Usage> {
            unimplemented!()
        }

        fn set_steer(&mut self, steer: Option<Steer>) {
            self.steer = steer;
        }
    }

    /// An engineer whose conversation is a [`ScriptedModel`], on a running simulator's
    /// recording, as the app drives it.
    struct Scripted {
        engineer: Engineer,
        sim: Box<Sim>,
        synth: Synthesizer,
        config: Config,
        asked: Arc<std::sync::Mutex<Asked>>,
        acts: mpsc::UnboundedSender<Act>,
        _dir: tempfile::TempDir,
        runtime: tokio::runtime::Runtime,
    }

    impl Scripted {
        /// No voices: nothing is synthesized, and tones stand in for "say again".
        fn new() -> Self {
            Self::with_config("{}")
        }

        /// With the config `raw`.
        fn with_config(raw: &str) -> Self {
            let runtime = tokio::runtime::Runtime::new().unwrap();
            let _entered = runtime.enter();
            let rt = runtime.handle();
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("config.json");
            std::fs::write(&path, raw).unwrap();
            let config = rt.block_on(Config::new(&path)).unwrap();
            let synth = Synthesizer::new(rt, &config.get());
            let one = NonZeroUsize::new(1).unwrap();
            let pool = Pool::new(
                Database::read_only(Arc::new(dir.path().join("a.db"))),
                one,
                one,
            );
            let sim = crate::sim::recording(pool.clone());
            let asked = Arc::new(std::sync::Mutex::new(Asked::default()));
            let (acts, script) = mpsc::unbounded_channel();
            let mut engineer = Engineer::new(reqwest::Client::new(), None);
            // As `ask` would start it, so it keeps it
            engineer.session = Some(Session {
                db: pool.database().path().to_owned(),
                model: config.get().model.clone(),
                corner_names: *config.get().corner_names(),
                user: None,
                lease: None,
                state: Arc::new(Mutex::new(SessionState {
                    conversation: Box::new(ScriptedModel {
                        asked: asked.clone(),
                        acts: script,
                        steer: None,
                    }),
                    tool: EngineerTool::new(pool.clone()),
                    pool,
                    sim: "test",
                    game_session: None,
                })),
            });
            Self {
                engineer,
                sim,
                synth,
                config,
                asked,
                acts,
                _dir: dir,
                runtime,
            }
        }

        fn ask(&mut self, question: &str) {
            let _entered = self.runtime.enter();
            let sim = Some(&mut *self.sim);
            let (config, synth) = (&self.config, &mut self.synth);
            (self.engineer).ask_text(self.runtime.handle(), sim, config, synth, question.into());
        }

        fn hear(&mut self, heard: Heard) {
            let _entered = self.runtime.enter();
            let sim = Some(&mut *self.sim);
            let (config, synth) = (&self.config, &mut self.synth);
            (self.engineer).hear(self.runtime.handle(), sim, config, synth, heard);
        }

        /// Runs the app's frames until `done`, failing if that takes too long.
        fn until(&mut self, what: &str, done: impl Fn(&Self) -> bool) {
            let _entered = self.runtime.enter();
            let deadline = Instant::now() + Duration::from_secs(10);
            while !done(self) {
                assert!(Instant::now() < deadline, "never {what}");
                std::thread::sleep(Duration::from_millis(1));
                let (sim, config, synth) = (Some(&mut *self.sim), &self.config, &mut self.synth);
                (self.engineer).update(self.runtime.handle(), sim, config, synth, None);
            }
        }

        fn questions(&self) -> Vec<String> {
            self.asked.lock().unwrap().questions.clone()
        }

        fn cancelled(&self) -> bool {
            (self.engineer.chat.iter())
                .any(|e| e.speaker == Speaker::Notice && e.text == t!("engineer.cancelled"))
        }
    }

    impl Drop for Scripted {
        fn drop(&mut self) {
            let _entered = self.runtime.enter();
            let quit = self.engineer.quit(self.runtime.handle());
            let _ = self.runtime.block_on(quit);
        }
    }

    #[test]
    fn asking_during_a_turn_steers_it() {
        let mut scripted = Scripted::new();
        scripted.ask("How are the fronts?");
        scripted.until("asked", |s| s.questions().len() == 1);

        scripted.ask("And the rears?");
        assert!(scripted.engineer.is_thinking());
        assert!(!scripted.cancelled());
        let last = scripted.engineer.chat.last().unwrap();
        assert_eq!(last.speaker, Speaker::Pilot);
        assert_eq!(last.text, "And the rears?");

        // The running turn takes it: one reply covers both
        scripted.acts.send(Act::Steered).unwrap();
        scripted.until("answered", |s| !s.engineer.is_thinking());
        let asked = scripted.asked.lock().unwrap();
        assert_eq!(asked.steered, [vec!["And the rears?".to_owned()]]);
        assert_eq!(asked.questions.len(), 1);
    }

    #[test]
    fn left_over_steers_run_as_the_next_turn() {
        let mut scripted = Scripted::new();
        scripted.ask("How are the fronts?");
        scripted.ask("And the rears?");
        // The turn was writing its final text: it ends without taking the steer
        scripted.acts.send(Act::Reply).unwrap();
        scripted.until("asked again", |s| s.questions().len() == 2);
        assert!(scripted.questions()[1].contains("And the rears?"));
        assert!(scripted.engineer.is_thinking());
        // The first reply was had
        assert!((scripted.engineer.chat.iter()).any(|e| e.speaker == Speaker::Engineer));
    }

    #[test]
    fn a_refused_turn_drops_its_left_over_steers() {
        for act in [Act::Refuse, Act::Fail] {
            let mut scripted = Scripted::new();
            scripted.ask("How are the fronts?");
            scripted.ask("No, I meant sector two.");
            scripted.acts.send(act).unwrap();
            // A turn for the steer would start in the frame the reply is handled
            scripted.until("answered", |s| !s.engineer.is_thinking());
            assert_eq!(scripted.questions().len(), 1);
        }
    }

    #[test]
    fn left_over_steers_start_over_once_the_conversation_is_full() {
        let mut scripted = Scripted::new();
        let full = scripted.engineer.session.as_ref().unwrap().state.clone();
        scripted.ask("How are the fronts?");
        scripted.ask("And the rears?");
        scripted.acts.send(Act::Full).unwrap();
        // Asked on a new conversation, or told why there isn't one (Bono Cloud, signed out)
        let error = t!(
            "engineer.error_starting",
            error = t!("provider.bono_cloud_sign_in")
        );
        scripted.until("started over", |s| {
            let restarted = (s.engineer.session.as_ref())
                .is_some_and(|session| !Arc::ptr_eq(&session.state, &full));
            let failed =
                (s.engineer.chat.iter()).any(|e| e.speaker == Speaker::Notice && e.text == error);
            restarted || failed
        });
        // Not on the full one
        assert_eq!(scripted.questions().len(), 1);
    }

    #[test]
    fn left_over_steers_are_answered_in_their_own_language() {
        // Voice paths only stand in for installed choices: nothing is synthesized
        let mut scripted =
            Scripted::with_config(r#"{"voices": {"en": "en.onnx", "es": "es.onnx"}}"#);
        let heard = |text: &str, lang| Heard::Question {
            text: text.into(),
            lang,
            released: Instant::now(),
        };
        scripted.hear(heard("How are the fronts?", arrstr!("en")));
        assert_eq!(scripted.engineer.turn.as_ref().unwrap().lang, arrstr!("en"));
        scripted.hear(heard("¿Y las traseras?", arrstr!("es")));
        scripted.acts.send(Act::Reply).unwrap();
        scripted.until("asked again", |s| s.questions().len() == 2);
        assert_eq!(scripted.engineer.turn.as_ref().unwrap().lang, arrstr!("es"));
    }

    #[test]
    fn hearing_nothing_keeps_the_turn() {
        let mut scripted = Scripted::new();
        scripted.ask("How are the fronts?");

        scripted.hear(Heard::Nothing);
        assert!(scripted.engineer.is_thinking());
        // "Say again", as the tone without a voice
        assert_eq!(scripted.engineer.incoming.len(), 1);

        scripted.hear(Heard::TooShort);
        assert!(scripted.engineer.is_thinking());
        assert_eq!(scripted.engineer.incoming.len(), 1);

        scripted.hear(Heard::Failed(anyhow::anyhow!("no audio")));
        assert!(scripted.engineer.is_thinking());
        assert_eq!(scripted.engineer.incoming.len(), 2);
        assert!(!scripted.cancelled());
    }

    #[test]
    fn new_session_clears_the_turn_its_steers_and_the_line() {
        let mut scripted = Scripted::new();
        scripted.ask("How are the fronts?");
        scripted.until("asked", |s| s.questions().len() == 1);
        scripted.ask("And the rears?");
        scripted.engineer.line.push_back(Playback::tone());
        scripted.engineer.queue(Playback::tone());

        scripted.engineer.new_session();
        assert!(!scripted.engineer.is_thinking());
        assert!(scripted.engineer.line.is_empty());
        assert!(scripted.engineer.incoming.is_empty());

        // The turn is gone: its reply, if it still came, would start no turn for the steer
        scripted.acts.send(Act::Reply).unwrap();
        let start = Instant::now();
        scripted.until("idle a while", |_| {
            start.elapsed() > Duration::from_millis(100)
        });
        assert_eq!(scripted.questions().len(), 1);
        assert!(!scripted.engineer.is_thinking());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn quitting_releases_the_persistent_tool_in_the_shutdown_task() {
        let rt = tokio::runtime::Handle::current();
        let dir = tempfile::tempdir().unwrap();
        let one = NonZeroUsize::new(1).unwrap();
        let pool = Pool::new(
            Database::read_only(Arc::new(dir.path().join("telemetry.db"))),
            one,
            one,
        );
        let mut engineer = Engineer::new(reqwest::Client::new(), None);
        let mut sim = crate::sim::recording(pool);
        engineer.follow_recording(&rt, Some(&mut *sim));
        assert!(engineer.tool.is_some());
        let shutdown = engineer.quit(&rt);
        assert!(engineer.tool.is_none());
        shutdown.await.unwrap();
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
                tool: EngineerTool::new(pool.clone()),
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
