//! Text to speech with [Piper](https://github.com/rhasspy/piper) voices.
//!
//! A voice is an `.onnx` model plus its `.onnx.json` config, and synthesis goes:
//! 1. text → IPA phonemes, with espeak-ng in a separate program (see [`phonemizer`])
//! 2. phonemes → IDs, with the config's `phoneme_id_map`
//! 3. IDs → PCM, by running the model with ONNX Runtime
//!
//! Complete utterances use sentence synthesis; engineer streams render words separately
//! to retain exact PCM boundaries for interruption and resumption.

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
use ort::{session::Session, value::Tensor};
use sdl3::audio::{AudioFormat, AudioSpec, AudioStream, AudioStreamOwner};
use serde::Deserialize;
use std::{
    collections::{BTreeMap, HashMap, VecDeque},
    mem::ManuallyDrop,
    num::NonZeroUsize,
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
use unicode_segmentation::UnicodeSegmentation;

mod phonemizer;
mod radio;

const PAD: char = '_';
const BOS: char = '^';
const EOS: char = '$';

/// Silence between sentences, in seconds (Piper's default)
const SENTENCE_SILENCE: f32 = 0.2;

/// Silence between the end of the voice and the roger beep, in seconds
const BEEP_GAP: f32 = 0.1;

pub struct Synthesizer {
    pub(crate) inner: BTreeMap<ArrayStr<2>, SynthesizerInner>,
}

impl Synthesizer {
    pub fn new(rt: &Handle, config: &RawConfig) -> Self {
        let mut inner = BTreeMap::new();
        for (&k, v) in config.voices() {
            let synth = SynthesizerInner::new(rt.clone());
            synth.set_model(Some(v.clone()));
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

    /// Speaks text as it comes in, a word at a time: each one is synthesized as soon as
    /// its boundary is known, and comes out of the [`SpeechOutput`] while the next is still being
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

/// The speech of a [`Synthesizer::stream`], a word at a time: dropping it stops the
/// synthesis.
pub struct SpeechOutput(mpsc::UnboundedReceiver<anyhow::Result<Speech>>);

impl SpeechOutput {
    /// Just `speech`, already synthesized.
    pub fn ready(speech: Speech) -> Self {
        let (output, recv) = mpsc::unbounded_channel();
        let _ = output.send(Ok(speech));
        Self(recv)
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

pub struct SynthesizerInner {
    handle: ManuallyDrop<std::thread::JoinHandle<()>>,
    model: ManuallyDrop<watch::Sender<Option<Utf8PathBuf>>>,
    jobs: ManuallyDrop<mpsc::UnboundedSender<Job>>,
}

impl SynthesizerInner {
    pub fn new(rt: Handle) -> Self {
        let threads = std::thread::available_parallelism().unwrap_or(NonZeroUsize::MIN);
        let (model_send, model_recv) = watch::channel(None);
        let (jobs_send, jobs_recv) = mpsc::unbounded_channel();
        let handle = std::thread::spawn(move || Self::worker(rt, model_recv, jobs_recv, threads));

        Self {
            handle: ManuallyDrop::new(handle),
            model: ManuallyDrop::new(model_send),
            jobs: ManuallyDrop::new(jobs_send),
        }
    }

    pub fn set_model(&self, model: Option<Utf8PathBuf>) {
        let _ = self.model.send(model);
    }

    fn send(&self, job: Job) {
        if let Err(SendError(job)) = self.jobs.send(job) {
            job.fail(anyhow::Error::msg("channel closed"));
        }
    }

    fn worker(
        handle: Handle,
        mut model: watch::Receiver<Option<Utf8PathBuf>>,
        mut jobs: mpsc::UnboundedReceiver<Job>,
        threads: NonZeroUsize,
    ) {
        let mut voice = None::<Voice>;
        // The transmission being spoken: one at a time, the others wait their turn
        let mut stream = None::<Stream>;
        let mut waiting = VecDeque::<Job>::new();

        enum Event {
            Model,
            Job(Job),
            Text(Option<String>),
            Stopped,
        }

        loop {
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
            let event = handle.block_on(async {
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
            });

            match event {
                Some(Event::Model) => {
                    // Its speech so far was in the old voice
                    stream = None;
                    let model = model.borrow();
                    match &*model {
                        Some(model) => match Voice::load(model, threads) {
                            Ok(next_voice) => voice = Some(next_voice),
                            Err(e) => error(t!("error.tts_model"), e),
                        },
                        None => return,
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

    /// Says the next complete word, or the end of the transmission once its text is over.
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

        // Render complete words separately: each output piece is an exact restart boundary.
        let len = match word_len(&self.pending) {
            Some(len) => len,
            None if self.ended => self.pending.len(),
            None => 0,
        };

        let (samples, last) = match len {
            0 if self.ended => (Ok(voice.end(&mut self.transmission)), true),
            0 => return Step::Waiting,
            len => {
                let word = self.pending.drain(..len).collect::<String>();
                (voice.speak_word(&mut self.transmission, &word), false)
            }
        };

        let (result, last) = match samples {
            Ok(samples) if samples.is_empty() => (None, last),
            Ok(samples) => (
                Some(Ok(Speech {
                    samples,
                    sample_rate: voice.sample_rate(),
                })),
                last,
            ),
            Err(e) => (Some(Err(e)), true),
        };

        if let Some(result) = result
            && self.output.send(result).is_err()
        {
            return Step::Done;
        }

        return match last {
            true => Step::Done,
            false => Step::Spoke,
        };
    }
}

/// Wait for the next Unicode word before committing the preceding word and its
/// punctuation. This avoids splitting a word across model text deltas, including
/// scripts that do not separate words with spaces.
fn word_len(text: &str) -> Option<usize> {
    text.unicode_word_indices().nth(1).map(|(start, _)| start)
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
}

/// A loaded Piper voice.
pub struct Voice {
    session: Session,
    sample_rate: u32,
    espeak_voice: String,
    phoneme_ids: BTreeMap<char, Vec<i64>>,
    scales: [f32; 3],
    /// Multi-speaker models take a speaker ID; we always use the first speaker
    multi_speaker: bool,
    radio: Radio,
    beep: Vec<f32>,
}

impl Voice {
    /// Loads the model at `model` and its config at `{model}.json`.
    ///
    /// This blocks while the model is read and compiled.
    pub fn load(model: &Utf8Path, threads: NonZeroUsize) -> anyhow::Result<Self> {
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

        let session = (|| {
            Session::builder()?
                .with_intra_threads(threads.get())?
                .commit_from_file(model)
        })()
        .with_context(|| format!("Error loading voice model '{model}'"))?;

        let multi_speaker = session.inputs().iter().any(|input| input.name() == "sid");
        let inference = config.inference;

        let radio = Radio::default();
        let beep = radio.beep(config.audio.sample_rate as f32);

        return Ok(Self {
            session,
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
        let mut samples = self.speak(&mut transmission, text)?;
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

    /// The next of the transmission's sentences, as mono PCM at [`Self::sample_rate`]: the
    /// first opens the squelch. Nothing if they say nothing.
    pub fn speak(
        &mut self,
        transmission: &mut Transmission,
        text: &str,
    ) -> anyhow::Result<Vec<f32>> {
        self.speak_piece(transmission, text, true)
    }

    /// Independently rendered words give the engineer exact PCM restart boundaries.
    fn speak_word(
        &mut self,
        transmission: &mut Transmission,
        text: &str,
    ) -> anyhow::Result<Vec<f32>> {
        self.speak_piece(transmission, text, false)
    }

    fn speak_piece(
        &mut self,
        transmission: &mut Transmission,
        text: &str,
        sentence_pause: bool,
    ) -> anyhow::Result<Vec<f32>> {
        let sentences = phonemize(text, &self.espeak_voice)?;
        let silence = (self.sample_rate as f32 * SENTENCE_SILENCE) as usize;
        let mut voice = Vec::<f32>::new();

        for phonemes in sentences {
            let ids = self.phoneme_ids(&phonemes);
            // After the sentence before, even if it was in an earlier piece
            if sentence_pause && (transmission.open || !voice.is_empty()) {
                voice.resize(voice.len() + silence, 0.0);
            }

            let start = voice.len();
            self.infer(ids, &mut voice)?;
            normalize(&mut voice[start..]);
        }

        if voice.is_empty() {
            return Ok(voice);
        }

        let mut samples = match std::mem::replace(&mut transmission.open, true) {
            true => Vec::new(),
            false => transmission.radio.open(),
        };
        samples.extend(transmission.radio.voice(&voice));
        clip(&mut samples);
        return Ok(samples);
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

    fn infer(&mut self, ids: Vec<i64>, dest: &mut Vec<f32>) -> anyhow::Result<()> {
        let len = ids.len();
        let mut inputs = ort::inputs![
            "input" => Tensor::from_array(([1, len], ids))?,
            "input_lengths" => Tensor::from_array(([1], vec![len as i64]))?,
            "scales" => Tensor::from_array(([3], self.scales.to_vec()))?,
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
        let mut samples = tone(&Radio::default(), SAMPLE_RATE as f32);
        for sample in &mut samples {
            *sample = (*sample * volume).clamp(-1.0, 1.0);
        }
        return Self {
            samples,
            sample_rate: SAMPLE_RATE,
        };
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
        let stream = self.start(device)?;
        stream.flush()?;
        return Ok(stream);
    }

    /// Starts playing on `device`, with more to come: put it on the returned stream, and flush
    /// it after the last. Dropping the stream stops playback.
    pub fn start(
        &self,
        device: &sdl3::audio::AudioDevice,
    ) -> Result<AudioStreamOwner, sdl3::Error> {
        self.start_held(device, false)
    }

    /// Queue before starting the device, so an engineer held by the spotter never leaks audio.
    pub fn start_held(
        &self,
        device: &sdl3::audio::AudioDevice,
        held: bool,
    ) -> Result<AudioStreamOwner, sdl3::Error> {
        let stream = device.clone().open_device_stream(Some(&self.spec()))?;
        stream.put_data_f32(&self.samples)?;
        if !held {
            stream.resume()?;
        }
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
    fn words_wait_for_boundaries_and_preserve_punctuation() {
        assert_eq!(word_len("Box"), None);
        assert_eq!(word_len("Box this"), Some(4));
        assert_eq!(word_len("You're P3, the"), Some(7));
        assert_eq!(word_len("3.5 seconds"), Some(4));
        assert_eq!(word_len("¡Sí! Ahora"), Some("¡Sí! ".len()));
        assert_eq!(word_len("好。下"), Some("好。".len()));
        // Arbitrary model deltas cannot split the unfinished word.
        let mut pending = String::new();
        let mut spoken = String::new();
        for delta in ["Bo", "x th", "is lap", "."] {
            pending.push_str(delta);
            while let Some(len) = word_len(&pending) {
                spoken.extend(pending.drain(..len));
            }
        }
        assert_eq!(spoken, "Box this ");
        assert_eq!(pending, "lap.");
    }

    #[test]
    fn dropping_speech_output_cancels_delivery() {
        let (send, recv) = mpsc::unbounded_channel();
        let mut output = SpeechOutput(recv);
        assert!(output.try_next().is_pending());
        send.send(Ok(Speech::no_beep(0.0))).unwrap();
        drop(output);
        assert!(send.is_closed());
        assert!(send.send(Ok(Speech::no_beep(0.0))).is_err());
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

    /// Needs a Piper voice: `BONO_VOICE` is the path to its `.onnx`.
    #[tokio::test(flavor = "multi_thread")]
    #[ignore]
    async fn speaks_a_word_at_a_time() {
        let model = std::env::var("BONO_VOICE").expect("BONO_VOICE isn't set");
        let rt = Handle::current();
        let inner = SynthesizerInner::new(rt.clone());
        inner.set_model(Some(model.into()));
        let mut synth = Synthesizer {
            inner: BTreeMap::from([(crate::arrstr!("en"), inner)]),
        };

        let effects = Effects {
            radio: 0.65,
            beep_volume: 1.0,
        };
        let (input, mut output) = synth.stream(&rt, crate::arrstr!("en"), effects);

        // The first complete word is said while the next is still coming
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

        // Twelve words, then the beep and the squelch closing
        assert_eq!(pieces.len(), 13);
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
