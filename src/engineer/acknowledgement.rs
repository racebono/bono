//! What the pilot hears of a turn while its first word is late: the app says a short
//! acknowledgement ("Copiado, lo miro.") once [`super::ACKNOWLEDGE_AFTER`] passes with nothing
//! said, and the model's own acknowledgement, if it then writes one, isn't said again.

use bono_i18n::t;

/// A turn's acknowledgement: what of the model's text is said, around the app's own. Each method
/// returns what to say now, if anything, as the model's text is said.
#[derive(Debug, Default)]
pub struct Acknowledgement {
    state: State,
    /// The model's client tools ran, with nothing said before them
    tools_ran: bool,
    /// What the model wrote since the app acknowledged, not said yet: its acknowledgement, or the
    /// start of its answer
    held: String,
    /// Everything said since the app acknowledged, the acknowledgement included
    heard: Option<String>,
    /// The model looked something up, and wrote nothing since
    unanswered: bool,
}

#[derive(Debug, Default, PartialEq, Eq)]
enum State {
    /// Nothing said yet
    #[default]
    Waiting,
    /// The app acknowledged before any tools ran: the model's text is held until it shows
    /// whether it's an acknowledgement (tools follow it) or the answer (more text does)
    Acknowledged,
    /// Whatever the model writes is said as it comes
    Passed,
}

impl Acknowledgement {
    pub fn new() -> Self {
        Self::default()
    }

    /// The model wrote `text`.
    pub fn text(&mut self, text: &str) -> Option<String> {
        if !text.trim().is_empty() {
            self.unanswered = false;
        }
        match self.state {
            State::Waiting => {
                self.state = State::Passed;
                Some(text.to_owned())
            }
            State::Acknowledged => {
                self.held.push_str(text);
                // Something to say after its first line: that line began the answer
                let (_, after) = self.held.split_once('\n')?;
                if after.trim().is_empty() {
                    return None;
                }
                self.state = State::Passed;
                let held = std::mem::take(&mut self.held);
                self.say(held)
            }
            State::Passed => self.say(text.to_owned()),
        }
    }

    /// The model's client tools are about to run: `queried`, if any of them looks something up.
    /// What it wrote before a query is its acknowledgement; what it wrote before tools that only
    /// act (a `schedule` or `pit` call, confirmed once made) answered.
    pub fn tools(&mut self, queried: bool) -> Option<String> {
        self.unanswered |= queried;
        match self.state {
            State::Waiting => {
                self.tools_ran = true;
                None
            }
            State::Acknowledged => {
                self.state = State::Passed;
                let held = std::mem::take(&mut self.held);
                match queried || held.is_empty() {
                    // The pilot heard the app's acknowledgement instead
                    true => None,
                    false => self.say(held),
                }
            }
            State::Passed => None,
        }
    }

    /// The timer fired: `phrase` is said, if nothing was yet.
    pub fn fire(&mut self, phrase: &str) -> Option<String> {
        if self.state != State::Waiting {
            return None;
        }
        // After a tool result, the model writes its answer, never an acknowledgement
        self.state = match self.tools_ran {
            true => State::Passed,
            false => State::Acknowledged,
        };
        self.heard = Some(String::new());
        self.say(format!("{phrase}\n"))
    }

    /// The turn ended, however it ended: what was held is said.
    pub fn end(&mut self) -> Option<String> {
        let state = std::mem::replace(&mut self.state, State::Passed);
        let held = std::mem::take(&mut self.held);
        match state == State::Acknowledged && !held.is_empty() {
            true => self.say(held),
            false => None,
        }
    }

    /// Everything the pilot heard, if the app acknowledged: what the chat shows.
    pub fn heard(&self) -> Option<&str> {
        self.heard.as_deref()
    }

    /// Whether the model's last query has no answer: it wrote nothing after it, so what it wrote
    /// before only acknowledged.
    pub fn unanswered(&self) -> bool {
        self.unanswered
    }

    fn say(&mut self, text: String) -> Option<String> {
        if let Some(heard) = &mut self.heard {
            heard.push_str(&text);
        }
        Some(text)
    }
}

/// The app's acknowledgement in the voice language `lang` (a voice code, e.g. `es` or `no`): a
/// short, common phrase for "received, I'll check", hard to get wrong. `None` for a language
/// without one, which isn't acknowledged: another language's words can't be understood in its
/// voice.
pub fn phrase(lang: &str) -> Option<&'static str> {
    let phrase = t!("radio.acknowledgement": lang);
    // A language without its own is given English's
    (lang == "en" || phrase != t!("radio.acknowledgement": "en")).then_some(phrase)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The voice codes of every language the app tells apart (`lingua`'s, by default and with
    /// `all-languages`), as [`crate::language::voice_code`] has them
    const VOICE_LANGUAGES: &[&str] = &[
        "ar", "bg", "bn", "ca", "cs", "cy", "da", "de", "el", "en", "es", "et", "eu", "fa", "fi",
        "fr", "he", "hi", "hu", "hy", "id", "is", "it", "ja", "ka", "kk", "ko", "lt", "lv", "mr",
        "nl", "no", "pl", "pt", "ro", "ru", "sk", "sl", "sq", "sr", "sv", "sw", "te", "th", "tr",
        "uk", "ur", "vi", "zh",
        // Voices Piper has that `lingua` doesn't tell apart (Whisper does)
        "ku", "lb", "ml", "ne",
    ];

    #[test]
    fn passes_text_when_the_model_speaks_first() {
        let mut ack = Acknowledgement::new();
        assert_eq!(ack.text("Delanteros bien.").as_deref(), Some("Delanteros bien."));
        assert_eq!(ack.fire("Copiado."), None);
        assert_eq!(ack.end(), None);
        assert_eq!(ack.heard(), None);
    }

    #[test]
    fn drops_the_models_late_acknowledgement() {
        let mut ack = Acknowledgement::new();
        assert_eq!(ack.fire("Copiado.").as_deref(), Some("Copiado.\n"));
        assert_eq!(ack.text("Copio, lo compruebo.\n"), None);
        // Claude's separator before tools is only whitespace
        assert_eq!(ack.text("\n\n"), None);
        ack.tools(true);
        assert_eq!(ack.text("Delanteros bien.").as_deref(), Some("Delanteros bien."));
        assert_eq!(ack.end(), None);
        assert_eq!(ack.heard(), Some("Copiado.\nDelanteros bien."));
    }

    #[test]
    fn releases_held_text_once_the_answer_carries_on() {
        let mut ack = Acknowledgement::new();
        ack.fire("Copiado.");
        assert_eq!(ack.text("Los delanteros"), None);
        assert_eq!(
            ack.text(" van bien.\nLos traseros").as_deref(),
            Some("Los delanteros van bien.\nLos traseros")
        );
        assert_eq!(ack.text(" también.").as_deref(), Some(" también."));
        ack.tools(true);
        assert_eq!(ack.text("Y el resto.").as_deref(), Some("Y el resto."));
    }

    #[test]
    fn keeps_an_answer_written_before_a_quiet_tool() {
        let mut ack = Acknowledgement::new();
        ack.fire("Copiado.");
        assert_eq!(ack.text("Delanteros bien.\n"), None);
        // A `schedule` or `pit` call needs no acknowledgement: what came before it answered
        assert_eq!(ack.tools(false).as_deref(), Some("Delanteros bien.\n"));
        assert_eq!(ack.text("Te aviso.").as_deref(), Some("Te aviso."));
        assert_eq!(ack.heard(), Some("Copiado.\nDelanteros bien.\nTe aviso."));
    }

    #[test]
    fn releases_a_one_line_answer_when_the_turn_ends() {
        let mut ack = Acknowledgement::new();
        ack.fire("Copiado.");
        assert_eq!(ack.text("Delanteros bien."), None);
        assert_eq!(ack.end().as_deref(), Some("Delanteros bien."));
        assert_eq!(ack.heard(), Some("Copiado.\nDelanteros bien."));
    }

    #[test]
    fn a_silent_tool_call_keeps_the_timer_and_holds_nothing() {
        let mut ack = Acknowledgement::new();
        ack.tools(true);
        assert_eq!(ack.fire("Copiado.").as_deref(), Some("Copiado.\n"));
        // Written after a tool result: the answer, never an acknowledgement
        assert_eq!(
            ack.text("Entramos esta vuelta.").as_deref(),
            Some("Entramos esta vuelta.")
        );
    }

    #[test]
    fn a_turn_that_ended_is_never_acknowledged() {
        let mut ack = Acknowledgement::new();
        assert_eq!(ack.end(), None);
        assert_eq!(ack.fire("Copiado."), None);
        assert_eq!(ack.heard(), None);
    }

    #[test]
    fn only_text_after_the_last_query_answers() {
        // Whether the app acknowledged first or not
        for fires in [true, false] {
            let mut ack = Acknowledgement::new();
            if fires {
                ack.fire("Copiado.");
            }
            ack.text("Copio, lo compruebo.\n");
            assert!(!ack.unanswered(), "it looked nothing up");
            ack.tools(true);
            assert!(ack.unanswered());
            // Nothing to say: only whitespace, and tools that only act
            ack.text("\n\n");
            ack.tools(false);
            assert!(ack.unanswered());
            ack.text("Delanteros bien.");
            assert!(!ack.unanswered());
            ack.tools(false);
            assert!(!ack.unanswered(), "what came before tools that only act answered");
            // It looks up more
            ack.tools(true);
            assert!(ack.unanswered());
            ack.text("Traseros también.");
            ack.end();
            assert!(!ack.unanswered());
        }
    }

    #[test]
    fn has_a_phrase_for_every_voice_language() {
        for lang in VOICE_LANGUAGES {
            assert!(phrase(lang).is_some_and(|p| !p.trim().is_empty()), "{lang}");
        }
    }

    #[test]
    fn has_no_phrase_for_an_unknown_language() {
        assert_eq!(phrase("en"), Some("Copy, I'll check."));
        assert_eq!(phrase("xx"), None);
    }
}
