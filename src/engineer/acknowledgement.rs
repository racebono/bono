//! What the pilot hears of a turn while its first word is late: the app says a short
//! acknowledgement ("Copiado.") once [`super::ACKNOWLEDGE_AFTER`] passes with nothing said, and
//! the model's own acknowledgement, if it then writes one, isn't said again.

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

    /// The model's client tools are about to run.
    pub fn tools(&mut self) {
        match self.state {
            State::Waiting => self.tools_ran = true,
            // What it wrote before them was its acknowledgement: the pilot heard the app's
            State::Acknowledged => {
                self.held.clear();
                self.state = State::Passed;
            }
            State::Passed => {}
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

    fn say(&mut self, text: String) -> Option<String> {
        if let Some(heard) = &mut self.heard {
            heard.push_str(&text);
        }
        Some(text)
    }
}

/// The app's acknowledgement in the voice language `lang` (a voice code, e.g. `es` or `no`): a
/// short, common word for "received", hard to get wrong. `None` for a language without one, which
/// isn't acknowledged: another language's word can't be understood in its voice.
pub fn phrase(lang: &str) -> Option<&'static str> {
    Some(match lang {
        "ar" => "مفهوم.",
        "bg" => "Разбрано.",
        "bn" => "বুঝেছি।",
        "ca" => "Copiat.",
        "cs" => "Rozumím.",
        "cy" => "Wedi derbyn.",
        "da" => "Modtaget.",
        "de" => "Verstanden.",
        "el" => "Ελήφθη.",
        "en" => "Copy.",
        "es" => "Copiado.",
        "et" => "Selge.",
        "eu" => "Ulertuta.",
        "fa" => "دریافت شد.",
        "fi" => "Selvä.",
        "fr" => "Reçu.",
        "he" => "קיבלתי.",
        "hi" => "ठीक है।",
        "hu" => "Vettem.",
        "hy" => "Հասկացա։",
        "id" => "Diterima.",
        "is" => "Móttekið.",
        "it" => "Ricevuto.",
        "ja" => "了解。",
        "ka" => "გასაგებია.",
        "kk" => "Түсінікті.",
        "ko" => "알겠습니다.",
        "lt" => "Supratau.",
        "lv" => "Sapratu.",
        "mr" => "समजलं.",
        "nl" => "Begrepen.",
        "no" => "Oppfattet.",
        "pl" => "Zrozumiano.",
        "pt" => "Copiado.",
        "ro" => "Recepționat.",
        "ru" => "Принято.",
        "sk" => "Rozumiem.",
        "sl" => "Razumem.",
        "sq" => "Kuptova.",
        "sr" => "Примљено.",
        "sv" => "Uppfattat.",
        "sw" => "Nimepokea.",
        "te" => "అర్థమైంది.",
        "th" => "รับทราบ",
        "tr" => "Anlaşıldı.",
        "uk" => "Прийнято.",
        "ur" => "ٹھیک ہے۔",
        "vi" => "Đã rõ.",
        "zh" => "收到。",
        _ => return None,
    })
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
        ack.tools();
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
        ack.tools();
        assert_eq!(ack.text("Y el resto.").as_deref(), Some("Y el resto."));
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
        ack.tools();
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
    fn has_a_phrase_for_every_voice_language() {
        for lang in VOICE_LANGUAGES {
            assert!(phrase(lang).is_some_and(|p| !p.trim().is_empty()), "{lang}");
        }
    }

    #[test]
    fn has_no_phrase_for_an_unknown_language() {
        assert_eq!(phrase("lb"), None);
        assert_eq!(phrase("xx"), None);
    }
}
