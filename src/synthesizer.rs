//! Text to speech with [Piper](https://github.com/rhasspy/piper) voices.
//!
//! A voice is an `.onnx` model plus its `.onnx.json` config, and synthesis goes:
//! 1. text → IPA phonemes, with espeak-ng in a separate program (see [`phonemizer`])
//! 2. phonemes → IDs, with the config's `phoneme_id_map`
//! 3. IDs → PCM, by running the model with ONNX Runtime
//!
//! Each sentence is synthesized separately, like Piper does.

use crate::{
    config::{ArrayStr, RawConfig},
    error,
    synthesizer::{
        phonemizer::phonemize,
        radio::{Radio, RadioStream},
    },
};
use anyhow::{Context, bail};
use bono_i18n::t;
use camino::{Utf8Path, Utf8PathBuf};
use ort::{
    environment::Environment,
    session::{Session, builder::AutoDevicePolicy},
    value::Tensor,
};
use sdl3::audio::{AudioFormat, AudioSpec, AudioStream, AudioStreamOwner};
use serde::Deserialize;
use std::{
    collections::{BTreeMap, HashMap, VecDeque},
    mem::ManuallyDrop,
    num::NonZeroUsize,
    sync::{Arc, Mutex, MutexGuard, OnceLock, PoisonError},
    task::Poll,
    time::{Duration, Instant},
};
use tokio::{
    runtime::Handle,
    sync::{
        mpsc::{
            self,
            error::{SendError, TryRecvError},
        },
        oneshot, watch,
    },
};
use unicode_normalization::UnicodeNormalization;

pub mod phonemizer;
mod radio;

const PAD: char = '_';
const BOS: char = '^';
const EOS: char = '$';

/// Silence between sentences, in seconds (Piper's default)
const SENTENCE_SILENCE: f32 = 0.2;

/// Silence between the end of the voice and the roger beep, in seconds
const BEEP_GAP: f32 = 0.1;

/// The slowest [`Effects::speed`]: slower, the voices drag
pub const MIN_SPEED: f32 = 0.5;
/// The fastest [`Effects::speed`]: faster, the voices slur
pub const MAX_SPEED: f32 = 2.0;

pub struct Synthesizer {
    pub(crate) inner: BTreeMap<ArrayStr<2>, SynthesizerInner>,
}

impl Synthesizer {
    pub fn new(rt: &Handle, config: &RawConfig) -> Self {
        let mut inner = BTreeMap::new();
        for (&k, v) in config.voices() {
            let synth = SynthesizerInner::new(rt.clone());
            synth.set_model(Some(v.clone()), *config.voice_gpu());
            inner.insert(k, synth);
        }

        Self { inner }
    }

    pub fn synthesize(
        &mut self,
        rt: &Handle,
        lang: ArrayStr<2>,
        text: String,
        effects: Effects,
    ) -> oneshot::Receiver<anyhow::Result<Speech>> {
        let (output, recv) = oneshot::channel();
        self.inner(rt, lang).send(Job::Speech {
            text,
            effects,
            output,
        });
        return recv;
    }

    /// [`Self::synthesize`]s `text` in one piece, as a [`SpeechOutput`] that ends after it: the
    /// worker takes it between a stream's sentences, so it never waits behind a reply still being
    /// written. Dropping the output before it's synthesized skips the synthesis.
    pub fn synthesize_output(
        &mut self,
        rt: &Handle,
        lang: ArrayStr<2>,
        text: String,
        effects: Effects,
    ) -> SpeechOutput {
        let speech = self.synthesize(rt, lang, text, effects);
        let (output, recv) = mpsc::unbounded_channel();
        rt.spawn(async move {
            tokio::select! {
                // Nobody's listening: the worker sees its receiver dropped, and skips the synthesis
                // if it hasn't started it
                () = output.closed() => {}
                // Without a sender, the worker stopped: the output ends with nothing
                speech = speech => if let Ok(speech) = speech {
                    let _ = output.send(speech);
                },
            }
        });
        return SpeechOutput(recv);
    }

    /// Speaks text as it comes in, a sentence at a time: each one is synthesized as soon as
    /// it's complete, and comes out of the [`SpeechOutput`] while the next is still being
    /// written. The transmission ends when the [`SpeechInput`] is dropped, and stops early when
    /// the [`SpeechOutput`] is.
    pub fn stream(
        &mut self,
        rt: &Handle,
        lang: ArrayStr<2>,
        effects: Effects,
    ) -> (SpeechInput, SpeechOutput) {
        let (input, text) = mpsc::unbounded_channel();
        let (output, recv) = mpsc::unbounded_channel();
        self.inner(rt, lang).send(Job::Stream {
            text,
            effects,
            output,
        });
        return (SpeechInput(input), SpeechOutput(recv));
    }

    /// Reloads every voice to run on the GPU if `gpu`, or on the processor.
    pub fn set_gpu(&self, gpu: bool) {
        for inner in self.inner.values() {
            inner.set_gpu(gpu);
        }
    }

    /// How the voice for `lang` loaded: `None` if none was set.
    pub fn load(&self, lang: ArrayStr<2>) -> Option<VoiceLoad> {
        self.inner.get(&lang)?.load()
    }

    /// Whether `model`, the voice picked for `lang`, failed to load: picking it again retries.
    pub fn failed(&self, lang: ArrayStr<2>, model: &Utf8Path) -> bool {
        self.inner
            .get(&lang)
            .is_some_and(|inner| inner.failed(model))
    }

    fn inner(&mut self, rt: &Handle, lang: ArrayStr<2>) -> &SynthesizerInner {
        self.inner
            .entry(lang)
            .or_insert_with(|| SynthesizerInner::new(rt.clone()))
    }
}

/// The text of a [`Synthesizer::stream`]: dropping it ends the transmission.
pub struct SpeechInput(mpsc::UnboundedSender<String>);

impl SpeechInput {
    #[inline]
    pub fn push(&self, text: impl Into<String>) {
        // Nobody's listening any more: nothing to do
        let _ = self.0.send(text.into());
    }
}

/// The speech of a [`Synthesizer::stream`], a sentence or so at a time (or of a
/// [`Synthesizer::synthesize_output`], in one piece): dropping it stops the synthesis.
pub struct SpeechOutput(mpsc::UnboundedReceiver<anyhow::Result<Speech>>);

impl SpeechOutput {
    /// Just `speech`, already synthesized.
    pub fn ready(speech: Speech) -> Self {
        let (output, recv) = mpsc::unbounded_channel();
        let _ = output.send(Ok(speech));
        Self(recv)
    }

    /// An output fed by hand, as a synthesizer would: it ends once the sender is dropped.
    #[cfg(test)]
    pub fn channel() -> (mpsc::UnboundedSender<anyhow::Result<Speech>>, Self) {
        let (output, recv) = mpsc::unbounded_channel();
        (output, Self(recv))
    }

    /// The next piece of speech if it's ready, or `Ready(None)` once the transmission is over
    /// (an error ends it too).
    pub fn try_next(&mut self) -> Poll<Option<anyhow::Result<Speech>>> {
        match self.0.try_recv() {
            Ok(speech) => Poll::Ready(Some(speech)),
            Err(TryRecvError::Empty) => Poll::Pending,
            Err(TryRecvError::Disconnected) => Poll::Ready(None),
        }
    }

    /// Whether nothing is ready to [take](Self::try_next): no piece of speech or error, though
    /// the transmission may be over.
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

enum Job {
    Speech {
        text: String,
        effects: Effects,
        output: oneshot::Sender<anyhow::Result<Speech>>,
    },
    Stream {
        text: mpsc::UnboundedReceiver<String>,
        effects: Effects,
        output: mpsc::UnboundedSender<anyhow::Result<Speech>>,
    },
}

impl Job {
    fn fail(self, error: anyhow::Error) {
        match self {
            Job::Speech { output, .. } => {
                let _ = output.send(Err(error));
            }
            Job::Stream { output, .. } => {
                let _ = output.send(Err(error));
            }
        }
    }
}

/// A voice's model, and whether it runs on the GPU.
#[derive(Clone)]
struct Model {
    path: Utf8PathBuf,
    gpu: bool,
}

/// How the latest voice given to a [`SynthesizerInner`] loaded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VoiceLoad {
    Loading,
    Loaded,
    /// It couldn't be loaded: the voice before it (if any) is still the one speaking
    Failed,
}

/// The voices given to a [`SynthesizerInner`], counted, and how the latest loaded.
#[derive(Default)]
struct LoadStatus {
    requested: u64,
    /// The latest voice given
    model: Option<Utf8PathBuf>,
    /// `None` until a voice is given
    state: Option<VoiceLoad>,
}

pub struct SynthesizerInner {
    handle: ManuallyDrop<std::thread::JoinHandle<()>>,
    /// The voice to load, with its number in [`LoadStatus::requested`]
    model: ManuallyDrop<watch::Sender<Option<(u64, Model)>>>,
    jobs: ManuallyDrop<mpsc::UnboundedSender<Job>>,
    load: Arc<Mutex<LoadStatus>>,
}

impl SynthesizerInner {
    pub fn new(rt: Handle) -> Self {
        let threads = std::thread::available_parallelism().unwrap_or(NonZeroUsize::MIN);
        let (model_send, model_recv) = watch::channel(None);
        let (jobs_send, jobs_recv) = mpsc::unbounded_channel();
        let load = Arc::new(Mutex::new(LoadStatus::default()));
        let handle = {
            let load = load.clone();
            std::thread::spawn(move || Self::worker(rt, model_recv, jobs_recv, load, threads))
        };

        Self {
            handle: ManuallyDrop::new(handle),
            model: ManuallyDrop::new(model_send),
            jobs: ManuallyDrop::new(jobs_send),
            load,
        }
    }

    /// Loads `model` (again, if it's the one already set), to run on the GPU if `gpu`, or with
    /// `None`, stops.
    pub fn set_model(&self, model: Option<Utf8PathBuf>, gpu: bool) {
        // Sent while locked, so the latest request is the one the worker gets last
        let mut load = lock(&self.load);
        load.requested += 1;
        let loading = model.is_some();
        load.state = loading.then_some(VoiceLoad::Loading);
        load.model = model.clone();
        let sent = (self.model).send(model.map(|path| (load.requested, Model { path, gpu })));
        // The worker stopped, so nothing will load it
        if sent.is_err() && loading {
            load.state = Some(VoiceLoad::Failed);
        }
    }

    /// How the latest voice set loaded: `None` if none was.
    pub fn load(&self) -> Option<VoiceLoad> {
        lock(&self.load).state
    }

    /// Whether `model` is the voice that last failed to load. One still loading hasn't failed.
    pub fn failed(&self, model: &Utf8Path) -> bool {
        let load = lock(&self.load);
        load.state == Some(VoiceLoad::Failed) && load.model.as_deref() == Some(model)
    }

    /// Reloads the voice to run on the GPU if `gpu`, if there's one and it isn't already there.
    /// The reload counts as the latest request, as one from [`Self::set_model`] does.
    fn set_gpu(&self, gpu: bool) {
        // Sent while locked, as `set_model` does
        let mut load = lock(&self.load);
        let requested = load.requested + 1;
        let reloads = self.model.send_if_modified(|model| match model {
            Some((number, model)) if model.gpu != gpu => {
                *number = requested;
                model.gpu = gpu;
                true
            }
            _ => false,
        });
        if reloads {
            load.requested = requested;
            // The worker stopped, so nothing will load it
            load.state = Some(match self.model.is_closed() {
                true => VoiceLoad::Failed,
                false => VoiceLoad::Loading,
            });
        }
    }

    /// Whether its worker stopped, so it can't load or speak anything.
    pub fn stopped(&self) -> bool {
        self.model.is_closed()
    }

    fn send(&self, job: Job) {
        if let Err(SendError(job)) = self.jobs.send(job) {
            job.fail(anyhow::Error::msg("channel closed"));
        }
    }

    fn worker(
        handle: Handle,
        model: watch::Receiver<Option<(u64, Model)>>,
        mut jobs: mpsc::UnboundedReceiver<Job>,
        load: Arc<Mutex<LoadStatus>>,
        threads: NonZeroUsize,
    ) {
        let mut voice = None::<Voice>;
        // The model `voice` was loaded from
        let mut loaded = None::<Utf8PathBuf>;
        // The transmission being spoken: one at a time, the others wait their turn
        let mut stream = None::<Stream>;
        let mut waiting = VecDeque::<Job>::new();
        // After the voice, so it's dropped first: requests stop before the voice is freed
        let mut stopping = Stopping {
            load: load.clone(),
            model: Some(model),
        };
        let model = stopping.model.as_mut().expect("just set");

        enum Event {
            Model,
            Job(Job),
            Text(Option<String>),
            Stopped,
        }

        loop {
            // Check jobs between sentences, so one-shot calls can interrupt a long reply.
            // Keep model changes ahead of jobs sent after them.
            let job = if !model.has_changed().unwrap_or(true) {
                jobs.try_recv().ok()
            } else {
                None
            };
            let event = if let Some(job) = job {
                Some(Event::Job(job))
            } else {
                match &mut stream {
                    Some(active) => match active.step(voice.as_mut()) {
                        Step::Spoke => continue,
                        Step::Done => {
                            stream = None;
                            continue;
                        }
                        Step::Waiting => {}
                    },
                    None => {
                        if let Some(job) = waiting.pop_front() {
                            stream = Stream::start(job, voice.as_ref());
                            continue;
                        }
                    }
                }

                // Waits for a voice, a job, or the transmission's text (or whoever plays it leaving)
                let (text, output) = match &mut stream {
                    Some(Stream { text, output, .. }) => (Some(text), Some(&*output)),
                    None => (None, None),
                };
                handle.block_on(async {
                    tokio::select! {
                        // A voice first, for the jobs sent after it
                        biased;
                        res = model.changed() => res.ok().map(|()| Event::Model),
                        job = jobs.recv() => job.map(Event::Job),
                        text = async {
                            match text {
                                Some(text) => text.recv().await,
                                None => std::future::pending().await,
                            }
                        } => Some(Event::Text(text)),
                        () = async {
                            match output {
                                Some(output) => output.closed().await,
                                None => std::future::pending().await,
                            }
                        } => Some(Event::Stopped),
                    }
                })
            };

            match event {
                Some(Event::Model) => {
                    // Cloned so the load doesn't hold the read lock `set_model` waits on
                    let next = model.borrow_and_update().clone();
                    let Some((requested, Model { path, gpu })) = next else {
                        return;
                    };
                    let state = match Voice::load(&path, threads, gpu) {
                        Ok(next_voice) => {
                            // Its speech so far was in the old voice. The same voice moving to or
                            // from the GPU carries on with it, and so does the old voice when the
                            // new one doesn't load
                            if loaded.as_ref() != Some(&path) {
                                stream = None;
                            }
                            voice = Some(next_voice);
                            loaded = Some(path);
                            VoiceLoad::Loaded
                        }
                        Err(e) => {
                            error(t!("error.tts_model"), e);
                            VoiceLoad::Failed
                        }
                    };
                    // A voice set during the load is the one shown
                    let mut load = lock(&load);
                    if load.requested == requested {
                        load.state = Some(state);
                    }
                }
                Some(Event::Job(Job::Speech {
                    text,
                    effects,
                    output,
                })) => {
                    if let Some(ref mut voice) = voice {
                        if !output.is_closed() {
                            let _ = output.send(voice.synthesize(&text, effects));
                        }
                    } else {
                        let _ = output.send(Err(no_model()));
                    };
                }
                Some(Event::Job(job @ Job::Stream { .. })) => waiting.push_back(job),
                Some(Event::Text(text)) => {
                    if let Some(active) = &mut stream {
                        match text {
                            Some(text) => active.pending.push_str(&text),
                            None => active.ended = true,
                        }
                    }
                }
                Some(Event::Stopped) => stream = None,
                None => return,
            }
        }
    }
}

/// The worker's voice requests, closed when it stops (or panics): a voice it didn't load, or any
/// after a panic, is marked failed, since nothing will load or speak it.
struct Stopping {
    load: Arc<Mutex<LoadStatus>>,
    model: Option<watch::Receiver<Option<(u64, Model)>>>,
}

impl Drop for Stopping {
    fn drop(&mut self) {
        // Closed while locked, as `set_model` sends while locked: a request is either seen here
        // or fails to send
        let mut load = lock(&self.load);
        drop(self.model.take());
        let failed = match load.state {
            Some(VoiceLoad::Loading) => true,
            Some(VoiceLoad::Loaded) => std::thread::panicking(),
            Some(VoiceLoad::Failed) | None => false,
        };
        if failed {
            load.state = Some(VoiceLoad::Failed);
        }
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

fn no_model() -> anyhow::Error {
    anyhow::Error::msg("No text to speech model loaded yet")
}

/// A [`Synthesizer::stream`] being spoken.
struct Stream {
    transmission: Transmission,
    text: mpsc::UnboundedReceiver<String>,
    output: mpsc::UnboundedSender<anyhow::Result<Speech>>,
    /// Text that came in and hasn't been said: an unfinished sentence
    pending: String,
    /// All the text came in
    ended: bool,
}

enum Step {
    Spoke,
    Waiting,
    Done,
}

impl Stream {
    fn start(job: Job, voice: Option<&Voice>) -> Option<Self> {
        let Job::Stream {
            text,
            effects,
            output,
        } = job
        else {
            unreachable!("only streams wait their turn");
        };

        let Some(voice) = voice else {
            let _ = output.send(Err(no_model()));
            return None;
        };

        Some(Self {
            transmission: voice.transmission(effects),
            text,
            output,
            pending: String::new(),
            ended: false,
        })
    }

    /// Says the next complete sentence, or the end of the transmission once its text is over.
    fn step(&mut self, voice: Option<&mut Voice>) -> Step {
        if self.output.is_closed() {
            return Step::Done;
        }

        loop {
            match self.text.try_recv() {
                Ok(text) => self.pending.push_str(&text),
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    self.ended = true;
                    break;
                }
            }
        }

        let Some(voice) = voice else {
            let _ = self.output.send(Err(no_model()));
            return Step::Done;
        };

        // A sentence at a time even once all the text is in, so the first is heard sooner
        let len = match sentence_len(&self.pending) {
            Some(len) => len,
            None if self.ended => self.pending.len(),
            None => 0,
        };

        let (pieces, last) = match len {
            0 if self.ended => (Ok(vec![voice.end(&mut self.transmission)]), true),
            0 => return Step::Waiting,
            len => {
                let sentence = self.pending.drain(..len).collect::<String>();
                (voice.speak(&mut self.transmission, &sentence), false)
            }
        };

        let pieces = match pieces {
            Ok(pieces) => pieces,
            Err(e) => {
                let _ = self.output.send(Err(e));
                return Step::Done;
            }
        };

        for samples in pieces {
            if samples.is_empty() {
                continue;
            }
            let speech = Speech {
                samples,
                sample_rate: voice.sample_rate(),
            };
            if self.output.send(Ok(speech)).is_err() {
                return Step::Done;
            }
        }

        return match last {
            true => Step::Done,
            false => Step::Spoke,
        };
    }
}

/// How much of `text` to say now: up to the end of its first sentence (its punctuation, then a
/// space) or line. `None` while the first sentence is still being written.
fn sentence_len(text: &str) -> Option<usize> {
    let mut chars = text.char_indices().peekable();
    while let Some((i, c)) = chars.next() {
        let mut end = i + c.len_utf8();
        match c {
            // No space after these
            '\n' | '。' | '？' | '！' => return Some(end),
            '.' | '?' | '!' | '…' => {
                // Past closing quotes and brackets
                while let Some(&(j, q)) = chars.peek()
                    && matches!(q, '"' | '\'' | ')' | ']' | '”' | '’' | '»')
                {
                    end = j + q.len_utf8();
                    chars.next();
                }

                // Not "3.5", nor a sentence that may still be going
                if chars.peek().is_some_and(|&(_, next)| next.is_whitespace()) {
                    return Some(end);
                }
            }
            _ => {}
        }
    }

    return None;
}

impl Drop for SynthesizerInner {
    fn drop(&mut self) {
        let handle = unsafe { ManuallyDrop::take(&mut self.handle) };
        unsafe { ManuallyDrop::drop(&mut self.model) };
        unsafe { ManuallyDrop::drop(&mut self.jobs) };
        if tokio::task::block_in_place(|| handle.join()).is_err() {
            log::error!("The speech synthesis thread panicked");
        }
    }
}

/// How a transmission sounds.
#[derive(Debug, Clone, Copy)]
pub struct Effects {
    /// How strong the radio effect is, from 0 (clean voice, no squelch) to 1
    pub radio: f32,
    /// Volume of the roger beep (0 for none)
    pub beep_volume: f32,
    /// How fast it's said: 1 is the voice's own pace. Kept within [`MIN_SPEED`] and
    /// [`MAX_SPEED`]
    pub speed: f32,
}

/// A loaded Piper voice.
pub struct Voice {
    session: Session,
    /// While it runs on the GPU: its model and threads, to move it to the processor if the GPU
    /// stops working
    gpu: Option<(Utf8PathBuf, NonZeroUsize)>,
    sample_rate: u32,
    espeak_voice: String,
    phoneme_ids: BTreeMap<char, Vec<i64>>,
    scales: [f32; 3],
    /// Multi-speaker models take a speaker ID; we always use the first speaker
    multi_speaker: bool,
    radio: Radio,
    beep: Vec<f32>,
}

/// The ONNX Runtime environment every voice's session is created in.
///
/// ONNX Runtime allows one environment per process, and `ort` refuses to build another even after the first is
/// dropped, so it's built once and kept. A failed build is retried by the next voice.
fn environment() -> ort::Result<&'static Environment> {
    static ENVIRONMENT: OnceLock<Environment> = OnceLock::new();
    static BUILDING: Mutex<()> = Mutex::new(());

    if let Some(environment) = ENVIRONMENT.get() {
        return Ok(environment);
    }
    let _building = BUILDING.lock().unwrap_or_else(PoisonError::into_inner);
    if let Some(environment) = ENVIRONMENT.get() {
        return Ok(environment);
    }
    let environment = ort::init().build()?;
    Ok(ENVIRONMENT.get_or_init(|| environment))
}

impl Voice {
    /// Loads the model at `model` and its config at `{model}.json`, using at most 8 intra-op threads.
    /// With `gpu`, it runs on the GPU with DirectML on Windows, or on the processor if that fails.
    ///
    /// This blocks while the model is read and compiled.
    pub fn load(model: &Utf8Path, threads: NonZeroUsize, gpu: bool) -> anyhow::Result<Self> {
        let config_path = model.with_added_extension("json");
        let config = std::fs::read(&config_path)
            .with_context(|| format!("Error reading voice config '{config_path}'"))?;
        let config = serde_json::from_slice::<VoiceConfig>(&config)
            .with_context(|| format!("Invalid voice config '{config_path}'"))?;

        if config.phoneme_type != "espeak" {
            bail!(
                "Unsupported phoneme type '{}' (only 'espeak' is supported)",
                config.phoneme_type
            );
        }

        let mut phoneme_ids = BTreeMap::new();
        for (phoneme, ids) in config.phoneme_id_map {
            let mut chars = phoneme.chars();
            match (chars.next(), chars.next()) {
                (Some(c), None) => {
                    phoneme_ids.insert(c, ids);
                }
                _ => log::warn!("Ignoring multi-character phoneme '{phoneme}' in '{config_path}'"),
            }
        }

        for c in [PAD, BOS, EOS] {
            if !phoneme_ids.contains_key(&c) {
                bail!("Voice config '{config_path}' has no ID for '{c}'");
            }
        }

        // Starts the phonemizer, if it isn't yet, and checks it knows the voice
        phonemize("", &config.espeak.voice)?;

        let context = || format!("Error loading voice model '{model}'");
        let gpu = gpu && cfg!(windows);
        let (session, gpu) = match Self::session(model, threads, gpu) {
            Ok(session) => (session, gpu),
            Err(e) if gpu => {
                log::warn!("Couldn't run voice model '{model}' on the GPU, so it runs on the processor: {e}");
                let session = Self::session(model, threads, false).with_context(context)?;
                (session, false)
            }
            Err(e) => return Err(e).with_context(context),
        };

        log::info!("Loaded voice model '{model}' (GPU: {gpu})");
        let multi_speaker = session.inputs().iter().any(|input| input.name() == "sid");
        let inference = config.inference;

        let radio = Radio::default();
        let beep = radio.beep(config.audio.sample_rate as f32);

        return Ok(Self {
            session,
            gpu: gpu.then(|| (model.to_owned(), threads)),
            sample_rate: config.audio.sample_rate,
            espeak_voice: config.espeak.voice,
            phoneme_ids,
            scales: [
                inference.noise_scale,
                inference.length_scale,
                inference.noise_w,
            ],
            multi_speaker,
            radio,
            beep,
        });
    }

    #[inline]
    pub fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    /// Synthesizes `text` into mono PCM at [`Self::sample_rate`], as a radio transmission:
    /// the squelch opens, the voice plays through the radio effect, then the roger beep (mixed
    /// in clean) and the squelch closes.
    pub fn synthesize(&mut self, text: &str, effects: Effects) -> anyhow::Result<Speech> {
        let mut transmission = self.transmission(effects);
        let mut samples = self.speak(&mut transmission, text)?.concat();
        samples.extend(self.end(&mut transmission));
        return Ok(Speech {
            samples,
            sample_rate: self.sample_rate,
        });
    }

    /// A transmission to [`Self::speak`] on a piece at a time, as its text comes in.
    pub fn transmission(&self, effects: Effects) -> Transmission {
        Transmission {
            radio: self.radio.stream(self.sample_rate as f32, effects.radio),
            effects,
            open: false,
        }
    }

    /// The next of the transmission's text, as mono PCM at [`Self::sample_rate`], one piece per
    /// sentence (the text may hold several): the first opens the squelch. Nothing if they say
    /// nothing.
    pub fn speak(
        &mut self,
        transmission: &mut Transmission,
        text: &str,
    ) -> anyhow::Result<Vec<Vec<f32>>> {
        let started = Instant::now();
        let sentences = phonemize(text, &self.espeak_voice)?;
        let silence = (self.sample_rate as f32 * SENTENCE_SILENCE) as usize;
        let mut voice = Vec::<f32>::new();
        // Where the sentences after the first start, with the silence before them
        let mut splits = Vec::new();

        for phonemes in sentences {
            let ids = self.phoneme_ids(&phonemes);
            if !voice.is_empty() {
                splits.push(voice.len());
            }
            // After the sentence before, even if it was in an earlier piece
            if transmission.open || !voice.is_empty() {
                voice.resize(voice.len() + silence, 0.0);
            }

            let start = voice.len();
            self.infer(ids, transmission.effects.speed, &mut voice)?;
            normalize(&mut voice[start..]);
        }

        if voice.is_empty() {
            return Ok(Vec::new());
        }

        let mut samples = match std::mem::replace(&mut transmission.open, true) {
            true => Vec::new(),
            false => transmission.radio.open(),
        };
        let voice_start = samples.len();
        samples.extend(transmission.radio.voice(&voice));
        clip(&mut samples);
        log::debug!(
            "Synthesized {} characters in {:?}",
            text.chars().count(),
            started.elapsed()
        );
        return Ok(split_sentences(samples, voice_start, &splits));
    }

    /// The end of the transmission: the roger beep and the squelch closing. Nothing if it
    /// never said anything.
    pub fn end(&self, transmission: &mut Transmission) -> Vec<f32> {
        if !std::mem::take(&mut transmission.open) {
            return Vec::new();
        }

        let Effects { beep_volume, .. } = transmission.effects;
        let beep_at = (self.sample_rate as f32 * BEEP_GAP) as usize;
        let tail = match beep_volume > 0.0 {
            true => beep_at + self.beep.len(),
            false => 0,
        };

        // The beep is mixed in clean, over the radio's hiss, before the squelch closes
        let mut samples = transmission.radio.voice(&vec![0.0; tail]);
        if beep_volume > 0.0 {
            for (sample, &beep) in samples[beep_at..].iter_mut().zip(&self.beep) {
                *sample += beep_volume * beep;
            }
        }
        samples.extend(transmission.radio.close());
        clip(&mut samples);
        return samples;
    }

    /// The ONNX Runtime session of the model at `model`, on the GPU with DirectML if `gpu` (only
    /// on Windows, the only platform `ort` is built with DirectML for), or else on the processor.
    fn session(model: &Utf8Path, threads: NonZeroUsize, gpu: bool) -> ort::Result<Session> {
        // `ort` lets ONNX Runtime pick an accelerator by default (CoreML, on macOS). Preferring the
        // processor never picks one, and DirectML registered below takes precedence over it
        let builder = Session::builder(environment()?)?
            .with_auto_device(AutoDevicePolicy::PreferCPU)?
            .with_intra_threads(threads.get().min(8))?;
        #[cfg(windows)]
        let mut builder = match gpu {
            // DirectML doesn't support memory patterns, nor parallel execution (off by default).
            // Registering fails, rather than falling back, so the caller knows it isn't used
            true => builder.with_memory_pattern(false)?.with_execution_providers([
                ort::ep::DirectML::default().build().error_on_failure(),
            ])?,
            false => builder,
        };
        #[cfg(not(windows))]
        let mut builder = {
            let _ = gpu;
            builder
        };
        builder.commit_from_file(model)
    }

    /// Converts phonemes to IDs the way piper-phonemize does: BOS, then every phoneme followed
    /// by PAD, then EOS.
    fn phoneme_ids(&self, phonemes: &str) -> Vec<i64> {
        let id = |c| self.phoneme_ids[&c].as_slice();
        let mut ids = Vec::with_capacity(2 * phonemes.len() + 3);
        ids.extend_from_slice(id(BOS));
        ids.extend_from_slice(id(PAD));

        // The map is keyed by single codepoints, with diacritics as their own entries
        for c in phonemes.nfd() {
            match self.phoneme_ids.get(&c) {
                Some(phoneme) => {
                    ids.extend_from_slice(phoneme);
                    ids.extend_from_slice(id(PAD));
                }
                None => log::warn!("Voice has no ID for phoneme '{c}' ({:#x})", c as u32),
            }
        }

        ids.extend_from_slice(id(EOS));
        return ids;
    }

    fn infer(&mut self, ids: Vec<i64>, speed: f32, dest: &mut Vec<f32>) -> anyhow::Result<()> {
        let e = match self.run(ids.clone(), speed, dest) {
            Err(e) if self.gpu.is_some() => e,
            res => return res,
        };

        // The GPU can stop working once loaded (a driver reset removes its device), and every
        // sentence would fail: the voice moves to the processor for good instead. It stays on the
        // GPU if the move fails, so the next sentence tries again
        let Some((model, threads)) = &self.gpu else {
            unreachable!("only GPU failures get here");
        };
        log::warn!("Voice model '{model}' failed on the GPU, so it moves to the processor: {e:#}");
        self.session = Self::session(model, *threads, false)
            .with_context(|| format!("Error loading voice model '{model}'"))?;
        self.gpu = None;
        return self.run(ids, speed, dest);
    }

    fn run(&mut self, ids: Vec<i64>, speed: f32, dest: &mut Vec<f32>) -> anyhow::Result<()> {
        let len = ids.len();
        let [noise_scale, voice_length_scale, noise_w] = self.scales;
        let scales = vec![
            noise_scale,
            length_scale(voice_length_scale, speed),
            noise_w,
        ];
        let mut inputs = ort::inputs![
            "input" => Tensor::from_array(([1, len], ids))?,
            "input_lengths" => Tensor::from_array(([1], vec![len as i64]))?,
            "scales" => Tensor::from_array(([3], scales))?,
        ];

        if self.multi_speaker {
            inputs.push(("sid".into(), Tensor::from_array(([1], vec![0i64]))?.into()));
        }

        let outputs = self.session.run(inputs)?;
        let (_, audio) = outputs[0].try_extract_tensor::<f32>()?;
        dest.extend_from_slice(audio);
        return Ok(());
    }
}

/// Speech synthesized as its text comes in: the squelch opens with the first sentence and closes,
/// with the roger beep, at the end.
pub struct Transmission {
    radio: RadioStream,
    effects: Effects,
    /// Something was said
    open: bool,
}

/// `samples` in a piece per sentence, given where the sentences after the first start (`splits`,
/// ascending) in the voice that starts at `voice_start`.
fn split_sentences(mut samples: Vec<f32>, voice_start: usize, splits: &[usize]) -> Vec<Vec<f32>> {
    let mut pieces = Vec::with_capacity(splits.len() + 1);
    for split in splits.iter().rev() {
        pieces.push(samples.split_off(voice_start + split));
    }
    pieces.push(samples);
    pieces.reverse();
    return pieces;
}

/// The voice's `length_scale` (how long it draws each sound out) at `speed`: faster is shorter.
fn length_scale(voice: f32, speed: f32) -> f32 {
    voice / speed.clamp(MIN_SPEED, MAX_SPEED)
}

/// The radio's hiss and the beep can push peaks past full scale.
fn clip(samples: &mut [f32]) {
    for sample in samples {
        *sample = sample.clamp(-1.0, 1.0);
    }
}

/// Synthesized mono audio.
pub struct Speech {
    pub samples: Vec<f32>,
    pub sample_rate: u32,
}

impl Speech {
    /// The radio's beep at `volume`, telling the pilot the microphone is open.
    pub fn talk_beep(volume: f32) -> Self {
        Self::tone(Radio::beep, volume)
    }

    /// The radio's low double tone at `volume`, for when the engineer can't answer: there's no
    /// voice to do it with.
    pub fn no_beep(volume: f32) -> Self {
        Self::tone(Radio::no_beep, volume)
    }

    fn tone(tone: impl FnOnce(&Radio, f32) -> Vec<f32>, volume: f32) -> Self {
        const SAMPLE_RATE: u32 = 48000;
        let mut speech = Self {
            samples: tone(&Radio::default(), SAMPLE_RATE as f32),
            sample_rate: SAMPLE_RATE,
        };
        speech.amplify(volume);
        return speech;
    }

    /// Scales it by `gain`, clipped to full scale: a volume over 100% pushes peaks past it.
    pub fn amplify(&mut self, gain: f32) {
        for sample in &mut self.samples {
            *sample = (*sample * gain).clamp(-1.0, 1.0);
        }
    }

    #[inline]
    pub fn spec(&self) -> AudioSpec {
        AudioSpec {
            freq: Some(self.sample_rate as i32),
            channels: Some(1),
            format: Some(AudioFormat::f32_sys()),
        }
    }

    /// Starts playing on `device`. Dropping the returned stream stops playback.
    pub fn play(&self, device: &sdl3::audio::AudioDevice) -> Result<AudioStreamOwner, sdl3::Error> {
        let stream = self.start(device, 0)?;
        stream.flush()?;
        return Ok(stream);
    }

    /// Starts playing on `device` from the sample `skip`, with more to come: put it on the
    /// returned stream, and flush it after the last. Dropping the stream stops playback.
    pub fn start(
        &self,
        device: &sdl3::audio::AudioDevice,
        skip: usize,
    ) -> Result<AudioStreamOwner, sdl3::Error> {
        let stream = device.clone().open_device_stream(Some(&self.spec()))?;
        stream.put_data_f32(&self.samples[skip..])?;
        stream.resume()?;
        return Ok(stream);
    }
}

/// Whether `stream` plays on the audio device `which`, as SDL reports removed devices. Every
/// stream opened on a device gets a logical device of its own, and SDL says when each is removed.
pub fn plays_on(stream: &AudioStream, which: u32) -> bool {
    stream.device_id().is_some_and(|id| id.id() == which)
}

/// How long a stream may go without playing anything before it's given up on: the speaker
/// stopped taking audio without SDL saying it was removed.
pub const STALL_TIMEOUT: Duration = Duration::from_secs(2);

/// How a stream's playback is going, from [`Drain::check`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Drained {
    Playing,
    /// All of it was played
    Done,
    /// The speaker stopped taking audio, or the stream can't be read: what's left won't be played
    Stalled,
}

/// Watches a stream play out what's queued on it, so a speaker that stops taking audio doesn't
/// keep it playing forever.
#[derive(Debug, Clone)]
pub struct Drain {
    /// What was queued when last looked at
    queued: Option<i32>,
    /// When it last played something, or was paused
    progress: Instant,
}

impl Drain {
    pub fn new(now: Instant) -> Self {
        Self {
            queued: None,
            progress: now,
        }
    }

    /// Whether the stream played out, given what's still `queued` on it (`None` if that can't be
    /// read). One `paused` on purpose isn't stalling.
    pub fn check(&mut self, queued: Option<i32>, paused: bool, now: Instant) -> Drained {
        let Some(queued) = queued else {
            return Drained::Stalled;
        };
        if queued <= 0 {
            return Drained::Done;
        }

        if paused || self.queued != Some(queued) {
            self.queued = Some(queued);
            self.progress = now;
            return Drained::Playing;
        }
        match now.saturating_duration_since(self.progress) >= STALL_TIMEOUT {
            true => Drained::Stalled,
            false => Drained::Playing,
        }
    }
}

/// Scales audio so its peak is at full volume, like Piper does before writing samples.
// FIXME(TODO): Vectorize this method
fn normalize(samples: &mut [f32]) {
    let peak = samples.iter().fold(0.0f32, |peak, s| peak.max(s.abs()));
    let scale = peak.max(0.01).recip();
    for sample in samples {
        *sample = (*sample * scale).clamp(-1.0, 1.0);
    }
}

#[derive(Deserialize)]
struct VoiceConfig {
    audio: AudioConfig,
    espeak: EspeakConfig,
    #[serde(default)]
    inference: InferenceConfig,
    #[serde(default = "default_phoneme_type")]
    phoneme_type: String,
    phoneme_id_map: HashMap<String, Vec<i64>>,
}

#[derive(Deserialize)]
struct AudioConfig {
    sample_rate: u32,
}

#[derive(Deserialize)]
struct EspeakConfig {
    voice: String,
}

#[derive(Deserialize)]
struct InferenceConfig {
    #[serde(default = "default_noise_scale")]
    noise_scale: f32,
    #[serde(default = "default_length_scale")]
    length_scale: f32,
    #[serde(default = "default_noise_w")]
    noise_w: f32,
}

impl Default for InferenceConfig {
    fn default() -> Self {
        Self {
            noise_scale: default_noise_scale(),
            length_scale: default_length_scale(),
            noise_w: default_noise_w(),
        }
    }
}

fn default_phoneme_type() -> String {
    "espeak".to_owned()
}

#[inline(always)]
const fn default_noise_scale() -> f32 {
    0.667
}

#[inline(always)]
const fn default_length_scale() -> f32 {
    1.0
}

#[inline(always)]
const fn default_noise_w() -> f32 {
    0.8
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Whether `done` holds within ten seconds: the worker loads voices in the background.
    fn wait_until(mut done: impl FnMut() -> bool) -> bool {
        let start = Instant::now();
        while start.elapsed() < Duration::from_secs(10) {
            if done() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        false
    }

    #[test]
    fn a_voice_that_fails_to_load_is_reported_until_its_picked_again() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let path = |name: &str| Utf8PathBuf::try_from(dir.path().join(name)).unwrap();
        let (missing, other) = (path("missing.onnx"), path("other.onnx"));

        let synth = SynthesizerInner::new(rt.handle().clone());
        assert!(!synth.failed(&missing));
        synth.set_model(Some(missing.clone()), false);
        assert!(wait_until(|| synth.failed(&missing)));

        // Picked again, it fails again. Whether it shows as loading in between isn't checked:
        // a missing voice fails before a check right after picking it is sure to run
        synth.set_model(Some(missing.clone()), false);
        assert!(wait_until(|| synth.failed(&missing)));

        // Picking another one clears the failure of the one before
        synth.set_model(Some(other.clone()), false);
        assert!(!synth.failed(&missing));
        assert!(wait_until(|| synth.failed(&other)));
        assert!(!synth.failed(&missing));
    }

    async fn loaded(inner: &SynthesizerInner) -> Option<VoiceLoad> {
        let deadline = Instant::now() + Duration::from_secs(10);
        while inner.load() == Some(VoiceLoad::Loading) {
            assert!(Instant::now() < deadline, "timed out");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        inner.load()
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_voice_that_wont_load_is_failed() {
        let inner = SynthesizerInner::new(Handle::current());
        assert_eq!(inner.load(), None);

        let missing = std::env::temp_dir().join("bono-missing-voice.onnx");
        let missing = Utf8PathBuf::from_path_buf(missing).unwrap();
        inner.set_model(Some(missing.clone()), false);
        assert_eq!(loaded(&inner).await, Some(VoiceLoad::Failed));

        // Setting it again tries again
        inner.set_model(Some(missing.clone()), false);
        assert_eq!(loaded(&inner).await, Some(VoiceLoad::Failed));

        // Once the worker stops, nothing loads it
        inner.set_model(None, false);
        assert_eq!(inner.load(), None);
        stopped(&inner).await;
        inner.set_model(Some(missing), false);
        assert_eq!(inner.load(), Some(VoiceLoad::Failed));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn moving_a_voice_to_the_gpu_loads_it_again() {
        let inner = SynthesizerInner::new(Handle::current());
        let missing = std::env::temp_dir().join("bono-missing-gpu-voice.onnx");
        let missing = Utf8PathBuf::from_path_buf(missing).unwrap();
        inner.set_model(Some(missing), false);
        assert_eq!(loaded(&inner).await, Some(VoiceLoad::Failed));

        // Already where it's asked to run: nothing to load
        inner.set_gpu(false);
        assert_eq!(lock(&inner.load).requested, 1);
        assert_eq!(inner.load(), Some(VoiceLoad::Failed));

        // Moved, it's the latest request, and loads (and fails) again
        inner.set_gpu(true);
        assert_eq!(lock(&inner.load).requested, 2);
        assert_eq!(loaded(&inner).await, Some(VoiceLoad::Failed));
    }

    async fn stopped(inner: &SynthesizerInner) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while !inner.stopped() {
            assert!(Instant::now() < deadline, "timed out");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn setting_a_voice_starts_a_stopped_synthesizer_again() {
        let rt = Handle::current();
        let lang = crate::arrstr!("en");
        let mut synth = Synthesizer {
            inner: BTreeMap::new(),
        };
        let mut config = RawConfig::default();
        let missing = std::env::temp_dir().join("bono-missing-voice.onnx");
        let missing = Utf8PathBuf::from_path_buf(missing).unwrap();

        let inner = SynthesizerInner::new(rt.clone());
        inner.set_model(None, false);
        stopped(&inner).await;
        synth.inner.insert(lang, inner);

        config.set_voice(&rt, lang, missing, &mut synth);
        assert_eq!(loaded(&synth.inner[&lang]).await, Some(VoiceLoad::Failed));
        // It was loaded (and failed) by a worker that's still there for the next
        assert!(!synth.inner[&lang].stopped());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn removing_a_voice_removes_every_language_it_speaks() {
        let rt = Handle::current();
        let (en, es, ca) = (
            crate::arrstr!("en"),
            crate::arrstr!("es"),
            crate::arrstr!("ca"),
        );
        let mut synth = Synthesizer {
            inner: BTreeMap::new(),
        };
        let mut config = RawConfig::default();
        let dir = tempfile::tempdir().unwrap();
        let path = |name: &str| Utf8PathBuf::try_from(dir.path().join(name)).unwrap();
        let (removed, kept) = (path("removed.onnx"), path("kept.onnx"));
        config.set_voice(&rt, en, removed.clone(), &mut synth);
        // Set by hand in the config file
        config.set_voice(&rt, es, removed.clone(), &mut synth);
        config.set_voice(&rt, ca, kept.clone(), &mut synth);

        let workers = config.remove_voice(&removed, &mut synth);
        assert_eq!(*config.voices(), BTreeMap::from([(ca, kept.clone())]));
        assert_eq!(synth.inner.keys().copied().collect::<Vec<_>>(), [ca]);
        assert_eq!(synth.load(en), None);

        // Another is picked while the one before is still being unloaded
        config.set_voice(&rt, en, kept, &mut synth);
        let loads = workers
            .iter()
            .map(|worker| worker.load.clone())
            .collect::<Vec<_>>();
        assert_eq!(loads.len(), 2);
        rt.spawn_blocking(move || drop(workers)).await.unwrap();
        // Their threads are done with everything they held
        assert!(loads.iter().all(|load| Arc::strong_count(load) == 1));
        assert!(!synth.inner[&en].stopped());
        assert!(synth.load(en).is_some());

        // Nobody speaks with it any more: nothing to remove
        assert!(config.remove_voice(&removed, &mut synth).is_empty());
    }

    #[test]
    fn amplified_speech_clips_at_full_scale() {
        let mut speech = Speech {
            samples: vec![0.25, -0.25, 0.75, -0.75],
            sample_rate: 48000,
        };
        speech.amplify(2.0);
        assert_eq!(speech.samples, [0.5, -0.5, 1.0, -1.0]);
    }

    #[test]
    fn a_stream_that_never_drains_is_given_up_on() {
        let start = Instant::now();
        let mut drain = Drain::new(start);
        assert_eq!(drain.check(Some(4800), false, start), Drained::Playing);
        assert_eq!(
            drain.check(Some(4800), false, start + STALL_TIMEOUT / 2),
            Drained::Playing
        );
        assert_eq!(
            drain.check(Some(4800), false, start + STALL_TIMEOUT),
            Drained::Stalled
        );
    }

    #[test]
    fn a_draining_stream_plays_out() {
        let start = Instant::now();
        let mut drain = Drain::new(start);
        let mut now = start;
        for queued in [4800, 3200, 1600] {
            assert_eq!(drain.check(Some(queued), false, now), Drained::Playing);
            now += STALL_TIMEOUT - Duration::from_millis(1);
        }
        assert_eq!(drain.check(Some(0), false, now), Drained::Done);
    }

    #[test]
    fn a_paused_stream_isnt_stalled() {
        let start = Instant::now();
        let mut drain = Drain::new(start);
        let held = start + STALL_TIMEOUT * 10;
        assert_eq!(drain.check(Some(4800), true, held), Drained::Playing);
        // Released: stalling counts from there
        let released = held + STALL_TIMEOUT / 2;
        assert_eq!(drain.check(Some(4800), false, released), Drained::Playing);
        assert_eq!(
            drain.check(Some(4800), false, held + STALL_TIMEOUT),
            Drained::Stalled
        );
    }

    #[test]
    fn an_unreadable_stream_is_finished() {
        let now = Instant::now();
        assert_eq!(Drain::new(now).check(None, false, now), Drained::Stalled);
        assert_eq!(Drain::new(now).check(None, true, now), Drained::Stalled);
    }

    #[test]
    fn speed_divides_the_voices_own_pace() {
        assert_eq!(length_scale(1.2, 1.0), 1.2);
        assert_eq!(length_scale(1.0, 2.0), 0.5);
        assert_eq!(length_scale(1.0, 0.5), 2.0);
    }

    #[test]
    fn a_speed_outside_the_range_is_clamped() {
        assert_eq!(length_scale(1.0, 10.0), 0.5);
        assert_eq!(length_scale(1.0, 0.0), 2.0);
        assert_eq!(length_scale(1.0, -1.0), 2.0);
    }

    #[test]
    fn speech_is_at_the_voices_own_pace_by_default() {
        assert_eq!(RawConfig::default().synth_effects().speed, 1.0);
        let mut config = RawConfig::default();
        config.speech_speed.set(1.5);
        assert_eq!(config.synth_effects().speed, 1.5);
    }

    #[test]
    fn splits_a_piece_per_sentence() {
        let samples = vec![9.0, 9.0, 1.0, 1.0, 2.0, 2.0, 2.0, 3.0];
        assert_eq!(
            split_sentences(samples.clone(), 2, &[2, 5]),
            [vec![9.0, 9.0, 1.0, 1.0], vec![2.0, 2.0, 2.0], vec![3.0]]
        );
        assert_eq!(split_sentences(samples.clone(), 2, &[]), [samples]);
    }

    #[test]
    fn says_sentences_once_theyre_complete() {
        assert_eq!(sentence_len("You are P3"), None);
        assert_eq!(sentence_len("It's 3.5 seconds"), None);
        // Could still be going ("lap...")
        assert_eq!(sentence_len("Box this lap."), None);
        assert_eq!(sentence_len("Box this lap. Tyres"), Some(13));
        assert_eq!(sentence_len("He said \"go!\" Then"), Some(13));
        assert_eq!(sentence_len("Copy\nNext"), Some(5));
        assert_eq!(sentence_len("好。下"), Some("好。".len()));
    }

    async fn next(output: &mut SpeechOutput) -> Option<anyhow::Result<Speech>> {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
        loop {
            if let Poll::Ready(speech) = output.try_next() {
                return speech;
            }
            assert!(std::time::Instant::now() < deadline, "timed out");
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn one_piece_output_ends_after_its_speech() {
        let rt = Handle::current();
        let mut synth = Synthesizer {
            inner: BTreeMap::new(),
        };
        let effects = Effects {
            radio: 0.65,
            beep_volume: 1.0,
            speed: 1.0,
        };
        // Without a voice, the one piece is the error saying so
        let mut output =
            synth.synthesize_output(&rt, crate::arrstr!("en"), "Copy.".into(), effects);
        let Some(Err(error)) = next(&mut output).await else {
            panic!("there's no voice to speak with");
        };
        assert_eq!(error.to_string(), no_model().to_string());
        assert!(next(&mut output).await.is_none());
    }

    /// Needs a Piper voice: `BONO_VOICE` is the path to its `.onnx`.
    #[tokio::test(flavor = "multi_thread")]
    #[ignore]
    async fn a_voice_removed_mid_speech_can_be_deleted_once_its_unloaded() {
        let model = Utf8PathBuf::from(std::env::var("BONO_VOICE").expect("BONO_VOICE isn't set"));
        let dir = tempfile::tempdir().unwrap();
        let copy = Utf8PathBuf::try_from(dir.path().join("voice.onnx")).unwrap();
        let json = |path: &Utf8Path| path.with_added_extension("json");
        std::fs::copy(&model, &copy).unwrap();
        std::fs::copy(json(&model), json(&copy)).unwrap();

        let (rt, en) = (Handle::current(), crate::arrstr!("en"));
        let mut synth = Synthesizer {
            inner: BTreeMap::new(),
        };
        let mut config = RawConfig::default();
        config.set_voice(&rt, en, copy.clone(), &mut synth);
        let effects = Effects {
            radio: 0.65,
            beep_volume: 1.0,
            speed: 1.0,
        };
        let (input, mut output) = synth.stream(&rt, en, effects);
        input.push("Copy. ".repeat(32));
        assert!(!next(&mut output).await.unwrap().unwrap().samples.is_empty());

        let workers = config.remove_voice(&copy, &mut synth);
        let deleted = rt.spawn_blocking(move || {
            drop(workers);
            std::fs::remove_file(&copy)?;
            std::fs::remove_file(json(&copy))
        });
        // What it had to say comes out, and then it ends
        while let Some(speech) = next(&mut output).await {
            assert!(!speech.unwrap().samples.is_empty());
        }
        deleted.await.unwrap().unwrap();
    }

    /// Needs a Piper voice: `BONO_VOICE` is the path to its `.onnx`.
    #[tokio::test(flavor = "multi_thread")]
    #[ignore]
    async fn one_shot_interrupts_a_stream_with_pending_sentences() {
        let model = std::env::var("BONO_VOICE").expect("BONO_VOICE isn't set");
        let rt = Handle::current();
        let inner = SynthesizerInner::new(rt.clone());
        inner.set_model(Some(model.into()), false);
        let mut synth = Synthesizer {
            inner: BTreeMap::from([(crate::arrstr!("en"), inner)]),
        };
        let effects = Effects {
            radio: 0.65,
            beep_volume: 1.0,
            speed: 1.0,
        };
        let (input, mut output) = synth.stream(&rt, crate::arrstr!("en"), effects);
        input.push("Copy. ".repeat(32));
        drop(input);
        assert!(!next(&mut output).await.unwrap().unwrap().samples.is_empty());

        let call = synth.synthesize(
            &rt,
            crate::arrstr!("en"),
            "Limiter, limiter.".into(),
            effects,
        );
        let speech = tokio::time::timeout(Duration::from_secs(60), call)
            .await
            .expect("timed out")
            .unwrap()
            .unwrap();
        assert!(!speech.samples.is_empty());

        // The call must finish before all remaining sentences and the closing beep.
        let mut ready = 0;
        while let Poll::Ready(Some(speech)) = output.try_next() {
            speech.unwrap();
            ready += 1;
        }
        assert!(ready < 32, "the one-shot waited for the entire stream");
    }

    /// Needs a Piper voice (`BONO_VOICE` is the path to its `.onnx`) and a DirectX 12 GPU.
    #[test]
    #[ignore]
    #[cfg(windows)]
    fn speaks_on_the_gpu() {
        let model = std::env::var("BONO_VOICE").expect("BONO_VOICE isn't set");
        let model = Utf8Path::new(&model);
        let threads = NonZeroUsize::new(4).unwrap();
        // Errs, rather than falling back to the processor, if DirectML can't be registered (ONNX
        // Runtime may still run operators DirectML doesn't support on the processor)
        Voice::session(model, threads, true).unwrap();

        let mut voice = Voice::load(model, threads, true).unwrap();
        assert!(voice.gpu.is_some(), "the voice fell back to the processor");
        let effects = Effects {
            radio: 0.65,
            beep_volume: 1.0,
            speed: 1.0,
        };
        let speech = voice.synthesize("Copy. Box this lap.", effects).unwrap();
        assert!(!speech.samples.is_empty());
        assert!(speech.samples.iter().all(|sample| sample.abs() <= 1.0));
        assert!(voice.gpu.is_some(), "the GPU failed while speaking");
    }

    /// Needs a Piper voice: `BONO_VOICE` is the path to its `.onnx`.
    #[tokio::test(flavor = "multi_thread")]
    #[ignore]
    async fn speaks_a_sentence_at_a_time() {
        let model = std::env::var("BONO_VOICE").expect("BONO_VOICE isn't set");
        let rt = Handle::current();
        let inner = SynthesizerInner::new(rt.clone());
        inner.set_model(Some(model.into()), false);
        let mut synth = Synthesizer {
            inner: BTreeMap::from([(crate::arrstr!("en"), inner)]),
        };

        let effects = Effects {
            radio: 0.65,
            beep_volume: 1.0,
            speed: 1.0,
        };
        let (input, mut output) = synth.stream(&rt, crate::arrstr!("en"), effects);

        // The first sentence is said while the next is still coming
        input.push("Copy. You're P3, the gap");
        let first = next(&mut output).await.unwrap().unwrap();
        assert!(!first.samples.is_empty());

        input.push(" behind is 2.1 seconds. Box");
        input.push(" this lap.");
        drop(input);

        let mut pieces = vec![first];
        while let Some(speech) = next(&mut output).await {
            pieces.push(speech.unwrap());
        }

        // The other two sentences, then the beep and the squelch closing
        assert_eq!(pieces.len(), 4);
        let samples = pieces.iter().flat_map(|piece| &piece.samples);
        assert!(samples.clone().all(|sample| sample.abs() <= 1.0));
        let seconds = samples.count() as f32 / pieces[0].sample_rate as f32;
        assert!((2.0..10.0).contains(&seconds), "{seconds}s");

        // Stopping a transmission midway frees the voice for the next
        let (input, output) = synth.stream(&rt, crate::arrstr!("en"), effects);
        input.push("Box");
        drop(output);
        let speech = synth.synthesize(&rt, crate::arrstr!("en"), "Copy.".into(), effects);
        let speech = tokio::time::timeout(std::time::Duration::from_secs(60), speech)
            .await
            .expect("timed out");
        assert!(!speech.unwrap().unwrap().samples.is_empty());
        drop(input);
    }
}
