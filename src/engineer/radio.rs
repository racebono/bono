//! Conservative local replies: match a whole utterance, never a keyword in a request.

/// Unknown languages and mixed requests go through the ordinary engineer path.
pub fn reply(question: &str, language: &str) -> Option<&'static str> {
    if question.len() > 128 {
        return None;
    }
    let question = question
        .trim()
        .trim_end_matches(['.', '!', '?'])
        .to_lowercase();
    let radio = matches!(
        question.as_str(),
        "radio check"
            | "can you hear me"
            | "do you copy"
            | "prueba de radio"
            | "me oyes"
            | "mi senti"
            | "prova radio"
            | "test radio"
            | "tu m'entends"
            | "funkcheck"
            | "hörst du mich"
    );
    let acknowledgement = matches!(
        question.as_str(),
        "copy"
            | "okay"
            | "ok"
            | "understood"
            | "thanks"
            | "thank you"
            | "recibido"
            | "entendido"
            | "gracias"
            | "ricevuto"
            | "capito"
            | "grazie"
            | "compris"
            | "reçu"
            | "merci"
            | "verstanden"
            | "danke"
    );
    if !radio && !acknowledgement {
        return None;
    }
    Some(match (language, radio) {
        ("en", true) => "Loud and clear.",
        ("en", false) => "Copy.",
        ("es", true) => "Te oigo alto y claro.",
        ("es", false) => "Recibido.",
        ("it", true) => "Ti sento forte e chiaro.",
        ("it", false) => "Ricevuto.",
        ("fr", true) => "Je te reçois fort et clair.",
        ("fr", false) => "Reçu.",
        ("de", true) => "Laut und deutlich.",
        ("de", false) => "Verstanden.",
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::reply;

    #[test]
    fn replies_in_the_requested_language() {
        assert_eq!(
            reply(" Radio check?!", "it"),
            Some("Ti sento forte e chiaro.")
        );
        assert_eq!(reply("Gracias.", "es"), Some("Recibido."));
        assert_eq!(reply("copy", "ja"), None);
    }

    #[test]
    fn never_swallows_a_request() {
        for question in [
            "Copy, how much fuel?",
            "Thanks, check damage",
            "radio check and position",
            "Can you copy my setup?",
            "okay then",
        ] {
            assert_eq!(reply(question, "en"), None, "{question}");
        }
    }
}
