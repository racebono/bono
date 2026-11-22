use super::Streamed;

/// Puts a turn's text together and hands it out as it comes: blocks of text (e.g. what the model
/// wrote before and after its tool calls) are separated by blank lines, without the whitespace
/// they start with, and those with nothing to say are left out.
pub(super) struct TextSink<'a> {
    on_event: &'a mut (dyn FnMut(Streamed<'_>) + Send),
    /// Everything handed out
    pub text: String,
    /// Whitespace the current block started with, held until the block says something (and only
    /// said when it carries on from the last)
    pending: String,
    /// The current block said something
    started: bool,
    /// The next block carries on from the last, without a blank line between them
    carry_on: bool,
}

impl<'a> TextSink<'a> {
    pub fn new(on_event: &'a mut (dyn FnMut(Streamed<'_>) + Send)) -> Self {
        Self {
            on_event,
            text: String::new(),
            pending: String::new(),
            started: false,
            carry_on: false,
        }
    }

    /// A new response starts (or the same one again, retried).
    pub fn start_response(&mut self) {
        self.end_block();
        self.carry_on = false;
    }

    /// The next block carries on from the last: e.g. a model declined midway through it, and a
    /// fallback model continues where it left off.
    pub fn carry_on(&mut self) {
        self.carry_on = true;
    }

    /// More of the current text block.
    pub fn push(&mut self, text: &str) {
        if !self.started {
            if text.trim().is_empty() {
                self.pending.push_str(text);
                return;
            }

            self.started = true;
            let pending = std::mem::take(&mut self.pending);
            match std::mem::take(&mut self.carry_on) {
                // Mid-sentence, where a space matters
                true => self.emit(&pending),
                false => {
                    if !self.text.is_empty() && !self.text.ends_with("\n\n") {
                        self.emit("\n\n");
                    }
                    self.emit(text.trim_start());
                    return;
                }
            }
        }
        self.emit(text);
    }

    pub fn end_block(&mut self) {
        self.pending.clear();
        self.started = false;
    }

    /// Flush spoken text before waiting for client tools, even when the model omitted
    /// trailing whitespace. Do not wait until the next response to emit its separator. Then tells
    /// the tools are next, those with `names`.
    pub fn before_tools(&mut self, names: &[&str]) {
        self.end_block();
        if !self.text.is_empty() && !self.text.ends_with("\n\n") {
            self.emit("\n\n");
        }
        (self.on_event)(Streamed::Tools(names));
    }

    fn emit(&mut self, text: &str) {
        if !text.is_empty() {
            self.text.push_str(text);
            (self.on_event)(Streamed::Text(text));
        }
    }
}

/// Runs `conversation`'s turn for `text`, keeping what it hands out: a piece of text, or the
/// names of the tools it told were next.
#[cfg(test)]
pub(super) async fn send_streaming_events(
    conversation: &mut dyn super::Model,
    text: &str,
) -> (anyhow::Result<super::Reply>, Vec<Result<String, Vec<String>>>) {
    let mut events = Vec::new();
    let reply = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        conversation.send_streaming(text.into(), &mut |event| {
            events.push(match event {
                Streamed::Text(piece) => Ok(piece.to_owned()),
                Streamed::Tools(names) => Err(names.iter().map(|&name| name.to_owned()).collect()),
            })
        }),
    )
    .await
    .expect("timed out");
    (reply, events)
}

/// The text handed out before and after the one time `events` (see [`send_streaming_events`])
/// told tools were next, and which.
#[cfg(test)]
pub(super) fn around_tools(
    events: &[Result<String, Vec<String>>],
) -> (String, Vec<String>, String) {
    let tools = events.iter().filter(|event| event.is_err()).count();
    assert_eq!(tools, 1, "{events:?}");
    let at = events.iter().position(Result::is_err).unwrap();
    let text = |events: &[Result<String, Vec<String>>]| {
        events.iter().flatten().cloned().collect::<String>()
    };
    let names = events[at].clone().unwrap_err();
    (text(&events[..at]), names, text(&events[at + 1..]))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flushes_acknowledgement_before_tools_without_duplicate_separator() {
        let mut pieces = Vec::new();
        let mut callback = |event: Streamed<'_>| {
            if let Streamed::Text(text) = event {
                pieces.push(text.to_owned());
            }
        };
        let mut sink = TextSink::new(&mut callback);
        sink.push("Copy, checking.");
        sink.end_block();
        sink.before_tools(&["lookup"]);
        assert_eq!(sink.text, "Copy, checking.\n\n");
        sink.start_response();
        sink.push("The hairpin on entry.");
        assert_eq!(sink.text, "Copy, checking.\n\nThe hairpin on entry.");
        let text = sink.text.clone();
        drop(sink);
        assert_eq!(pieces.concat(), text);
    }

    #[test]
    fn tells_tools_are_next_after_the_text_before_them() {
        let mut events = Vec::new();
        let mut callback = |event: Streamed<'_>| {
            events.push(match event {
                Streamed::Text(text) => format!("text:{text}"),
                Streamed::Tools(names) => format!("tools:{}", names.join(",")),
            })
        };
        let mut sink = TextSink::new(&mut callback);
        sink.push("Copy, checking.");
        sink.end_block();
        sink.before_tools(&["lookup", "schedule"]);
        sink.start_response();
        sink.push("The hairpin on entry.");
        drop(sink);
        assert_eq!(
            events,
            [
                "text:Copy, checking.",
                "text:\n\n",
                "tools:lookup,schedule",
                "text:The hairpin on entry."
            ]
        );
    }

    #[test]
    fn silent_tools_add_no_spoken_preamble() {
        let mut pieces = Vec::new();
        let mut callback = |event: Streamed<'_>| {
            if let Streamed::Text(text) = event {
                pieces.push(text.to_owned());
            }
        };
        let mut sink = TextSink::new(&mut callback);
        sink.before_tools(&["lookup"]);
        sink.start_response();
        sink.push("Radio check, loud and clear.");
        assert_eq!(sink.text, "Radio check, loud and clear.");
    }
}
