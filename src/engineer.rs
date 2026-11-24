//! The race engineer: what the pilot says on the radio (transcribed) goes to the language model,
//! which answers from the running simulator's telemetry with the `engineer` tool, and the answer
//! is spoken back over the radio as the model writes it.

use crate::{
    account::{Account, Status},
    arrstr,
    config::{ArrayStr, Config, ModelConfig, ModelProvider},
    language::{self, LanguageDetector},
    provider::{
        self, Model, Reply, Steer, StopReason, Streamed, Tools,
        bono_cloud::{GameSession, Lease},
    },
    sim::{
        Sim,
        pit::{PitMenu, PitService},
    },
    sqlite::pool::Pool,
    synthesizer::{Drain, Drained, Speech, SpeechInput, SpeechOutput, Synthesizer, plays_on},
    task::Task,
};
use bono_i18n::t;
use schedule::{
    Batch, Change, Fired, NO_REQUESTS, RequestId, SILENCE, Schedule, tool::ScheduleTool,
};
use sdl3::audio::{AudioDevice, AudioStreamOwner};
use std::{
    collections::VecDeque,
    path::PathBuf,
    sync::{
        Arc, Weak,
        atomic::{self, AtomicBool, AtomicU32},
    },
    task::Poll,
    time::{Duration, Instant},
};
use acknowledgement::Acknowledgement;
use timeline::Timeline;
use tokio::{
    runtime::Handle,
    sync::{Mutex, mpsc},
};
use pit::PitTool;
use tool::{ConditionId, Engineer as EngineerTool, Progress, SKILL, instructions};

mod acknowledgement;
mod pit;
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
    /// `released`: when the pilot stopped talking.
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
    /// The pilot stopped too soon to have said anything: likely pressed by accident
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

/// The longest a released talk button holds scheduled turns back while what was said is heard:
/// a transcription that never comes doesn't keep them from running.
const HEARING_WAIT: Duration = Duration::from_secs(15);

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
    /// The turns aborted in this conversation. Each one's future holds the conversation until
    /// the runtime drops it, so [`Self::drop_session`] waits for them first.
    aborted: Vec<Box<dyn AbortedTurn>>,
    /// Answers waiting for their first piece of speech, in the order they were
    /// [queued](Self::queue): each joins the back of `line` once it has one, so an answer still
    /// being thought about never holds up one that's ready
    incoming: Vec<Playback>,
    /// The line of answers, in the order they're played: the front one plays, the others wait
    /// with their speech buffered. Only [`Self::fill_gap`] moves one ahead. A scheduled answer
    /// leaves it through [`Self::left`], which resolves its requests
    line: VecDeque<Playback>,
    /// The spotter is speaking: the front answer waits, and carries on from there after
    spotter_held: bool,
    /// The pilot is talking: the front answer waits, as for the spotter
    talking: bool,
    /// The talk button's beep is playing: the front answer waits, as for the spotter
    beeping: bool,
    /// When the pilot stopped talking, while what was said hasn't been heard: a question may be
    /// on its way, which would drop a scheduled turn started now. Waited for no longer than
    /// [`HEARING_WAIT`]
    hearing: Option<Instant>,
    /// Times in a row nothing was understood on the radio
    misheard: u32,
    /// The pilot's requests for later. They outlive conversations: only a new weekend, the
    /// new-session button, the simulator stopping, the recording going away or being switched for
    /// another, and quitting clear them
    schedule: Schedule,
    /// The newest progress the followed tool published: what the schedule last stepped through,
    /// and what a turn's changes are applied at. Kept while the tool has none (its tables
    /// catching up): applied at an older one, the schedule would see the weekend change back
    progress: Option<Progress>,
    /// The conditions registered with the followed tool, as last pushed to it
    registered: Vec<(ConditionId, String)>,
    /// Notes for the model's next message, about what it doesn't know happened (e.g. a change it
    /// confirmed that didn't apply). They go when the conversation ends, and each is told only
    /// to the conversation it's about
    unheard: Vec<Note>,
    /// The scheduled turn running, answering requests that fired: at most one, and never while a
    /// pilot turn runs
    scheduled: Option<ScheduledTurn>,
    /// A scheduled turn failed, and none has finished since: the pilot was told, once for the
    /// outage rather than for every retry
    scheduled_failing: bool,
    /// The model the pilot was told the conversation changes to, while it hasn't started
    announced_model: Option<ModelConfig>,
    /// The schedule's clock, when a test sets it: it stands still, and the test moves it on
    #[cfg(test)]
    clock: Option<Instant>,
    /// How long after a question is sent the app pushes its acknowledgement to speech (see
    /// [`acknowledgement`]): `None` turns it off in tests whose turns may run on past
    /// [`ACKNOWLEDGE_AFTER`] without being about that, and a zero delay lets non-timing tests fire
    /// without waiting
    #[cfg(test)]
    acknowledge_after: Option<Duration>,
    /// Scheduled answers given to the synthesizer, when a test stands in for it: their speech
    /// comes when the test sends it
    #[cfg(test)]
    synthesis: Option<Vec<Synthesis>>,
    /// Makes the conversations started, when a test stands in for the model's provider
    #[cfg(test)]
    conversations: Option<Conversations>,
}

/// Makes a conversation with the tools given, standing in for the model's provider in a test.
#[cfg(test)]
type Conversations = Box<dyn Fn(Tools) -> Box<dyn Model> + Send>;

/// A scheduled answer given to the stand-in for the synthesizer, in a test.
#[cfg(test)]
struct Synthesis {
    lang: ArrayStr<2>,
    text: String,
    /// Its speech, sent by the test: it ends once this is dropped
    speech: Option<mpsc::UnboundedSender<anyhow::Result<Speech>>>,
}

/// The [unheard](Engineer::unheard) notes a turn's message carries: told again with the next
/// message if the conversation doesn't keep the exchange.
#[derive(Default)]
struct Told {
    notes: Vec<Note>,
    /// Its conversation started over (`auto_session`) before its message was sent: the notes,
    /// about the conversation that's over, were left out and aren't told again
    started_over: Arc<AtomicBool>,
}

impl Told {
    /// The notes to tell with the next message, its exchange forgotten.
    fn untold(&mut self) -> Vec<Note> {
        match self.started_over.load(atomic::Ordering::Relaxed) {
            true => Vec::new(),
            false => std::mem::take(&mut self.notes),
        }
    }
}

/// What a turn's message carries besides the question and its telemetry snapshot, taken from the
/// engineer as the turn starts (see [`SessionState::brief`]).
struct Briefing {
    auto_session: bool,
    /// The pending requests and the next id, which the `schedule` tool stages the turn's changes
    /// against
    pending: Vec<RequestId>,
    next_id: RequestId,
    pending_text: String,
    /// The [`Told`] notes: the message has those about its conversation as it is once the turn
    /// has the lock on it (see [`Note::conversation`])
    notes: Vec<Note>,
    /// The conversation the turn runs on
    state: Weak<Mutex<SessionState>>,
    /// Which language to reply in: told last, and after each tool result
    note: Option<Arc<str>>,
    /// The [`Told`] flag, set if the conversation starts over first
    started_over: Arc<AtomicBool>,
}

/// The engineer's own turn, answering requests that fired. It's never the pilot's: a question
/// asked while it runs drops it rather than steering it, and its text is only had once it's done.
struct ScheduledTurn {
    /// The reply, with the changes to the requests the turn staged
    task: Task<anyhow::Result<(Reply, Vec<Change>)>>,
    /// The requests it answers
    batch: Batch,
    /// The language it's answered in
    lang: ArrayStr<2>,
    told: Told,
}

/// A note for the model's next message (see [`Engineer::unheard`]).
#[derive(Clone)]
struct Note {
    text: String,
    /// The conversation it's about, the only one told it: checked by the turn telling it, once
    /// it has the lock on the conversation, so one started over in that very turn isn't told.
    /// `None`: no conversation has it
    conversation: Option<Conversation>,
}

/// What a scheduled turn's answer in the line answers (see [`Playback::scheduled`]).
struct ScheduledAnswer {
    /// What it says, for the note telling the model if the pilot doesn't hear it
    text: String,
    /// Where its requests fired: it's out of date once one of them is (see
    /// [`Schedule::is_stale`]), and they're resolved once it's played or dropped
    fired: Vec<Fired>,
    /// The conversation that gave it, the only one told if the pilot doesn't hear it
    conversation: Option<Conversation>,
    /// Its chat entry, which says so if it isn't said
    entry: Option<usize>,
}

/// Why a scheduled answer wasn't said: dropped at the front of the line, before it started.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Unsaid {
    /// Out of date by the time the radio was free
    Stale,
    /// One of its requests is gone: cancelled, or dropped as its condition kept failing
    Gone,
}

/// A conversation, as it was when it said something: another one, or the same one started over
/// (`auto_session`), doesn't have what it said.
#[derive(Clone)]
struct Conversation {
    state: Weak<Mutex<SessionState>>,
    starts: u32,
}

impl Conversation {
    /// The conversation `session` has now.
    fn of(session: &Session) -> Self {
        Self {
            state: Arc::downgrade(&session.state),
            starts: session.starts.load(atomic::Ordering::Relaxed),
        }
    }

    /// Whether it's the conversation `session` has now.
    fn is(&self, session: &Session) -> bool {
        // The weak reference keeps the allocation, so another conversation is never taken for it
        Weak::ptr_eq(&self.state, &Arc::downgrade(&session.state))
            && self.starts == session.starts.load(atomic::Ordering::Relaxed)
    }
}

/// An aborted turn's task, a pilot's or a scheduled one, whatever its result: what
/// [`Engineer::drop_session`] waits for.
trait AbortedTurn: Send {
    fn is_finished(&self) -> bool;
    fn ended(self: Box<Self>) -> futures::future::BoxFuture<'static, ()>;
}

impl<T: Send + 'static> AbortedTurn for Task<T> {
    fn is_finished(&self) -> bool {
        Task::is_finished(self)
    }

    fn ended(self: Box<Self>) -> futures::future::BoxFuture<'static, ()> {
        Box::pin(Task::ended(*self))
    }
}

/// A question being answered.
struct Turn {
    /// The reply, with the changes to the requests the turn staged, and everything the pilot
    /// heard of it if the app acknowledged it (see [`Acknowledgement::heard`])
    task: Task<anyhow::Result<(Reply, Vec<Change>, Option<String>)>>,
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
    /// The notes its question carries
    told: Told,
    /// The low tone already sounded for it, as its speech failed: it ends without another
    toned: bool,
}

impl Turn {
    /// Writes the answer so far into its entry in `chat`.
    fn write_into(&mut self, chat: &mut Vec<ChatEntry>) {
        while let Ok(text) = self.text.try_recv() {
            match self.entry {
                Some(entry) => chat[entry].text.push_str(&text),
                None => {
                    self.entry = Some(chat.len());
                    chat.push(ChatEntry {
                        speaker: Speaker::Engineer,
                        text,
                        detail: None,
                    });
                }
            }
        }
    }
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
    /// What it answers, if it's a scheduled answer: checked for staleness once it's at the front,
    /// about to start, and resolved once it leaves the line
    scheduled: Option<ScheduledAnswer>,
    /// Some of it went on a stream: from then on it's never dropped as stale
    started: bool,
}

/// How long the answer at the front waits on its turn, with nothing to play, before another
/// plays in its gap. The speaker may still be playing what SDL has taken from the stream (its
/// queued bytes count only what SDL hasn't), and a short pause sounds natural on the radio.
const GAP_AFTER: Duration = Duration::from_millis(300);

/// How long after a question is sent the app says a short acknowledgement, if the model hasn't
/// written anything yet (see [`acknowledgement`])
pub const ACKNOWLEDGE_AFTER: Duration = Duration::from_secs(2);

/// How much earlier than that the acknowledgement is pushed to speech: about how long it takes
/// to synthesize, so the pilot hears it at about [`ACKNOWLEDGE_AFTER`]
const ACKNOWLEDGE_LEAD: Duration = Duration::from_millis(250);

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
            scheduled: None,
            started: false,
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
                speech.amplify(gain);
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
    /// [`SessionState::starts`], read without waiting for the conversation
    starts: Arc<AtomicU32>,
}

struct SessionState {
    conversation: Box<dyn Model>,
    pool: Pool,
    /// The simulator writing the database, e.g. `lmu`
    sim: &'static str,
    tool: EngineerTool,
    /// Stages the turn's changes to the requests, for the engineer to apply once it finishes
    schedule_tool: ScheduleTool,
    /// Reads and changes the game's pit menu, in games the engineer can change it in
    pit_tool: Option<PitTool>,
    /// The weekend the conversation is about, for `auto_session`: the recording's oldest epoch,
    /// which only a clear (a new weekend) replaces
    weekend: Option<i64>,
    /// How many times `auto_session` started the conversation over: what it said before
    /// doesn't count
    starts: Arc<AtomicU32>,
    /// A hash of the setup's setting names this conversation was last shown (see
    /// [`EngineerTool::question_with_snapshot`]): None until it's shown them
    setup_outline: Option<u64>,
    /// The names the turn being answered offers, kept as shown once the turn is (see
    /// [`SessionState::keep_outline`])
    outline_offered: Option<u64>,
    /// The conversation's [`Model::forgotten`] as last seen
    forgotten: u64,
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
            aborted: Vec::new(),
            incoming: Vec::new(),
            line: VecDeque::new(),
            spotter_held: false,
            talking: false,
            beeping: false,
            hearing: None,
            misheard: 0,
            schedule: Schedule::new(),
            progress: None,
            registered: Vec::new(),
            unheard: Vec::new(),
            scheduled: None,
            scheduled_failing: false,
            announced_model: None,
            #[cfg(test)]
            clock: None,
            #[cfg(test)]
            acknowledge_after: Some(ACKNOWLEDGE_AFTER.saturating_sub(ACKNOWLEDGE_LEAD)),
            #[cfg(test)]
            synthesis: None,
            #[cfg(test)]
            conversations: None,
        }
    }

    /// How long after a question is sent the app pushes its acknowledgement to speech:
    /// [`ACKNOWLEDGE_AFTER`] minus [`ACKNOWLEDGE_LEAD`], unless a test overrides or disables it.
    fn acknowledge_delay(&self) -> Option<Duration> {
        #[cfg(test)]
        return self.acknowledge_after;
        #[cfg(not(test))]
        Some(ACKNOWLEDGE_AFTER.saturating_sub(ACKNOWLEDGE_LEAD))
    }

    /// The wall clock the schedule goes by.
    fn now(&self) -> Instant {
        #[cfg(test)]
        if let Some(clock) = self.clock {
            return clock;
        }
        Instant::now()
    }

    /// Notes `text` for the model's next message, about the conversation as it is now.
    fn note(&mut self, text: String) {
        let conversation = self.session.as_ref().map(Conversation::of);
        self.unheard.push(Note { text, conversation });
    }

    /// The pilot's requests for later.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn schedule(&self) -> &Schedule {
        &self.schedule
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
    /// to join it. A scheduled answer counts only once it's started: one waiting, which may yet
    /// be dropped as out of date, doesn't hold up the spotter's calls.
    #[inline]
    pub fn is_speaking(&self) -> bool {
        (self.incoming.iter().chain(&self.line))
            .any(|playback| playback.scheduled.is_none() || playback.started)
    }

    /// Adds `playback` to the line: how any answer joins it, a pilot turn's or one made
    /// elsewhere (e.g. scheduled). It waits in `incoming` until its first piece of speech is
    /// ready, then joins the back of the line (see [`Self::admit`]).
    pub fn queue(&mut self, playback: Playback) {
        self.incoming.push(playback);
    }

    /// Speaks a scheduled turn's `answer`, written in `lang` (the pilot's reply language when the
    /// turn started, which it was told to answer in): synthesized in one piece, it joins the back
    /// of the line once that's ready. Not streamed: the synthesizer streams one answer at a time
    /// per language, so it would wait behind a pilot answer still being written. Without any
    /// voice, its requests are resolved at once: the chat has it, and that's all.
    fn speak_scheduled(
        &mut self,
        rt: &Handle,
        config: &Config,
        synth: &mut Synthesizer,
        answer: ScheduledAnswer,
        lang: ArrayStr<2>,
        incomplete: bool,
    ) {
        let Some(lang) = self.voice(config, lang) else {
            self.schedule.resolved(&answer.fired);
            self.sync_conditions();
            return;
        };
        let text = answer.text.clone();
        #[cfg(test)]
        if let Some(synthesis) = &mut self.synthesis {
            let (send, speech) = SpeechOutput::channel();
            synthesis.push(Synthesis {
                lang,
                text,
                speech: Some(send),
            });
            self.queue_scheduled(speech, answer, incomplete);
            return;
        }
        // On the synthesizer's worker: nothing here waits for it
        let effects = config.get().synth_effects();
        let speech = synth.synthesize_output(rt, lang, text, effects);
        self.queue_scheduled(speech, answer, incomplete);
    }

    /// Adds a scheduled answer, its `speech` made in one piece, to the line. `incomplete`: it was
    /// cut off, and the incomplete tone follows it, as for a pilot answer.
    fn queue_scheduled(&mut self, speech: SpeechOutput, answer: ScheduledAnswer, incomplete: bool) {
        let mut playback = Playback::new(speech, true);
        playback.tone_after_drain = incomplete;
        playback.scheduled = Some(answer);
        self.queue(playback);
    }

    /// `playback` left the line (or never joined it), played or not: a scheduled answer's
    /// requests are resolved. Every way out goes through here.
    fn left(&mut self, playback: Playback) {
        if let Some(answer) = playback.scheduled {
            self.schedule.resolved(&answer.fired);
            self.sync_conditions();
        }
    }

    /// The answer at the front of the line [leaves](Self::left) it.
    fn drop_front(&mut self) {
        if let Some(playback) = self.line.pop_front() {
            self.left(playback);
        }
    }

    /// Every answer [leaves](Self::left) the line, and those waiting to join it.
    fn clear_line(&mut self) {
        let dropped: Vec<_> = (self.incoming.drain(..))
            .chain(self.line.drain(..))
            .collect();
        for playback in dropped {
            self.left(playback);
        }
    }

    /// The answers `which` picks [leave](Self::left) the line, and those waiting to join it;
    /// the others keep their places.
    fn drop_answers(&mut self, which: impl Fn(&Playback) -> bool) {
        let (dropped, kept): (Vec<_>, Vec<_>) = self.incoming.drain(..).partition(&which);
        self.incoming = kept;
        let (in_line, line): (VecDeque<_>, VecDeque<_>) = self.line.drain(..).partition(&which);
        self.line = line;
        for playback in dropped.into_iter().chain(in_line) {
            self.left(playback);
        }
    }

    /// The scheduled answers that haven't started leave the line, and those waiting to join it:
    /// their requests are being cleared. One that has started plays out.
    fn drop_waiting_answers(&mut self) {
        self.drop_answers(|playback| playback.scheduled.is_some() && !playback.started);
    }

    /// Drops the scheduled answer at the front of the line if it's out of date, before it starts
    /// (once started, it's never dropped as stale), and those after it that are too. Each
    /// dropped is resolved, and the model told the pilot didn't hear it, if its conversation is
    /// still the one it gave the answer in. The schedule steps through the newest progress first:
    /// staleness counts the epoch boundaries it's seen.
    fn drop_stale_front(&mut self) {
        loop {
            let unstarted = (self.line.front())
                .is_some_and(|playback| playback.scheduled.is_some() && !playback.started);
            if !unstarted {
                return;
            }
            // Nothing here waits for the data thread (see `follow_progress`); a new weekend may
            // drop the answer
            self.follow_progress();
            let Some(playback) = self.line.front() else {
                return;
            };
            let Some(answer) = &playback.scheduled else {
                return;
            };
            // Without progress, nothing moves on: it isn't out of date
            let stale = (self.progress.as_ref())
                .is_some_and(|progress| self.schedule.is_stale(&answer.fired, progress));
            if !stale {
                return;
            }
            let why = match self.schedule.is_gone(&answer.fired) {
                true => Unsaid::Gone,
                false => Unsaid::Stale,
            };
            let playback = self.line.pop_front().expect("just looked");
            let answer = playback.scheduled.as_ref().expect("just looked");
            // Told by its conversation's next turn (see `Note::conversation`); not even noted
            // once that's over
            let current = (answer.conversation.as_ref())
                .zip(self.session.as_ref())
                .is_some_and(|(conversation, session)| conversation.is(session));
            if current {
                self.unheard.push(Note {
                    text: unheard_note(&answer.text, why),
                    conversation: answer.conversation.clone(),
                });
            }
            if let Some(entry) = answer.entry.and_then(|entry| self.chat.get_mut(entry)) {
                let not_said = match why {
                    Unsaid::Stale => t!("engineer.detail_not_said_stale"),
                    Unsaid::Gone => t!("engineer.detail_not_said_gone"),
                };
                entry.detail = Some(match &entry.detail {
                    Some(detail) => format!("{detail} | {not_said}"),
                    None => not_said.to_owned(),
                });
            }
            log::info!("Dropped a scheduled answer ({why:?}): {}", answer.text);
            self.left(playback);
        }
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
    /// pilot talks, as [`Self::hold`] does for the spotter, so it doesn't play over them. It stays
    /// paused until nothing else holds it.
    ///
    /// Once they stop, scheduled turns are held back until what was said is [heard](Self::hear), so
    /// one doesn't start just to be dropped by the question.
    pub fn talk(&mut self, talking: bool) {
        if self.talking && !talking {
            self.hearing = Some(self.now());
        }
        self.talking = talking;
        self.hold_front();
    }

    /// Holds the answer at the front of the line while the talk button's beep plays, as
    /// [`Self::hold`] does for the spotter. Only the spotter's urgent calls play over it.
    pub fn beep(&mut self, beeping: bool) {
        self.beeping = beeping;
        self.hold_front();
    }

    /// Whether the talk button's beep is playing.
    pub fn is_beeping(&self) -> bool {
        self.beeping
    }

    /// Whether the front of the line waits: the spotter is speaking, the pilot is talking or the
    /// talk button's beep is playing.
    fn held(&self) -> bool {
        self.spotter_held || self.talking || self.beeping
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
            self.drop_front();
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
                self.drop_front();
            }
        }
        if on_it || picked {
            self.notice(t!("engineer.speaker_removed"));
        }
    }

    /// Starts a new conversation with the next question: the engineer forgets what was said,
    /// and what it was asked to do later. (A full context window starts one too, keeping the
    /// requests.)
    pub fn new_session(&mut self) {
        self.stop();
        if let Some(lease) = self.session.as_ref().and_then(|s| s.lease.as_ref()) {
            lease.end();
        }
        self.new_session = true;
        self.clear_requests();
        // About the conversation that's over
        self.unheard.clear();
        self.notice(t!("engineer.new_session"));
    }

    /// Drops every request, telling the pilot if there were any, with the scheduled turn
    /// answering some and the answers waiting to be spoken.
    fn clear_requests(&mut self) {
        if self.schedule.len() > 0 {
            self.notice(t!("engineer.requests_cleared"));
        }
        self.drop_scheduled();
        self.drop_waiting_answers();
        self.schedule.clear();
        self.sync_conditions();
    }

    /// Drops the scheduled turn running, if any, unnoticed: the pilot never asked for it. Its
    /// requests stay due, for the next one; what it was to be told is told next.
    fn drop_scheduled(&mut self) {
        if let Some(mut scheduled) = self.scheduled.take() {
            self.unheard.splice(0..0, scheduled.told.untold());
            self.schedule.batch_dropped();
            self.abort(scheduled.task);
        }
    }

    /// Aborts a turn's `task`, waited for until the conversation it holds is dropped (see
    /// [`Self::drop_session`]). Those whose futures are gone already are let go.
    fn abort<T: Send + 'static>(&mut self, task: Task<T>) {
        task.abort();
        self.aborted.retain(|task| !task.is_finished());
        self.aborted.push(Box::new(task));
    }

    /// Registers the schedule's conditions with the followed tool, if they changed: a swap of
    /// the list, not a wait for its data thread.
    fn sync_conditions(&mut self) {
        let conditions = self.schedule.conditions();
        if conditions == self.registered {
            return;
        }
        if let Some(tool) = &self.tool {
            tool.set_conditions(conditions.clone());
        }
        self.registered = conditions;
    }

    /// Steps the schedule through the followed tool's newest progress, once per snapshot. Only
    /// copies a new snapshot: nothing here waits for the data thread.
    fn follow_progress(&mut self) {
        let Some(tool) = &self.tool else {
            return;
        };
        // None while catching up: the last snapshot is kept
        let Some(seq) = tool.progress_seq() else {
            return;
        };
        if (self.progress.as_ref()).is_some_and(|last| last.seq == seq) {
            return;
        }
        // Read only once it's new: reading it is what tells the tool its epochs were seen
        let Some(progress) = tool.progress() else {
            return;
        };
        let advance = self.schedule.advance(&progress, self.now());
        self.progress = Some(progress);
        if advance.skipped_stale > 0 {
            log::debug!(
                "Skipped {} scheduled requests that went out of date before they were answered",
                advance.skipped_stale
            );
        }
        if advance.cleared {
            // A new weekend: what was being answered or said is about the last one. A scheduled
            // turn that already finished was kept by the conversation: it's taken in as usual
            // (its answer is about cleared requests, so it isn't said)
            if !self.scheduled.as_ref().is_some_and(|s| s.task.is_finished()) {
                self.drop_scheduled();
            }
            self.drop_waiting_answers();
            self.notice(t!("engineer.requests_cleared"));
        }
        for (id, instruction) in advance.dropped {
            self.notice(t!(
                "engineer.condition_dropped",
                id = id,
                instruction = instruction
            ));
        }
        self.sync_conditions();
    }

    /// Applies a finished turn's changes to the requests, at the newest progress. Those that
    /// couldn't apply (their request had ended meanwhile, or a new weekend started) are told to
    /// the pilot, and to the model with the next message. `answered`: what the turn answered, if
    /// it's a scheduled one (see [`Schedule::apply_answering`]).
    fn apply(&mut self, changes: Vec<Change>, answered: &[Fired]) {
        if changes.is_empty() {
            return;
        }
        // Not stepped through yet (published and used within a frame): the newest the turn used
        let made = changes
            .iter()
            .filter_map(|change| match change {
                Change::Add(_, new) | Change::Replace(_, new) => Some(&new.made),
                Change::Cancel(_) => None,
            })
            .max_by_key(|made| made.seq);
        // The newer of the two: one the turn used may be newer than any stepped through (published
        // and taken away unread while the tables caught up), and applying at an older one could
        // take its weekend for the one before
        let newest = match (self.progress.as_ref(), made) {
            (Some(stepped), Some(made)) if made.seq > stepped.seq => Some(made),
            (stepped, made) => stepped.or(made),
        };
        let Some(progress) = newest.cloned() else {
            log::warn!("No progress to apply the turn's changes to the requests at");
            return;
        };
        let applied = self.schedule.apply_answering(changes, &progress, answered);
        for id in applied.missing {
            self.notice(t!("engineer.request_missing", id = id));
            self.note(missing_note(id));
        }
        // The pilot heard them confirmed
        for id in applied.dropped {
            self.notice(t!("engineer.request_dropped", id = id));
            self.note(dropped_note(id));
        }
        self.sync_conditions();
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
        // Whatever it was: nothing else is on its way
        self.hearing = None;
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
    /// not on the UI thread. Its requests go with it: the recording is deleted with the weekend.
    fn drop_tool(&mut self, rt: &Handle) {
        if let Some(tool) = self.tool.take() {
            rt.spawn_blocking(move || drop(tool));
            // Registered with the tool that's gone
            self.registered.clear();
            self.progress = None;
            self.clear_requests();
        }
    }

    /// Ends the conversation: every path that ends one (a new one started over it, the simulator
    /// stopping, a sign-out, the recording switched) goes through here. A conversation may hold the
    /// last pool reference once its recording stops being followed.
    fn drop_session(&mut self, rt: &Handle) {
        // Its turn goes with it: it runs again on the next conversation
        self.drop_scheduled();
        let aborted = std::mem::take(&mut self.aborted);
        if let Some(session) = self.session.take() {
            // An aborted turn's future, dropped by a runtime worker, mustn't hold the last
            // reference: the session is dropped once they're gone, on a blocking thread
            rt.spawn(async move {
                for task in aborted {
                    task.ended().await;
                }
                let _ = tokio::task::spawn_blocking(move || drop(session)).await;
            });
            // About the conversation that's over
            self.unheard.clear();
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
        // A scheduled turn gives way, unnoticed: its requests stay due, for the next one once
        // this question is answered. One that finished meanwhile counts: the conversation kept it.
        // One finishing between this check and the drop below can't be told from one dropped
        // before it finished: the conversation keeps that exchange, but its answer and changes are
        // lost with the task. The window is the instant between the reply coming in and the task
        // ending, against a turn that takes seconds, and it costs little: the requests stay due
        // and the next scheduled turn answers them again (every message lists what's pending, so
        // the model sees which changes didn't apply), and the pilot never hears that one answer.
        // Closing it would take waiting on this thread for the aborted task to end.
        if let Some(scheduled) = &mut self.scheduled
            && let Some(result) = scheduled.task.update(rt)
            && let Some(scheduled) = self.scheduled.take()
        {
            self.finish_scheduled(rt, config, synth, scheduled, result);
        }
        self.drop_scheduled();
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
            // A tone that already sounded doesn't answer a question asked after it
            turn.toned = false;
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
            .and_then(|sim| Some((sim.id(), sim.database()?.clone(), sim.pit())));
        // After `database`, which takes in the latest look at the plugin
        let outdated = sim.as_deref().is_some_and(|sim| sim.plugin_outdated());
        let Some((sim, pool, pit)) = database else {
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

        if let Err(e) = self.follow_session(rt, config, sim, pool, pit) {
            self.notice(t!("engineer.error_starting", error = format!("{e:#}")));
            self.no_beep();
            return;
        }
        self.start_turn(rt, config, synth, question, lang, known, timeline);
    }

    /// Makes the conversation the one for `pool`'s recording, of the simulator `sim`, starting it
    /// over first if it must be (a new session was asked for, the model settings changed,
    /// another recording). `Err`: why it couldn't be started; the one before stays.
    fn follow_session(
        &mut self,
        rt: &Handle,
        config: &Config,
        sim: &'static str,
        pool: Pool,
        pit: Option<Arc<dyn PitService>>,
    ) -> anyhow::Result<()> {
        let db = pool.database().path().to_owned();
        let model_changed = self
            .session
            .as_ref()
            .is_some_and(|s| s.model != config.get().model);
        let corners_changed = self
            .session
            .as_ref()
            .is_some_and(|s| s.corner_names != *config.get().corner_names());
        // Once for each change, though the new model may fail to start again and again
        let model = &config.get().model;
        if !model_changed {
            self.announced_model = None;
        } else if self.announced_model.as_ref() != Some(model) {
            self.announced_model = Some(model.clone());
            self.notice(t!("engineer.new_model"));
        }
        if self.new_session
            || model_changed
            || corners_changed
            || self.session.as_ref().is_none_or(|s| s.db != db)
        {
            let tool = self.tool_for(rt, &pool);
            let session = self.start(config, sim, pool, tool, pit)?;
            self.drop_session(rt);
            self.session = Some(session);
            self.new_session = false;
        }
        Ok(())
    }

    /// What a turn's message carries after its snapshot, and the notes it tells, taken now:
    /// the pending requests, and what the model doesn't know happened. `note`: which language
    /// to reply in, if it's told.
    fn briefing(&mut self, config: &Config, note: Option<Arc<str>>) -> (Briefing, Told) {
        let told = Told {
            notes: std::mem::take(&mut self.unheard),
            started_over: Arc::default(),
        };
        let progress = self.progress.clone().unwrap_or_default();
        let state = (self.session.as_ref()).map_or_else(Weak::new, |s| Arc::downgrade(&s.state));
        let briefing = Briefing {
            auto_session: *config.get().auto_session(),
            pending: self.schedule.pending_ids(),
            next_id: self.schedule.next_id(),
            pending_text: self.schedule.pending_text(&progress),
            notes: told.notes.clone(),
            state,
            note,
            started_over: told.started_over.clone(),
        };
        (briefing, told)
    }

    /// Starts a turn on the current session for `question`, answered in `lang` (`known`: told
    /// to the model, see [`Self::ask`]). If the model hasn't said anything [`ACKNOWLEDGE_AFTER`]
    /// after the turn starts, the app acknowledges the question (see [`acknowledgement`]).
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

        // What the model writes goes straight to the synthesizer, and to the chat
        let speech = self.speak(rt, config, synth, lang, Some(timeline.clone()));
        let lang = speech.as_ref().map_or(lang, |(lang, _)| *lang);
        let speech = speech.map(|(_, speech)| speech);
        // Pushed a little early, to be heard on time once it's synthesized
        let acknowledge = acknowledgement::phrase(&lang).zip(self.acknowledge_delay());
        // Told last, after the question (and its snapshot) and after each tool result: by the
        // answer, the skill, the telemetry and often earlier answers are all in English
        let note: Option<Arc<str>> = known.then(|| language::reply_note(lang).into());
        // Questions steering the turn join it, so the notes are told once
        let (briefing, told) = self.briefing(config, note);
        let (chat, text) = mpsc::unbounded_channel();
        let steer = Steer::default();
        let slot = steer.clone();
        let task = Task::spawn(async move {
            // Shared by the model's text and the timer, never locked across an await
            let acknowledgement = std::sync::Mutex::new(Acknowledgement::new());
            let say = |text: Option<String>| {
                let Some(text) = text else {
                    return;
                };
                if let Some(speech) = &speech {
                    speech.push(text.as_str());
                }
                let _ = chat.send(text);
            };
            let mut on_event = |event: Streamed<'_>| {
                let text = {
                    let mut acknowledgement = acknowledgement.lock().unwrap();
                    match event {
                        Streamed::Text(text) => {
                            timeline.first_text();
                            acknowledgement.text(text)
                        }
                        // Not a query: a `schedule` or `pit` call is confirmed once made, and
                        // needs no acknowledgement first (see the skill)
                        Streamed::Tools(names) => {
                            let queried = (names.iter())
                                .any(|&name| name != ScheduleTool::NAME && name != PitTool::NAME);
                            acknowledgement.tools(queried)
                        }
                    }
                };
                say(text);
            };
            let turn = async {
                let mut state = state.lock().await;
                // Once per turn, steers included: its changes apply together, or not at all
                let question = state.brief(question, briefing).await;
                state.conversation.set_steer(Some(slot));
                let reply = state.conversation.send_streaming(question, &mut on_event).await;
                if reply.is_ok() {
                    timeline.answered();
                }
                state.keep_outline(&reply);
                let changes = state.schedule_tool.take();
                reply.map(|reply| (reply, changes))
            };
            // From when the question is asked (the conversation's wait and the briefing included),
            // not from the talk button's release: slow transcription mustn't spend the delay
            // before the model sees the question. It never ends, so the turn does
            let acknowledge = async {
                if let Some((phrase, delay)) = acknowledge {
                    tokio::time::sleep(delay).await;
                    let text = acknowledgement.lock().unwrap().fire(phrase);
                    if text.is_some() {
                        timeline.acknowledged();
                    }
                    say(text);
                }
                std::future::pending::<()>().await
            };
            let result = tokio::select! {
                // A turn that ends at once (a request refused before anything was sent) ends
                // unacknowledged, every time
                biased;
                result = turn => result,
                () = acknowledge => unreachable!("never ends"),
            };
            let end = acknowledgement.lock().unwrap().end();
            say(end);
            // For the chat, which shows what the pilot heard
            let heard = (acknowledgement.lock().unwrap().heard()).map(str::to_owned);
            result.map(|(reply, changes)| (reply, changes, heard))
            // The speech's text ends here, with the turn
        });

        self.turn = Some(Turn {
            task,
            lang,
            text,
            entry: None,
            steer,
            steered: None,
            told,
            toned: false,
        });
    }

    /// Starts a scheduled turn for the requests that are due, once they've gathered, if nothing
    /// is answering: never while a pilot turn runs, or while a question left over from one waits
    /// to become the next (it's asked earlier in the same frame). Nothing here waits: the
    /// conversation is locked, the snapshot taken and the model asked in the turn's task.
    fn run_scheduled(&mut self, rt: &Handle, sim: Option<&mut Sim>, config: &Config) {
        // A pilot talking is about to ask something, which would drop it, and one who stopped may
        // have: what they said is still being heard
        if self.turn.is_some() || self.scheduled.is_some() || self.talking {
            return;
        }
        let now = self.now();
        if let Some(released) = self.hearing {
            if now.saturating_duration_since(released) < HEARING_WAIT {
                return;
            }
            // Lost: no question is coming
            self.hearing = None;
        }
        let Some(batch) = self.schedule.take_batch(now) else {
            return;
        };
        let database =
            sim.and_then(|sim| Some((sim.id(), sim.database()?.clone(), sim.pit())));
        // Nothing to answer from: they stay due (no progress comes either, so nothing moves on)
        let Some((sim, pool, pit)) = database else {
            self.schedule.batch_dropped();
            return;
        };
        if let Err(e) = self.follow_session(rt, config, sim, pool, pit) {
            self.scheduled_failed(&e, now);
            return;
        }
        let Some(session) = &self.session else {
            self.schedule.batch_dropped();
            return;
        };
        let state = session.state.clone();
        // The pilot's current reply language: the selected one, or the last they used
        let lang = (config.get().engineer_reply_language()).unwrap_or(self.last_lang);
        let lang = self.voice(config, lang).unwrap_or(lang);
        let message = scheduled_message(&batch);
        let (briefing, told) = self.briefing(config, Some(language::reply_note(lang).into()));
        let task = Task::spawn(async move {
            let mut state = state.lock().await;
            let message = state.brief(message, briefing).await;
            // Not the pilot's turn: a question asked meanwhile drops it rather than steering it
            state.conversation.set_steer(None);
            let reply = state.conversation.send(message).await;
            state.keep_outline(&reply);
            let changes = state.schedule_tool.take();
            reply.map(|reply| (reply, changes))
        });
        self.scheduled = Some(ScheduledTurn {
            task,
            batch,
            lang,
            told,
        });
    }

    /// A scheduled turn couldn't run: its requests stay due, tried again after
    /// [`schedule::RETRY_AFTER`] until they go stale. The pilot is told once per outage, not for
    /// every retry, and with no tone: they didn't ask.
    fn scheduled_failed(&mut self, e: &anyhow::Error, now: Instant) {
        self.schedule.batch_failed(now);
        if !std::mem::replace(&mut self.scheduled_failing, true) {
            self.notice(t!("engineer.scheduled_failed", error = format!("{e:#}")));
        }
    }

    /// A scheduled turn finished with `result`: its requests are answered, or tried again if it
    /// failed. Its answer is spoken, or, with nothing worth saying, the chat says so.
    fn finish_scheduled(
        &mut self,
        rt: &Handle,
        config: &Config,
        synth: &mut Synthesizer,
        scheduled: ScheduledTurn,
        result: anyhow::Result<(Reply, Vec<Change>)>,
    ) {
        let ScheduledTurn {
            batch,
            lang,
            mut told,
            ..
        } = scheduled;
        let (reply, changes) = match result {
            Ok(done) => done,
            // Forgotten: what it was told is told next
            Err(e) => {
                self.unheard.splice(0..0, told.untold());
                let now = self.now();
                self.scheduled_failed(&e, now);
                return;
            }
        };
        self.scheduled_failing = false;
        log_reply(&reply);
        // Cut off before it said anything: nothing was answered. Its requests stay due, tried
        // again after a pause rather than at once, so a turn that keeps ending this way (its
        // tokens spent on lookups, or a conversation filled within the turn) doesn't run back to
        // back. A full context window starts the conversation over for it. The conversation keeps
        // the exchange all the same, with the changes it confirmed: they count, as a pilot
        // turn's do, answering nothing
        let cut_off = matches!(
            reply.stop,
            StopReason::MaxTokens | StopReason::ContextWindowExceeded
        );
        if cut_off && reply.text.trim().is_empty() {
            if reply.stop == StopReason::ContextWindowExceeded {
                self.new_session = true;
            }
            // Due again first: a request it replaced stays due, as replaced
            let now = self.now();
            self.schedule.batch_failed(now);
            self.apply(changes, &[]);
            return;
        }
        let what = what_fired(
            (batch.requests.iter()).map(|(id, instruction, ..)| (*id, instruction.as_str())),
        );
        // Before its own changes: a request it replaces keeps this answer waiting in its place
        self.schedule.answered(&batch.fired);
        // A one-shot's trigger isn't checked once it's answered
        self.sync_conditions();
        let silent = match reply.stop {
            // Forgotten, and what it changed with it: answered with silence
            StopReason::Refusal => {
                self.unheard.splice(0..0, told.untold());
                true
            }
            stop => {
                // Every later turn would end the same way: the next one starts over
                if stop == StopReason::ContextWindowExceeded {
                    self.new_session = true;
                }
                self.apply(changes, &batch.fired);
                // Empty: it ended its turn with nothing to say (a cut-off one went above)
                reply.text.contains(SILENCE) || reply.text.trim().is_empty()
            }
        };
        // Nothing plays, not even a tone
        if silent {
            self.notice(t!("engineer.nothing_to_report", what = what));
            self.schedule.resolved(&batch.fired);
            self.sync_conditions();
            return;
        }

        let mut detail = tokens_detail(&reply);
        let incomplete = match reply.stop {
            StopReason::MaxTokens => {
                detail = format!("{detail} | {}", t!("engineer.detail_cut_off"));
                true
            }
            StopReason::ContextWindowExceeded => {
                detail = format!("{detail} | {}", t!("engineer.detail_full"));
                true
            }
            StopReason::EndTurn | StopReason::StopSequence | StopReason::Refusal => false,
        };
        self.notice(t!("engineer.scheduled", what = what));
        self.reply(reply.text.clone(), Some(detail));
        let answer = ScheduledAnswer {
            text: reply.text,
            fired: batch.fired,
            conversation: self.session.as_ref().map(Conversation::of),
            entry: Some(self.chat.len() - 1),
        };
        self.speak_scheduled(rt, config, synth, answer, lang, incomplete);
    }

    /// Called every frame: answers come in, are synthesized and played. `sim`: the running
    /// simulator, if any, for the questions a turn left over.
    pub fn update(
        &mut self,
        rt: &Handle,
        mut sim: Option<&mut Sim>,
        config: &Config,
        synth: &mut Synthesizer,
        speaker: Option<&AudioDevice>,
    ) {
        self.languages.update(rt, config.get().voices());
        // The game stopped: the conversation lets go of the database, so the plugin can delete it
        if sim.is_none() && self.session.is_some() {
            let turn = self.turn.take();
            let interrupted = turn.is_some();
            if let Some(turn) = turn {
                self.abort(turn.task);
            }
            self.drop_session(rt);
            self.notice(t!("engineer.sim_stopped"));
            // The recording goes with the game
            self.clear_requests();
            if interrupted {
                // What it was saying goes with the turn, which the conversation forgot
                self.clear_line();
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

        // Before a turn's changes are applied, at the newest progress
        self.follow_progress();

        if let Some(turn) = &mut self.turn {
            turn.write_into(&mut self.chat);

            if let Some(result) = turn.task.update(rt)
                && let Some(mut turn) = self.turn.take()
            {
                // What it wrote just before it ended
                turn.write_into(&mut self.chat);
                // Questions the turn missed (it was writing its final text) are asked next, as
                // any question is. A failed or refused turn was forgotten, and they go with it:
                // they may build on its question
                let missed = match &result {
                    Ok((reply, ..)) if reply.stop != StopReason::Refusal => turn.steer.take(),
                    Ok(_) | Err(_) => Vec::new(),
                };
                let (lang, released) = turn.steered.unwrap_or_default();
                match result {
                    // The conversation kept the exchange: what it changed counts
                    Ok((reply, changes, heard)) if reply.stop != StopReason::Refusal => {
                        // The model's reachable: a scheduled turn failing next is another outage
                        self.scheduled_failing = false;
                        self.answer(rt, config, synth, reply, heard, turn);
                        self.apply(changes, &[]);
                    }
                    // Forgotten, and what it changed with it
                    Ok((reply, _, heard)) => {
                        self.scheduled_failing = false;
                        self.unheard.splice(0..0, turn.told.untold());
                        self.answer(rt, config, synth, reply, heard, turn);
                    }
                    Err(e) => {
                        self.unheard.splice(0..0, turn.told.untold());
                        if let Some(entry) = turn.entry {
                            self.chat[entry].detail = Some(t!("engineer.detail_cut_off").into());
                        }
                        self.notice(t!("engineer.error_asking", error = format!("{e:#}")));
                        // What it said before failing plays out first
                        if !turn.toned {
                            self.incomplete_beep();
                        }
                    }
                }
                // Its answer has all it will have
                for playback in self.incoming.iter_mut().chain(&mut self.line) {
                    playback.writing = false;
                }
                if !missed.is_empty() {
                    let question = missed.join("\n");
                    let sim = sim.as_deref_mut();
                    self.send_question(rt, sim, config, synth, question, lang, released);
                }
            }
        }

        if let Some(scheduled) = &mut self.scheduled
            && let Some(result) = scheduled.task.update(rt)
            && let Some(scheduled) = self.scheduled.take()
        {
            self.finish_scheduled(rt, config, synth, scheduled, result);
        }
        // The account the conversation is had as signed out (or another signed in): it ends,
        // answer on its way included. After the turns that finished are taken in: the conversation
        // kept their exchanges, and the requests outlive it, so what they changed counts rather
        // than being dropped as if they never ran
        if self.session.as_ref().is_some_and(|s| !self.signed_in_as(s)) {
            let interrupted = self.turn.is_some();
            self.stop();
            self.drop_session(rt);
            self.notice(t!("engineer.signed_out"));
            if interrupted {
                self.no_beep();
            }
        }
        self.run_scheduled(rt, sim, config);

        self.play(config, speaker);
    }

    /// Shows the turn's `reply` in the chat, and sounds what its end calls for. `heard`: everything
    /// the pilot heard of it, if the app acknowledged it, shown instead of the model's text.
    fn answer(
        &mut self,
        rt: &Handle,
        config: &Config,
        synth: &mut Synthesizer,
        reply: Reply,
        heard: Option<String>,
        turn: Turn,
    ) {
        log_reply(&reply);
        let mut detail = tokens_detail(&reply);
        // What the pilot heard after the app's acknowledgement, if it acknowledged
        let heard_after = (heard.as_deref())
            .map(|heard| heard.split_once('\n').map_or("", |(_, after)| after).to_owned());
        // The model's text, unless the pilot heard something else
        let shown = heard.unwrap_or_else(|| reply.text.clone());

        let text = match reply.stop {
            // The exchange was forgotten, and whatever was said with it: the pilot can ask again
            StopReason::Refusal => {
                detail = format!("{detail} | {}", t!("engineer.detail_declined"));
                // Its answer stops, unheard or not, as the chat only shows "say again"
                self.drop_answers(|playback| playback.writing);
                let say_again = t!("engineer.say_again": &turn.lang);
                // A voice that failed this turn would fail again, sounding a second tone
                if !turn.toned {
                    self.say(rt, config, synth, say_again, turn.lang, None);
                    if self.voice(config, turn.lang).is_none() {
                        self.no_beep();
                    }
                }
                say_again.to_owned()
            }
            StopReason::MaxTokens => {
                detail = format!("{detail} | {}", t!("engineer.detail_cut_off"));
                if !turn.toned {
                    self.incomplete_beep();
                }
                shown
            }
            // Every later question would end the same way: the next one starts over
            StopReason::ContextWindowExceeded => {
                self.new_session = true;
                detail = format!("{detail} | {}", t!("engineer.detail_full"));
                if !turn.toned {
                    self.incomplete_beep();
                }
                shown
            }
            // Its speech may still fail after this, sounding the tone: it's the one tone then
            StopReason::EndTurn | StopReason::StopSequence => {
                // Nothing answered: the pilot heard nothing after the app's acknowledgement (its
                // phrase is one line), or the model wrote nothing
                let answered = match &heard_after {
                    Some(after) => !after.trim().is_empty(),
                    None => !reply.text.trim().is_empty(),
                };
                // After what's still on its way (the acknowledgement, or nothing at all)
                if !answered && !turn.toned {
                    self.incomplete_beep();
                }
                shown
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
                // No tone for a scheduled answer: the pilot didn't ask
                Poll::Ready(Some(Err(e))) => {
                    let playback = self.incoming.remove(i);
                    let tone = playback.scheduled.is_none();
                    let writing = playback.writing;
                    self.left(playback);
                    self.notice(t!("engineer.error_synthesizing", error = format!("{e:#}")));
                    if tone {
                        self.no_beep();
                        // Its pilot turn ends without another
                        if writing && let Some(turn) = &mut self.turn {
                            turn.toned = true;
                        }
                    }
                }
                // It said nothing
                Poll::Ready(None) => {
                    let playback = self.incoming.remove(i);
                    let tone = playback.tone_after_drain && playback.scheduled.is_none();
                    self.left(playback);
                    if tone {
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
        // It's checked for staleness at the front, before it starts (see `drop_stale_front`)
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
        // A scheduled answer about to start may be out of date by now
        if !self.held() {
            self.drop_stale_front();
        }
        // Whichever answer is at the front waits while something else is heard (see `held`)
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
                    // No tone for a scheduled answer: the pilot didn't ask
                    let tone = playback.scheduled.is_none();
                    let writing = playback.writing;
                    self.drop_front();
                    self.notice(t!("engineer.error_synthesizing", error = format!("{e:#}")));
                    if tone {
                        self.no_beep();
                        // Its pilot turn ends without another
                        if writing && let Some(turn) = &mut self.turn {
                            turn.toned = true;
                        }
                    }
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
                    self.drop_front();
                    self.notice(t!("engineer.no_speaker"));
                    return;
                }
            };

            if let Err(e) = played {
                self.drop_front();
                self.notice(t!("engineer.error_playing", error = e));
                return;
            }
            playback.sentences.submitted();
            playback.started = true;
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
                self.drop_front();
                // Right after the answer it belongs to; the next one starts on a later frame
                if tone {
                    self.line.push_front(Playback::tone());
                }
            }
            Drained::Stalled => {
                self.drop_front();
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
        self.drop_scheduled();
        if let Some(mut turn) = self.turn.take() {
            // What it changed goes with it; what it was to be told is told next
            self.unheard.splice(0..0, turn.told.untold());
            if let Some(entry) = turn.entry {
                self.chat[entry].detail = Some(t!("engineer.detail_cut_off").into());
            }
            self.notice(t!("engineer.cancelled"));
            self.abort(turn.task);
        }
        self.clear_line();
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

    /// A new conversation with `model`, given `tools`.
    fn conversation(
        &self,
        model: &ModelConfig,
        corner_names: bool,
        tools: Tools,
    ) -> anyhow::Result<Box<dyn Model>> {
        #[cfg(test)]
        if let Some(make) = &self.conversations {
            return Ok(make(tools));
        }
        provider::conversation(
            model,
            self.account.as_ref(),
            system_prompt(&self.skill, corner_names),
            tools,
            self.client.clone(),
        )
    }

    fn start(
        &self,
        config: &Config,
        sim: &'static str,
        pool: Pool,
        tool: EngineerTool,
        pit: Option<Arc<dyn PitService>>,
    ) -> anyhow::Result<Session> {
        let schedule_tool = ScheduleTool::new(tool.clone());
        // Only for a game the engineer can change the pit menu in
        let pit_tool = pit.map(PitTool::new);
        let tools = session_tools(&tool, &schedule_tool, pit_tool.as_ref());
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
        let conversation = self.conversation(&model, corner_names, tools)?;

        let db = pool.database().path().to_owned();
        log::info!("Engineer session started on {}", db.display());
        let starts = Arc::<AtomicU32>::default();
        Ok(Session {
            db,
            model,
            corner_names,
            user,
            lease: conversation.lease(),
            state: Arc::new(Mutex::new(SessionState {
                conversation,
                tool,
                schedule_tool,
                pit_tool,
                pool,
                sim,
                weekend: None,
                starts: starts.clone(),
                setup_outline: None,
                forgotten: 0,
                outline_offered: None,
            })),
            starts,
        })
    }

    /// The app is quitting: Bono Cloud's lease is ended, waiting for the website a moment, as
    /// nothing will be left to after.
    pub fn quit(&mut self, rt: &Handle) -> tokio::task::JoinHandle<()> {
        /// The longest the app waits on the website to quit
        const WAIT: Duration = Duration::from_secs(3);

        self.stop();
        self.schedule.clear();
        let session = self.session.take();
        let tool = self.tool.take();
        let aborted = std::mem::take(&mut self.aborted);
        rt.spawn(async move {
            if let Some(lease) = session.as_ref().and_then(|s| s.lease.as_ref())
                && tokio::time::timeout(WAIT, lease.end_and_wait())
                    .await
                    .is_err()
            {
                log::warn!("Gave up ending the Cloud session lease: it expires on its own");
            }
            // Closing the final pool reference joins SQLite workers, including an interrupted
            // snapshot's job. Keep this off both the UI and the async executor's workers, and
            // so after the aborted turns holding it are gone (see `drop_session`).
            for task in aborted {
                task.ended().await;
            }
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

/// What the model is told when a change it made to request `id` didn't apply: the request was
/// gone by the time its reply finished.
fn missing_note(id: RequestId) -> String {
    format!(
        "[Request {id} had already ended by the time your last reply finished, so your change to \
         it didn't apply.]"
    )
}

/// What the model is told when request `id`, which it added, wasn't kept: the recording was
/// cleared (a new weekend) before its reply finished.
fn dropped_note(id: RequestId) -> String {
    format!(
        "[Request {id}, which you added, wasn't kept: the recorded data was cleared for a new \
         weekend before your last reply finished, so nothing is scheduled for it.]"
    )
}

/// How much of an unheard answer the note about it quotes, in characters.
const UNHEARD_QUOTE: usize = 80;

/// What the model is told when its scheduled answer `text` was dropped before the pilot heard
/// it, and `why`.
fn unheard_note(text: &str, why: Unsaid) -> String {
    let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let quote = match text.char_indices().nth(UNHEARD_QUOTE) {
        Some((end, _)) => format!("{}...", &text[..end]),
        None => text,
    };
    let why = match why {
        Unsaid::Stale => "it was out of date by the time the radio was free",
        Unsaid::Gone => "its request was cancelled or dropped before the radio was free",
    };
    format!(
        "[The pilot didn't hear your scheduled answer \"{quote}\": {why}, so it wasn't said. \
         Don't refer to it as said.]"
    )
}

/// What the model reads for a scheduled turn answering `batch`: that it's the engineer's own
/// turn, the requests and why each fired, and how to answer them. The notes, the setup outline
/// (when shown), the telemetry snapshot, the pending requests and the reply-language note follow,
/// as for the pilot's questions (see [`SessionState::brief`]).
pub(crate) fn scheduled_message(batch: &Batch) -> String {
    let mut message = String::from(
        "[Scheduled turn: the pilot isn't speaking. Requests they made earlier have fired, and \
         you answer them now, on your own.]\n\nRequests that fired:",
    );
    for (id, instruction, made, why) in &batch.requests {
        message.push_str(&format!(
            "\n- {id}: {instruction}\n  Made {made}. Fired now: {why}."
        ));
    }
    message.push_str(&format!(
        "\n\nDon't acknowledge (no \"Copy\" or \"Checking\"): the pilot isn't waiting for you. \
         Open with the subject (\"Fuel check: ...\", \"Lap four: one minute forty two point \
         three\"), as the pilot didn't just ask. Answer each request briefly, leaving out any \
         with nothing worth saying. If none of them has anything worth saying, reply with \
         exactly {SILENCE}."
    ));
    message
}

/// A turn's `message`, its telemetry snapshot attached, with what follows it: the pending
/// requests and the reply-language `note`, last.
pub(crate) fn briefed(message: String, pending_text: &str, note: Option<&str>) -> String {
    let message = format!("{message}\n\n{pending_text}");
    match note {
        Some(note) => format!("{message}\n\n{note}"),
        None => message,
    }
}

/// How the chat names the requests that fired: each one's id and instruction.
fn what_fired<'a>(requests: impl IntoIterator<Item = (RequestId, &'a str)>) -> String {
    (requests.into_iter())
        .map(|(id, instruction)| format!("{id} {}", instruction.trim_end_matches('.')))
        .collect::<Vec<_>>()
        .join("; ")
}

/// Logs what the model answered.
fn log_reply(reply: &Reply) {
    log::info!(
        "Engineer ({}, {:?}; {} in, {} cached, {} out): {}",
        reply.model,
        reply.stop,
        reply.usage.input_tokens + reply.usage.cache_write_tokens,
        reply.usage.cache_read_tokens,
        reply.usage.output_tokens,
        reply.text
    );
}

/// The small print under an answer: the model and the tokens it took.
fn tokens_detail(reply: &Reply) -> String {
    let usage = &reply.usage;
    t!(
        "engineer.detail_tokens",
        model = reply.model,
        input = usage.input_tokens + usage.cache_write_tokens,
        cached = usage.cache_read_tokens,
        output = usage.output_tokens
    )
}

/// The skill's instructions with how to call corners.
fn system_prompt(skill: &str, corner_names: bool) -> Arc<str> {
    let corners = match corner_names {
        true => CORNERS_BY_NAME,
        false => CORNERS_BY_NUMBER,
    };
    format!("{skill}\n\n## Corners\n\n{corners}\n").into()
}

impl SessionState {
    /// Readies the conversation for a turn, and writes its `message` out with `briefing`'s notes,
    /// then the setup outline (when shown), the telemetry snapshot, the pending requests and the
    /// reply-language note. Once per turn: the `schedule` tool stages its changes from here.
    async fn brief(&mut self, message: String, briefing: Briefing) -> String {
        // Started over, it's a new weekend, which clears the requests: the briefing, taken before
        // the engineer saw it, still has the last weekend's, and its notes are about the
        // conversation that's over. The next progress clears the requests too.
        let started_over = self.follow_game_session(briefing.auto_session).await;
        let pending_text = match started_over {
            true => {
                (briefing.started_over).store(true, atomic::Ordering::Relaxed);
                self.schedule_tool.begin(Vec::new(), briefing.next_id);
                NO_REQUESTS.to_owned()
            }
            false => {
                self.schedule_tool.begin(briefing.pending, briefing.next_id);
                briefing.pending_text
            }
        };
        // Only those about this conversation as it is now: one that's started over, here or
        // since a note was made, never had what it's about
        let starts = self.starts.load(atomic::Ordering::Relaxed);
        let notes: String = (briefing.notes.iter())
            .filter(|note| {
                !started_over
                    && note.conversation.as_ref().is_some_and(|conversation| {
                        Weak::ptr_eq(&conversation.state, &briefing.state)
                            && conversation.starts == starts
                    })
            })
            .map(|note| format!("\n\n{}", note.text))
            .collect();
        // The setup's names, unless this conversation still has them
        self.notice_forgotten();
        let mut shown = self.setup_outline;
        let pit: Option<PitMenu> = match &self.pit_tool {
            Some(pit_tool) => pit_tool.menu().await,
            None => None,
        };
        // The notes are said once, so they go with the question, before what's sent every turn
        let message = (self.tool)
            .question_with_snapshot(message + &notes, &mut shown, pit.as_ref())
            .await;
        self.outline_offered = shown;
        let message = briefed(message, &pending_text, briefing.note.as_deref());
        self.conversation.set_tool_result_note(briefing.note);
        message
    }

    /// The setup's names were shown in messages the conversation may have forgotten on its own
    /// since (see [`Model::forgotten`]): they're shown again.
    fn notice_forgotten(&mut self) {
        let forgotten = self.conversation.forgotten();
        if forgotten != self.forgotten {
            self.forgotten = forgotten;
            self.setup_outline = None;
        }
    }

    /// The names the turn offered are in the conversation once it keeps the turn: one that failed
    /// or was refused shows them again next time (a stopped one never gets here).
    fn keep_outline(&mut self, reply: &anyhow::Result<Reply>) {
        if reply
            .as_ref()
            .is_ok_and(|reply| reply.stop != StopReason::Refusal)
        {
            self.setup_outline = self.outline_offered;
        }
    }

    /// Tells the conversation which game session it's about (Bono Cloud leases it). With
    /// `auto_session`, a new weekend (the recording cleared: another track or event) starts the
    /// conversation over; the next practice, qualifying or race of the same one doesn't. Returns
    /// whether it started over.
    async fn follow_game_session(&mut self, auto_session: bool) -> bool {
        let (weekend, current) = match game_session(&self.pool, self.sim).await {
            Ok(current) => current,
            Err(e) => {
                log::warn!("Error reading the game session: {e:#}");
                return false;
            }
        };

        let mut started_over = false;
        // A recording without epochs can't tell weekends apart: the conversation carries on. A
        // clear counts even before the next session's scoring
        if let Some(weekend) = weekend {
            if auto_session && self.weekend.is_some_and(|known| known != weekend) {
                log::info!("New weekend: the engineer starts over");
                self.conversation.reset();
                self.setup_outline = None;
                self.starts.fetch_add(1, atomic::Ordering::Relaxed);
                started_over = true;
            }
            self.weekend = Some(weekend);
        }
        if let Some(current) = current {
            self.conversation.set_game_session(&current);
        }
        started_over
    }
}

/// A conversation's tools: the engineer's, the schedule's, and the pit menu's in a game the
/// engineer can change it in.
fn session_tools(
    tool: &EngineerTool,
    schedule_tool: &ScheduleTool,
    pit_tool: Option<&PitTool>,
) -> Tools {
    let mut tools = Tools::default();
    tools.insert(
        EngineerTool::NAME,
        Some(EngineerTool::description()),
        EngineerTool::input_schema(),
        tool.clone(),
    );
    tools.insert(
        ScheduleTool::NAME,
        Some(ScheduleTool::description()),
        ScheduleTool::input_schema(),
        schedule_tool.clone(),
    );
    if let Some(pit_tool) = pit_tool {
        tools.insert(
            PitTool::NAME,
            Some(PitTool::description()),
            PitTool::input_schema(),
            pit_tool.clone(),
        );
    }
    tools
}

/// The game session in the database (its track, type and number, and how long it's scheduled to
/// last), if there's one yet, with the weekend it's in: the oldest epoch's id, `None` for
/// recordings without epochs.
async fn game_session(
    pool: &Pool,
    sim: &'static str,
) -> anyhow::Result<(Option<i64>, Option<GameSession>)> {
    let conn = pool.get().await?;
    Ok(conn
        .run(move |conn| {
            use rusqlite::OptionalExtension;
            // The session and its weekend in one snapshot: a clear between them would pair the
            // old weekend's session with the new one
            let _snapshot = conn.unchecked_transaction()?;
            let epochs = conn
                .prepare_cached(
                    "SELECT 1 FROM pragma_table_list WHERE name = 'epoch' AND type = 'table'",
                )?
                .exists([])?;
            let weekend = match epochs {
                true => conn
                    .prepare_cached("SELECT min(id) FROM epoch")?
                    .query_row([], |row| row.get::<_, Option<i64>>(0))?,
                false => None,
            };
            // The active epoch's: a session just started has none until its first scoring, and
            // the epoch before's isn't the one being driven
            let epoch = bono_sim::sql::active_epoch(conn)?;
            let session = conn
                .query_row(
                    "SELECT track, session_type, session_number, session_end_time FROM session \
                     WHERE id >= ?1 ORDER BY id DESC LIMIT 1",
                    [epoch.session_from],
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
                .optional()?;
            Ok::<_, rusqlite::Error>((weekend, session))
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
                None,
                Turn {
                    task: Task::spawn(std::future::pending()),
                    lang: arrstr!("en"),
                    text,
                    entry: None,
                    steer: Steer::default(),
                    steered: None,
                    told: Told::default(),
                    toned: false,
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
    fn an_answer_waits_for_the_talk_beep() {
        let mut engineer = Engineer::new(reqwest::Client::new(), None);
        engineer.line.push_back(Playback::tone());
        engineer.talk(true);
        // The closing beep starts as the pilot stops talking
        engineer.beep(true);
        engineer.talk(false);
        assert!(engineer.line[0].held);
        // What was said is waited for from then, not from the beep's end
        assert!(engineer.hearing.is_some());
        engineer.beep(false);
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
            told: Told::default(),
            toned: false,
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
                None,
                Turn {
                    task: Task::spawn(std::future::pending()),
                    lang: arrstr!("en"),
                    text,
                    entry: None,
                    steer: Steer::default(),
                    steered: None,
                    told: Told::default(),
                    toned: false,
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
    fn the_no_answer_tone_follows_the_acknowledgement() {
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let _entered = runtime.enter();
        let rt = runtime.handle();
        let dir = tempfile::tempdir().unwrap();
        let config = rt
            .block_on(Config::new(dir.path().join("config.json")))
            .unwrap();
        let mut synth = Synthesizer::new(rt, &config.get());
        let mut engineer = Engineer::new(reqwest::Client::new(), None);
        // "Copiado, lo miro." is still being synthesized when the turn ends with nothing after it
        let (_send, answer) = being_written();
        engineer.incoming.push(answer);
        let (_, text) = mpsc::unbounded_channel();
        let turn = Turn {
            task: Task::spawn(std::future::pending()),
            lang: arrstr!("es"),
            text,
            entry: None,
            steer: Steer::default(),
            steered: None,
            told: Told::default(),
            toned: false,
        };
        let reply = Reply {
            text: String::new(),
            stop: StopReason::EndTurn,
            usage: Default::default(),
            model: "test".into(),
        };
        engineer.answer(rt, &config, &mut synth, reply, Some("Copiado, lo miro.\n".into()), turn);
        // After it, not ahead of it
        assert_eq!(engineer.incoming.len(), 1);
        assert!(engineer.incoming[0].tone_after_drain);
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

        for (stop, text, heard, tone) in [
            (StopReason::MaxTokens, "Partial answer", None, true),
            (StopReason::ContextWindowExceeded, "Partial answer", None, true),
            (StopReason::Refusal, "", None, true),
            (StopReason::EndTurn, "", None, true),
            // The app acknowledged, and the model said nothing after it
            (StopReason::EndTurn, "", Some("Copiado, lo miro.\n"), true),
            // Nor after its own acknowledgement, dropped
            (StopReason::EndTurn, "Copio, miro.\n\n", Some("Copiado, lo miro.\n"), true),
            (StopReason::EndTurn, "Copio.\n\nBien.", Some("Copiado, lo miro.\nBien."), false),
            (StopReason::EndTurn, "Complete answer", None, false),
            (StopReason::StopSequence, "Complete answer", None, false),
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
                told: Told::default(),
                toned: false,
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
                heard.map(str::to_owned),
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

    #[test]
    fn failed_speech_and_an_empty_answer_sound_one_tone() {
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let _entered = runtime.enter();
        let rt = runtime.handle();
        let dir = tempfile::tempdir().unwrap();
        let config = rt
            .block_on(Config::new(dir.path().join("config.json")))
            .unwrap();
        let mut synth = Synthesizer::new(rt, &config.get());

        // The speech fails while the turn runs (as without a voice), or after it ended
        for fails_first in [true, false] {
            let mut engineer = Engineer::new(reqwest::Client::new(), None);
            let (speech, output) = SpeechOutput::channel();
            let mut answer = Playback::new(output, false);
            answer.writing = true;
            engineer.queue(answer);
            let (_, text) = mpsc::unbounded_channel();
            engineer.turn = Some(Turn {
                task: Task::spawn(std::future::pending()),
                lang: arrstr!("en"),
                text,
                entry: None,
                steer: Steer::default(),
                steered: None,
                told: Told::default(),
                toned: false,
            });
            if fails_first {
                speech.send(Err(anyhow::anyhow!("no voice"))).unwrap();
                engineer.admit(1.0);
            }
            let turn = engineer.turn.take().unwrap();
            engineer.answer(
                rt,
                &config,
                &mut synth,
                Reply {
                    text: String::new(),
                    stop: StopReason::EndTurn,
                    usage: Default::default(),
                    model: "test".into(),
                },
                None,
                turn,
            );
            for playback in engineer.incoming.iter_mut().chain(&mut engineer.line) {
                playback.writing = false;
            }
            if !fails_first {
                speech.send(Err(anyhow::anyhow!("no voice"))).unwrap();
                engineer.admit(1.0);
            }
            let waiting = engineer.incoming.len() + engineer.line.len();
            assert_eq!(waiting, 1, "one tone (fails first: {fails_first})");
            assert!(engineer.line[0].complete);
        }

        // A refused answer's "say again" isn't tried with a voice that just failed
        let mut engineer = Engineer::new(reqwest::Client::new(), None);
        let (speech, output) = SpeechOutput::channel();
        let mut answer = Playback::new(output, false);
        answer.writing = true;
        engineer.queue(answer);
        let (_, text) = mpsc::unbounded_channel();
        engineer.turn = Some(Turn {
            task: Task::spawn(std::future::pending()),
            lang: arrstr!("en"),
            text,
            entry: None,
            steer: Steer::default(),
            steered: None,
            told: Told::default(),
            toned: false,
        });
        speech.send(Err(anyhow::anyhow!("no voice"))).unwrap();
        engineer.admit(1.0);
        let turn = engineer.turn.take().unwrap();
        engineer.answer(
            rt,
            &config,
            &mut synth,
            Reply {
                text: String::new(),
                stop: StopReason::Refusal,
                usage: Default::default(),
                model: "test".into(),
            },
            None,
            turn,
        );
        let waiting = engineer.incoming.len() + engineer.line.len();
        assert_eq!(waiting, 1, "one tone for a refused answer");
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
        forgotten: u64,
    }

    struct RecordingModel(Arc<std::sync::Mutex<Recorded>>);

    impl Model for RecordingModel {
        fn send_streaming<'a>(
            &'a mut self,
            _: String,
            _: &'a mut (dyn FnMut(Streamed<'_>) + Send),
        ) -> futures::future::BoxFuture<'a, anyhow::Result<Reply>> {
            unimplemented!()
        }

        fn reset(&mut self) {
            self.0.lock().unwrap().resets += 1;
        }

        fn forgotten(&self) -> u64 {
            self.0.lock().unwrap().forgotten
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
    async fn pit_tool_registered_only_with_service() {
        struct NoMenu;
        impl PitService for NoMenu {
            fn read(&self) -> anyhow::Result<PitMenu> {
                anyhow::bail!("no game")
            }
            fn write(&self, _: &PitMenu, _: &crate::sim::pit::Resolved) -> anyhow::Result<()> {
                anyhow::bail!("no game")
            }
        }
        let rt = tokio::runtime::Handle::current();
        let dir = tempfile::tempdir().unwrap();
        let one = NonZeroUsize::new(1).unwrap();
        let pool = Pool::new(Database::read_only(Arc::new(dir.path().join("a.db"))), one, one);
        let mut engineer = Engineer::new(reqwest::Client::new(), None);
        let tool = engineer.tool_for(&rt, &pool);
        let schedule_tool = ScheduleTool::new(tool.clone());
        let names = |pit: Option<&PitTool>| {
            (session_tools(&tool, &schedule_tool, pit).definitions())
                .map(|definition| definition.name)
                .collect::<Vec<_>>()
        };
        assert_eq!(names(None), [EngineerTool::NAME, ScheduleTool::NAME]);
        let pit = PitTool::new(Arc::new(NoMenu));
        assert_eq!(
            names(Some(&pit)),
            [EngineerTool::NAME, ScheduleTool::NAME, PitTool::NAME]
        );
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
                    schedule_tool: ScheduleTool::new(first.clone()),
                    pit_tool: None,
                    sim: "lmu",
                    weekend: None,
                    starts: Arc::default(),
                    setup_outline: None,
                    forgotten: 0,
                    outline_offered: None,
                })),
                starts: Arc::default(),
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

    #[tokio::test(flavor = "multi_thread")]
    async fn an_aborted_turn_never_drops_the_last_hold_on_its_conversation() {
        /// An aborted turn's hold on the conversation: whether it was the last one, once the
        /// runtime dropped it (slowly, so the conversation's own drop has time to go first)
        struct Hold {
            state: Arc<Mutex<SessionState>>,
            last: Arc<AtomicBool>,
        }
        impl Drop for Hold {
            fn drop(&mut self) {
                std::thread::sleep(Duration::from_millis(100));
                let last = Arc::strong_count(&self.state) == 1;
                self.last.store(last, atomic::Ordering::SeqCst);
            }
        }

        let rt = tokio::runtime::Handle::current();
        let dir = tempfile::tempdir().unwrap();
        let one = NonZeroUsize::new(1).unwrap();
        let pool = Pool::new(Database::read_only(Arc::new(dir.path().join("a.db"))), one, one);
        let mut engineer = Engineer::new(reqwest::Client::new(), None);
        let tool = engineer.tool_for(&rt, &pool);
        let state = Arc::new(Mutex::new(SessionState {
            conversation: Box::new(RecordingModel(Arc::default())),
            pool: pool.clone(),
            tool: tool.clone(),
            schedule_tool: ScheduleTool::new(tool),
            pit_tool: None,
            sim: "lmu",
            weekend: None,
            starts: Arc::default(),
            setup_outline: None,
            forgotten: 0,
            outline_offered: None,
        }));
        let gone = Arc::downgrade(&state);
        let last = Arc::new(AtomicBool::new(false));
        let hold = Hold {
            state: state.clone(),
            last: last.clone(),
        };
        engineer.session = Some(Session {
            db: pool.database().path().to_owned(),
            model: ModelConfig::default(),
            corner_names: false,
            user: None,
            lease: None,
            state,
            starts: Arc::default(),
        });
        let (_, text) = mpsc::unbounded_channel();
        engineer.turn = Some(Turn {
            task: Task::spawn(async move {
                let _hold = hold;
                std::future::pending().await
            }),
            lang: arrstr!("en"),
            text,
            entry: None,
            steer: Steer::default(),
            steered: None,
            told: Told::default(),
            toned: false,
        });

        // The game stopped
        engineer.stop();
        engineer.drop_session(&rt);
        tokio::time::timeout(Duration::from_secs(10), async {
            while gone.strong_count() > 0 {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("the conversation was dropped");
        assert!(!last.load(atomic::Ordering::SeqCst));
        engineer.quit(&rt).await.unwrap();
    }

    /// How a [`ScriptedModel`]'s turn ends, once the test says.
    enum Act {
        /// It takes the questions steering it, then replies
        Steered,
        /// It takes the questions steering it, and carries on
        TakeSteer,
        /// It calls the `schedule` tool with this input, and carries on
        Schedule(serde_json::Value),
        /// It writes this text, and carries on
        Write(&'static str),
        /// It's about to run these tools, and carries on
        Tools(&'static [&'static str]),
        /// It replies without looking at them
        Reply,
        /// It replies with this text
        Say(String),
        /// It replies with this text, ending for this reason
        End(StopReason, &'static str),
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
        /// What each [`Act::Steered`] or [`Act::TakeSteer`] took from its turn's steer
        steered: Vec<Vec<String>>,
        /// What each [`Act::Schedule`] call returned
        calls: Vec<Result<String, String>>,
    }

    /// A conversation whose turns each wait for an [`Act`] from the test.
    struct ScriptedModel {
        asked: Arc<std::sync::Mutex<Asked>>,
        acts: mpsc::UnboundedReceiver<Act>,
        steer: Option<Steer>,
        /// The `schedule` tool, as the conversation calls it
        tools: Tools,
    }

    impl Model for ScriptedModel {
        fn send_streaming<'a>(
            &'a mut self,
            text: String,
            on_event: &'a mut (dyn FnMut(Streamed<'_>) + Send),
        ) -> futures::future::BoxFuture<'a, anyhow::Result<Reply>> {
            self.asked.lock().unwrap().questions.push(text);
            Box::pin(async move {
                let (stop, text): (_, String) = loop {
                    match self.acts.recv().await {
                        Some(Act::Steered) => {
                            let taken = self.steer.as_ref().map(Steer::take).unwrap_or_default();
                            self.asked.lock().unwrap().steered.push(taken);
                            break (StopReason::EndTurn, "Copy.".into());
                        }
                        Some(Act::TakeSteer) => {
                            let taken = self.steer.as_ref().map(Steer::take).unwrap_or_default();
                            self.asked.lock().unwrap().steered.push(taken);
                        }
                        Some(Act::Schedule(input)) => {
                            let result = self.tools.handle(ScheduleTool::NAME, &input).await;
                            let result = result.map_err(|e| format!("{e:#}"));
                            self.asked.lock().unwrap().calls.push(result);
                        }
                        Some(Act::Write(text)) => on_event(Streamed::Text(text)),
                        Some(Act::Tools(names)) => on_event(Streamed::Tools(names)),
                        Some(Act::Reply) => break (StopReason::EndTurn, "Copy.".into()),
                        Some(Act::Say(text)) => break (StopReason::EndTurn, text),
                        Some(Act::End(stop, text)) => break (stop, text.into()),
                        Some(Act::Refuse) => break (StopReason::Refusal, "Copy.".into()),
                        Some(Act::Full) => {
                            break (StopReason::ContextWindowExceeded, "Copy.".into());
                        }
                        Some(Act::Fail) | None => anyhow::bail!("scripted failure"),
                    }
                };
                Ok(Reply {
                    text,
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
            Self::build(raw, false)
        }

        /// On a recorded race (see [`record_race`]), once the tool has its first progress, so
        /// requests can be made.
        fn recorded() -> Self {
            Self::build("{}", true)
        }

        /// On a recorded race, as [`Self::recorded`], with the config `raw`.
        fn recorded_with(raw: &str) -> Self {
            Self::build(raw, true)
        }

        fn build(raw: &str, recorded: bool) -> Self {
            let runtime = tokio::runtime::Runtime::new().unwrap();
            let _entered = runtime.enter();
            let rt = runtime.handle();
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("config.json");
            std::fs::write(&path, raw).unwrap();
            let config = rt.block_on(Config::new(&path)).unwrap();
            let synth = Synthesizer::new(rt, &config.get());
            let one = NonZeroUsize::new(1).unwrap();
            let db = match recorded {
                true => {
                    record_race(dir.path());
                    dir.path().join(DATABASE)
                }
                false => dir.path().join("a.db"),
            };
            let pool = Pool::new(Database::read_only(Arc::new(db)), one, one);
            let mut sim = crate::sim::recording(pool.clone());
            let asked = Arc::new(std::sync::Mutex::new(Asked::default()));
            let (acts, script) = mpsc::unbounded_channel();
            let mut engineer = Engineer::new(reqwest::Client::new(), None);
            // The schedule's clock moves only when the test says
            engineer.clock = Some(Instant::now());
            // Turns take as long as the test says: only those about it are acknowledged
            engineer.acknowledge_after = None;
            // Scheduled answers' speech comes when the test says
            engineer.synthesis = Some(Vec::new());
            // Followed as the app does every frame, and used by the conversation
            engineer.follow_recording(rt, Some(&mut *sim));
            let tool = engineer.tool.clone().unwrap();
            let schedule_tool = ScheduleTool::new(tool.clone());
            let mut tools = Tools::default();
            tools.insert(
                ScheduleTool::NAME,
                Some(ScheduleTool::description()),
                ScheduleTool::input_schema(),
                schedule_tool.clone(),
            );
            // As `ask` would start it, so it keeps it
            let starts = Arc::<AtomicU32>::default();
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
                        tools,
                    }),
                    tool: tool.clone(),
                    schedule_tool,
                    pit_tool: None,
                    pool,
                    sim: "test",
                    weekend: None,
                    starts: starts.clone(),
                    setup_outline: None,
                    forgotten: 0,
                    outline_offered: None,
                })),
                starts,
            });
            if recorded {
                let deadline = Instant::now() + Duration::from_secs(10);
                while tool.progress().is_none() {
                    assert!(Instant::now() < deadline, "no progress published");
                    std::thread::sleep(Duration::from_millis(10));
                }
            }
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

        /// The engineer's latest entry in the chat
        fn answer(&self) -> Option<&ChatEntry> {
            (self.engineer.chat.iter()).rfind(|e| e.speaker == Speaker::Engineer)
        }

        /// Hears `text` in Spanish, let go of `ago` before now.
        fn hear_ago(&mut self, text: &str, ago: Duration) {
            self.hear(Heard::Question {
                text: text.into(),
                lang: arrstr!("es"),
                released: Instant::now().checked_sub(ago).unwrap(),
            });
        }

        fn cancelled(&self) -> bool {
            (self.engineer.chat.iter())
                .any(|e| e.speaker == Speaker::Notice && e.text == t!("engineer.cancelled"))
        }

        fn noticed(&self, text: &str) -> bool {
            (self.engineer.chat.iter()).any(|e| e.speaker == Speaker::Notice && e.text == text)
        }

        /// Has the running turn call the `schedule` tool with `input`, and returns what the call
        /// returned.
        fn schedule(&mut self, input: serde_json::Value) -> Result<String, String> {
            let calls = self.asked.lock().unwrap().calls.len();
            self.acts.send(Act::Schedule(input)).unwrap();
            self.until("called the tool", |s| {
                s.asked.lock().unwrap().calls.len() > calls
            });
            self.asked.lock().unwrap().calls[calls].clone()
        }

        /// Ends the running turn with `act`, and runs frames until it's handled.
        fn finish(&mut self, act: Act) {
            self.acts.send(act).unwrap();
            self.until("answered", |s| !s.engineer.is_thinking());
        }

        /// A turn for `question` that calls the `schedule` tool with `input`, finished.
        fn add(&mut self, question: &str, input: serde_json::Value) {
            let turns = self.questions().len();
            self.ask(question);
            self.until("asked", |s| s.questions().len() > turns);
            self.schedule(input).unwrap();
            self.finish(Act::Reply);
        }

        fn pending(&self) -> Vec<RequestId> {
            self.engineer.schedule().pending_ids()
        }

        /// Has a pilot turn make request #1, a one-shot for when the recording's marker is set,
        /// which then is: it's due from the frame that steps through the last snapshot.
        fn falls_due(&mut self) {
            self.falls_due_with(marker_set());
        }

        /// As [`Self::falls_due`], the request made with `input`, a variant of [`marker_set`].
        fn falls_due_with(&mut self, input: serde_json::Value) {
            // Only the snapshots asked for from here on
            self.tick();
            let recording = self.recording();
            recording.execute_batch("CREATE TABLE marker (x)").unwrap();
            self.add("Tell me when the marker's set.", input);
            // Its starting state: not set
            self.tick();
            recording
                .execute_batch("INSERT INTO marker VALUES (1)")
                .unwrap();
            self.tick();
        }

        /// Has the engineer start its new conversations as scripted ones, asked as the first one
        /// is: each one's acts come through the sender the receiver returned gets (see
        /// [`Self::next_conversation`]).
        fn new_conversations(&mut self) -> std::sync::mpsc::Receiver<mpsc::UnboundedSender<Act>> {
            let (made, conversations) = std::sync::mpsc::channel();
            let asked = self.asked.clone();
            self.engineer.conversations = Some(Box::new(move |tools| {
                let (acts, script) = mpsc::unbounded_channel();
                made.send(acts).unwrap();
                Box::new(ScriptedModel {
                    asked: asked.clone(),
                    acts: script,
                    steer: None,
                    tools,
                })
            }));
            conversations
        }

        /// Runs frames until the engineer starts a new conversation (see
        /// [`Self::new_conversations`]): the test's acts go to it from then on.
        fn next_conversation(
            &mut self,
            conversations: &std::sync::mpsc::Receiver<mpsc::UnboundedSender<Act>>,
        ) {
            let deadline = Instant::now() + Duration::from_secs(10);
            loop {
                if let Ok(acts) = conversations.try_recv() {
                    self.acts = acts;
                    return;
                }
                assert!(Instant::now() < deadline, "never started a conversation");
                std::thread::sleep(Duration::from_millis(1));
                self.frame();
            }
        }

        /// Moves the schedule's wall clock on by `by`, without waiting for it.
        fn wait(&mut self, by: Duration) {
            *self.engineer.clock.as_mut().unwrap() += by;
        }

        /// How many notices in the chat say `text`.
        fn notices(&self, text: &str) -> usize {
            (self.engineer.chat.iter())
                .filter(|e| e.speaker == Speaker::Notice && e.text == text)
                .count()
        }

        /// Has request #1 fall due and the scheduled turn for it start: returns how many
        /// messages the model had before it.
        fn scheduled_running(&mut self) -> usize {
            self.scheduled_running_with(marker_set())
        }

        /// As [`Self::scheduled_running`], the request made with `input` (see
        /// [`Self::falls_due_with`]).
        fn scheduled_running_with(&mut self, input: serde_json::Value) -> usize {
            self.falls_due_with(input);
            let turns = self.questions().len();
            self.wait(schedule::GATHER);
            self.scheduled_asked(turns);
            turns
        }

        /// Runs frames until a scheduled turn has sent its message, the `turns`th the model got.
        fn scheduled_asked(&mut self, turns: usize) -> String {
            self.until("ran a scheduled turn", |s| {
                s.engineer.scheduled.is_some() && s.questions().len() > turns
            });
            self.questions()[turns].clone()
        }

        /// Ends the running scheduled turn with `act`, and runs frames until it's handled.
        fn finish_scheduled(&mut self, act: Act) {
            self.acts.send(act).unwrap();
            self.until("answered", |s| s.engineer.scheduled.is_none());
        }

        /// The scheduled answers on their way: being synthesized, or in the line.
        fn answers_waiting(&self) -> Vec<&Playback> {
            (self.engineer.incoming.iter().chain(&self.engineer.line))
                .filter(|playback| playback.scheduled.is_some())
                .collect()
        }

        /// Synthesizes the last scheduled answer given to the synthesizer, as `speech` in one
        /// piece, and runs frames until it's in the line.
        fn synthesize(&mut self, speech: Speech) {
            let synthesis = self.engineer.synthesis.as_mut().unwrap();
            let output = synthesis.last_mut().unwrap().speech.take().unwrap();
            output.send(Ok(speech)).unwrap();
            self.until("in the line", |s| {
                (s.engineer.line.iter()).any(|playback| playback.scheduled.is_some())
            });
        }

        /// A scheduled answer, `text`, for request #1 when the marker's set, waiting at the front
        /// of the line while the pilot holds the talk button.
        fn answer_waiting(&mut self, text: &str) {
            self.answer_waiting_with(marker_set(), text);
        }

        /// As [`Self::answer_waiting`], the request made with `input` (see
        /// [`Self::falls_due_with`]).
        fn answer_waiting_with(&mut self, input: serde_json::Value, text: &str) {
            self.scheduled_running_with(input);
            self.finish_scheduled(Act::Say(text.into()));
            self.engineer.talk(true);
            self.synthesize(piece());
            assert!(self.engineer.line[0].scheduled.is_some());
            assert_eq!(self.engineer.schedule().waiting(RequestId(1)), 1);
        }

        /// A new session loads in the recording, and the engineer steps through it: whatever
        /// fired before it is out of date.
        fn new_game_session(&mut self) {
            use bono_sim::sql::{EpochReason, begin_epoch};
            begin_epoch(&self.recording(), i64::MAX, EpochReason::Session).unwrap();
            self.tick();
        }

        /// The recording is cleared for a new weekend, as the recorder clears it: emptied, and a
        /// newer epoch begun. The engineer hasn't stepped through it yet.
        fn new_weekend(&self) {
            use bono_sim::sql::{EpochReason, begin_epoch, clear_tables};
            let recording = self.recording();
            let newest: i64 =
                (recording.query_row("SELECT max(id) FROM epoch", [], |row| row.get(0))).unwrap();
            clear_tables(&recording, None).unwrap();
            begin_epoch(&recording, newest.saturating_add(1), EpochReason::Cleared).unwrap();
        }

        /// The notes for the model's next message, as it would be told them.
        fn notes(&self) -> Vec<String> {
            (self.engineer.unheard.iter())
                .map(|note| note.text.clone())
                .collect()
        }

        /// How many chat entries are the engineer's answers.
        fn answers(&self) -> usize {
            (self.engineer.chat.iter())
                .filter(|e| e.speaker == Speaker::Engineer)
                .count()
        }

        /// Runs one of the app's frames.
        fn frame(&mut self) {
            let _entered = self.runtime.enter();
            let (sim, config, synth) = (Some(&mut *self.sim), &self.config, &mut self.synth);
            (self.engineer).update(self.runtime.handle(), sim, config, synth, None);
        }

        /// Has the tool publish a progress snapshot now, and runs frames until the engineer has
        /// stepped through it. From then on the tool publishes only the ones asked for.
        fn tick(&mut self) {
            let tool = self.engineer.tool.clone().unwrap();
            self.runtime.block_on(tool.publish_progress()).unwrap();
            let seq = tool.progress_seq();
            assert!(seq.is_some());
            self.until("stepped through it", |s| {
                s.engineer.progress.as_ref().map(|p| p.seq) == seq
            });
        }

        /// The recording, to write to as the simulator would.
        fn recording(&self) -> rusqlite::Connection {
            rusqlite::Connection::open(self._dir.path().join(DATABASE)).unwrap()
        }

        /// A change to request #1 that didn't apply, as it ended while the turn ran: returns the
        /// note the model's next message carries.
        fn missed_change(&mut self) -> String {
            self.add("Fuel every lap, please.", fuel_every_lap());
            let turns = self.questions().len();
            self.ask("Forget the fuel.");
            self.until("asked", |s| s.questions().len() > turns);
            self.schedule(cancel(RequestId(1))).unwrap();
            self.engineer.schedule.clear();
            self.finish(Act::Reply);
            let note = missing_note(RequestId(1));
            assert_eq!(self.notes(), std::slice::from_ref(&note));
            note
        }
    }

    /// Records a race in `dir`'s recording, as the simulator would: the player's car alone, its
    /// laps recorded, going 36 km/h.
    fn record_race(dir: &std::path::Path) {
        use bono_sim::sys::{bono_standing, bono_telemetry};
        let recorder = Recorder::new(dir, None).unwrap();
        for t in 0..10 {
            let t = f64::from(t);
            let laps = 2;
            let session = bono_session_info {
                session_time: Some(t).into(),
                track: Some("Test Ring".to_owned()).into(),
                track_length: Some(1000.0).into(),
                session_type: Some("race".to_owned()).into(),
                phase: Some("green".to_owned()).into(),
                ..Default::default()
            };
            let standing = bono_standing {
                session_time: Some(t).into(),
                vehicle_id: Some(1).into(),
                is_player: Some(true).into(),
                driver: Some("Ana Lopes".to_owned()).into(),
                laps_completed: Some(laps).into(),
                ..Default::default()
            };
            recorder.push_scoring(session, vec![standing]);
            let telemetry = bono_telemetry {
                vehicle_id: Some(1).into(),
                session_time: Some(t).into(),
                // On the lap after those done, as the game counts from lap 1
                lap: Some(laps + 1).into(),
                lap_distance: Some(t * 10.0).into(),
                speed: Some(10.0).into(),
                ..Default::default()
            };
            if recorder.wants_telemetry(1, t, 10.0) {
                recorder.push_telemetry(&telemetry);
            }
        }
    }

    /// A request for the fuel left every lap.
    fn fuel_every_lap() -> serde_json::Value {
        serde_json::json!({
            "action": "add",
            "instruction": "Tell the pilot the fuel left.",
            "trigger": {"kind": "laps", "count": 1},
            "repeat": "every",
        })
    }

    /// A request for when the player's car goes faster than 100 km/h, which it doesn't.
    fn when_fast() -> serde_json::Value {
        serde_json::json!({
            "action": "add",
            "instruction": "Tell the pilot they're past 100 km/h.",
            "trigger": {
                "kind": "condition",
                "query": "SELECT 1 FROM cars_now WHERE is_player = 1 AND speed_kmh > 100",
            },
        })
    }

    fn cancel(id: RequestId) -> serde_json::Value {
        serde_json::json!({"action": "cancel", "id": id.to_string()})
    }

    /// The instruction of [`marker_set`]'s request.
    const MARKER: &str = "Tell the pilot the marker's set.";

    /// A one-shot request for when the recording's `marker` table has a row.
    fn marker_set() -> serde_json::Value {
        serde_json::json!({
            "action": "add",
            "instruction": MARKER,
            "trigger": {"kind": "condition", "query": "SELECT 1 FROM recording.marker"},
        })
    }

    /// How the chat names [`marker_set`]'s request, #1, once it has fired.
    fn marker_fired() -> String {
        what_fired([(RequestId(1), MARKER)])
    }

    /// Voices that only stand in for installed choices: nothing is synthesized.
    const VOICES: &str = r#"{"voices": {"en": "en.onnx"}}"#;

    #[test]
    fn finished_turn_applies_its_changes() {
        let mut scripted = Scripted::recorded();
        scripted.ask("Fuel every lap, please.");
        let added = scripted.schedule(fuel_every_lap()).unwrap();
        assert!(added.contains("#1"), "{added}");
        // Staged only, until the reply is in
        assert!(scripted.pending().is_empty());
        scripted.finish(Act::Reply);
        assert_eq!(scripted.pending(), [RequestId(1)]);
    }

    #[test]
    fn dropped_turn_discards_its_changes() {
        // The turn fails, or it's dropped with its conversation (the account signing out) after
        // the pilot talked over it
        for signed_out in [false, true] {
            let mut scripted = Scripted::recorded();
            scripted.add("Fuel every lap, please.", fuel_every_lap());
            scripted.ask("And the gap behind every lap.");
            scripted.until("asked", |s| s.questions().len() == 2);
            let added = scripted.schedule(fuel_every_lap()).unwrap();
            assert!(added.contains("#2"), "{added}");
            scripted.ask("No, forget the gap.");
            assert!(scripted.engineer.is_thinking());
            match signed_out {
                true => {
                    let session = scripted.engineer.session.as_mut().unwrap();
                    session.user = Some("pilot".into());
                    scripted.until("dropped", |s| !s.engineer.is_thinking());
                    assert!(scripted.cancelled());
                }
                false => scripted.finish(Act::Fail),
            }
            assert_eq!(
                scripted.pending(),
                [RequestId(1)],
                "signed out: {signed_out}"
            );
            if signed_out {
                continue;
            }
            // The next turn starts from what was applied: the dropped turn's id is given again
            scripted.ask("The gap behind every lap, then.");
            scripted.until("asked", |s| s.questions().len() == 3);
            let added = scripted.schedule(fuel_every_lap()).unwrap();
            assert!(added.contains("#2"), "{added}");
            scripted.finish(Act::Reply);
            assert_eq!(scripted.pending(), [RequestId(1), RequestId(2)]);
        }
    }

    #[test]
    fn a_new_weekend_takes_in_a_scheduled_turn_that_finished() {
        // Its answer in but not yet taken when a new weekend clears the requests: the
        // conversation kept the exchange, so it's taken in, not put back as due
        let mut scripted = Scripted::recorded();
        scripted.scheduled_running();
        scripted.acts.send(Act::Say(SILENCE.into())).unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        while !(scripted.engineer.scheduled.as_ref()).is_some_and(|s| s.task.is_finished()) {
            assert!(Instant::now() < deadline, "never finished");
            std::thread::sleep(Duration::from_millis(1));
        }
        scripted.new_weekend();
        scripted.tick();
        assert!(scripted.engineer.scheduled.is_none());
        let nothing = t!("engineer.nothing_to_report", what = marker_fired());
        assert!(scripted.noticed(&nothing));
        assert!(scripted.pending().is_empty());
    }

    #[test]
    fn signing_out_takes_in_a_turn_that_finished() {
        // A pilot turn or a scheduled one, its answer in but not yet taken when the account signs
        // out: the conversation kept the exchange, and the requests outlive it
        for scheduled in [false, true] {
            let mut scripted = Scripted::recorded();
            match scheduled {
                false => {
                    scripted.ask("Fuel every lap, please.");
                    scripted.until("asked", |s| s.questions().len() == 1);
                    scripted.schedule(fuel_every_lap()).unwrap();
                    scripted.acts.send(Act::Reply).unwrap();
                }
                true => {
                    scripted.scheduled_running();
                    scripted.acts.send(Act::Say(SILENCE.into())).unwrap();
                }
            }
            let finished = |e: &Engineer| match scheduled {
                false => e.turn.as_ref().is_some_and(|t| t.task.is_finished()),
                true => e.scheduled.as_ref().is_some_and(|s| s.task.is_finished()),
            };
            let deadline = Instant::now() + Duration::from_secs(10);
            while !finished(&scripted.engineer) {
                assert!(Instant::now() < deadline, "never finished");
                std::thread::sleep(Duration::from_millis(1));
            }
            let session = scripted.engineer.session.as_mut().unwrap();
            session.user = Some("pilot".into());
            scripted.frame();
            assert!(scripted.engineer.session.is_none(), "scheduled: {scheduled}");
            assert!(!scripted.cancelled(), "scheduled: {scheduled}");
            match scheduled {
                // Its request applied
                false => assert_eq!(scripted.pending(), [RequestId(1)]),
                // Answered, not due again
                true => {
                    let nothing = t!("engineer.nothing_to_report", what = marker_fired());
                    assert!(scripted.noticed(&nothing));
                    assert!(scripted.pending().is_empty());
                }
            }
        }
    }

    #[test]
    fn refusal_discards_its_changes() {
        let mut scripted = Scripted::recorded();
        scripted.ask("Fuel every lap, please.");
        scripted.schedule(fuel_every_lap()).unwrap();
        scripted.finish(Act::Refuse);
        assert!(scripted.pending().is_empty());
    }

    #[test]
    fn steered_turn_applies_both_questions_changes() {
        for refused in [false, true] {
            let mut scripted = Scripted::recorded();
            scripted.ask("Fuel every lap, please.");
            scripted.schedule(fuel_every_lap()).unwrap();
            scripted.ask("And tell me when I'm past 100.");
            scripted.acts.send(Act::TakeSteer).unwrap();
            scripted.until("steered", |s| s.asked.lock().unwrap().steered.len() == 1);
            let added = scripted.schedule(when_fast()).unwrap();
            assert!(added.contains("#2"), "{added}");
            scripted.finish(match refused {
                true => Act::Refuse,
                false => Act::Reply,
            });
            // One turn for both
            assert_eq!(scripted.questions().len(), 1);
            let applied = match refused {
                true => vec![],
                false => vec![RequestId(1), RequestId(2)],
            };
            assert_eq!(scripted.pending(), applied, "refused: {refused}");
        }
    }

    #[test]
    fn new_session_button_clears_requests() {
        let mut scripted = Scripted::recorded();
        scripted.add("Tell me when I'm past 100.", when_fast());
        assert_eq!(scripted.pending(), [RequestId(1)]);
        scripted.engineer.new_session();
        assert!(scripted.pending().is_empty());
        assert!(
            scripted
                .engineer
                .tool
                .as_ref()
                .unwrap()
                .conditions()
                .is_empty()
        );
        assert!(scripted.noticed(t!("engineer.requests_cleared")));
    }

    #[test]
    fn context_full_keeps_them() {
        let mut scripted = Scripted::recorded();
        scripted.add("Fuel every lap, please.", fuel_every_lap());
        scripted.ask("And the gap behind every lap.");
        scripted.until("asked", |s| s.questions().len() == 2);
        scripted.schedule(fuel_every_lap()).unwrap();
        scripted.finish(Act::Full);
        // The next question starts a new conversation, with the requests
        assert!(scripted.engineer.new_session);
        assert_eq!(scripted.pending(), [RequestId(1), RequestId(2)]);
    }

    #[test]
    fn recording_gone_clears_requests() {
        let mut scripted = Scripted::recorded();
        scripted.add("Tell me when I'm past 100.", when_fast());
        let _entered = scripted.runtime.enter();
        (scripted.engineer).follow_recording(scripted.runtime.handle(), None);
        assert!(scripted.pending().is_empty());
        assert!(scripted.engineer.registered.is_empty());
        assert!(scripted.engineer.progress.is_none());
        assert!(scripted.noticed(t!("engineer.requests_cleared")));
    }

    #[test]
    fn simulator_stopping_clears_requests() {
        let mut scripted = Scripted::recorded();
        scripted.add("Fuel every lap, please.", fuel_every_lap());
        let _entered = scripted.runtime.enter();
        let (config, synth) = (&scripted.config, &mut scripted.synth);
        (scripted.engineer).update(scripted.runtime.handle(), None, config, synth, None);
        assert!(scripted.pending().is_empty());
    }

    #[test]
    fn question_carries_the_pending_list() {
        let mut scripted = Scripted::recorded();
        scripted.ask("Fuel every lap, please.");
        scripted.until("asked", |s| s.questions().len() == 1);
        assert!(scripted.questions()[0].contains("Scheduled requests: none."));
        scripted.schedule(fuel_every_lap()).unwrap();
        scripted.finish(Act::Reply);

        // Heard on the radio: the model is told which language to reply in, last
        scripted.hear(Heard::Question {
            text: "What have you got scheduled?".into(),
            lang: arrstr!("en"),
            released: Instant::now(),
        });
        scripted.until("asked", |s| s.questions().len() == 2);
        let progress = scripted.engineer.progress.clone().unwrap();
        let pending = scripted.engineer.schedule().pending_text(&progress);
        assert!(pending.contains("#1"), "{pending}");
        let question = &scripted.questions()[1];
        let at = question.find(&pending).expect("the pending list");
        let snapshot = question.find("[END UNTRUSTED TELEMETRY SNAPSHOT]");
        assert!(snapshot.is_some_and(|end| end < at), "{question}");
        let note = question.rfind(&language::reply_note(arrstr!("en")));
        assert!(note.is_some_and(|note| at < note), "{question}");
        scripted.finish(Act::Reply);
    }

    #[test]
    fn update_registers_conditions() {
        let mut scripted = Scripted::recorded();
        scripted.add("Tell me when I'm past 100.", when_fast());
        let tool = scripted.engineer.tool.clone().unwrap();
        let conditions = scripted.engineer.schedule().conditions();
        assert_eq!(conditions.len(), 1);
        assert_eq!(tool.conditions(), conditions);

        // Cancelled, it's unregistered
        scripted.add("Forget that.", cancel(RequestId(1)));
        assert!(scripted.pending().is_empty());
        assert!(tool.conditions().is_empty());
    }

    #[test]
    fn a_change_that_did_not_apply_is_told() {
        let mut scripted = Scripted::recorded();
        scripted.add("Fuel every lap, please.", fuel_every_lap());
        scripted.ask("Forget the fuel.");
        scripted.until("asked", |s| s.questions().len() == 2);
        scripted.schedule(cancel(RequestId(1))).unwrap();
        // It ends while the turn runs
        scripted.engineer.schedule.clear();
        scripted.finish(Act::Reply);
        let notice = t!("engineer.request_missing", id = RequestId(1));
        assert!(scripted.noticed(&notice));

        // The model is told with the next question, once
        let note = missing_note(RequestId(1));
        for (question, told) in [("Anything else?", true), ("And now?", false)] {
            let turns = scripted.questions().len();
            scripted.ask(question);
            scripted.until("asked", |s| s.questions().len() > turns);
            assert_eq!(scripted.questions()[turns].contains(&note), told);
            scripted.finish(Act::Reply);
        }
    }

    #[test]
    fn an_add_from_another_weekend_is_told() {
        use bono_sim::sql::{EpochReason, begin_epoch};

        for auto_session in [false, true] {
            let raw = serde_json::json!({ "auto_session": auto_session }).to_string();
            let mut scripted = Scripted::recorded_with(&raw);
            scripted.ask("Fuel every lap, please.");
            scripted.until("asked", |s| s.questions().len() == 1);
            scripted.schedule(fuel_every_lap()).unwrap();
            // The recording is cleared while the turn runs: the oldest epoch, the weekend, changes
            begin_epoch(&scripted.recording(), 1, EpochReason::Cleared).unwrap();
            scripted.tick();
            assert_eq!(
                scripted.engineer.progress.as_ref().unwrap().weekend,
                Some(1)
            );
            scripted.finish(Act::Reply);
            assert!(scripted.pending().is_empty());
            let notice = t!("engineer.request_dropped", id = RequestId(1));
            assert!(scripted.noticed(&notice));

            // The model is told with the next question, once. With `auto_session`, that question
            // starts the conversation over at the new weekend: it never made the add
            let note = dropped_note(RequestId(1));
            for (question, told) in [("Anything else?", !auto_session), ("And now?", false)] {
                let turns = scripted.questions().len();
                scripted.ask(question);
                scripted.until("asked", |s| s.questions().len() > turns);
                let question = &scripted.questions()[turns];
                assert_eq!(question.contains(&note), told, "{auto_session}: {question}");
                scripted.finish(Act::Reply);
            }
            assert!(scripted.engineer.unheard.is_empty());
        }
    }

    #[test]
    fn notes_are_told_again_when_the_exchange_is_forgotten() {
        for how in ["refused", "failed", "stopped"] {
            let mut scripted = Scripted::recorded();
            let note = scripted.missed_change();
            let turns = scripted.questions().len();
            scripted.ask("Anything else?");
            scripted.until("asked", |s| s.questions().len() > turns);
            assert!(scripted.questions()[turns].contains(&note));
            assert!(scripted.engineer.unheard.is_empty());
            match how {
                "refused" => scripted.finish(Act::Refuse),
                "failed" => scripted.finish(Act::Fail),
                _ => scripted.engineer.stop(),
            }
            assert_eq!(scripted.notes(), std::slice::from_ref(&note), "{how}");
        }
    }

    #[test]
    fn notes_come_before_the_snapshot() {
        let mut scripted = Scripted::recorded();
        let note = scripted.missed_change();
        let turns = scripted.questions().len();
        scripted.ask("Anything else?");
        scripted.until("asked", |s| s.questions().len() > turns);
        let question = &scripted.questions()[turns];
        let snapshot = question
            .find("\n[BEGIN UNTRUSTED TELEMETRY SNAPSHOT]\n")
            .expect("a snapshot");
        assert!(
            question.find(&note).is_some_and(|at| at < snapshot),
            "{question}"
        );
        scripted.finish(Act::Reply);
    }

    #[test]
    fn a_new_conversation_forgets_the_notes() {
        // The account the conversation is had as signs out
        let mut scripted = Scripted::recorded();
        scripted.missed_change();
        scripted.engineer.session.as_mut().unwrap().user = Some("pilot".into());
        scripted.frame();
        assert!(scripted.engineer.session.is_none());
        assert!(scripted.engineer.unheard.is_empty());

        // The game moves on to another weekend, and `auto_session` starts the conversation over
        // before the engineer has stepped through it (only the snapshots asked for from here on)
        let mut scripted = Scripted::recorded();
        let note = scripted.missed_change();
        scripted.tick();
        scripted.new_weekend();
        let turns = scripted.questions().len();
        scripted.ask("Anything else?");
        scripted.until("asked", |s| s.questions().len() > turns);
        assert!(!scripted.questions()[turns].contains(&note));
        // Not told to it again, either
        scripted.finish(Act::Fail);
        assert!(scripted.engineer.unheard.is_empty());
    }

    #[test]
    fn a_new_weekends_conversation_is_not_told_the_last_ones_requests() {
        let mut scripted = Scripted::recorded();
        scripted.add("Fuel every lap, please.", fuel_every_lap());
        // Only the snapshots asked for from here on: the turn starts before the engineer steps
        // through the new weekend, and its briefing still has the request
        scripted.tick();
        assert_eq!(scripted.pending(), [RequestId(1)]);
        scripted.new_weekend();
        let turns = scripted.questions().len();
        scripted.ask("How are the tyres?");
        scripted.until("asked", |s| s.questions().len() > turns);
        let question = &scripted.questions()[turns];
        assert!(question.contains(NO_REQUESTS), "{question}");
        assert!(
            !question.contains("Tell the pilot the fuel left."),
            "{question}"
        );
        // Nor can it change it
        let error = scripted.schedule(cancel(RequestId(1))).unwrap_err();
        assert!(error.contains("none are pending"), "{error}");
        scripted.finish(Act::Reply);

        // The next progress clears it
        scripted.tick();
        assert!(scripted.pending().is_empty());
    }

    #[test]
    fn progress_is_kept_while_the_tool_has_none() {
        let mut scripted = Scripted::recorded();
        scripted.tick();
        let kept = scripted.engineer.progress.as_ref().unwrap().seq;
        // Its tables catching up
        let tool = scripted.engineer.tool.clone().unwrap();
        assert!(tool.take_progress().is_some());
        scripted.frame();
        assert_eq!(
            scripted.engineer.progress.as_ref().map(|p| p.seq),
            Some(kept)
        );
    }

    #[test]
    fn a_failing_condition_counts_once_per_snapshot() {
        let mut scripted = Scripted::recorded();
        // Only the snapshots asked for from here on
        scripted.tick();
        let recording = scripted.recording();
        recording.execute_batch("CREATE TABLE marker (x)").unwrap();
        let instruction = "Tell the pilot the marker's set.";
        scripted.add(
            "Tell me when the marker's set.",
            serde_json::json!({
                "action": "add",
                "instruction": instruction,
                "trigger": {"kind": "condition", "query": "SELECT 1 FROM recording.marker"},
            }),
        );
        assert_eq!(scripted.pending(), [RequestId(1)]);
        // Every check fails from now on
        recording.execute_batch("DROP TABLE marker").unwrap();
        for _ in 1..schedule::CONDITION_FAILURES {
            scripted.tick();
            // Frames on the same snapshot don't count its failure again
            for _ in 0..10 {
                scripted.frame();
            }
            assert_eq!(scripted.pending(), [RequestId(1)]);
        }
        scripted.tick();
        assert!(scripted.pending().is_empty());
        let notice = t!(
            "engineer.condition_dropped",
            id = RequestId(1),
            instruction = instruction
        );
        assert!(scripted.noticed(&notice));
    }

    #[test]
    fn a_new_weekend_clears_requests() {
        use bono_sim::sql::{EpochReason, begin_epoch};

        let mut scripted = Scripted::recorded();
        scripted.add("Fuel every lap, please.", fuel_every_lap());
        assert!(!scripted.noticed(t!("engineer.requests_cleared")));
        begin_epoch(&scripted.recording(), 1, EpochReason::Cleared).unwrap();
        scripted.tick();
        assert!(scripted.pending().is_empty());
        assert!(scripted.noticed(t!("engineer.requests_cleared")));
    }

    #[test]
    fn a_left_over_steer_starts_from_the_applied_changes() {
        let mut scripted = Scripted::recorded();
        scripted.ask("Fuel every lap, please.");
        scripted.until("asked", |s| s.questions().len() == 1);
        scripted.schedule(fuel_every_lap()).unwrap();
        scripted.ask("And the gap behind every lap.");
        // The turn was writing its final text: the steer is asked next, after its changes apply
        scripted.acts.send(Act::Reply).unwrap();
        scripted.until("asked again", |s| s.questions().len() == 2);
        let question = &scripted.questions()[1];
        assert!(question.contains("And the gap behind every lap."));
        assert!(question.contains("#1"), "{question}");
        assert!(
            !question.contains("Scheduled requests: none."),
            "{question}"
        );
        let added = scripted.schedule(fuel_every_lap()).unwrap();
        assert!(added.contains("#2"), "{added}");
        scripted.finish(Act::Reply);
        assert_eq!(scripted.pending(), [RequestId(1), RequestId(2)]);
    }

    #[test]
    fn scheduled_turn_starts_after_the_gather_window() {
        let mut scripted = Scripted::recorded();
        scripted.falls_due();
        let turns = scripted.questions().len();
        let just_before = schedule::GATHER - Duration::from_millis(1);
        scripted.wait(just_before);
        for _ in 0..10 {
            scripted.frame();
        }
        assert!(scripted.engineer.scheduled.is_none());
        scripted.wait(schedule::GATHER - just_before);
        scripted.frame();
        assert!(scripted.engineer.scheduled.is_some());
        let message = scripted.scheduled_asked(turns);
        assert!(message.contains(MARKER), "{message}");
        // Nobody asked: it isn't the pilot's turn
        assert!(!scripted.engineer.is_thinking());
        assert_eq!(scripted.questions().len(), turns + 1);
    }

    #[test]
    fn no_scheduled_turn_during_a_pilot_turn() {
        let mut scripted = Scripted::recorded();
        scripted.falls_due();
        let turns = scripted.questions().len();
        scripted.ask("How are the tyres?");
        scripted.until("asked", |s| s.questions().len() > turns);
        scripted.wait(schedule::GATHER);
        for _ in 0..10 {
            scripted.frame();
        }
        assert!(scripted.engineer.scheduled.is_none());
        assert_eq!(scripted.questions().len(), turns + 1);

        // Once it's answered
        scripted.finish(Act::Reply);
        let message = scripted.scheduled_asked(turns + 1);
        assert!(message.contains(MARKER), "{message}");
    }

    #[test]
    fn pilot_question_drops_the_scheduled_turn() {
        let mut scripted = Scripted::recorded();
        scripted.falls_due();
        let turns = scripted.questions().len();
        scripted.wait(schedule::GATHER);
        scripted.scheduled_asked(turns);

        scripted.ask("How are the tyres?");
        assert!(scripted.engineer.scheduled.is_none());
        assert!(scripted.engineer.is_thinking());
        assert!(!scripted.cancelled());
        // Asked as a question of its own, once the dropped turn lets go of the conversation
        scripted.until("asked", |s| s.questions().len() > turns + 1);
        let question = &scripted.questions()[turns + 1];
        assert!(question.contains("How are the tyres?"), "{question}");
        assert!(!question.contains("Scheduled turn"), "{question}");
        for _ in 0..10 {
            scripted.frame();
        }
        assert!(scripted.engineer.scheduled.is_none());

        // Still due: it runs again once the pilot's answered
        scripted.finish(Act::Reply);
        let message = scripted.scheduled_asked(turns + 2);
        assert!(message.contains(MARKER), "{message}");
        assert!(!scripted.cancelled());
    }

    #[test]
    fn scheduled_message_has_the_rules() {
        let mut scripted = Scripted::recorded();
        scripted.falls_due();
        let note = "[Something the model doesn't know happened.]".to_owned();
        scripted.engineer.note(note.clone());
        let turns = scripted.questions().len();
        scripted.wait(schedule::GATHER);
        let message = scripted.scheduled_asked(turns);
        // Told once
        assert!(scripted.engineer.unheard.is_empty());
        for part in [
            "#1",
            MARKER,
            "started returning a row",
            "acknowledge",
            "Open with the subject",
            schedule::SILENCE,
        ] {
            assert!(message.contains(part), "{part:?} in {message}");
        }
        let progress = scripted.engineer.progress.clone().unwrap();
        let pending = scripted.engineer.schedule().pending_text(&progress);
        // The note is said once, so it's kept with the message, before what comes every turn
        let begin = message
            .find("[BEGIN UNTRUSTED TELEMETRY SNAPSHOT]")
            .unwrap();
        let snapshot = message.find("[END UNTRUSTED TELEMETRY SNAPSHOT]").unwrap();
        let at = message.find(&pending).expect("the pending list");
        let told = message.find(&note).expect("the note");
        let reply = message.rfind(&language::reply_note(arrstr!("en"))).unwrap();
        assert!(told < begin && snapshot < at && at < reply, "{message}");
    }

    #[test]
    fn silence_marker_anywhere_is_silent() {
        let mut scripted = Scripted::recorded();
        scripted.falls_due();
        let answers = scripted.answers();
        let turns = scripted.questions().len();
        scripted.wait(schedule::GATHER);
        scripted.scheduled_asked(turns);
        scripted.finish_scheduled(Act::Say("Copy, checking.\n[SILENCE]".into()));
        assert!(scripted.answers_waiting().is_empty());
        // No tone either
        assert!(!scripted.engineer.is_speaking());
        assert_eq!(scripted.answers(), answers);
        let nothing = t!("engineer.nothing_to_report", what = marker_fired());
        assert!(scripted.noticed(&nothing));
        // Answered: the one-shot is done
        assert!(scripted.pending().is_empty());
    }

    #[test]
    fn failed_turn_retries_with_one_notice() {
        let mut scripted = Scripted::recorded();
        scripted.falls_due();
        let mut turns = scripted.questions().len();
        scripted.wait(schedule::GATHER);
        for _ in 0..3 {
            scripted.scheduled_asked(turns);
            turns += 1;
            scripted.finish_scheduled(Act::Fail);
            assert!(!scripted.engineer.is_speaking());
            // Not again until `RETRY_AFTER` has passed
            scripted.wait(schedule::RETRY_AFTER / 2);
            for _ in 0..10 {
                scripted.frame();
            }
            assert!(scripted.engineer.scheduled.is_none());
            assert_eq!(scripted.questions().len(), turns);
            scripted.wait(schedule::RETRY_AFTER / 2);
        }
        scripted.scheduled_asked(turns);
        scripted.finish_scheduled(Act::Say("[SILENCE]".into()));
        let failed = t!("engineer.scheduled_failed", error = "scripted failure");
        let notices = (scripted.engineer.chat.iter())
            .filter(|e| e.speaker == Speaker::Notice && e.text == failed)
            .count();
        assert_eq!(notices, 1);
        assert!(scripted.pending().is_empty());
    }

    #[test]
    fn refusal_counts_as_silence() {
        let mut scripted = Scripted::recorded();
        scripted.falls_due();
        let answers = scripted.answers();
        let turns = scripted.questions().len();
        scripted.wait(schedule::GATHER);
        scripted.scheduled_asked(turns);
        // What it staged goes with the exchange
        scripted.schedule(fuel_every_lap()).unwrap();
        scripted.finish_scheduled(Act::Refuse);
        let nothing = t!("engineer.nothing_to_report", what = marker_fired());
        assert!(scripted.noticed(&nothing));
        assert_eq!(scripted.answers(), answers);
        assert!(!scripted.engineer.is_speaking());
        assert!(scripted.pending().is_empty());
    }

    #[test]
    fn unplayed_answer_resolves_its_requests() {
        // No voice: an answer can't be spoken
        for act in [Act::Say("[SILENCE]".into()), Act::Refuse, Act::Reply] {
            let mut scripted = Scripted::recorded();
            scripted.falls_due();
            assert_eq!(scripted.engineer.schedule().conditions().len(), 1);
            let answers = scripted.answers();
            let turns = scripted.questions().len();
            scripted.wait(schedule::GATHER);
            scripted.scheduled_asked(turns);
            let answered = matches!(act, Act::Reply);
            scripted.finish_scheduled(act);
            assert!(scripted.answers_waiting().is_empty());
            assert!(scripted.pending().is_empty());
            assert!(scripted.engineer.schedule().conditions().is_empty());
            let tool = scripted.engineer.tool.clone().unwrap();
            assert!(tool.conditions().is_empty());
            // Shown in the chat all the same
            assert_eq!(scripted.answers(), answers + usize::from(answered));
            let fired = t!("engineer.scheduled", what = marker_fired());
            assert_eq!(scripted.noticed(&fired), answered);
        }
    }

    #[test]
    fn scheduled_turn_can_schedule() {
        let mut scripted = Scripted::recorded();
        scripted.falls_due();
        let turns = scripted.questions().len();
        scripted.wait(schedule::GATHER);
        scripted.scheduled_asked(turns);
        let added = scripted.schedule(fuel_every_lap()).unwrap();
        assert!(added.contains("#2"), "{added}");
        assert_eq!(scripted.pending(), [RequestId(1)]);
        scripted.finish_scheduled(Act::Reply);
        assert_eq!(scripted.pending(), [RequestId(2)]);
    }

    #[test]
    fn answered_before_its_own_changes() {
        let mut scripted = Scripted::recorded_with(VOICES);
        scripted.falls_due();
        let turns = scripted.questions().len();
        scripted.wait(schedule::GATHER);
        scripted.scheduled_asked(turns);
        // It makes its own request repeat
        let mut replace = marker_set();
        replace["action"] = serde_json::json!("replace");
        replace["id"] = serde_json::json!("#1");
        replace["repeat"] = serde_json::json!("every");
        scripted.schedule(replace).unwrap();
        scripted.finish_scheduled(Act::Reply);
        assert_eq!(scripted.pending(), [RequestId(1)]);
        // The replacement waits for the answer to be heard
        assert_eq!(scripted.engineer.schedule().waiting(RequestId(1)), 1);
        let ready = scripted.answers_waiting();
        assert_eq!(ready.len(), 1);
        let answer = ready[0].scheduled.as_ref().unwrap();
        assert_eq!(answer.text, "Copy.");
        assert!(!ready[0].tone_after_drain);
        assert_eq!(answer.fired.len(), 1);
        assert_eq!(answer.fired[0].id, RequestId(1));
        let synthesis = scripted.engineer.synthesis.as_ref().unwrap();
        assert_eq!(synthesis.len(), 1);
        assert_eq!(
            (synthesis[0].lang, synthesis[0].text.as_str()),
            (arrstr!("en"), "Copy.")
        );
        // The notice naming what fired, then the answer
        let chat = &scripted.engineer.chat;
        let fired = t!("engineer.scheduled", what = marker_fired());
        let at = (chat.iter()).rposition(|e| e.speaker == Speaker::Notice && e.text == fired);
        let answer = &chat[at.expect("the notice") + 1];
        assert_eq!(answer.speaker, Speaker::Engineer);
        assert_eq!(answer.text, "Copy.");
    }

    #[test]
    fn cut_off_without_a_word_is_tried_again() {
        for stop in [StopReason::MaxTokens, StopReason::ContextWindowExceeded] {
            let mut scripted = Scripted::recorded();
            let turns = scripted.scheduled_running();
            let full = scripted.engineer.session.as_ref().unwrap().state.clone();
            scripted.finish_scheduled(Act::End(stop, ""));
            // Not answered: still due, and nothing in the chat
            let nothing = t!("engineer.nothing_to_report", what = marker_fired());
            assert!(!scripted.noticed(&nothing), "{stop:?}");
            assert!(!scripted.noticed(&t!("engineer.scheduled", what = marker_fired())));
            let failed = t!("engineer.scheduled_failed", error = "scripted failure");
            assert!(!scripted.noticed(&failed), "{stop:?}");
            assert!(scripted.answers_waiting().is_empty());
            assert!(!scripted.engineer.is_speaking());
            assert_eq!(scripted.pending(), [RequestId(1)], "{stop:?}");
            let full_context = stop == StopReason::ContextWindowExceeded;
            assert_eq!(scripted.engineer.new_session, full_context);

            // Tried again after a pause
            scripted.wait(schedule::RETRY_AFTER / 2);
            for _ in 0..10 {
                scripted.frame();
            }
            assert!(scripted.engineer.scheduled.is_none(), "{stop:?}");
            assert_eq!(scripted.questions().len(), turns + 1);
            scripted.wait(schedule::RETRY_AFTER / 2);
            match full_context {
                // On a new conversation, or told why there isn't one (Bono Cloud, signed out)
                true => {
                    let error = t!(
                        "engineer.scheduled_failed",
                        error = t!("provider.bono_cloud_sign_in")
                    );
                    scripted.until("started over", |s| {
                        let restarted = (s.engineer.session.as_ref())
                            .is_some_and(|session| !Arc::ptr_eq(&session.state, &full));
                        restarted || s.noticed(&error)
                    });
                }
                false => {
                    let message = scripted.scheduled_asked(turns + 1);
                    assert!(message.contains(MARKER), "{message}");
                }
            }
        }
    }

    #[test]
    fn cut_off_answer_keeps_what_it_said() {
        let mut scripted = Scripted::recorded_with(VOICES);
        scripted.scheduled_running();
        scripted.finish_scheduled(Act::End(StopReason::MaxTokens, "Marker's set, and"));
        let ready = scripted.answers_waiting();
        assert_eq!(ready.len(), 1);
        assert_eq!(
            ready[0].scheduled.as_ref().unwrap().text,
            "Marker's set, and"
        );
        // The incomplete tone follows it
        assert!(ready[0].tone_after_drain);
        assert!(scripted.noticed(&t!("engineer.scheduled", what = marker_fired())));
        assert!(scripted.pending().is_empty());
    }

    #[test]
    fn cancelling_its_own_request_keeps_the_answer() {
        let mut scripted = Scripted::recorded_with(VOICES);
        scripted.scheduled_running();
        // Answered for the last time: no more firings
        scripted.schedule(cancel(RequestId(1))).unwrap();
        scripted.finish_scheduled(Act::Reply);
        assert!(scripted.pending().is_empty());
        assert!(!scripted.noticed(&t!("engineer.request_missing", id = RequestId(1))));
        // The answer is still on its way, and isn't out of date for being cancelled
        assert_eq!(scripted.engineer.schedule().waiting(RequestId(1)), 1);
        let ready = scripted.answers_waiting();
        assert_eq!(ready.len(), 1);
        let progress = scripted.engineer.progress.clone().unwrap();
        let schedule = scripted.engineer.schedule();
        let fired = &ready[0].scheduled.as_ref().unwrap().fired;
        assert!(!schedule.is_stale(fired, &progress));
    }

    #[test]
    fn no_scheduled_turn_while_the_pilot_talks() {
        let mut scripted = Scripted::recorded();
        scripted.falls_due();
        let turns = scripted.questions().len();
        scripted.engineer.talk(true);
        scripted.wait(schedule::GATHER);
        for _ in 0..10 {
            scripted.frame();
        }
        assert!(scripted.engineer.scheduled.is_none());
        assert_eq!(scripted.questions().len(), turns);
        scripted.engineer.talk(false);
        scripted.hear(Heard::TooShort);
        let message = scripted.scheduled_asked(turns);
        assert!(message.contains(MARKER), "{message}");
    }

    #[test]
    fn no_scheduled_turn_while_what_was_said_is_heard() {
        // Whatever is heard, nothing else is on its way
        let heard = |which| match which {
            0 => Heard::TooShort,
            1 => Heard::Nothing,
            _ => Heard::Failed(anyhow::anyhow!("no audio")),
        };
        for which in 0..3 {
            let mut scripted = Scripted::recorded();
            scripted.falls_due();
            let turns = scripted.questions().len();
            scripted.engineer.talk(true);
            scripted.engineer.talk(false);
            // A question may be on its way, which would drop a turn started now
            scripted.wait(schedule::GATHER);
            for _ in 0..10 {
                scripted.frame();
            }
            assert!(scripted.engineer.scheduled.is_none(), "{which}");
            assert_eq!(scripted.questions().len(), turns, "{which}");
            scripted.hear(heard(which));
            let message = scripted.scheduled_asked(turns);
            assert!(message.contains(MARKER), "{which}: {message}");
        }
    }

    #[test]
    fn a_lost_transcription_holds_scheduled_turns_back_for_a_while() {
        let mut scripted = Scripted::recorded();
        scripted.falls_due();
        let turns = scripted.questions().len();
        scripted.engineer.talk(true);
        scripted.engineer.talk(false);
        // Nothing is ever heard
        scripted.wait(HEARING_WAIT - Duration::from_millis(1));
        for _ in 0..10 {
            scripted.frame();
        }
        assert!(scripted.engineer.scheduled.is_none());
        scripted.wait(Duration::from_millis(1));
        let message = scripted.scheduled_asked(turns);
        assert!(message.contains(MARKER), "{message}");
    }

    #[test]
    fn requests_outlive_the_conversation() {
        // Kept whatever ends the conversation within the weekend: the next scheduled turn runs on
        // the new one
        for how in ["signed out", "new model", "corner names"] {
            let mut scripted = Scripted::recorded_with(r#"{"model": {"provider": "ollama"}}"#);
            let made = scripted.new_conversations();
            scripted.falls_due();
            let old = scripted.engineer.session.as_ref().unwrap().state.clone();
            match how {
                "signed out" => {
                    let session = scripted.engineer.session.as_mut().unwrap();
                    session.user = Some("pilot".into());
                    scripted.frame();
                    assert!(scripted.engineer.session.is_none(), "{how}");
                    assert!(scripted.noticed(t!("engineer.signed_out")), "{how}");
                }
                "new model" => {
                    (scripted.config).update(|raw| raw.model.ollama.model.set("other".into()))
                }
                _ => scripted.config.update(|raw| raw.corner_names.set(true)),
            }
            assert_eq!(scripted.pending(), [RequestId(1)], "{how}");
            let turns = scripted.questions().len();
            scripted.wait(schedule::GATHER);
            scripted.next_conversation(&made);
            let message = scripted.scheduled_asked(turns);
            assert!(message.contains(MARKER), "{how}: {message}");
            let new = &scripted.engineer.session.as_ref().unwrap().state;
            assert!(!Arc::ptr_eq(&old, new), "{how}");
            scripted.finish_scheduled(Act::Say("Marker: it's set.".into()));
            let fired = t!("engineer.scheduled", what = marker_fired());
            assert!(scripted.noticed(&fired), "{how}");
            assert!(scripted.pending().is_empty(), "{how}");
        }
    }

    #[test]
    fn scheduled_turn_starts_once_the_next_snapshots_are_in() {
        let mut scripted = Scripted::recorded();
        scripted.falls_due();
        let turns = scripted.questions().len();
        // The schedule's wall clock stands still: only snapshots move it on
        for _ in 1..schedule::GATHER_SNAPSHOTS {
            scripted.tick();
            for _ in 0..10 {
                scripted.frame();
            }
            assert!(scripted.engineer.scheduled.is_none());
        }
        scripted.tick();
        let message = scripted.scheduled_asked(turns);
        assert!(message.contains(MARKER), "{message}");
    }

    #[test]
    fn a_new_model_is_noticed_once() {
        let mut scripted = Scripted::recorded();
        scripted.falls_due();
        // A model that can't start: Bono Cloud's, signed out
        (scripted.config).update(|raw| raw.model.claude.model.set("claude-other".into()));
        scripted.wait(schedule::GATHER);
        let failed = t!(
            "engineer.scheduled_failed",
            error = t!("provider.bono_cloud_sign_in")
        );
        scripted.until("tried", |s| s.noticed(&failed));
        for _ in 0..3 {
            scripted.wait(schedule::RETRY_AFTER);
            scripted.frame();
        }
        assert_eq!(scripted.notices(t!("engineer.new_model")), 1);
        assert_eq!(scripted.notices(&failed), 1);
        assert!(scripted.engineer.scheduled.is_none());
    }

    #[test]
    fn ask_takes_in_a_scheduled_turn_that_finished() {
        let mut scripted = Scripted::recorded();
        let turns = scripted.scheduled_running();
        scripted.acts.send(Act::Say("[SILENCE]".into())).unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        while !(scripted.engineer.scheduled.as_ref()).is_some_and(|s| s.task.is_finished()) {
            assert!(Instant::now() < deadline, "never finished");
            std::thread::sleep(Duration::from_millis(1));
        }
        scripted.ask("How are the tyres?");
        // Answered before the question, not dropped
        let nothing = t!("engineer.nothing_to_report", what = marker_fired());
        let chat = &scripted.engineer.chat;
        let at = chat.iter().position(|e| e.text == nothing).unwrap();
        assert_eq!(chat[at + 1].speaker, Speaker::Pilot);
        assert!(scripted.pending().is_empty());
        scripted.until("asked", |s| s.questions().len() > turns + 1);
        scripted.finish(Act::Reply);
        for _ in 0..10 {
            scripted.frame();
        }
        assert!(scripted.engineer.scheduled.is_none());
        assert_eq!(scripted.questions().len(), turns + 2);
    }

    #[test]
    fn clearing_and_stopping_drop_the_scheduled_turn() {
        use bono_sim::sql::{EpochReason, begin_epoch};

        for how in ["new session", "new weekend", "stopped"] {
            let mut scripted = Scripted::recorded();
            let turns = scripted.scheduled_running();
            match how {
                "new session" => scripted.engineer.new_session(),
                "new weekend" => {
                    begin_epoch(&scripted.recording(), 1, EpochReason::Cleared).unwrap();
                    scripted.tick();
                }
                _ => scripted.engineer.stop(),
            }
            assert!(scripted.engineer.scheduled.is_none(), "{how}");
            assert!(!scripted.cancelled(), "{how}");
            match how {
                // Still due: it runs again
                "stopped" => {
                    let message = scripted.scheduled_asked(turns + 1);
                    assert!(message.contains(MARKER), "{message}");
                }
                _ => {
                    assert!(scripted.pending().is_empty(), "{how}");
                    for _ in 0..10 {
                        scripted.frame();
                    }
                    assert!(scripted.engineer.scheduled.is_none(), "{how}");
                    assert_eq!(scripted.questions().len(), turns + 1, "{how}");
                }
            }
        }
    }

    #[test]
    fn a_pilot_answer_ends_the_outage() {
        let mut scripted = Scripted::recorded();
        let turns = scripted.scheduled_running();
        let failed = t!("engineer.scheduled_failed", error = "scripted failure");
        scripted.finish_scheduled(Act::Fail);
        assert_eq!(scripted.notices(&failed), 1);
        // The model answers the pilot: it's reachable again
        scripted.ask("How are the tyres?");
        scripted.until("asked", |s| s.questions().len() > turns + 1);
        scripted.finish(Act::Reply);
        // So the next failure is another outage
        scripted.wait(schedule::RETRY_AFTER);
        scripted.scheduled_asked(turns + 2);
        scripted.finish_scheduled(Act::Fail);
        assert_eq!(scripted.notices(&failed), 2);
    }

    /// A scheduled answer that answers nothing the schedule has: never out of date.
    fn scheduled_answer(text: &str) -> ScheduledAnswer {
        ScheduledAnswer {
            text: text.into(),
            fired: Vec::new(),
            conversation: None,
            entry: None,
        }
    }

    /// How many times the chat says there's no speaker to play on: what playing looks like in a
    /// test.
    fn no_speaker(engineer: &Engineer) -> usize {
        (engineer.chat.iter())
            .filter(|e| e.speaker == Speaker::Notice && e.text == t!("engineer.no_speaker"))
            .count()
    }

    #[test]
    fn scheduled_answer_joins_the_back() {
        // A pilot answer is playing, with more to come
        let mut engineer = Engineer::new(reqwest::Client::new(), None);
        let (_send, mut playing) = being_written();
        playing.sentences.pieces.push_back(piece());
        engineer.line.push_back(playing);
        let answer = scheduled_answer("Lap four: one minute forty two point three.");
        engineer.queue_scheduled(SpeechOutput::ready(piece()), answer, false);
        engineer.admit(1.0);
        assert_eq!(engineer.line.len(), 2);
        assert!(engineer.line[0].scheduled.is_none());
        assert!(engineer.line[1].scheduled.is_some());
        // Made in one piece, it may play in the pilot answer's gap, as any complete answer
        assert!(engineer.line[1].complete);
        // Waiting, it isn't speaking: the pilot answer is
        engineer.line.pop_front();
        assert!(!engineer.is_speaking());
    }

    #[test]
    fn ready_scheduled_answer_plays_while_a_pilot_question_is_thought_about() {
        // Its answer was ready before the pilot asked: the question doesn't drop it
        let mut scripted = Scripted::recorded_with(VOICES);
        scripted.scheduled_running();
        scripted.finish_scheduled(Act::Say("Marker: it's set.".into()));
        let turns = scripted.questions().len();
        scripted.ask("How are the tyres?");
        scripted.until("asked", |s| s.questions().len() > turns);
        assert!(scripted.engineer.is_thinking());
        assert_eq!(scripted.answers_waiting().len(), 1);

        // Ready while the pilot's answer has nothing to say yet: it goes first
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let _entered = runtime.enter();
        let dir = tempfile::tempdir().unwrap();
        let config = runtime
            .block_on(Config::new(dir.path().join("config.json")))
            .unwrap();
        let mut engineer = Engineer::new(reqwest::Client::new(), None);
        let (_send, thinking) = being_written();
        engineer.queue(thinking);
        let answer = scheduled_answer("Lap four: one minute forty two point three.");
        engineer.queue_scheduled(SpeechOutput::ready(piece()), answer, false);
        engineer.play(&config, None);
        // It played (and found no speaker to play on), the pilot's answer still to come
        assert_eq!(no_speaker(&engineer), 1);
        assert_eq!(engineer.incoming.len(), 1);
        assert!(engineer.incoming[0].writing);
    }

    #[test]
    fn stale_answer_is_dropped_at_the_front_and_noted() {
        let mut scripted = Scripted::recorded_with(VOICES);
        scripted.answer_waiting("Marker: it's set.");
        scripted.new_game_session();
        // Not checked while it waits for the pilot
        assert_eq!(scripted.answers_waiting().len(), 1);
        let played = no_speaker(&scripted.engineer);
        scripted.engineer.talk(false);
        scripted.frame();
        // Dropped without playing, and its request resolved
        assert!(scripted.answers_waiting().is_empty());
        assert_eq!(no_speaker(&scripted.engineer), played);
        assert!(scripted.pending().is_empty());
        assert_eq!(scripted.engineer.schedule().waiting(RequestId(1)), 0);
        assert!(scripted.engineer.schedule().conditions().is_empty());
        let tool = scripted.engineer.tool.clone().unwrap();
        assert!(tool.conditions().is_empty());
        // The chat says it wasn't said, and why
        let answer = (scripted.engineer.chat.iter())
            .rfind(|e| e.speaker == Speaker::Engineer)
            .unwrap();
        assert_eq!(answer.text, "Marker: it's set.");
        let detail = answer.detail.as_deref().unwrap();
        assert!(
            detail.ends_with(t!("engineer.detail_not_said_stale")),
            "{detail}"
        );

        // The model is told the pilot didn't hear it
        let turns = scripted.questions().len();
        scripted.ask("How are the tyres?");
        scripted.until("asked", |s| s.questions().len() > turns);
        let question = &scripted.questions()[turns];
        let note = unheard_note("Marker: it's set.", Unsaid::Stale);
        assert!(question.contains(&note), "{question}");
        assert!(
            note.contains("out of date") && !note.contains("  "),
            "{note}"
        );
    }

    #[test]
    fn a_cancelled_requests_answer_is_dropped_for_that_reason() {
        let mut scripted = Scripted::recorded_with(VOICES);
        let mut every = marker_set();
        every["repeat"] = serde_json::json!("every");
        scripted.answer_waiting_with(every, "Marker: it's set.");
        // The pilot cancels the request while its answer waits
        let turns = scripted.questions().len();
        scripted.ask("Forget the marker.");
        scripted.until("asked", |s| s.questions().len() > turns);
        scripted.schedule(cancel(RequestId(1))).unwrap();
        scripted.finish(Act::Reply);
        assert!(scripted.pending().is_empty());
        scripted.engineer.talk(false);
        scripted.frame();
        assert!(scripted.answers_waiting().is_empty());
        let answer = (scripted.engineer.chat.iter())
            .find(|e| e.speaker == Speaker::Engineer && e.text == "Marker: it's set.")
            .unwrap();
        let detail = answer.detail.as_deref().unwrap();
        assert!(
            detail.ends_with(t!("engineer.detail_not_said_gone")),
            "{detail}"
        );
        // Not said to be out of date
        let notes = scripted.notes();
        assert_eq!(notes, [unheard_note("Marker: it's set.", Unsaid::Gone)]);
        assert!(!notes[0].contains("out of date"), "{}", notes[0]);
    }

    #[test]
    fn a_note_is_not_told_to_a_conversation_started_over_since() {
        let mut scripted = Scripted::recorded_with(VOICES);
        scripted.answer_waiting("Marker: it's set.");
        scripted.new_game_session();
        // A new weekend: the next turn starts the conversation over (`auto_session`), held up
        // until the answer has been dropped, in the frame the turn started
        let state = scripted.engineer.session.as_ref().unwrap().state.clone();
        scripted.new_weekend();
        let held = state.blocking_lock();
        let turns = scripted.questions().len();
        scripted.ask("How are the tyres?");
        scripted.engineer.talk(false);
        scripted.frame();
        assert!(scripted.answers_waiting().is_empty());
        drop(held);
        scripted.until("asked", |s| s.questions().len() > turns);
        scripted.finish(Act::Reply);

        // The conversation started over never gave that answer
        let note = unheard_note("Marker: it's set.", Unsaid::Stale);
        let turns = scripted.questions().len();
        scripted.ask("Anything else?");
        scripted.until("asked", |s| s.questions().len() > turns);
        for question in &scripted.questions()[turns - 1..] {
            assert!(!question.contains(&note), "{question}");
        }
    }

    #[test]
    fn cut_off_turn_keeps_its_changes() {
        for stop in [StopReason::MaxTokens, StopReason::ContextWindowExceeded] {
            let mut scripted = Scripted::recorded();
            let turns = scripted.scheduled_running();
            // It confirmed an add, then ran out before it said anything: the conversation keeps
            // the exchange, so the add counts
            let added = scripted.schedule(fuel_every_lap()).unwrap();
            assert!(added.contains("#2"), "{added}");
            scripted.finish_scheduled(Act::End(stop, ""));
            assert_eq!(scripted.pending(), [RequestId(1), RequestId(2)], "{stop:?}");
            if stop == StopReason::MaxTokens {
                // Nothing was answered: tried again after a pause
                scripted.wait(schedule::RETRY_AFTER);
                let message = scripted.scheduled_asked(turns + 1);
                assert!(message.contains(MARKER), "{message}");
            }
        }
    }

    #[test]
    fn an_unheard_answer_is_told_only_to_its_conversation() {
        // Another conversation started (e.g. the model changed) while it waited
        let mut scripted = Scripted::recorded_with(VOICES);
        scripted.answer_waiting("Marker: it's set.");
        let state = scripted.engineer.session.as_ref().unwrap().state.clone();
        let other = scripted.runtime.block_on(async {
            let state = state.lock().await;
            SessionState {
                conversation: Box::new(RecordingModel(Arc::default())),
                pool: state.pool.clone(),
                sim: state.sim,
                tool: state.tool.clone(),
                schedule_tool: state.schedule_tool.clone(),
                pit_tool: None,
                weekend: None,
                starts: Arc::default(),
                setup_outline: None,
                forgotten: 0,
                outline_offered: None,
            }
        });
        scripted.engineer.session.as_mut().unwrap().state = Arc::new(Mutex::new(other));
        scripted.new_game_session();
        scripted.engineer.talk(false);
        scripted.frame();
        assert!(scripted.answers_waiting().is_empty());
        assert!(scripted.engineer.unheard.is_empty());

        // The same conversation, started over since (`auto_session`)
        let mut scripted = Scripted::recorded_with(VOICES);
        scripted.answer_waiting("Marker: it's set.");
        let session = scripted.engineer.session.as_ref().unwrap();
        session.starts.fetch_add(1, atomic::Ordering::Relaxed);
        scripted.new_game_session();
        scripted.engineer.talk(false);
        scripted.frame();
        assert!(scripted.answers_waiting().is_empty());
        assert!(scripted.engineer.unheard.is_empty());
    }

    #[test]
    fn started_answer_is_never_dropped_as_stale() {
        let mut scripted = Scripted::recorded_with(VOICES);
        scripted.answer_waiting("Marker: it's set.");
        assert!(!scripted.engineer.is_speaking());
        // It had started playing when the pilot pressed the talk button
        scripted.engineer.line[0].started = true;
        assert!(scripted.engineer.is_speaking());
        scripted.new_game_session();
        let played = no_speaker(&scripted.engineer);
        scripted.engineer.talk(false);
        scripted.frame();
        // It carries on (and finds no speaker to play on), not dropped as stale
        assert_eq!(no_speaker(&scripted.engineer), played + 1);
        assert!(scripted.engineer.unheard.is_empty());
        assert!(scripted.answers_waiting().is_empty());
        assert!(scripted.pending().is_empty());
    }

    #[test]
    fn spoken_in_the_current_reply_language() {
        let voices = r#"{"voices": {"en": "en.onnx", "es": "es.onnx"}}"#;
        let mut scripted = Scripted::recorded_with(voices);
        // Asked for in English
        scripted.falls_due();
        assert_eq!(scripted.engineer.last_lang, arrstr!("en"));
        // Spanish replies picked since
        let es = Some(arrstr!("es"));
        (scripted.config).update(|raw| raw.spoken_language.set(es));
        let turns = scripted.questions().len();
        scripted.wait(schedule::GATHER);
        let message = scripted.scheduled_asked(turns);
        let note = language::reply_note(arrstr!("es"));
        assert!(message.contains(&note), "{message}");
        scripted.finish_scheduled(Act::Say("Marcador: puesto.".into()));
        let synthesis = scripted.engineer.synthesis.as_ref().unwrap();
        assert_eq!(synthesis.len(), 1);
        assert_eq!(synthesis[0].lang, arrstr!("es"));
        assert_eq!(synthesis[0].text, "Marcador: puesto.");
    }

    #[test]
    fn talk_button_pauses_and_release_resumes() {
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let _entered = runtime.enter();
        let dir = tempfile::tempdir().unwrap();
        let config = runtime
            .block_on(Config::new(dir.path().join("config.json")))
            .unwrap();
        let mut engineer = Engineer::new(reqwest::Client::new(), None);
        let answer = scheduled_answer("Lap four: one minute forty two point three.");
        engineer.queue_scheduled(SpeechOutput::ready(piece()), answer, false);
        // Ready while the pilot talks: it waits at the front
        engineer.talk(true);
        engineer.play(&config, None);
        engineer.play(&config, None);
        assert_eq!(engineer.line.len(), 1);
        assert!(engineer.line[0].held);
        assert_eq!(no_speaker(&engineer), 0);
        // The spotter speaking keeps it paused once the pilot lets go
        engineer.hold(true);
        engineer.talk(false);
        engineer.play(&config, None);
        assert!(engineer.line[0].held);
        assert_eq!(no_speaker(&engineer), 0);
        // Released, it plays (and finds no speaker to play on)
        engineer.hold(false);
        assert!(!engineer.line[0].held);
        engineer.play(&config, None);
        assert_eq!(no_speaker(&engineer), 1);
    }

    #[test]
    fn clearing_drops_waiting_scheduled_answers() {
        use bono_sim::sql::{EpochReason, begin_epoch};

        for how in ["new session", "new weekend", "simulator stopped"] {
            // Being synthesized, or synthesized and waiting in the line
            for in_line in [false, true] {
                let mut scripted = Scripted::recorded_with(VOICES);
                scripted.scheduled_running();
                scripted.finish_scheduled(Act::Say("Marker: it's set.".into()));
                if in_line {
                    scripted.engineer.talk(true);
                    scripted.synthesize(piece());
                }
                assert_eq!(scripted.answers_waiting().len(), 1);
                match how {
                    "new session" => scripted.engineer.new_session(),
                    "new weekend" => {
                        begin_epoch(&scripted.recording(), 1, EpochReason::Cleared).unwrap();
                        scripted.tick();
                    }
                    _ => {
                        let _entered = scripted.runtime.enter();
                        let (config, synth) = (&scripted.config, &mut scripted.synth);
                        let rt = scripted.runtime.handle();
                        scripted.engineer.update(rt, None, config, synth, None);
                    }
                }
                assert!(scripted.answers_waiting().is_empty(), "{how}, {in_line}");
                assert!(scripted.pending().is_empty(), "{how}, {in_line}");
                assert!(scripted.engineer.unheard.is_empty(), "{how}, {in_line}");
            }
        }
    }

    #[test]
    fn played_answer_resolves_its_requests() {
        let mut scripted = Scripted::recorded_with(VOICES);
        // Out of date once there's a flag: a condition checked while its answer waits
        scripted
            .recording()
            .execute_batch("CREATE TABLE flag (x)")
            .unwrap();
        let mut request = marker_set();
        let flag = "SELECT 1 FROM recording.flag";
        request["stale_after"] = serde_json::json!({"kind": "condition", "query": flag});
        scripted.answer_waiting_with(request, "Marker: it's set.");
        // The one-shot stays, its staleness checked, until its answer has played
        assert_eq!(scripted.engineer.schedule().conditions().len(), 1);
        let tool = scripted.engineer.tool.clone().unwrap();
        assert_eq!(tool.conditions().len(), 1);
        // As if it had played all it had: its speech ends there
        let front = &mut scripted.engineer.line[0];
        front.sentences = SentenceQueue::default();
        front.started = true;
        drop(
            scripted.engineer.synthesis.as_mut().unwrap()[0]
                .speech
                .take(),
        );
        let played = no_speaker(&scripted.engineer);
        scripted.engineer.talk(false);
        scripted.frame();
        assert!(scripted.answers_waiting().is_empty());
        assert_eq!(no_speaker(&scripted.engineer), played);
        assert_eq!(scripted.engineer.schedule().waiting(RequestId(1)), 0);
        assert!(scripted.pending().is_empty());
        assert!(scripted.engineer.schedule().conditions().is_empty());
        assert!(tool.conditions().is_empty());
    }

    #[test]
    fn an_incomplete_scheduled_answer_is_followed_by_the_tone() {
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let _entered = runtime.enter();
        let dir = tempfile::tempdir().unwrap();
        let config = runtime
            .block_on(Config::new(dir.path().join("config.json")))
            .unwrap();
        let mut engineer = Engineer::new(reqwest::Client::new(), None);
        let answer = scheduled_answer("Lap four: one minute forty two point three, and");
        engineer.queue_scheduled(SpeechOutput::ready(piece()), answer, true);
        engineer.admit(1.0);
        // As if it had played all it had
        engineer.line[0].sentences = SentenceQueue::default();
        engineer.line[0].started = true;
        engineer.play(&config, None);
        assert_eq!(engineer.line.len(), 1, "the tone follows the answer");
        let tone = &mut engineer.line[0];
        assert!(tone.complete && tone.scheduled.is_none());
        let Poll::Ready(Some(Ok(tone))) = tone.speech.try_next() else {
            panic!("the tone must follow the answer");
        };
        assert_eq!(tone.samples, Speech::no_beep(1.0).samples);

        // One whose synthesis gave nothing has no tone: the pilot didn't ask
        let mut engineer = Engineer::new(reqwest::Client::new(), None);
        let (send, speech) = SpeechOutput::channel();
        drop(send);
        engineer.queue_scheduled(speech, scheduled_answer("And"), true);
        engineer.admit(1.0);
        assert!(!engineer.is_speaking());
        assert!(engineer.incoming.is_empty() && engineer.line.is_empty());
    }

    #[test]
    fn a_scheduled_answer_that_cannot_be_synthesized_is_resolved() {
        let mut scripted = Scripted::recorded_with(VOICES);
        scripted.scheduled_running();
        scripted.finish_scheduled(Act::Say("Marker: it's set.".into()));
        let synthesis = scripted.engineer.synthesis.as_mut().unwrap();
        let output = synthesis[0].speech.take().unwrap();
        output
            .send(Err(anyhow::anyhow!("no voice loaded")))
            .unwrap();
        scripted.until("dropped", |s| s.answers_waiting().is_empty());
        let error = t!("engineer.error_synthesizing", error = "no voice loaded");
        assert!(scripted.noticed(&error));
        // No tone: the pilot didn't ask
        assert!(!scripted.engineer.is_speaking());
        assert!(scripted.pending().is_empty());
        assert_eq!(scripted.engineer.schedule().waiting(RequestId(1)), 0);
    }

    /// A scheduled answer playing on a speaker: the talk button pauses it, it isn't dropped
    /// once it's started however out of date it gets, and once it's played out its request is
    /// resolved.
    #[test]
    #[ignore = "run alone with SDL_AUDIO_DRIVER=dummy"]
    fn a_scheduled_answer_pauses_resumes_and_plays_out() {
        let sdl = sdl3::init().unwrap();
        let audio = sdl.audio().unwrap();
        // A fifth of a second: long enough to be paused partway
        let speech = Speech {
            samples: vec![0.0; 4410],
            sample_rate: 22050,
        };
        let device = audio.open_playback_device(&speech.spec()).unwrap();
        let mut scripted = Scripted::recorded_with(VOICES);
        scripted.scheduled_running();
        scripted.finish_scheduled(Act::Say("Marker: it's set.".into()));
        let play_until = |scripted: &mut Scripted, done: fn(&Scripted) -> bool| {
            let _entered = scripted.runtime.enter();
            let deadline = Instant::now() + Duration::from_secs(5);
            while !done(scripted) {
                assert!(Instant::now() < deadline, "it never got there");
                let rt = scripted.runtime.handle();
                let (sim, config) = (Some(&mut *scripted.sim), &scripted.config);
                let synth = &mut scripted.synth;
                scripted
                    .engineer
                    .update(rt, sim, config, synth, Some(&device));
                std::thread::sleep(Duration::from_millis(5));
            }
        };
        let output = scripted.engineer.synthesis.as_mut().unwrap()[0]
            .speech
            .take();
        output.unwrap().send(Ok(speech)).unwrap();
        play_until(&mut scripted, |s| {
            (s.engineer.line.front()).is_some_and(|playback| playback.started)
        });
        assert!(scripted.engineer.is_speaking());

        scripted.engineer.talk(true);
        assert!(scripted.engineer.line[0].held);
        scripted.new_game_session();
        assert_eq!(scripted.answers_waiting().len(), 1);
        scripted.engineer.talk(false);
        assert!(!scripted.engineer.line[0].held);
        let played = no_speaker(&scripted.engineer);
        play_until(&mut scripted, |s| s.answers_waiting().is_empty());
        assert!(scripted.engineer.unheard.is_empty());
        assert_eq!(no_speaker(&scripted.engineer), played);
        assert_eq!(scripted.engineer.schedule().waiting(RequestId(1)), 0);
        assert!(scripted.engineer.schedule().conditions().is_empty());
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

    /// A scripted engineer that acknowledges slow answers after the real delay.
    fn acknowledging() -> Scripted {
        let mut scripted = Scripted::new();
        scripted.engineer.acknowledge_after =
            Some(ACKNOWLEDGE_AFTER.saturating_sub(ACKNOWLEDGE_LEAD));
        scripted
    }

    /// A scripted engineer whose acknowledgement fires without waiting, for non-timing tests.
    fn acknowledging_immediately() -> Scripted {
        let mut scripted = Scripted::new();
        scripted.engineer.acknowledge_after = Some(Duration::ZERO);
        scripted
    }

    /// Waits for the app's acknowledgement in the chat.
    fn until_acknowledged(scripted: &mut Scripted) {
        scripted.until("acknowledged", |s| {
            s.answer().is_some_and(|e| e.text.starts_with("Copiado, lo miro.\n"))
        });
    }

    #[test]
    fn acknowledges_a_slow_answer() {
        let mut scripted = acknowledging();
        let heard = Instant::now();
        // Slow transcription before `hear` must not make the acknowledgement fire immediately
        scripted.hear_ago("¿Cómo van los neumáticos?", ACKNOWLEDGE_AFTER);
        until_acknowledged(&mut scripted);
        assert!(heard.elapsed() > ACKNOWLEDGE_AFTER / 2, "{:?}", heard.elapsed());
        scripted.acts.send(Act::Write("Delanteros bien.")).unwrap();
        scripted.finish(Act::End(StopReason::EndTurn, "Delanteros bien."));
        assert_eq!(scripted.answer().unwrap().text, "Copiado, lo miro.\nDelanteros bien.");
    }

    #[test]
    fn acknowledges_while_the_question_waits_its_turn() {
        let mut scripted = acknowledging_immediately();
        let state = scripted.engineer.session.as_ref().unwrap().state.clone();
        // The conversation is busy: the question can't be briefed, let alone asked
        let busy = scripted.runtime.block_on(state.lock_owned());
        scripted.hear_ago("¿Cómo van los neumáticos?", Duration::ZERO);
        until_acknowledged(&mut scripted);
        assert!(scripted.questions().is_empty());
        drop(busy);
        scripted.acts.send(Act::Write("Delanteros bien.")).unwrap();
        scripted.finish(Act::End(StopReason::EndTurn, "Delanteros bien."));
        assert_eq!(scripted.answer().unwrap().text, "Copiado, lo miro.\nDelanteros bien.");
    }

    #[test]
    fn a_quick_answer_is_not_acknowledged() {
        let mut scripted = acknowledging();
        // Even if transcription took `ACKNOWLEDGE_AFTER`, a quick model reply beats the timer
        scripted.hear_ago("¿Cómo van los neumáticos?", ACKNOWLEDGE_AFTER);
        scripted.acts.send(Act::Write("Delanteros bien.")).unwrap();
        scripted.finish(Act::End(StopReason::EndTurn, "Delanteros bien."));
        assert_eq!(scripted.answer().unwrap().text, "Delanteros bien.");
    }

    #[test]
    fn drops_the_models_late_acknowledgement() {
        let mut scripted = acknowledging_immediately();
        scripted.hear_ago("¿Cómo van los neumáticos?", Duration::ZERO);
        until_acknowledged(&mut scripted);
        let acts = [
            Act::Write("Copio, miro los neumáticos.\n"),
            Act::Tools(&[EngineerTool::NAME]),
            Act::Write("Delanteros bien."),
        ];
        for act in acts {
            scripted.acts.send(act).unwrap();
        }
        scripted.finish(Act::End(
            StopReason::EndTurn,
            "Copio, miro los neumáticos.\n\nDelanteros bien.",
        ));
        assert_eq!(scripted.answer().unwrap().text, "Copiado, lo miro.\nDelanteros bien.");
    }

    #[test]
    fn keeps_an_answer_written_before_a_schedule_or_pit_call() {
        // What came before tools that only act answered; before any query, it acknowledged
        let groups: [(&'static [&'static str], &str); 4] = [
            (&[ScheduleTool::NAME], "Copiado, lo miro.\nDelanteros bien.\nHecho."),
            (&[PitTool::NAME], "Copiado, lo miro.\nDelanteros bien.\nHecho."),
            (&[PitTool::NAME, ScheduleTool::NAME], "Copiado, lo miro.\nDelanteros bien.\nHecho."),
            (&[ScheduleTool::NAME, EngineerTool::NAME], "Copiado, lo miro.\nHecho."),
        ];
        for (tools, heard) in groups {
            let mut scripted = acknowledging_immediately();
            scripted.hear_ago("¿Delanteros? Y avísame al final de la vuelta.", Duration::ZERO);
            until_acknowledged(&mut scripted);
            let acts = [Act::Write("Delanteros bien.\n"), Act::Tools(tools), Act::Write("Hecho.")];
            for act in acts {
                scripted.acts.send(act).unwrap();
            }
            scripted.finish(Act::End(StopReason::EndTurn, "Delanteros bien.\n\nHecho."));
            assert_eq!(scripted.answer().unwrap().text, heard, "{tools:?}");
        }
    }

    #[test]
    fn acknowledges_a_typed_question_from_when_its_asked() {
        let mut scripted = acknowledging();
        let asked = Instant::now();
        scripted.ask("How are the tyres?");
        scripted.until("acknowledged", |s| s.answer().is_some());
        assert!(asked.elapsed() > ACKNOWLEDGE_AFTER / 2, "{:?}", asked.elapsed());
        scripted.acts.send(Act::Write("Fronts are fine.")).unwrap();
        scripted.finish(Act::End(StopReason::EndTurn, "Fronts are fine."));
        let answer = &scripted.answer().unwrap().text;
        assert!(answer.ends_with("\nFronts are fine."), "{answer}");
    }

    #[test]
    fn an_acknowledged_turn_that_says_nothing_shows_the_acknowledgement() {
        // Its tone: see `incomplete_answers_play_a_tone_without_a_voice`
        let mut scripted = acknowledging_immediately();
        scripted.hear_ago("¿Cómo van los neumáticos?", Duration::ZERO);
        until_acknowledged(&mut scripted);
        scripted.finish(Act::End(StopReason::EndTurn, ""));
        assert_eq!(scripted.answer().unwrap().text, "Copiado, lo miro.\n");
    }

    #[test]
    fn a_failed_turn_keeps_what_it_held() {
        let mut scripted = acknowledging_immediately();
        scripted.hear_ago("¿Cómo van los neumáticos?", Duration::ZERO);
        until_acknowledged(&mut scripted);
        scripted.acts.send(Act::Write("Los delanteros")).unwrap();
        scripted.finish(Act::Fail);
        let answer = scripted.answer().unwrap();
        assert_eq!(answer.text, "Copiado, lo miro.\nLos delanteros");
        assert_eq!(answer.detail.as_deref(), Some(t!("engineer.detail_cut_off")));
    }

    #[test]
    fn a_steering_question_is_not_acknowledged_again() {
        let mut scripted = acknowledging_immediately();
        scripted.hear_ago("¿Cómo van los neumáticos?", Duration::ZERO);
        until_acknowledged(&mut scripted);
        scripted.hear_ago("¿Y los traseros?", Duration::ZERO);
        // The running turn takes it, and answers both
        scripted.acts.send(Act::TakeSteer).unwrap();
        scripted.acts.send(Act::Write("Todos bien.")).unwrap();
        scripted.finish(Act::End(StopReason::EndTurn, "Todos bien."));
        let acknowledged = (scripted.engineer.chat.iter())
            .filter(|e| e.text.contains("Copiado, lo miro."))
            .count();
        assert_eq!(acknowledged, 1);
        assert_eq!(scripted.answer().unwrap().text, "Copiado, lo miro.\nTodos bien.");
    }

    #[test]
    fn a_missed_question_waits_its_own_threshold() {
        let mut scripted = acknowledging_immediately();
        scripted.hear_ago("¿Cómo van los neumáticos?", Duration::ZERO);
        until_acknowledged(&mut scripted);
        scripted.hear_ago("¿Y los traseros?", ACKNOWLEDGE_AFTER);
        // The first turn ends without taking the steer: it's asked next, with the real delay
        scripted.engineer.acknowledge_after =
            Some(ACKNOWLEDGE_AFTER.saturating_sub(ACKNOWLEDGE_LEAD));
        let asked = Instant::now();
        scripted.acts.send(Act::Write("Delanteros bien.")).unwrap();
        scripted.acts.send(Act::End(StopReason::EndTurn, "Delanteros bien.")).unwrap();
        scripted.until("asked again", |s| s.questions().len() == 2);
        let answers = |s: &Scripted| {
            (s.engineer.chat.iter())
                .filter(|e| e.speaker == Speaker::Engineer)
                .count()
        };
        scripted.until("acknowledged again", |s| answers(s) == 2);
        // Not straight away, from the first question's release
        assert!(asked.elapsed() > ACKNOWLEDGE_AFTER / 2, "{:?}", asked.elapsed());
        assert_eq!(scripted.answer().unwrap().text, "Copiado, lo miro.\n");
        scripted.acts.send(Act::Write("Traseros bien.")).unwrap();
        scripted.finish(Act::End(StopReason::EndTurn, "Traseros bien."));
        assert_eq!(scripted.answer().unwrap().text, "Copiado, lo miro.\nTraseros bien.");
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

        /// Asks until the conversation is told about `key`, in another weekend than `old`.
        async fn follow_until(
            state: &mut SessionState,
            recorded: &std::sync::Mutex<Recorded>,
            auto_session: bool,
            key: &str,
            old: Option<i64>,
        ) {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
            loop {
                state.follow_game_session(auto_session).await;
                let told = recorded
                    .lock()
                    .unwrap()
                    .games
                    .last()
                    .map(|game| game.key.clone());
                if told.as_deref() == Some(key) && state.weekend.is_some() && state.weekend != old {
                    return;
                }
                assert!(std::time::Instant::now() < deadline, "{key} not recorded");
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            }
        }

        for auto_session in [true, false] {
            let dir = tempfile::tempdir().unwrap();
            let recorder = Recorder::new(dir.path(), None).unwrap();
            recorder.push_scoring(session("practice", 1), Vec::new());
            // Written by the recorder's thread, soon
            let path = dir.path().join(DATABASE);
            let one = NonZeroUsize::new(1).unwrap();
            let pool = Pool::new(Database::read_only(Arc::new(path)), one, one);

            let recorded = Arc::new(std::sync::Mutex::new(Recorded::default()));
            let tool = EngineerTool::new(pool.clone());
            let mut state = SessionState {
                conversation: Box::new(RecordingModel(recorded.clone())),
                schedule_tool: ScheduleTool::new(tool.clone()),
                pit_tool: None,
                tool,
                pool,
                sim: "lmu",
                weekend: None,
                starts: Arc::default(),
                setup_outline: None,
                forgotten: 0,
                outline_offered: None,
            };

            let practice = "Circuit de la Sarthe|practice|1";
            follow_until(&mut state, &recorded, auto_session, practice, None).await;
            let weekend = state.weekend;
            // Names already shown in this conversation stay shown
            state.setup_outline = Some(7);
            state.follow_game_session(auto_session).await;
            assert_eq!(state.setup_outline, Some(7));
            state.notice_forgotten();
            assert_eq!(state.setup_outline, Some(7));
            // The conversation forgot earlier messages on its own (trimmed to fit): shown again
            recorded.lock().unwrap().forgotten += 1;
            state.notice_forgotten();
            assert_eq!(state.setup_outline, None);
            state.setup_outline = Some(7);
            state.notice_forgotten();
            assert_eq!(state.setup_outline, Some(7));
            // Told every turn, so a lease ended meanwhile is started again
            let told = recorded.lock().unwrap().games.len();
            state.follow_game_session(auto_session).await;
            {
                let recorded = recorded.lock().unwrap();
                let games = &recorded.games;
                assert_eq!(games.len(), told + 1);
                assert_eq!(games[told - 1], games[told]);
                assert_eq!(games[told].key, practice);
                assert_eq!(games[told].sim, "lmu");
                assert_eq!(games[told].session_type, SessionType::Practice);
                // Not scheduled: the type's default
                assert_eq!(games[told].expected_minutes, 60);
            }

            // The race of the same weekend: another game session, the same conversation
            let mut race = session("race", 1);
            race.session_end_time = Some(5400.0).into();
            recorder.push_scoring(race, Vec::new());
            let race = "Circuit de la Sarthe|race|1";
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
            while recorded.lock().unwrap().games.last().unwrap().key != race {
                assert!(std::time::Instant::now() < deadline, "race not recorded");
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
                state.follow_game_session(auto_session).await;
            }
            assert_eq!(state.weekend, weekend);
            {
                let recorded = recorded.lock().unwrap();
                let game = recorded.games.last().unwrap();
                assert_eq!(game.session_type, SessionType::Race);
                assert_eq!(game.expected_minutes, 90);
                assert_eq!(recorded.resets, 0);
            }

            // A new weekend clears the recording, even back at the same track and session
            recorder.clear();
            // Noticed before any scoring comes in (the game paused, say)
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
            while state.weekend == weekend {
                assert!(std::time::Instant::now() < deadline, "clear not recorded");
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
                state.follow_game_session(auto_session).await;
            }
            assert_eq!(recorded.lock().unwrap().resets, usize::from(auto_session));
            // A conversation started over shows the names again
            assert_eq!(state.setup_outline.is_none(), auto_session);
            let cleared = state.weekend;
            recorder.push_scoring(session("practice", 1), Vec::new());
            follow_until(&mut state, &recorded, auto_session, practice, weekend).await;
            assert_eq!(state.weekend, cleared);
            drop(recorder);
            state.follow_game_session(auto_session).await;

            // Only auto sessions start the conversation over, once
            assert_eq!(recorded.lock().unwrap().resets, usize::from(auto_session));
        }
    }
}
