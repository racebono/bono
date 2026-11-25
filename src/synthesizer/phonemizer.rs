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

/// Stops the phonemizer, after the request it's answering. The next one starts it again. Until it
/// stops, the files that were open to be inherited when it started can't be deleted on Windows.
pub fn stop() {
    PROCESS
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .take();
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
        let stdout = child.stdout.take().context("The phonemizer has no stdout")?;
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

/// Distributions ship it in `phonemizer/`, next to its espeak-ng data. During development it's
/// built in the profile directory: the executable's directory, or for test binaries its parent
/// (`deps/`) or fourth ancestor (`build/{package}/{hash}/out/`, where Cargo nests build outputs by
/// package).
fn exe_path() -> Option<PathBuf> {
    let name = format!("{EXE_NAME}{}", std::env::consts::EXE_SUFFIX);
    let exe = std::env::current_exe().ok()?;
    let exe_dir = exe.parent()?;

    std::iter::once(exe_dir.join("phonemizer"))
        .chain(exe_dir.ancestors().take(5).map(PathBuf::from))
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

    /// Needs the phonemizer: `cargo build -p bono-phonemizer` first.
    #[cfg(windows)]
    #[test]
    #[ignore]
    fn a_file_it_inherited_can_be_deleted_once_its_stopped() {
        use std::os::windows::{fs::OpenOptionsExt, io::AsRawHandle};
        use windows_sys::Win32::{
            Foundation::{HANDLE_FLAG_INHERIT, SetHandleInformation},
            Storage::FileSystem::FILE_SHARE_READ,
        };

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("model.bin");
        std::fs::write(&path, []).unwrap();
        // As whisper.cpp opens models: inherited, and not shared for deletion
        let file = std::fs::OpenOptions::new()
            .read(true)
            .share_mode(FILE_SHARE_READ)
            .open(&path)
            .unwrap();
        // SAFETY: the handle is open
        let inherited = unsafe {
            SetHandleInformation(
                file.as_raw_handle(),
                HANDLE_FLAG_INHERIT,
                HANDLE_FLAG_INHERIT,
            )
        };
        assert_ne!(inherited, 0);

        stop();
        phonemize("", "en-us").unwrap();
        drop(file);
        assert!(std::fs::remove_file(&path).is_err());
        stop();
        std::fs::remove_file(&path).unwrap();
        // It starts again
        assert_eq!(phonemize("Hello.", "en-us").unwrap().len(), 1);
    }
}
