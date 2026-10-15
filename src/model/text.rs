/// Puts a turn's text together and hands it out as it comes: blocks of text (e.g. what the model
/// wrote before and after its tool calls) are separated by blank lines, without the whitespace
/// they start with, and those with nothing to say are left out.
pub(super) struct TextSink<'a> {
    on_text: &'a mut (dyn FnMut(&str) + Send),
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
    pub fn new(on_text: &'a mut (dyn FnMut(&str) + Send)) -> Self {
        Self {
            on_text,
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
                    if !self.text.is_empty() {
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

    fn emit(&mut self, text: &str) {
        if !text.is_empty() {
            self.text.push_str(text);
            (self.on_text)(text);
        }
    }
}
