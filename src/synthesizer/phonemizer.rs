//! Talks to `bono-phonemizer`, the separate program that turns text into phonemes with
//! espeak-ng (which is GPL, so it isn't linked into Bono).
//!
//! Every voice shares one phonemizer process, started on first use and again if it dies. It
//! answers one request at a time, so callers take turns.

use anyhow::{Context, bail};
use serde::{Deserialize, Serialize};
use std::{
    io::{BufRead, BufReader, Write},
    path::PathBuf,
    process::{Child, ChildStdin, ChildStdout, Command, Stdio},
    sync::{Mutex, PoisonError},
};

const EXE_NAME: &str = "bono-phonemizer";

static PROCESS: Mutex<Option<Process>> = Mutex::new(None);

#[derive(Serialize)]
struct Request<'a> {
    voice: &'a str,
    text: &'a str,
}

#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
enum Response {
    Sentences(Vec<String>),
    Error(String),
}

/// Converts `text` to IPA phonemes with espeak-ng's `voice`, one string per sentence. Empty
/// text only checks the voice exists (and starts the phonemizer).
pub fn phonemize(text: &str, voice: &str) -> anyhow::Result<Vec<String>> {
    let mut process = PROCESS.lock().unwrap_or_else(PoisonError::into_inner);
    let running = match &mut *process {
        Some(running) => running,
        None => process.insert(Process::spawn()?),
    };

    match running.request(&Request { voice, text }) {
        Ok(Response::Sentences(sentences)) => Ok(sentences),
        Ok(Response::Error(e)) => Err(anyhow::Error::msg(e).context("Phonemizer error")),
        // It's gone or out of step: the next request starts a new one
        Err(e) => {
            *process = None;
            Err(e)
        }
    }
}

struct Process {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    line: String,
}

impl Process {
    fn spawn() -> anyhow::Result<Self> {
        let exe = exe_path().with_context(|| {
            format!(
                "Couldn't find {EXE_NAME} (in 'phonemizer' next to the executable, or built with \
                 `cargo build -p {EXE_NAME}`)"
            )
        })?;

        let mut command = Command::new(&exe);
        command
            .arg("--serve")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            // Its errors end up in our log
            .stderr(Stdio::inherit());

        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            // Without it, a console window opens for it
            const CREATE_NO_WINDOW: u32 = 0x0800_0000;
            command.creation_flags(CREATE_NO_WINDOW);
        }

        let mut child = command
            .spawn()
            .with_context(|| format!("Error starting '{}'", exe.display()))?;
        let stdin = child.stdin.take().context("The phonemizer has no stdin")?;
        let stdout = child
            .stdout
            .take()
            .context("The phonemizer has no stdout")?;
        log::info!("Started the phonemizer ({})", exe.display());

        Ok(Self {
            child,
            stdin,
            stdout: BufReader::new(stdout),
            line: String::new(),
        })
    }

    fn request(&mut self, request: &Request) -> anyhow::Result<Response> {
        let mut line = serde_json::to_vec(request)?;
        line.push(b'\n');
        self.stdin
            .write_all(&line)
            .and_then(|()| self.stdin.flush())
            .context("Error sending text to the phonemizer")?;

        self.line.clear();
        let read = self
            .stdout
            .read_line(&mut self.line)
            .context("Error reading the phonemizer's answer")?;
        if read == 0 {
            bail!("The phonemizer stopped");
        }

        return serde_json::from_str(&self.line).context("Invalid answer from the phonemizer");
    }
}

impl Drop for Process {
    fn drop(&mut self) {
        // It exits by itself once its stdin closes, but it may be stuck
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Distributions ship it in `phonemizer/`, next to its espeak-ng data. During development
/// it's built next to the executable (or its parent for test binaries).
fn exe_path() -> Option<PathBuf> {
    let name = format!("{EXE_NAME}{}", std::env::consts::EXE_SUFFIX);
    let exe = std::env::current_exe().ok()?;
    let exe_dir = exe.parent()?;

    std::iter::once(exe_dir.join("phonemizer"))
        .chain(exe_dir.ancestors().take(2).map(PathBuf::from))
        .map(|dir| dir.join(&name))
        .find(|path| path.is_file())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Needs the phonemizer: `cargo build -p bono-phonemizer` first.
    #[test]
    #[ignore]
    fn phonemizes_through_the_process() {
        let sentences = phonemize("Hello, world. Does it?", "en-us").unwrap();
        assert_eq!(sentences.len(), 2);
        assert!(sentences[1].ends_with('?'));

        assert!(phonemize("", "en-us").unwrap().is_empty());
        assert!(phonemize("Hello.", "not-a-voice").is_err());
        // Its errors don't stop it
        assert_eq!(phonemize("Hello.", "en-us").unwrap().len(), 1);
    }
}
