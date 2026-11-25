//! Parakeet TDT 0.6B v3: speech to text on the processor, so it costs the simulator no frames. It
//! can't be told which language to expect, and doesn't say which it heard.

use crate::synthesizer;
use camino::{Utf8Path, Utf8PathBuf};
use parakeet_rs::{ExecutionConfig, ParakeetTDT, Transcriber};
use std::{num::NonZeroUsize, sync::Mutex};

/// The most threads it runs on: more are no quicker, and leave the simulator fewer.
const MAX_THREADS: usize = 4;

/// The loaded model, shared by workers: it hears one recording at a time. Everything blocks, so it's
/// only used on workers.
pub struct Parakeet {
    dir: Utf8PathBuf,
    threads: NonZeroUsize,
    model: Mutex<ParakeetTDT>,
}

impl Parakeet {
    /// Loads the model in `dir`.
    pub fn load(dir: &Utf8Path, threads: NonZeroUsize) -> anyhow::Result<Self> {
        Ok(Self {
            dir: dir.to_owned(),
            threads,
            model: Mutex::new(Self::model(dir, threads)?),
        })
    }

    fn model(dir: &Utf8Path, threads: NonZeroUsize) -> anyhow::Result<ParakeetTDT> {
        let config = ExecutionConfig::new()
            .with_intra_threads(threads.get().min(MAX_THREADS))
            // Its threads wait for the next question without using the processor
            .with_custom_configure(|builder| Ok(builder.with_intra_op_spinning(false)?))
            .with_environment(synthesizer::environment()?.clone());
        Ok(ParakeetTDT::from_pretrained(dir, Some(config))?)
    }

    /// What was said in `samples` (16 kHz, mono): empty if nothing.
    pub fn transcribe(&self, samples: &[f32]) -> anyhow::Result<String> {
        let mut model = match self.model.lock() {
            Ok(model) => model,
            // Hearing a recording panicked: what that left isn't known, so it's loaded again
            Err(poisoned) => {
                let mut model = poisoned.into_inner();
                *model = Self::model(&self.dir, self.threads)?;
                self.model.clear_poison();
                model
            }
        };
        let heard = model.transcribe_samples(samples.to_vec(), 16000, 1, None)?;
        Ok(heard.text.trim().to_owned())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::synthesizer::{Effects, Voice};

    const FOUR: NonZeroUsize = NonZeroUsize::new(4).unwrap();

    /// `text` spoken by the voice in the environment variable `voice`, as a recording.
    fn spoken(voice: &str, text: &str) -> Vec<f32> {
        let path = Utf8PathBuf::from(std::env::var(voice).unwrap());
        let effects = Effects {
            radio: 0.0,
            beep_volume: 0.0,
            speed: 1.0,
        };
        let mut voice = Voice::load(&path, FOUR, false).unwrap();
        let speech = voice.synthesize(text, effects).unwrap();
        let ratio = speech.sample_rate as f64 / 16000.0;
        (0..(speech.samples.len() as f64 / ratio) as usize)
            .map(|i| speech.samples[(i as f64 * ratio) as usize])
            .collect()
    }

    fn load() -> Parakeet {
        let dir = std::env::var("BONO_PARAKEET").unwrap();
        Parakeet::load(Utf8Path::new(&dir), FOUR).unwrap()
    }

    #[test]
    fn a_folder_without_the_model_does_not_load() {
        let dir = tempfile::tempdir().unwrap();
        assert!(Parakeet::load(Utf8Path::from_path(dir.path()).unwrap(), FOUR).is_err());
    }

    /// Needs `BONO_PARAKEET` (the model's folder) and `BONO_VOICE_EN`/`_ES` (Piper voices).
    #[test]
    #[ignore]
    fn hears_spanish_and_english() {
        let parakeet = load();
        let hear = |voice, text| {
            let heard = parakeet.transcribe(&spoken(voice, text)).unwrap();
            println!("{heard}");
            heard.to_lowercase()
        };
        let es = hear(
            "BONO_VOICE_ES",
            "¿Cuánto combustible me queda y cuándo tengo que parar?",
        );
        assert!(es.contains("combustible") && es.contains("parar"));
        let en = hear(
            "BONO_VOICE_EN",
            "How much fuel do I have left, and when do I need to pit?",
        );
        assert!(en.contains("fuel") && en.contains("pit"));
    }

    /// Needs `BONO_PARAKEET` (the model's folder) and `BONO_VOICE_EN` (a Piper voice).
    #[test]
    #[ignore]
    fn a_panic_does_not_silence_the_next_recording() {
        let parakeet = load();
        let panicked = std::thread::scope(|scope| {
            let thread = scope.spawn(|| {
                let _model = parakeet.model.lock().unwrap();
                panic!("while hearing");
            });
            thread.join().is_err()
        });
        assert!(panicked && parakeet.model.is_poisoned());
        let heard = parakeet.transcribe(&spoken("BONO_VOICE_EN", "How much fuel do I have left?"));
        assert!(heard.unwrap().to_lowercase().contains("fuel"));
        assert!(!parakeet.model.is_poisoned());
    }

    /// Needs `BONO_PARAKEET` (the model's folder).
    #[test]
    #[ignore]
    fn silence_is_nothing() {
        assert_eq!(load().transcribe(&[0.0; 16000]).unwrap(), "");
    }
}
