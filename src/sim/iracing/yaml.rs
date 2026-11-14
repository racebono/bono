//! The session info iRacing publishes: a YAML document of maps, lists of maps and one-line
//! values. Its values aren't quoted or escaped (driver and team names are written as they are, so
//! they can hold `: `, `#`, quotes...), which a YAML parser would misread or reject, so this reads
//! it line by line instead: a key is what comes before the first `: ` on its line, its value the
//! rest of the line.

/// A value of the document.
#[derive(Debug, Clone, PartialEq)]
pub enum Node {
    Scalar(String),
    Map(Vec<(String, Node)>),
    List(Vec<Node>),
}

impl Node {
    /// The value at `key`, in a map.
    pub fn get(&self, key: &str) -> Option<&Node> {
        match self {
            Node::Map(entries) => entries.iter().find(|(k, _)| k == key).map(|(_, v)| v),
            _ => None,
        }
    }

    /// The value down the keys of `path`.
    pub fn at(&self, path: &[&str]) -> Option<&Node> {
        path.iter().try_fold(self, |node, key| node.get(key))
    }

    /// A one-line value, as written (quotes removed).
    pub fn str(&self) -> Option<&str> {
        match self {
            Node::Scalar(value) => Some(value),
            _ => None,
        }
    }

    /// The value at `key` as text, if it's a non-empty one-line value.
    pub fn text(&self, key: &str) -> Option<&str> {
        self.get(key)?.str().filter(|value| !value.is_empty())
    }

    /// The value at `key` as a number: the first word of its text, so units (`4.05 km`) are left
    /// out.
    pub fn number(&self, key: &str) -> Option<f64> {
        number(self.text(key)?)
    }

    /// The value at `key` as a whole number.
    pub fn int(&self, key: &str) -> Option<i64> {
        self.text(key)?.split_whitespace().next()?.parse().ok()
    }

    /// A list's items; nothing for anything else.
    pub fn items(&self) -> &[Node] {
        match self {
            Node::List(items) => items,
            _ => &[],
        }
    }
}

/// The number a value starts with (`72.42 kph` is 72.42), if it does.
pub fn number(text: &str) -> Option<f64> {
    text.split_whitespace()
        .next()?
        .trim_end_matches('%')
        .parse::<f64>()
        .ok()
        .filter(|n| n.is_finite())
}

/// Windows-1252's characters for 0x80 to 0x9F (`None` where it has none).
#[rustfmt::skip]
const CP1252: [Option<char>; 32] = [
    Some('€'), None, Some('‚'), Some('ƒ'), Some('„'), Some('…'), Some('†'), Some('‡'),
    Some('ˆ'), Some('‰'), Some('Š'), Some('‹'), Some('Œ'), None, Some('Ž'), None,
    None, Some('‘'), Some('’'), Some('“'), Some('”'), Some('•'), Some('–'), Some('—'),
    Some('˜'), Some('™'), Some('š'), Some('›'), Some('œ'), None, Some('ž'), Some('Ÿ'),
];

/// The session info's text. The document says its own encoding (`WeekendInfo: Encoding:`), which
/// iRacing gives as ISO-8859-1: each byte is the character of the same code. Bytes 0x80 to 0x9F
/// are only control characters there, which names don't hold: they're read as Windows-1252's
/// letters and punctuation (`š`, `Ž`, curly quotes), which ISO-8859-1 has no room for, and as a
/// space where it has none. A document saying it's UTF-8 is read as UTF-8.
pub fn decode(bytes: &[u8]) -> String {
    let end = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());
    let bytes = &bytes[..end];
    let latin1: String = bytes
        .iter()
        .map(|&b| match b {
            0x80..=0x9F => CP1252[usize::from(b - 0x80)].unwrap_or(' '),
            _ => b as char,
        })
        .collect();
    let utf8 = parse(&latin1)
        .at(&["WeekendInfo", "Encoding"])
        .and_then(Node::str)
        .is_some_and(|encoding| {
            let encoding = encoding.to_ascii_lowercase().replace(['-', '_'], "");
            encoding == "utf8"
        });
    match utf8 {
        true => String::from_utf8_lossy(bytes).into_owned(),
        false => latin1,
    }
}

/// One line of the document.
struct Line<'a> {
    /// The column of its key (or value)
    indent: usize,
    /// The column of its `- `, for a list item's first line
    dash: Option<usize>,
    /// `None` for a line that's only a value (a list of values' item)
    key: Option<&'a str>,
    value: &'a str,
}

fn lines(text: &str) -> Vec<Line<'_>> {
    let mut out = Vec::new();
    for raw in text.lines() {
        let raw = raw.trim_end_matches('\r');
        let content = raw.trim_start_matches(' ');
        if content.trim().is_empty() || content == "---" || content == "..." {
            continue;
        }
        let mut indent = raw.len() - content.len();
        let mut dash = None;
        let mut content = content;
        if let Some(rest) = content.strip_prefix("- ").or(match content {
            "-" => Some(""),
            _ => None,
        }) {
            dash = Some(indent);
            let trimmed = rest.trim_start_matches(' ');
            indent += 2 + (rest.len() - trimmed.len());
            content = trimmed;
        }
        let (key, value) = match content.split_once(": ") {
            Some((key, value)) => (Some(key.trim()), value.trim()),
            None => match content.trim_end().strip_suffix(':') {
                Some(key) => (Some(key.trim()), ""),
                None => (None, content.trim()),
            },
        };
        out.push(Line {
            indent,
            dash,
            key,
            value: unquote(value),
        });
    }
    out
}

/// A value without the double quotes iRacing puts around some (`"1"`). A value with quotes inside
/// is a name as written (`"Speedy" Bob "Jones"`).
fn unquote(value: &str) -> &str {
    value
        .strip_prefix('"')
        .and_then(|v| v.strip_suffix('"'))
        .filter(|inner| !inner.contains('"'))
        .unwrap_or(value)
}

/// Reads the document. Lines that don't fit where they are (indented under nothing) are skipped.
pub fn parse(text: &str) -> Node {
    let lines = lines(text);
    let mut pos = 0;
    let mut entries = Vec::new();
    while pos < lines.len() {
        let indent = lines[pos].indent;
        let before = pos;
        entries.extend(map(&lines, &mut pos, indent));
        if pos == before {
            pos += 1;
        }
    }
    Node::Map(entries)
}

/// The entries of a map whose keys are at `indent`, from `pos` on.
fn map(lines: &[Line<'_>], pos: &mut usize, indent: usize) -> Vec<(String, Node)> {
    let mut entries = Vec::new();
    while let Some(line) = lines.get(*pos) {
        if line.dash.is_some() || line.indent < indent {
            break;
        }
        *pos += 1;
        let Some(key) = line.key.filter(|_| line.indent == indent) else {
            // Indented under nothing, or a bare value in a map
            continue;
        };
        let value = match line.value.is_empty() {
            true => child(lines, pos, indent),
            false => Node::Scalar(line.value.to_owned()),
        };
        entries.push((key.to_owned(), value));
    }
    entries
}

/// What's under a key at `indent` with no value on its line: a list, a map, or nothing.
fn child(lines: &[Line<'_>], pos: &mut usize, indent: usize) -> Node {
    match lines.get(*pos) {
        Some(Line {
            dash: Some(dash), ..
        }) if *dash >= indent => Node::List(list(lines, pos, *dash)),
        Some(line) if line.dash.is_none() && line.indent > indent => {
            Node::Map(map(lines, pos, line.indent))
        }
        _ => Node::Scalar(String::new()),
    }
}

/// The items of a list whose `- ` are at `dash`, from `pos` on.
fn list(lines: &[Line<'_>], pos: &mut usize, dash: usize) -> Vec<Node> {
    let mut items = Vec::new();
    while let Some(line) = lines.get(*pos) {
        if line.dash != Some(dash) {
            break;
        }
        *pos += 1;
        let Some(key) = line.key else {
            items.push(Node::Scalar(line.value.to_owned()));
            continue;
        };
        let first = match line.value.is_empty() {
            true => child(lines, pos, line.indent),
            false => Node::Scalar(line.value.to_owned()),
        };
        let mut entries = vec![(key.to_owned(), first)];
        entries.extend(map(lines, pos, line.indent));
        items.push(Node::Map(entries));
    }
    items
}

#[cfg(test)]
mod tests {
    use super::*;

    const DOC: &str = "---
WeekendInfo:
 TrackName: roadatlanta full
 TrackLength: 4.0569 km
 WeekendOptions:
  NumStarters: 20
  IncidentLimit: unlimited
 TelemetryOptions:
  TelemetryDiskFile: \"\"

SessionInfo:
 CurrentSessionNum: 2
 Sessions:
 - SessionNum: 0
   SessionType: Practice
   ResultsPositions:
   - Position: 1
     CarIdx: 4
   - Position: 2
     CarIdx: 0
   ResultsFastestLap:
   - CarIdx: 4
     FastestTime: 95.2254
 - SessionNum: 1
   SessionType: Race
   ResultsPositions:
DriverInfo:
 Drivers:
 - CarIdx: 0
   UserName: Team: \"Fast\" #1 Racing
   CarNumber: \"007\"
 - CarIdx: 1
   UserName: Ana Müller
...
";

    #[test]
    fn reads_maps_lists_and_values_as_written() {
        let doc = parse(DOC);
        assert_eq!(
            doc.at(&["WeekendInfo", "TrackName"]).and_then(Node::str),
            Some("roadatlanta full")
        );
        let weekend = doc.get("WeekendInfo").unwrap();
        assert_eq!(weekend.number("TrackLength"), Some(4.0569));
        assert_eq!(
            weekend.at(&["WeekendOptions", "NumStarters"]).unwrap(),
            &Node::Scalar("20".into())
        );
        assert_eq!(
            weekend.at(&["TelemetryOptions", "TelemetryDiskFile"]),
            Some(&Node::Scalar(String::new()))
        );

        let sessions = doc.at(&["SessionInfo", "Sessions"]).unwrap().items();
        assert_eq!(sessions.len(), 2);
        assert_eq!(sessions[0].text("SessionType"), Some("Practice"));
        let results = sessions[0].get("ResultsPositions").unwrap().items();
        assert_eq!(results.len(), 2);
        assert_eq!(results[1].int("CarIdx"), Some(0));
        assert_eq!(
            sessions[0].get("ResultsFastestLap").unwrap().items()[0].number("FastestTime"),
            Some(95.2254)
        );
        // An empty list is an empty value
        assert_eq!(sessions[1].text("SessionType"), Some("Race"));
        assert!(
            sessions[1]
                .get("ResultsPositions")
                .unwrap()
                .items()
                .is_empty()
        );

        // Names are the rest of the line, whatever's in them
        let drivers = doc.at(&["DriverInfo", "Drivers"]).unwrap().items();
        assert_eq!(
            drivers[0].text("UserName"),
            Some("Team: \"Fast\" #1 Racing")
        );
        assert_eq!(drivers[0].text("CarNumber"), Some("007"));
        assert_eq!(drivers[1].text("UserName"), Some("Ana Müller"));
    }

    #[test]
    fn decodes_the_encoding_the_document_says() {
        let latin1 =
            b"WeekendInfo:\n Encoding: ISO_8859_1\nDriverInfo:\n Name: Ana M\xfcller\n\0junk";
        let text = decode(latin1);
        assert_eq!(
            parse(&text).at(&["DriverInfo", "Name"]).and_then(Node::str),
            Some("Ana Müller")
        );
        // Windows-1252's letters and quotes, and a space for what it hasn't got
        let cp1252 = b"DriverInfo:\n Name: Tom\xe1\x9a \x93Fast\x94\x81\n";
        assert_eq!(
            parse(&decode(cp1252))
                .at(&["DriverInfo", "Name"])
                .and_then(Node::str),
            Some("Tomáš “Fast”")
        );
        assert_eq!(
            unquote(r#""Speedy" Bob "Jones""#),
            r#""Speedy" Bob "Jones""#
        );
        assert_eq!(unquote(r#""007""#), "007");

        let utf8 = "WeekendInfo:\n Encoding: UTF-8\nDriverInfo:\n Name: Ana Müller\n";
        assert_eq!(
            parse(&decode(utf8.as_bytes()))
                .at(&["DriverInfo", "Name"])
                .and_then(Node::str),
            Some("Ana Müller")
        );
    }

    #[test]
    fn skips_lines_indented_under_nothing() {
        let doc = parse("A:\n B: 1\n   stray\n C: 2\nD: 3\n");
        assert_eq!(doc.at(&["A", "C"]).and_then(Node::str), Some("2"));
        assert_eq!(doc.text("D"), Some("3"));
    }
}
