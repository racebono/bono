#![cfg_attr(target_os = "windows", windows_subsystem = "windows")]

use crate::{app::App, config::ArrayStr};
use bono_i18n::t;
use sdl3::{
    event::Event,
    filesystem::get_pref_path,
    messagebox::{MessageBoxFlag, show_simple_message_box},
    sys::{events::SDL_Event, guid::SDL_GUID},
};
use sdl3_main::{AppResult, MainThreadData, MainThreadToken, app_impl};
use sha2::Digest;
use std::{
    cell::RefCell,
    fmt::Display,
    mem::ManuallyDrop,
    path::{Path, PathBuf},
    pin::pin,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};
use tokio::{io::AsyncWriteExt, runtime::Runtime, time::MissedTickBehavior};

#[doc(hidden)]
pub extern crate std;

mod account;
mod app;
mod button;
mod config;
mod engineer;
mod i18n;
mod language;
mod provider;
mod sim;
mod spotter;
mod sqlite;
mod steam;
mod synthesizer;
mod task;
mod track;

struct AppImpl {
    app: MainThreadData<RefCell<Lifecycle>>,
    rt: ManuallyDrop<Runtime>,
}

enum Lifecycle {
    Loading(task::LocalTask<anyhow::Result<App>>),
    Running(App),
    Failed(task::Task<()>),
    Stopped,
}

impl Drop for AppImpl {
    fn drop(&mut self) {
        {
            let _guard = self.rt.enter();
            // SDL objects are disposed on their owning thread while their worker runtime is alive.
            self.app.get_on_main_thread(|app| {
                drop(std::mem::replace(
                    &mut *app.borrow_mut(),
                    Lifecycle::Stopped,
                ));
            });
        }
        // Normal shutdown has already awaited required work through App::iterate. Never join
        // leftover runtime tasks on the UI thread (also covers an OS-forced early exit).
        unsafe { ManuallyDrop::take(&mut self.rt) }.shutdown_background();
    }
}

#[app_impl]
impl AppImpl {
    fn app_init() -> Option<Box<Self>> {
        if let Err(e) = setup_logger() {
            eprintln!("failed to set up logger: {e}");
        }
        log::info!("bono {}", env!("BONO_VERSION"));
        match init(MainThreadToken::get()?) {
            Ok(app) => Some(Box::new(app)),
            Err(e) => {
                error_blocking(t!("error.initializing"), e);
                None
            }
        }
    }

    fn app_iterate(&self) -> AppResult {
        let Some(token) = MainThreadToken::get() else {
            return AppResult::Continue;
        };

        let _guard = self.rt.enter();
        let mut state = self.app.get(token).borrow_mut();
        if let Lifecycle::Loading(task) = &mut *state {
            match task.update(self.rt.handle()) {
                None => return AppResult::Continue,
                Some(Ok(app)) => *state = Lifecycle::Running(app),
                Some(Err(e)) => {
                    let title = t!("error.initializing");
                    // Keep SDL alive until the worker's dialog is dismissed, without blocking
                    // event delivery or racing process exit against presentation of the error.
                    *state = Lifecycle::Failed(task::Task::spawn_blocking(move || {
                        error_blocking(title, e);
                    }));
                }
            }
        }
        match &mut *state {
            Lifecycle::Running(app) => app.iterate(self.rt.handle()),
            Lifecycle::Loading(_) => AppResult::Continue,
            Lifecycle::Failed(dialog) => {
                if dialog.update(self.rt.handle()).is_some() {
                    AppResult::Failure
                } else {
                    AppResult::Continue
                }
            }
            Lifecycle::Stopped => AppResult::Success,
        }
    }

    fn app_event(&self, event: &SDL_Event) -> AppResult {
        let event = Event::from_ll(*event);
        let mut result = AppResult::Continue;
        let handle = self.rt.handle().clone();

        if !self.app.get_on_main_thread(|app| {
            let _guard = handle.enter();
            result = match &mut *app.borrow_mut() {
                Lifecycle::Running(app) => app.event(&handle, event),
                Lifecycle::Loading(_) if matches!(event, Event::Quit { .. }) => AppResult::Success,
                Lifecycle::Loading(_) => AppResult::Continue,
                Lifecycle::Failed(_) => AppResult::Continue,
                Lifecycle::Stopped => AppResult::Success,
            };
        }) {
            error_blocking(t!("error.title"), t!("error.handling_event"));
            return AppResult::Failure;
        }

        return result;
    }

    fn app_quit(&self) {
        let _guard = self.rt.enter();
        let Some(token) = MainThreadToken::get() else {
            return;
        };
        if let Lifecycle::Running(app) = &mut *self.app.get(token).borrow_mut() {
            app.quit(self.rt.handle());
        }
    }
}

fn setup_logger() -> Result<(), fern::InitError> {
    let dispatch = fern::Dispatch::new()
        .format(|out, message, record| {
            out.finish(format_args!(
                "[{} {} {}] {}",
                humantime::format_rfc3339_seconds(std::time::SystemTime::now()),
                record.level(),
                record.target(),
                message
            ))
        })
        .level(log::LevelFilter::Debug)
        .chain(std::io::stdout());

    // Log next to config.json rather than in the working directory, which may be
    // read-only (e.g. `C:\Windows\System32` when started by a launcher).
    let log_file = get_pref_path(env!("CARGO_PKG_NAME"), env!("CARGO_PKG_NAME"))
        .map_err(|e| e.to_string())
        .and_then(|dir| fern::log_file(dir.join("output.log")).map_err(|e| e.to_string()));

    match log_file {
        Ok(file) => dispatch.chain(file).apply()?,
        Err(e) => {
            dispatch.apply()?;
            log::warn!("Could not open log file, logging to stdout only: {e}");
        }
    }
    Ok(())
}

fn init(token: MainThreadToken) -> anyhow::Result<AppImpl> {
    let rt = Runtime::new()?;
    let handle = rt.handle().clone();
    let _guard = rt.enter();
    // SDL work stays on the main thread; each iteration only polls ready startup work.
    // File and credential operations yield to workers instead of blocking event delivery.
    let loading = task::LocalTask::spawn_local(async move { App::new(&handle).await });
    let app = MainThreadData::new(token, RefCell::new(Lifecycle::Loading(loading)));
    drop(_guard);
    Ok(AppImpl {
        app,
        rt: ManuallyDrop::new(rt),
    })
}

pub(crate) fn error(title: &'static str, message: impl Display) {
    let message = message.to_string();
    log::error!("{title}: {message}");
    std::thread::spawn(move || {
        let _ = show_simple_message_box(MessageBoxFlag::ERROR, title, &message, None);
    });
}

pub(crate) fn error_blocking(title: &str, message: impl Display) {
    let message = message.to_string();
    log::error!("{title}: {message}");
    let _ = show_simple_message_box(MessageBoxFlag::ERROR, title, &message, None);
}

pub(crate) fn guid_to_str(guid: &SDL_GUID) -> ArrayStr<32> {
    const HEX: &[u8; 16] = b"0123456789abcdef";

    #[inline(always)]
    fn out(buf: &mut [u8; 32], i: usize) -> &mut u8 {
        unsafe { buf.get_unchecked_mut(i) }
    }

    #[inline(always)]
    fn hex(i: usize) -> u8 {
        unsafe { *HEX.get_unchecked(i) }
    }

    let mut inner = [0u8; 32];
    for (i, &b) in guid.data.iter().enumerate() {
        *out(&mut inner, 2 * i) = hex((b >> 4) as usize);
        *out(&mut inner, 2 * i + 1) = hex((b & 0x0F) as usize);
    }

    return unsafe { ArrayStr::from_utf8_unchecked(inner) };
}

/// How long a download may go without receiving anything before it's given up on.
const DOWNLOAD_STALL: Duration = Duration::from_secs(30);
/// How long a download may take in all, overriding the client's (much shorter) timeout: a big
/// model on a slow connection takes long, so it's `DOWNLOAD_STALL` that catches a dead one
const DOWNLOAD_TIMEOUT: Duration = Duration::from_hours(24);

// Tokio file writes may still be running after write_all returns. Surface their errors
// and persist the data before making the completed download visible.
async fn publish_download(
    file: &mut tokio::fs::File,
    temp_path: &Path,
    perm_path: &Path,
) -> std::io::Result<()> {
    file.flush().await?;
    file.sync_all().await?;
    tokio::fs::rename(temp_path, perm_path).await
}

pub(crate) struct Download {
    task: tokio::task::JoinHandle<()>,
    progress: tokio::sync::watch::Receiver<f64>,
    /// Set once the file is in place
    done: Arc<AtomicBool>,
}

pub(crate) enum DownloadDigest {
    Sha256(String),
    Md5(String),
}

impl Download {
    pub fn new(
        src: reqwest::RequestBuilder,
        dst: impl AsRef<Path> + Send + 'static,
        size_hint: Option<u64>,
        digest: Option<DownloadDigest>,
    ) -> Self {
        struct TempFile<'a> {
            perm_path: &'a Path,
            temp_path: Option<PathBuf>,
            file: ManuallyDrop<tokio::fs::File>,
        }

        impl<'a> TempFile<'a> {
            async fn new(path: &'a Path) -> std::io::Result<Self> {
                let temp_path = path.with_added_extension("tmp");
                let file = tokio::fs::File::options()
                    .write(true)
                    // A crashed download can leave this file behind. Restart from byte zero.
                    .create(true)
                    .truncate(true)
                    .open(&temp_path)
                    .await?;

                return Ok(Self {
                    perm_path: path,
                    temp_path: Some(temp_path),
                    file: ManuallyDrop::new(file),
                });
            }

            async fn permanent(mut self) -> std::io::Result<()> {
                let Some(temp_path) = self.temp_path.as_ref() else {
                    return Ok(());
                };
                publish_download(&mut self.file, temp_path, self.perm_path).await?;
                self.temp_path = None;
                return Ok(());
            }
        }

        impl Drop for TempFile<'_> {
            fn drop(&mut self) {
                unsafe { ManuallyDrop::drop(&mut self.file) };
                if let Some(path) = self.temp_path.take() {
                    if let Err(e) = tokio::task::block_in_place(move || std::fs::remove_file(path))
                    {
                        error(t!("error.removing_temporary"), e);
                    }
                }
            }
        }

        let req = src.timeout(DOWNLOAD_TIMEOUT).send();
        let (send, recv) = tokio::sync::watch::channel(0.0);
        let done = Arc::new(AtomicBool::new(false));
        let file = dst.as_ref().to_path_buf();

        let set_done = done.clone();
        let download = async move {
            let path = dst.as_ref();
            let req = async move {
                let resp = tokio::time::timeout(DOWNLOAD_STALL, req)
                    .await
                    .map_err(|_| anyhow::anyhow!("No answer in {DOWNLOAD_STALL:?}"))??;
                resp.error_for_status().map_err(anyhow::Error::from)
            };
            let parent = async {
                if let Some(parent) = path.parent() {
                    tokio::fs::create_dir_all(parent).await?;
                }
                Ok::<_, anyhow::Error>(())
            };

            let (mut resp, ()) = tokio::try_join!(req, parent)?;

            let mut temp_file = TempFile::new(path).await?;
            let expected_size = size_hint.or(resp.content_length());
            let size_hint = expected_size.map(|s| s as f64);
            let mut sha256 = sha2::Sha256::new();
            let mut md5 = md5::Md5::new();
            let mut downloaded = 0u64;

            while let Some(mut chunk) =
                tokio::time::timeout(DOWNLOAD_STALL, resp.chunk())
                    .await
                    .map_err(|_| anyhow::anyhow!("Nothing received in {DOWNLOAD_STALL:?}"))??
            {
                match &digest {
                    Some(DownloadDigest::Sha256(_)) => sha256.update(&chunk),
                    Some(DownloadDigest::Md5(_)) => md5.update(&chunk),
                    None => {}
                }
                let chunk_size = chunk.len();
                temp_file.file.write_all_buf(&mut chunk).await?;
                downloaded = downloaded.saturating_add(chunk_size as u64);
                if let Some(size_hint) = size_hint {
                    let _ = send.send(f64::clamp((downloaded as f64) / size_hint, 0.0, 1.0));
                }
            }

            if let Some(expected) = expected_size {
                anyhow::ensure!(
                    downloaded == expected,
                    "Download size mismatch: expected {expected} bytes, received {downloaded}"
                );
            }
            if let Some(digest) = digest {
                let (algorithm, expected, actual) = match digest {
                    DownloadDigest::Sha256(expected) => {
                        ("SHA-256", expected, format!("{:x}", sha256.finalize()))
                    }
                    DownloadDigest::Md5(expected) => {
                        ("MD5", expected, format!("{:x}", md5.finalize()))
                    }
                };
                anyhow::ensure!(
                    actual.eq_ignore_ascii_case(&expected),
                    "{algorithm} mismatch: expected {expected}, received {actual}"
                );
            }
            temp_file.permanent().await?;
            set_done.store(true, Ordering::Release);
            let _ = send.send(1.0);
            Ok::<_, anyhow::Error>(())
        };

        let task = tokio::task::spawn(async move {
            if let Err(e) = download.await {
                error(t!("error.download"), format!("{}: {e:#}", file.display()));
            }
        });

        return Self {
            task,
            progress: recv,
            done,
        };
    }

    #[inline]
    pub fn progress(&self) -> f64 {
        *self.progress.borrow()
    }

    /// Whether the file was put in place.
    pub fn finished(&self) -> bool {
        self.done.load(Ordering::Acquire)
    }

    /// Whether it stopped before it was done (the error was already shown).
    pub fn failed(&self) -> bool {
        self.task.is_finished() && !self.done.load(Ordering::Acquire)
    }
}

impl Drop for Download {
    #[inline]
    fn drop(&mut self) {
        self.task.abort();
    }
}

#[cfg(test)]
mod download_tests {
    use super::*;
    use std::io::{Read, Write};

    async fn checked_download(
        stale: Option<&[u8]>,
        body: &'static [u8],
        digest: Option<DownloadDigest>,
        size: Option<u64>,
        succeeds: bool,
    ) {
        let dir = tempfile::tempdir().unwrap();
        let destination = dir.path().join("model.bin");
        let temporary = destination.with_added_extension("tmp");
        if let Some(stale) = stale {
            std::fs::write(&temporary, stale).unwrap();
        }

        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let (mut socket, _) = listener.accept().unwrap();
            socket
                .set_read_timeout(Some(Duration::from_secs(10)))
                .unwrap();
            let mut request = Vec::new();
            let mut byte = [0];
            while !request.ends_with(b"\r\n\r\n") {
                socket.read_exact(&mut byte).unwrap();
                request.push(byte[0]);
            }
            write!(
                socket,
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            )
            .unwrap();
            socket.write_all(body).unwrap();
        });

        let client = reqwest::Client::builder().no_proxy().build().unwrap();
        let mut download = Download::new(
            client.get(format!("http://{address}/model")),
            destination.clone(),
            size,
            digest,
        );
        tokio::time::timeout(Duration::from_secs(15), &mut download.task)
            .await
            .expect("download did not finish")
            .unwrap();
        server.join().unwrap();
        assert_eq!(download.done.load(Ordering::Acquire), succeeds);
        assert_eq!(download.failed(), !succeeds);
        if !succeeds {
            assert!(!destination.exists());
            assert!(!temporary.exists());
            return;
        }
        assert_eq!(download.progress(), 1.0);
        assert_eq!(std::fs::read(destination).unwrap(), body);
        assert!(!temporary.exists());
    }

    async fn download_over_temp(stale: Option<&[u8]>, body: &'static [u8]) {
        checked_download(stale, body, None, None, true).await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn matching_digests_succeed() {
        for digest in [
            DownloadDigest::Sha256(
                "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad".into(),
            ),
            DownloadDigest::Md5("900150983CD24FB0D6963F7D28E17F72".into()),
        ] {
            checked_download(None, b"abc", Some(digest), Some(3), true).await;
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn mismatched_digests_remove_download() {
        for digest in [
            DownloadDigest::Sha256("00".repeat(32)),
            DownloadDigest::Md5("00".repeat(16)),
        ] {
            checked_download(Some(b"stale"), b"abc", Some(digest), Some(3), false).await;
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn catalog_size_mismatch_removes_download() {
        checked_download(None, b"abc", None, Some(4), false).await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn stale_temp_is_truncated_before_retry() {
        download_over_temp(
            Some(b"old partial model with a trailing suffix"),
            b"new model",
        )
        .await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn empty_response_removes_all_stale_bytes() {
        download_over_temp(Some(b"old partial model"), b"").await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn download_creates_missing_temp_file() {
        download_over_temp(None, b"new model").await;
    }
}

pub(crate) fn updating<Fut>(
    interval: Duration,
    initial: Fut::Output,
    mut f: impl FnMut(Instant) -> Fut + Send + 'static,
) -> tokio::sync::watch::Receiver<Fut::Output>
where
    Fut: Future + Send,
    Fut::Output: Send + Sync + 'static,
{
    let (send, recv) = tokio::sync::watch::channel(initial);
    let mut interval = tokio::time::interval(interval);
    interval.set_missed_tick_behavior(MissedTickBehavior::Skip);
    tokio::task::spawn(async move {
        let mut closed = pin!(send.closed());

        loop {
            let instant = tokio::select! {
                instant = interval.tick() => instant,
                _ = closed.as_mut() => return,
            };

            tokio::select! {
                value = pin!(f(instant.into_std())) => if send.send(value).is_err() {
                    return
                },
                _ = closed.as_mut() => return,
            }
        }
    });
    return recv;
}

#[cfg(test)]
mod download_publication_tests {
    use super::*;

    #[tokio::test]
    async fn publishes_complete_final_chunk_and_empty_download() {
        let dir = tempfile::tempdir().unwrap();
        for data in [vec![42; 128 * 1024 + 7], vec![]] {
            let temp = dir.path().join("model.tmp");
            let dest = dir.path().join("model");
            let mut file = tokio::fs::File::create(&temp).await.unwrap();
            file.write_all(&data).await.unwrap();
            assert!(!dest.exists());
            publish_download(&mut file, &temp, &dest).await.unwrap();
            assert_eq!(std::fs::read(&dest).unwrap(), data);
            assert!(!temp.exists());
            drop(file);
            std::fs::remove_file(dest).unwrap();
        }
    }

    #[tokio::test]
    async fn deferred_write_error_prevents_publication() {
        let dir = tempfile::tempdir().unwrap();
        let temp = dir.path().join("model.tmp");
        let dest = dir.path().join("model");
        std::fs::write(&temp, b"partial").unwrap();
        // A read-only handle deterministically fails the background write, just as
        // a final write can fail on a full disk. Tokio initially buffers this write.
        let mut file = tokio::fs::File::open(&temp).await.unwrap();
        file.write_all(b"last chunk").await.unwrap();
        assert!(publish_download(&mut file, &temp, &dest).await.is_err());
        assert!(!dest.exists());
        assert_eq!(std::fs::read(&temp).unwrap(), b"partial");
    }

    #[tokio::test]
    async fn rename_error_keeps_temporary_download() {
        let dir = tempfile::tempdir().unwrap();
        let temp = dir.path().join("model.tmp");
        let dest = dir.path().join("missing").join("model");
        let mut file = tokio::fs::File::create(&temp).await.unwrap();
        file.write_all(b"complete").await.unwrap();
        assert!(publish_download(&mut file, &temp, &dest).await.is_err());
        assert!(!dest.exists());
        assert_eq!(std::fs::read(&temp).unwrap(), b"complete");
    }
}
