fn sentence_len(text: &str) -> Option<usize> {
    let mut chars = text.char_indices().peekable();
    while let Some((i, c)) = chars.next() {
        let mut end = i + c.len_utf8();
        match c {
            // No space after these
            '\n' | '。' | '？' | '！' => return Some(end),
            '.' | '?' | '!' | '…' => {
                // Past closing quotes and brackets
                while let Some(&(j, q)) = chars.peek()
                    && matches!(q, '"' | '\'' | ')' | ']' | '”' | '’' | '»')
                {
                    end = j + q.len_utf8();
                    chars.next();
                }

                // Not "3.5", nor a sentence that may still be going
                if chars.peek().is_some_and(|&(_, next)| next.is_whitespace()) {
                    return Some(end);
                }
            }
            _ => {}
        }
    }

    return None;
}

#[test]
fn acknowledgement_is_ready_before_answer() {
    assert_eq!(sentence_len("Copy, checking.\n"), Some(15));
    assert_eq!(sentence_len("Copy, checking."), None);
    assert_eq!(sentence_len("Copy, checking.\nThe hairpin on entry."), Some(15));
}

