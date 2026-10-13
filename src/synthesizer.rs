//! Text to speech with [Piper](https://github.com/rhasspy/piper) voices.
//!
//! A voice is an `.onnx` model plus its `.onnx.json` config, and synthesis goes:
//! 1. text → IPA phonemes, with espeak-ng
//! 2. phonemes → IDs, with the config's `phoneme_id_map`
//! 3. IDs → PCM, by running the model with ONNX Runtime
//!
//! Each sentence is synthesized separately, like Piper does.

use crate::{
    config::{ArrayStr, RawConfig},
    error,
    synthesizer::radio::Radio,
};
use anyhow::{Context, bail};
use camino::{Utf8Path, Utf8PathBuf};
use espeak_rs_sys::{
    espeak_AUDIO_OUTPUT_AUDIO_OUTPUT_SYNCHRONOUS, espeak_ERROR_EE_OK, espeak_Initialize,
    espeak_SetVoiceByName, espeak_TextToPhonemes, espeakCHARS_UTF8, espeakINITIALIZE_DONT_EXIT,
    espeakPHONEMES_IPA,
};
use ort::{session::Session, value::Tensor};
use sdl3::audio::{AudioFormat, AudioSpec, AudioStreamOwner};
use serde::Deserialize;
use std::{
    collections::{BTreeMap, HashMap},
    ffi::{CStr, CString, c_int, c_void},
    mem::ManuallyDrop,
    num::NonZeroUsize,
    path::{Path, PathBuf},
    sync::{Mutex, PoisonError},
    time::SystemTime,
};
use tokio::{runtime::Handle, sync::mpsc::error::SendError};
use unicode_normalization::UnicodeNormalization;

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
    ) -> tokio::sync::oneshot::Receiver<anyhow::Result<Speech>> {
        self.inner
            .entry(lang)
            .or_insert_with(|| SynthesizerInner::new(rt.clone()))
            .synthesize(text, effects)
    }
}

pub struct SynthesizerInner {
    handle: ManuallyDrop<std::thread::JoinHandle<()>>,
    model: ManuallyDrop<tokio::sync::watch::Sender<Option<Utf8PathBuf>>>,
    synths: ManuallyDrop<
        tokio::sync::mpsc::UnboundedSender<(
            String,
            Effects,
            tokio::sync::oneshot::Sender<anyhow::Result<Speech>>,
        )>,
    >,
}

impl SynthesizerInner {
    pub fn new(rt: Handle) -> Self {
        let threads = std::thread::available_parallelism().unwrap_or(NonZeroUsize::MIN);
        let (model_send, model_recv) = tokio::sync::watch::channel(None);
        let (synths_send, synths_recv) = tokio::sync::mpsc::unbounded_channel();
        let handle = std::thread::spawn(move || Self::worker(rt, model_recv, synths_recv, threads));

        Self {
            handle: ManuallyDrop::new(handle),
            model: ManuallyDrop::new(model_send),
            synths: ManuallyDrop::new(synths_send),
        }
    }

    pub fn set_model(&self, model: Option<Utf8PathBuf>) {
        let _ = self.model.send(model);
    }

    fn synthesize(
        &self,
        text: String,
        effects: Effects,
    ) -> tokio::sync::oneshot::Receiver<anyhow::Result<Speech>> {
        let (send, recv) = tokio::sync::oneshot::channel();
        if let Err(SendError((_, _, send))) = self.synths.send((text, effects, send)) {
            let _ = send.send(Err(anyhow::Error::msg("channel closed")));
        }
        return recv;
    }

    fn worker(
        handle: Handle,
        mut model: tokio::sync::watch::Receiver<Option<Utf8PathBuf>>,
        mut synths: tokio::sync::mpsc::UnboundedReceiver<(
            String,
            Effects,
            tokio::sync::oneshot::Sender<anyhow::Result<Speech>>,
        )>,
        threads: NonZeroUsize,
    ) {
        let mut voice = None;

        enum Task {
            Model,
            Speech {
                text: String,
                effects: Effects,
                output: tokio::sync::oneshot::Sender<anyhow::Result<Speech>>,
            },
        }

        loop {
            let task = handle.block_on(async {
                tokio::select! {
                    res = model.changed() => return match res {
                        Ok(()) => Some(Task::Model),
                        Err(_) => None
                    },
                    res = synths.recv() => return match res {
                        Some((text, effects, output)) => Some(Task::Speech { text, effects, output }),
                        None => None
                    },
                }
            });

            match task {
                Some(Task::Model) => {
                    let model = model.borrow();
                    match &*model {
                        Some(model) => match Voice::load(model, threads) {
                            Ok(next_voice) => voice = Some(next_voice),
                            Err(e) => error("Error loading text to speech model", e),
                        },
                        None => return,
                    }
                }
                Some(Task::Speech {
                    text,
                    effects,
                    output,
                }) => {
                    if let Some(ref mut voice) = voice {
                        if !output.is_closed() {
                            let _ = output.send(voice.synthesize(&text, effects));
                        }
                    } else {
                        let _ = output.send(Err(anyhow::Error::msg(
                            "No text to speech model loaded yet",
                        )));
                    };
                }
                None => return,
            }
        }
    }
}

impl Drop for SynthesizerInner {
    fn drop(&mut self) {
        let handle = unsafe { ManuallyDrop::take(&mut self.handle) };
        unsafe { ManuallyDrop::drop(&mut self.model) };
        unsafe { ManuallyDrop::drop(&mut self.synths) };
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
    espeak_voice: CString,
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
            espeak_voice: CString::new(config.espeak.voice)?,
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
        let sentences = phonemize(text, &self.espeak_voice)?;
        let sample_rate = self.sample_rate as f32;
        let silence = (sample_rate * SENTENCE_SILENCE) as usize;
        let mut voice = Vec::<f32>::new();

        for phonemes in sentences {
            let ids = self.phoneme_ids(&phonemes);
            if !voice.is_empty() {
                voice.resize(voice.len() + silence, 0.0);
            }

            let start = voice.len();
            self.infer(ids, &mut voice)?;
            normalize(&mut voice[start..]);
        }

        // Leave room after the voice for the beep, so it plays before the squelch closes
        let beep_at = match effects.beep_volume > 0.0 && !voice.is_empty() {
            true => {
                let at = voice.len() + (sample_rate * BEEP_GAP) as usize;
                voice.resize(at + self.beep.len(), 0.0);
                Some(at)
            }
            false => None,
        };

        let mut samples = self.radio.apply(&voice, sample_rate, effects.radio, true);

        if let Some(at) = beep_at {
            let at = at + self.radio.src_offset(sample_rate, effects.radio, true);
            for (sample, &beep) in samples[at..].iter_mut().zip(&self.beep) {
                *sample += effects.beep_volume * beep;
            }
        }

        // The radio's hiss and the beep can push peaks past full scale
        for sample in &mut samples {
            *sample = sample.clamp(-1.0, 1.0);
        }

        return Ok(Speech {
            samples,
            sample_rate: self.sample_rate,
        });
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

/// Synthesized mono audio.
pub struct Speech {
    pub samples: Vec<f32>,
    pub sample_rate: u32,
}

impl Speech {
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
        let stream = device.clone().open_device_stream(Some(&self.spec()))?;
        stream.put_data_f32(&self.samples)?;
        stream.flush()?;
        stream.resume()?;
        return Ok(stream);
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

/// espeak-ng keeps global state (current voice, clause decoder), so every call into it holds
/// this lock. The flag tracks whether it has been initialized.
static ESPEAK: Mutex<bool> = Mutex::new(false);

/// Converts `text` to IPA phonemes, one string per sentence.
fn phonemize(text: &str, voice: &CStr) -> anyhow::Result<Vec<String>> {
    let mut initialized = ESPEAK.lock().unwrap_or_else(PoisonError::into_inner);
    if !*initialized {
        init_espeak()?;
        *initialized = true;
    }

    if unsafe { espeak_SetVoiceByName(voice.as_ptr()) } != espeak_ERROR_EE_OK {
        bail!("Unknown espeak-ng voice '{}'", voice.to_string_lossy());
    }

    let c_text = CString::new(text).context("Text contains a null byte")?;
    let base = c_text.as_ptr().cast::<c_void>();
    let mut ptr = base;

    let mut sentences = Vec::new();
    let mut sentence = String::new();

    // espeak translates one clause per call, advancing `ptr` past it (null once done)
    while !ptr.is_null() {
        let start = unsafe { ptr.byte_offset_from_unsigned(base) };
        let phonemes = unsafe {
            espeak_TextToPhonemes(
                &mut ptr,
                espeakCHARS_UTF8 as c_int,
                espeakPHONEMES_IPA as c_int,
            )
        };

        if phonemes.is_null() {
            bail!("espeak-ng failed to translate text");
        }

        let end = match ptr.is_null() {
            true => text.len(),
            false => unsafe { ptr.byte_offset_from_unsigned(base) },
        };
        if end <= start {
            bail!("espeak-ng stopped advancing through the text");
        }

        push_without_lang_switches(&mut sentence, unsafe { CStr::from_ptr(phonemes) });

        // Unlike piper-phonemize's patched espeak-ng, upstream doesn't report the clause's
        // punctuation, but the voices are trained with it, so recover it from the source text.
        // Before the end of the text, espeak has read one character of the next clause ahead
        // (e.g. "Hello, w"), which it carries over to the next call.
        let mut clause = text.get(start..end).unwrap_or_default();
        if !ptr.is_null() {
            let mut chars = clause.chars();
            chars.next_back();
            clause = chars.as_str();
        }

        match clause_terminator(clause) {
            Some(c @ ('.' | '?' | '!')) => {
                sentence.push(c);
                sentences.push(std::mem::take(&mut sentence));
            }
            Some(c) => {
                sentence.push(c);
                sentence.push(' ');
            }
            None if !sentence.is_empty() && !sentence.ends_with(' ') => sentence.push(' '),
            None => {}
        }
    }

    if !sentence.trim().is_empty() {
        sentences.push(sentence);
    }

    return Ok(sentences);
}

/// The punctuation that ends a clause, normalized to ASCII.
fn clause_terminator(clause: &str) -> Option<char> {
    let clause = clause.trim_end_matches(|c: char| {
        c.is_whitespace() || matches!(c, '"' | '\'' | ')' | ']' | '}' | '”' | '’' | '»')
    });

    return match clause.chars().next_back()? {
        c @ ('.' | '?' | '!' | ',' | ':' | ';') => Some(c),
        '。' | '．' | '…' => Some('.'),
        '？' => Some('?'),
        '！' => Some('!'),
        '，' | '、' => Some(','),
        '：' => Some(':'),
        '；' => Some(';'),
        _ => None,
    };
}

/// Appends espeak's phonemes, dropping the `(en)`-style markers it adds when it switches
/// language mid-clause.
fn push_without_lang_switches(dest: &mut String, phonemes: &CStr) {
    let mut depth = 0usize;
    for c in phonemes.to_string_lossy().chars() {
        match c {
            '(' => depth += 1,
            ')' => depth = depth.saturating_sub(1),
            _ if depth == 0 => dest.push(c),
            _ => {}
        }
    }
}

fn init_espeak() -> anyhow::Result<()> {
    let data = espeak_data_dir().context(
        "Couldn't find espeak-ng's data ('espeak-ng-data' next to the executable or in the build \
         directory)",
    )?;

    let data = data
        .to_str()
        .context("espeak-ng's data path isn't valid UTF-8")?;
    let data = CString::new(data)?;

    let sample_rate = unsafe {
        espeak_Initialize(
            espeak_AUDIO_OUTPUT_AUDIO_OUTPUT_SYNCHRONOUS,
            0,
            data.as_ptr(),
            espeakINITIALIZE_DONT_EXIT as c_int,
        )
    };

    if sample_rate <= 0 {
        bail!("Error initializing espeak-ng (code {sample_rate})");
    }
    return Ok(());
}

/// The directory *containing* `espeak-ng-data`, which is what espeak-ng expects.
///
/// Distributions ship it next to the executable. During development it's only in
/// espeak-rs-sys's build output, next to the executable (or its parent for test binaries).
fn espeak_data_dir() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let exe_dir = exe.parent()?;

    if exe_dir.join("espeak-ng-data").is_dir() {
        return Some(exe_dir.to_path_buf());
    }

    return exe_dir.ancestors().take(2).find_map(newest_built_data);
}

/// The most recently built `espeak-rs-sys-*/out/share` under `{dir}/build`.
fn newest_built_data(dir: &Path) -> Option<PathBuf> {
    let mut newest = None::<(SystemTime, PathBuf)>;

    for entry in std::fs::read_dir(dir.join("build")).ok()?.flatten() {
        if !entry
            .file_name()
            .to_string_lossy()
            .starts_with("espeak-rs-sys-")
        {
            continue;
        }

        let share = entry.path().join("out").join("share");
        let Ok(modified) = share
            .join("espeak-ng-data")
            .join("phondata")
            .metadata()
            .and_then(|metadata| metadata.modified())
        else {
            continue;
        };

        if newest.as_ref().is_none_or(|(time, _)| modified > *time) {
            newest = Some((modified, share));
        }
    }

    return newest.map(|(_, share)| share);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn phonemizes_sentences() {
        let sentences = phonemize(
            "Hello, world. It costs 3.5 dollars; really! Does it?",
            c"en-us",
        )
        .unwrap();

        println!("{sentences:#?}");
        assert_eq!(sentences.len(), 3);
        assert!(sentences[0].contains(", ") && sentences[0].ends_with('.'));
        assert!(sentences[1].contains("; ") && sentences[1].ends_with('!'));
        assert!(sentences[2].ends_with('?'));
    }
}
