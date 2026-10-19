//! Text to IPA phonemes with espeak-ng, the way Piper voices are trained on.
//!
//! espeak-ng is GPL, so it lives in this separate program, and Lambiase talks to it through
//! its stdin and stdout:
//!
//! - `lambiase-phonemizer --voice <voice> <text>...` prints the text's phonemes, one sentence
//!   per line.
//! - `lambiase-phonemizer --serve` answers requests, one JSON object per line, until stdin
//!   closes: `{"voice": "en-us", "text": "Hello."}` gets `{"sentences": ["həlˈoʊ."]}`, or
//!   `{"error": "..."}` if it couldn't.

use anyhow::{Context, bail};
use espeak_rs_sys::{
    espeak_AUDIO_OUTPUT_AUDIO_OUTPUT_SYNCHRONOUS, espeak_ERROR_EE_OK, espeak_Initialize,
    espeak_SetVoiceByName, espeak_TextToPhonemes, espeakCHARS_UTF8, espeakINITIALIZE_DONT_EXIT,
    espeakPHONEMES_IPA,
};
use serde::{Deserialize, Serialize};
use std::{
    ffi::{CStr, CString, c_int, c_void},
    io::{BufRead, Write},
    path::{Path, PathBuf},
    process::ExitCode,
    time::SystemTime,
};

const USAGE: &str = "usage: lambiase-phonemizer --voice <voice> <text>...\n       \
                     lambiase-phonemizer --serve";

#[derive(Deserialize)]
struct Request {
    voice: String,
    text: String,
}

#[derive(Serialize)]
#[serde(rename_all = "snake_case")]
enum Response {
    Sentences(Vec<String>),
    Error(String),
}

fn main() -> ExitCode {
    let args = std::env::args().skip(1).collect::<Vec<_>>();
    let res = match args.iter().map(String::as_str).collect::<Vec<_>>().as_slice() {
        ["--serve"] => serve(),
        ["--voice", voice, text @ ..] if !text.is_empty() => print(voice, &text.join(" ")),
        _ => {
            eprintln!("{USAGE}");
            return ExitCode::from(2);
        }
    };

    match res {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("lambiase-phonemizer: {e:#}");
            ExitCode::FAILURE
        }
    }
}

fn print(voice: &str, text: &str) -> anyhow::Result<()> {
    init_espeak()?;
    for sentence in phonemize(text, voice)? {
        println!("{sentence}");
    }
    Ok(())
}

fn serve() -> anyhow::Result<()> {
    // Failing to start is an answer too, so the first request learns why
    let init = init_espeak().map_err(|e| format!("{e:#}"));
    let mut stdout = std::io::stdout().lock();

    for line in std::io::stdin().lock().lines() {
        let line = line.context("Error reading a request")?;
        if line.trim().is_empty() {
            continue;
        }

        let response = match (&init, serde_json::from_str::<Request>(&line)) {
            (Err(e), _) => Response::Error(e.clone()),
            (Ok(()), Err(e)) => Response::Error(format!("Invalid request: {e}")),
            (Ok(()), Ok(request)) => match phonemize(&request.text, &request.voice) {
                Ok(sentences) => Response::Sentences(sentences),
                Err(e) => Response::Error(format!("{e:#}")),
            },
        };

        serde_json::to_writer(&mut stdout, &response)?;
        // Lambiase waits for the whole line, so it can't sit in a buffer
        stdout.write_all(b"\n")?;
        stdout.flush()?;
    }

    Ok(())
}

/// Converts `text` to IPA phonemes, one string per sentence. espeak-ng keeps global state
/// (current voice, clause decoder), which is fine: this program does one request at a time.
fn phonemize(text: &str, voice: &str) -> anyhow::Result<Vec<String>> {
    let c_voice = CString::new(voice).context("The voice contains a null byte")?;
    if unsafe { espeak_SetVoiceByName(c_voice.as_ptr()) } != espeak_ERROR_EE_OK {
        bail!("Unknown espeak-ng voice '{voice}'");
    }

    // espeak would stop without advancing through it. This also checks a voice exists.
    if text.trim().is_empty() {
        return Ok(Vec::new());
    }

    let c_text = CString::new(text).context("Text contains a null byte")?;
    let base = c_text.as_ptr().cast::<c_void>();
    let mut ptr = base;

    let mut sentences = Vec::new();
    let mut sentence = String::new();

    // espeak translates one clause per call, advancing `ptr` past it (null once done)
    while !ptr.is_null() {
        let start = unsafe { ptr.byte_offset_from_unsigned(base) };
        let phonemes = unsafe {
            espeak_TextToPhonemes(
                &mut ptr,
                espeakCHARS_UTF8 as c_int,
                espeakPHONEMES_IPA as c_int,
            )
        };

        if phonemes.is_null() {
            bail!("espeak-ng failed to translate text");
        }

        let end = match ptr.is_null() {
            true => text.len(),
            false => unsafe { ptr.byte_offset_from_unsigned(base) },
        };
        if end <= start {
            bail!("espeak-ng stopped advancing through the text");
        }

        push_without_lang_switches(&mut sentence, unsafe { CStr::from_ptr(phonemes) });

        // Unlike piper-phonemize's patched espeak-ng, upstream doesn't report the clause's
        // punctuation, but the voices are trained with it, so recover it from the source text.
        // Before the end of the text, espeak has read one character of the next clause ahead
        // (e.g. "Hello, w"), which it carries over to the next call.
        let mut clause = text.get(start..end).unwrap_or_default();
        if !ptr.is_null() {
            let mut chars = clause.chars();
            chars.next_back();
            clause = chars.as_str();
        }

        match clause_terminator(clause) {
            Some(c @ ('.' | '?' | '!')) => {
                sentence.push(c);
                sentences.push(std::mem::take(&mut sentence));
            }
            Some(c) => {
                sentence.push(c);
                sentence.push(' ');
            }
            None if !sentence.is_empty() && !sentence.ends_with(' ') => sentence.push(' '),
            None => {}
        }
    }

    if !sentence.trim().is_empty() {
        sentences.push(sentence);
    }

    return Ok(sentences);
}

/// The punctuation that ends a clause, normalized to ASCII.
fn clause_terminator(clause: &str) -> Option<char> {
    let clause = clause.trim_end_matches(|c: char| {
        c.is_whitespace() || matches!(c, '"' | '\'' | ')' | ']' | '}' | '”' | '’' | '»')
    });

    return match clause.chars().next_back()? {
        c @ ('.' | '?' | '!' | ',' | ':' | ';') => Some(c),
        '。' | '．' | '…' => Some('.'),
        '？' => Some('?'),
        '！' => Some('!'),
        '，' | '、' => Some(','),
        '：' => Some(':'),
        '；' => Some(';'),
        _ => None,
    };
}

/// Appends espeak's phonemes, dropping the `(en)`-style markers it adds when it switches
/// language mid-clause.
fn push_without_lang_switches(dest: &mut String, phonemes: &CStr) {
    let mut depth = 0usize;
    for c in phonemes.to_string_lossy().chars() {
        match c {
            '(' => depth += 1,
            ')' => depth = depth.saturating_sub(1),
            _ if depth == 0 => dest.push(c),
            _ => {}
        }
    }
}

fn init_espeak() -> anyhow::Result<()> {
    let data = espeak_data_dir().context(
        "Couldn't find espeak-ng's data ('espeak-ng-data' next to the executable or in the build \
         directory)",
    )?;

    let data = data
        .to_str()
        .context("espeak-ng's data path isn't valid UTF-8")?;
    let data = CString::new(data)?;

    let sample_rate = unsafe {
        espeak_Initialize(
            espeak_AUDIO_OUTPUT_AUDIO_OUTPUT_SYNCHRONOUS,
            0,
            data.as_ptr(),
            espeakINITIALIZE_DONT_EXIT as c_int,
        )
    };

    if sample_rate <= 0 {
        bail!("Error initializing espeak-ng (code {sample_rate})");
    }
    return Ok(());
}

/// The directory *containing* `espeak-ng-data`, which is what espeak-ng expects.
///
/// Distributions ship it next to the executable. During development it's only in
/// espeak-rs-sys's build output, next to the executable (or its parent for test binaries).
fn espeak_data_dir() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let exe_dir = exe.parent()?;

    if exe_dir.join("espeak-ng-data").is_dir() {
        return Some(exe_dir.to_path_buf());
    }

    return exe_dir.ancestors().take(2).find_map(newest_built_data);
}

/// The most recently built `espeak-rs-sys-*/out/share` under `{dir}/build`.
fn newest_built_data(dir: &Path) -> Option<PathBuf> {
    let mut newest = None::<(SystemTime, PathBuf)>;

    for entry in std::fs::read_dir(dir.join("build")).ok()?.flatten() {
        if !entry
            .file_name()
            .to_string_lossy()
            .starts_with("espeak-rs-sys-")
        {
            continue;
        }

        let share = entry.path().join("out").join("share");
        let Ok(modified) = share
            .join("espeak-ng-data")
            .join("phondata")
            .metadata()
            .and_then(|metadata| metadata.modified())
        else {
            continue;
        };

        if newest.as_ref().is_none_or(|(time, _)| modified > *time) {
            newest = Some((modified, share));
        }
    }

    return newest.map(|(_, share)| share);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn phonemizes_sentences() {
        init_espeak().unwrap();
        let sentences =
            phonemize("Hello, world. It costs 3.5 dollars; really! Does it?", "en-us").unwrap();

        println!("{sentences:#?}");
        assert_eq!(sentences.len(), 3);
        assert!(sentences[0].contains(", ") && sentences[0].ends_with('.'));
        assert!(sentences[1].contains("; ") && sentences[1].ends_with('!'));
        assert!(sentences[2].ends_with('?'));

        assert!(phonemize("  ", "en-us").unwrap().is_empty());
        assert!(phonemize("Hello.", "not-a-voice").is_err());
    }
}
