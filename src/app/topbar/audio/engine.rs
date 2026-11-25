//! Which speech to text engine hears the pilot: Parakeet, which runs on the processor and costs the
//! simulator no frames, for the languages it knows, and Whisper for the rest.

use crate::{config::ArrayStr, language};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Engine {
    Parakeet,
    Whisper,
}

/// Why an engine is wanted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Why {
    /// Parakeet: it knows every language in use
    Known(Vec<ArrayStr<2>>),
    /// Whisper: these are in use, and Parakeet doesn't know them
    Unknown(Vec<ArrayStr<2>>),
    /// Whisper: Parakeet doesn't say which of these it heard, and they can't be told apart from text
    Ambiguous(Vec<ArrayStr<2>>),
    /// Whisper: replies are always in one language, so any may be spoken
    AnyLanguage,
}

/// What Automatic does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Used {
    Engine(Engine),
    /// Not known yet: what's loaded stays
    Wait,
    /// No engine is on disk
    Nothing,
}

/// The languages Parakeet TDT 0.6B v3 knows.
const PARAKEET: [&str; 25] = [
    "bg", "hr", "cs", "da", "nl", "en", "et", "fi", "fr", "de", "el", "hu", "it", "lv", "lt", "mt",
    "pl", "pt", "ro", "sk", "sl", "es", "sv", "ru", "uk",
];

pub fn parakeet_knows(code: ArrayStr<2>) -> bool {
    PARAKEET.contains(&&*language::voice_code(code))
}

/// The engine wanted to hear the pilot, and why. `voices`: the languages with a voice, or getting
/// one. `reply`: the fixed reply language, if it's usable. `app`: the app's language.
pub fn wanted(
    heard: Option<ArrayStr<2>>,
    voices: &[ArrayStr<2>],
    reply: Option<ArrayStr<2>>,
    app: ArrayStr<2>,
) -> (Engine, Why) {
    let languages = match heard {
        Some(heard) => vec![heard],
        None if voices.is_empty() => vec![app],
        None => voices.to_vec(),
    };
    let languages = (languages.into_iter())
        .map(language::voice_code)
        .collect::<Vec<_>>();
    // Whisper hears every language then, not only those with a voice
    if languages.len() > 1 && reply.is_some() {
        return (Engine::Whisper, Why::AnyLanguage);
    }
    let unknown = (languages.iter().copied())
        .filter(|&lang| !parakeet_knows(lang))
        .collect::<Vec<_>>();
    if !unknown.is_empty() {
        return (Engine::Whisper, Why::Unknown(unknown));
    }
    if languages.len() > 1 && !languages.iter().all(|&lang| language::told_apart(lang)) {
        return (Engine::Whisper, Why::Ambiguous(languages));
    }
    (Engine::Parakeet, Why::Known(languages))
}

/// The engine Automatic uses, from the one wanted and whether each one's files are on disk (`None`:
/// not checked yet). The other one hears until the wanted one is there.
pub fn used(wanted: Engine, parakeet: Option<bool>, whisper: Option<bool>) -> Used {
    let (other, wanted_on_disk, other_on_disk) = match wanted {
        Engine::Parakeet => (Engine::Whisper, parakeet, whisper),
        Engine::Whisper => (Engine::Parakeet, whisper, parakeet),
    };
    match (wanted_on_disk, other_on_disk) {
        (Some(true), _) => Used::Engine(wanted),
        (Some(false), Some(true)) => Used::Engine(other),
        (Some(false), Some(false)) => Used::Nothing,
        (None, _) | (Some(false), None) => Used::Wait,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::arrstr;

    const EN: ArrayStr<2> = arrstr!("en");
    const ES: ArrayStr<2> = arrstr!("es");
    const FR: ArrayStr<2> = arrstr!("fr");
    const PL: ArrayStr<2> = arrstr!("pl");
    const CA: ArrayStr<2> = arrstr!("ca");
    const EU: ArrayStr<2> = arrstr!("eu");

    #[test]
    fn one_language_decides_alone() {
        let known = |lang| (Engine::Parakeet, Why::Known(vec![lang]));
        let unknown = |lang| (Engine::Whisper, Why::Unknown(vec![lang]));
        // The Heard language, whatever the voices
        assert_eq!(wanted(Some(ES), &[ES, CA, EU], None, EN), known(ES));
        assert_eq!(wanted(Some(CA), &[ES], None, EN), unknown(CA));
        // A lone voice's: there's nothing to tell apart
        assert_eq!(wanted(None, &[PL], None, EN), known(PL));
        assert_eq!(wanted(None, &[CA], None, EN), unknown(CA));
        // The app's, without any voice
        assert_eq!(wanted(None, &[], None, ES), known(ES));
        assert_eq!(wanted(None, &[], None, CA), unknown(CA));
    }

    #[test]
    fn several_languages_must_be_known_and_told_apart() {
        assert_eq!(
            wanted(None, &[ES, FR], None, EN),
            (Engine::Parakeet, Why::Known(vec![ES, FR]))
        );
        assert_eq!(
            wanted(None, &[CA, ES, EU], None, EN),
            (Engine::Whisper, Why::Unknown(vec![CA, EU]))
        );
        #[cfg(not(feature = "all-languages"))]
        assert_eq!(
            wanted(None, &[ES, PL], None, EN),
            (Engine::Whisper, Why::Ambiguous(vec![ES, PL]))
        );
    }

    #[test]
    fn a_fixed_reply_leaves_any_language_to_be_heard() {
        assert_eq!(
            wanted(None, &[EN, ES], Some(EN), EN),
            (Engine::Whisper, Why::AnyLanguage)
        );
        assert_eq!(
            wanted(Some(ES), &[EN, ES], Some(EN), EN).0,
            Engine::Parakeet
        );
        // With only one voice, it's heard in its language, as ever
        assert_eq!(wanted(None, &[EN], Some(EN), EN).0, Engine::Parakeet);
    }

    #[test]
    fn norwegian_is_one_language_parakeet_does_not_know() {
        let no = arrstr!("no");
        for code in [arrstr!("nb"), arrstr!("nn"), no] {
            assert_eq!(
                wanted(Some(code), &[], None, EN),
                (Engine::Whisper, Why::Unknown(vec![no]))
            );
        }
    }

    #[test]
    fn parakeet_knows_its_25_languages() {
        for code in
            "bg hr cs da nl en et fi fr de el hu it lv lt mt pl pt ro sk sl es sv ru uk".split(' ')
        {
            assert!(parakeet_knows(ArrayStr::from_str(code).unwrap()), "{code}");
        }
        for code in ["ca", "eu", "no", "ja", "zh", "tr"] {
            assert!(!parakeet_knows(ArrayStr::from_str(code).unwrap()), "{code}");
        }
    }

    #[test]
    fn the_other_engine_is_used_until_the_wanted_one_is_there() {
        use Engine::*;
        assert_eq!(
            used(Parakeet, Some(true), Some(false)),
            Used::Engine(Parakeet)
        );
        assert_eq!(used(Parakeet, Some(true), None), Used::Engine(Parakeet));
        assert_eq!(
            used(Parakeet, Some(false), Some(true)),
            Used::Engine(Whisper)
        );
        assert_eq!(
            used(Whisper, Some(true), Some(false)),
            Used::Engine(Parakeet)
        );
        assert_eq!(used(Whisper, Some(false), Some(false)), Used::Nothing);
        assert_eq!(used(Parakeet, None, Some(true)), Used::Wait);
        assert_eq!(used(Parakeet, Some(false), None), Used::Wait);
    }
}
