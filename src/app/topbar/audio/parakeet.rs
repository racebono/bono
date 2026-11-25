//! Parakeet TDT 0.6B v3: speech to text on the processor, so it costs the simulator no frames. It
//! can't be told which language to expect, and doesn't say which it heard.

use super::ModelFiles;
use crate::{Download, DownloadDigest, synthesizer};
use camino::{Utf8Path, Utf8PathBuf};
use parakeet_rs::{ExecutionConfig, ParakeetTDT, Transcriber};
use std::{
    num::NonZeroUsize,
    sync::{Arc, Mutex},
};

/// The model's name, as the menu and the setup guide show it.
pub const NAME: &str = "Parakeet v3";

/// What the model's files take, in bytes.
pub const SIZE: u64 = 670_479_942;

const FOLDER: &str = "parakeet/parakeet-tdt-0.6b-v3-int8";

/// NVIDIA's model (CC-BY-4.0), converted to ONNX by istupakov.
const URL: &str = "https://huggingface.co/istupakov/parakeet-tdt-0.6b-v3-onnx/resolve/8f23f0c03c8761650bdb5b40aaf3e40d2c15f1ce/";

struct File {
    name: &'static str,
    size: u64,
    sha256: &'static str,
}

const FILES: [File; 3] = [
    File {
        name: "encoder-model.int8.onnx",
        size: 652_183_999,
        sha256: "6139d2fa7e1b086097b277c7149725edbab89cc7c7ae64b23c741be4055aff09",
    },
    File {
        name: "decoder_joint-model.int8.onnx",
        size: 18_202_004,
        sha256: "eea7483ee3d1a30375daedc8ed83e3960c91b098812127a0d99d1c8977667a70",
    },
    File {
        name: "vocab.txt",
        size: 93_939,
        sha256: "d58544679ea4bc6ac563d1f545eb7d474bd6cfa467f0a6e2c1dc1c7d37e3c35d",
    },
];

/// The model's files, and their downloads.
pub struct Files {
    folder: Utf8PathBuf,
    model: Arc<Utf8Path>,
    files: [File; 3],
    /// `None` for those that were on disk when the downloads started
    downloads: Option<[Option<Download>; 3]>,
}

impl Files {
    pub fn new(pref_path: &Utf8Path) -> Self {
        Self::with(pref_path, FILES)
    }

    fn with(pref_path: &Utf8Path, files: [File; 3]) -> Self {
        let folder = pref_path.join(FOLDER);
        Self {
            model: folder.join(files[0].name).into(),
            folder,
            files,
            downloads: None,
        }
    }

    /// The encoder, which is the model's path in the settings when it's picked.
    pub fn model(&self) -> &Arc<Utf8Path> {
        &self.model
    }

    pub fn folder(&self) -> &Utf8Path {
        &self.folder
    }

    pub fn paths(&self) -> [Utf8PathBuf; 3] {
        (self.files)
            .each_ref()
            .map(|file| self.folder.join(file.name))
    }

    /// Whether all of them are on disk, as of the last check: `None` until they've been checked.
    pub fn on_disk(&self, files: &ModelFiles) -> Option<bool> {
        // Every one is asked about, so they're all checked from now on
        let [a, b, c] = self.paths().map(|path| files.checked(&path));
        Some(a? && b? && c?)
    }

    /// Download progress of all of them, weighted by size: `None` if they aren't downloading, or
    /// any failed.
    pub fn progress(&self) -> Option<f64> {
        let downloads = self.downloads.as_ref()?;
        if self.failed() || downloads.iter().flatten().all(Download::finished) {
            return None;
        }
        let downloaded = (downloads.iter().zip(&self.files))
            .map(|(download, file)| {
                download.as_ref().map_or(1.0, Download::progress) * file.size as f64
            })
            .sum::<f64>();
        let total = self.files.iter().map(|file| file.size).sum::<u64>();
        Some(downloaded / total.max(1) as f64)
    }

    /// Whether the last download of any of them failed.
    pub fn failed(&self) -> bool {
        (self.downloads.iter().flatten().flatten()).any(Download::failed)
    }

    /// Downloads those that aren't on disk, unless they're being downloaded.
    pub fn start_download(&mut self, client: &reqwest::Client, files: &ModelFiles) {
        self.start(client, files, |_| URL.to_owned());
    }

    /// `url`: where each file's folder is.
    fn start(
        &mut self,
        client: &reqwest::Client,
        files: &ModelFiles,
        url: impl Fn(usize) -> String,
    ) {
        if self.progress().is_some() {
            return;
        }
        let paths = self.paths();
        self.downloads = Some(std::array::from_fn(|i| {
            let file = &self.files[i];
            // Only whole, checked files are ever put in place
            (files.checked(&paths[i]) != Some(true)).then(|| {
                Download::new(
                    client.get(format!("{}{}", url(i), file.name)),
                    paths[i].as_std_path().to_path_buf(),
                    Some(file.size),
                    Some(DownloadDigest::Sha256(file.sha256.to_owned())),
                )
            })
        }));
    }
}

/// The most threads it runs on: more are no quicker, and leave the simulator fewer.
const MAX_THREADS: usize = 4;

/// The loaded model, shared by workers: it hears one recording at a time. Everything blocks, so it's
/// only used on workers.
pub struct Parakeet {
    dir: Utf8PathBuf,
    threads: NonZeroUsize,
    model: Mutex<ParakeetTDT>,
}

impl Parakeet {
    /// Loads the model in `dir`.
    pub fn load(dir: &Utf8Path, threads: NonZeroUsize) -> anyhow::Result<Self> {
        Ok(Self {
            dir: dir.to_owned(),
            threads,
            model: Mutex::new(Self::model(dir, threads)?),
        })
    }

    fn model(dir: &Utf8Path, threads: NonZeroUsize) -> anyhow::Result<ParakeetTDT> {
        let config = ExecutionConfig::new()
            .with_intra_threads(threads.get().min(MAX_THREADS))
            // Its threads wait for the next question without using the processor
            .with_custom_configure(|builder| Ok(builder.with_intra_op_spinning(false)?))
            .with_environment(synthesizer::environment()?.clone());
        Ok(ParakeetTDT::from_pretrained(dir, Some(config))?)
    }

    /// What was said in `samples` (16 kHz, mono): empty if nothing.
    pub fn transcribe(&self, samples: &[f32]) -> anyhow::Result<String> {
        let mut model = match self.model.lock() {
            Ok(model) => model,
            // Hearing a recording panicked: what that left isn't known, so it's loaded again
            Err(poisoned) => {
                let mut model = poisoned.into_inner();
                *model = Self::model(&self.dir, self.threads)?;
                self.model.clear_poison();
                model
            }
        };
        let heard = model.transcribe_samples(samples.to_vec(), 16000, 1, None)?;
        Ok(heard.text.trim().to_owned())
    }
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;
    use crate::synthesizer::{Effects, Voice};

    const FOUR: NonZeroUsize = NonZeroUsize::new(4).unwrap();

    /// `text` spoken by the voice in the environment variable `voice`, as a recording.
    pub(in crate::app::topbar::audio) fn spoken(voice: &str, text: &str) -> Vec<f32> {
        let path = Utf8PathBuf::from(std::env::var(voice).unwrap());
        let effects = Effects {
            radio: 0.0,
            beep_volume: 0.0,
            speed: 1.0,
        };
        let mut voice = Voice::load(&path, FOUR, false).unwrap();
        let speech = voice.synthesize(text, effects).unwrap();
        let ratio = speech.sample_rate as f64 / 16000.0;
        (0..(speech.samples.len() as f64 / ratio) as usize)
            .map(|i| speech.samples[(i as f64 * ratio) as usize])
            .collect()
    }

    /// The model in `BONO_PARAKEET`.
    pub(in crate::app::topbar::audio) fn load() -> Parakeet {
        let dir = std::env::var("BONO_PARAKEET").unwrap();
        Parakeet::load(Utf8Path::new(&dir), FOUR).unwrap()
    }

    /// Serves `bodies` one after the other, one request each, and returns the paths requested.
    fn serve(bodies: Vec<&'static [u8]>) -> (String, std::thread::JoinHandle<Vec<String>>) {
        use std::io::{BufRead, BufReader, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let mut paths = Vec::new();
            for body in bodies {
                let (socket, _) = listener.accept().unwrap();
                let mut reader = BufReader::new(socket);
                let mut request = String::new();
                while !request.ends_with("\r\n\r\n") {
                    reader.read_line(&mut request).unwrap();
                }
                paths.push(request.split(' ').nth(1).unwrap().to_owned());
                let mut socket = reader.into_inner();
                write!(
                    socket,
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                )
                .unwrap();
                socket.write_all(body).unwrap();
            }
            paths
        });
        (format!("http://{address}/"), server)
    }

    const ABC: &str = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";
    const SMALL: [File; 3] = [
        File {
            name: "a",
            size: 3,
            sha256: ABC,
        },
        File {
            name: "b",
            size: 3,
            sha256: ABC,
        },
        File {
            name: "c",
            size: 3,
            sha256: ABC,
        },
    ];

    async fn until(mut done: impl FnMut() -> bool) {
        tokio::time::timeout(std::time::Duration::from_secs(20), async {
            while !done() {
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap();
    }

    #[test]
    fn sizes_add_up() {
        assert_eq!(FILES.iter().map(|file| file.size).sum::<u64>(), SIZE);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn all_three_files_make_it_on_disk() {
        let dir = tempfile::tempdir().unwrap();
        let files = Files::new(Utf8Path::from_path(dir.path()).unwrap());
        let on_disk = ModelFiles::new();
        assert_eq!(files.on_disk(&on_disk), None);
        std::fs::create_dir_all(files.folder()).unwrap();
        until(|| files.on_disk(&on_disk) == Some(false)).await;
        let [a, b, c] = files.paths();
        assert_eq!(a, **files.model());
        std::fs::write(a, "").unwrap();
        std::fs::write(b, "").unwrap();
        // Long enough for a check to have seen them
        tokio::time::sleep(std::time::Duration::from_secs(3)).await;
        assert_eq!(files.on_disk(&on_disk), Some(false));
        std::fs::write(c, "").unwrap();
        until(|| files.on_disk(&on_disk) == Some(true)).await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_file_that_fails_its_hash_is_downloaded_again() {
        let dir = tempfile::tempdir().unwrap();
        let client = reqwest::Client::builder().no_proxy().build().unwrap();
        let on_disk = ModelFiles::new();
        let mut files = Files::with(Utf8Path::from_path(dir.path()).unwrap(), SMALL);

        // One server per file, as they download at the same time
        let servers = [b"abc", b"xyz", b"abc"].map(|body| serve(vec![body.as_slice()]));
        until(|| files.on_disk(&on_disk) == Some(false)).await;
        files.start(&client, &on_disk, |i| servers[i].0.clone());
        until(|| files.failed()).await;
        assert_eq!(files.progress(), None);
        assert!(!files.paths()[1].exists());
        // The other two are in place once their downloads are done
        until(|| files.paths()[0].exists() && files.paths()[2].exists()).await;
        until(|| {
            on_disk.checked(&files.paths()[0]) == Some(true)
                && on_disk.checked(&files.paths()[2]) == Some(true)
        })
        .await;

        let (url, server) = serve(vec![b"abc"]);
        files.start(&client, &on_disk, |_| url.clone());
        until(|| files.on_disk(&on_disk) == Some(true)).await;
        assert!(!files.failed());
        // Only the one missing was asked for
        assert_eq!(server.join().unwrap(), ["/b"]);
    }

    #[test]
    fn a_folder_without_the_model_does_not_load() {
        let dir = tempfile::tempdir().unwrap();
        assert!(Parakeet::load(Utf8Path::from_path(dir.path()).unwrap(), FOUR).is_err());
    }

    /// Needs `BONO_PARAKEET` (the model's folder) and `BONO_VOICE_EN`/`_ES` (Piper voices).
    #[test]
    #[ignore]
    fn hears_spanish_and_english() {
        let parakeet = load();
        let hear = |voice, text| {
            let heard = parakeet.transcribe(&spoken(voice, text)).unwrap();
            println!("{heard}");
            heard.to_lowercase()
        };
        let es = hear(
            "BONO_VOICE_ES",
            "¿Cuánto combustible me queda y cuándo tengo que parar?",
        );
        assert!(es.contains("combustible") && es.contains("parar"));
        let en = hear(
            "BONO_VOICE_EN",
            "How much fuel do I have left, and when do I need to pit?",
        );
        assert!(en.contains("fuel") && en.contains("pit"));
    }

    /// Needs `BONO_PARAKEET` (the model's folder) and `BONO_VOICE_EN` (a Piper voice).
    #[test]
    #[ignore]
    fn a_panic_does_not_silence_the_next_recording() {
        let parakeet = load();
        let panicked = std::thread::scope(|scope| {
            let thread = scope.spawn(|| {
                let _model = parakeet.model.lock().unwrap();
                panic!("while hearing");
            });
            thread.join().is_err()
        });
        assert!(panicked && parakeet.model.is_poisoned());
        let heard = parakeet.transcribe(&spoken("BONO_VOICE_EN", "How much fuel do I have left?"));
        assert!(heard.unwrap().to_lowercase().contains("fuel"));
        assert!(!parakeet.model.is_poisoned());
    }

    /// Needs `BONO_PARAKEET` (the model's folder).
    #[test]
    #[ignore]
    fn silence_is_nothing() {
        assert_eq!(load().transcribe(&[0.0; 16000]).unwrap(), "");
    }
}
