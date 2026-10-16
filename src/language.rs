//! Which language a typed question is in, so the answer (which Claude writes in the pilot's
//! language) is spoken in that language's voice. Questions asked on the radio don't need it:
//! whisper says which language they're in.

use crate::{config::ArrayStr, task::Task};
use camino::Utf8PathBuf;
use lingua::{Language, LanguageDetectorBuilder};
use std::collections::{BTreeMap, BTreeSet};
use tokio::runtime::Handle;

/// How much likelier the language has to be than the next one: short questions ("gap?") often
/// aren't in any language in particular.
const MIN_RELATIVE_DISTANCE: f64 = 0.1;

pub struct LanguageDetector {
    /// The languages there are voices for, which are the only ones worth telling apart
    voices: BTreeSet<ArrayStr<2>>,
    detector: Detector,
}

enum Detector {
    /// Fewer than two languages to tell apart
    None,
    /// Its models are being loaded
    Loading(Task<lingua::LanguageDetector>),
    Ready(lingua::LanguageDetector),
}

impl LanguageDetector {
    pub fn new() -> Self {
        Self {
            voices: BTreeSet::new(),
            detector: Detector::None,
        }
    }

    /// Keeps up with the voices installed. Called every frame.
    pub fn update(&mut self, rt: &Handle, voices: &BTreeMap<ArrayStr<2>, Utf8PathBuf>) {
        if let Detector::Loading(task) = &mut self.detector
            && let Some(detector) = task.update(rt)
        {
            self.detector = Detector::Ready(detector);
        }

        if self.voices.iter().eq(voices.keys()) {
            return;
        }

        self.voices = voices.keys().copied().collect();
        let languages = Language::all()
            .into_iter()
            .filter(|&language| {
                voice_language(language).is_some_and(|code| self.voices.contains(&code))
            })
            .collect::<Vec<_>>();

        log::debug!("Detecting typed questions in {languages:?}");
        self.detector = match languages.len() {
            0 | 1 => Detector::None,
            _ => Detector::Loading(Task::spawn_blocking(move || {
                LanguageDetectorBuilder::from_languages(&languages)
                    .with_minimum_relative_distance(MIN_RELATIVE_DISTANCE)
                    .with_preloaded_language_models()
                    .build()
            })),
        };
    }

    /// The language `text` is in, among the ones with a voice. `None` if it's unclear, if there
    /// aren't two languages to choose from, or while the models load.
    pub fn detect(&self, text: &str) -> Option<ArrayStr<2>> {
        match &self.detector {
            Detector::Ready(detector) => detector.detect_language_of(text).and_then(voice_language),
            Detector::None | Detector::Loading(_) => None,
        }
    }
}

/// The English name of the language with `code` (as the voices have it), if it's one of those
/// built in (see the `all-languages` feature).
pub fn name(code: ArrayStr<2>) -> Option<String> {
    let code = match &*code {
        // Norwegian's voices are Bokmål's
        "no" => "nb",
        code => code,
    };
    let iso = code.parse::<lingua::IsoCode639_1>().ok()?;
    Some(Language::from_iso_code_639_1(&iso).to_string())
}

/// The name of the language with `code` (as the voices have it), in the app's language: in
/// English for those the app hasn't translated, or its code if it isn't built in either.
pub fn ui_name(code: ArrayStr<2>) -> String {
    use lambiase_i18n::t;

    let name = match &*code {
        "en" => t!("lang.en"),
        "es" => t!("lang.es"),
        "fr" => t!("lang.fr"),
        "de" => t!("lang.de"),
        "it" => t!("lang.it"),
        "pt" => t!("lang.pt"),
        "nl" => t!("lang.nl"),
        "zh" => t!("lang.zh"),
        "ja" => t!("lang.ja"),
        "ko" => t!("lang.ko"),
        _ => return name(code).unwrap_or_else(|| code.to_string()),
    };
    name.to_owned()
}

/// The language's code as the voices have it: whisper's two-letter codes, which make
/// Norwegian's two written forms one.
fn voice_language(language: Language) -> Option<ArrayStr<2>> {
    let code = language.iso_code_639_1().to_string();
    let code = match code.as_str() {
        "nb" | "nn" => "no",
        code => code,
    };
    ArrayStr::from_str(code)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::arrstr;

    // Driven like the app does, from outside the runtime
    #[test]
    fn detects_typed_questions_among_the_voices() {
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let _entered = runtime.enter();
        let rt = runtime.handle();
        let voices = BTreeMap::from([
            (arrstr!("en"), Utf8PathBuf::from("en.onnx")),
            (arrstr!("es"), Utf8PathBuf::from("es.onnx")),
        ]);

        let mut detector = LanguageDetector::new();
        detector.update(rt, &voices);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
        while !matches!(detector.detector, Detector::Ready(_)) {
            assert!(std::time::Instant::now() < deadline, "models never loaded");
            std::thread::sleep(std::time::Duration::from_millis(10));
            detector.update(rt, &voices);
        }

        assert_eq!(detector.detect("How are the tyres looking?"), Some(arrstr!("en")));
        assert_eq!(detector.detect("¿Cuánto combustible me queda?"), Some(arrstr!("es")));
        assert_eq!(detector.detect("cuándo paro"), Some(arrstr!("es")));

        // Nothing to choose between with a single voice
        let one = BTreeMap::from([(arrstr!("en"), Utf8PathBuf::from("en.onnx"))]);
        detector.update(rt, &one);
        assert_eq!(detector.detect("¿Cuánto combustible me queda?"), None);
    }

    #[test]
    fn names_the_languages() {
        assert_eq!(name(arrstr!("es")).as_deref(), Some("Spanish"));
        assert_eq!(name(arrstr!("en")).as_deref(), Some("English"));
        // Not built in
        assert_eq!(name(arrstr!("xx")), None);
    }
}
