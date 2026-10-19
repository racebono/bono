use crate::{app::App, config::ArrayStr};
use bono_i18n::t;
use sdl3::{
    event::Event,
    messagebox::{MessageBoxFlag, show_simple_message_box},
    sys::{events::SDL_Event, guid::SDL_GUID},
};
use sdl3_main::{AppResult, MainThreadData, MainThreadToken, app_impl};
use simple_logger::SimpleLogger;
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
    app: MainThreadData<RefCell<App>>,
    rt: Runtime,
}

#[app_impl]
impl AppImpl {
    fn app_init() -> Option<Box<Self>> {
        SimpleLogger::new().init().unwrap();
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
        return self.app.get(token).borrow_mut().iterate(self.rt.handle());
    }

    fn app_event(&self, event: &SDL_Event) -> AppResult {
        let event = Event::from_ll(*event);
        let mut result = AppResult::Continue;
        let handle = self.rt.handle().clone();

        if !self.app.get_on_main_thread(|app| {
            let _guard = handle.enter();
            result = app.borrow_mut().event(&handle, event);
        }) {
            error_blocking(t!("error.title"), t!("error.handling_event"));
            return AppResult::Failure;
        }

        return result;
    }
}

fn init(token: MainThreadToken) -> anyhow::Result<AppImpl> {
    let rt = Runtime::new()?;
    let app = MainThreadData::new(token, RefCell::new(rt.block_on(App::new(rt.handle()))?));
    Ok(AppImpl { app, rt })
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

impl Download {
    pub fn new(
        src: reqwest::RequestBuilder,
        dst: impl AsRef<Path> + Send + 'static,
        size_hint: Option<u64>,
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
                    .create_new(true)
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
            let size_hint = resp.content_length().or(size_hint).map(|s| s as f64);
            let mut downloaded = 0u64;

            while let Some(mut chunk) =
                tokio::time::timeout(DOWNLOAD_STALL, resp.chunk())
                    .await
                    .map_err(|_| anyhow::anyhow!("Nothing received in {DOWNLOAD_STALL:?}"))??
            {
                let chunk_size = chunk.len();
                temp_file.file.write_all_buf(&mut chunk).await?;
                downloaded = downloaded.saturating_add(chunk_size as u64);
                if let Some(size_hint) = size_hint {
                    let _ = send.send(f64::clamp((downloaded as f64) / size_hint, 0.0, 1.0));
                }
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
