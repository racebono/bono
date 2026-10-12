use crate::{app::App, config::ArrayStr};
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
    time::{Duration, Instant},
};
use tokio::{io::AsyncWriteExt, runtime::Runtime, task::AbortHandle, time::MissedTickBehavior};

#[doc(hidden)]
pub extern crate std;

mod app;
mod button;
mod config;
mod task;

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
                error_blocking("Error initializing application", e);
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
            error_blocking("Error", "Unexpected error handling event");
            return AppResult::Failure;
        }

        return result;
    }
}

fn init(token: MainThreadToken) -> anyhow::Result<AppImpl> {
    let rt = Runtime::new()?;
    let app = MainThreadData::new(token, RefCell::new(rt.block_on(App::new())?));
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

pub(crate) struct Download {
    aborter: AbortHandle,
    progress: tokio::sync::watch::Receiver<f64>,
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
                let Some(temp_path) = self.temp_path.take() else {
                    return Ok(());
                };
                tokio::fs::rename(temp_path, self.perm_path).await?;
                return Ok(());
            }
        }

        impl Drop for TempFile<'_> {
            fn drop(&mut self) {
                unsafe { ManuallyDrop::drop(&mut self.file) };
                if let Some(path) = self.temp_path.take() {
                    if let Err(e) = tokio::task::block_in_place(move || std::fs::remove_file(path))
                    {
                        error("Error removing temporary file", e);
                    }
                }
            }
        }

        let req = src.send();
        let (send, recv) = tokio::sync::watch::channel(0.0);

        let handle = tokio::task::spawn(async move {
            let path = dst.as_ref();
            let parent = async {
                if let Some(parent) = path.parent() {
                    tokio::fs::create_dir_all(parent).await?;
                }
                Ok::<_, anyhow::Error>(())
            };

            let (mut resp, ()) = tokio::try_join!(
                async move { req.await?.error_for_status().map_err(anyhow::Error::from) },
                parent
            )?;

            let mut temp_file = TempFile::new(path).await?;
            let size_hint = resp.content_length().or(size_hint).map(|s| s as f64);
            let mut downloaded = 0u64;

            while let Some(mut chunk) = resp.chunk().await? {
                let chunk_size = chunk.len();
                temp_file.file.write_all_buf(&mut chunk).await?;
                downloaded = downloaded.saturating_add(chunk_size as u64);
                if let Some(size_hint) = size_hint {
                    let _ = send.send(f64::clamp((downloaded as f64) / size_hint, 0.0, 1.0));
                }
            }

            temp_file.permanent().await?;
            let _ = send.send(1.0);
            Ok::<_, anyhow::Error>(())
        });

        return Self {
            aborter: handle.abort_handle(),
            progress: recv,
        };
    }

    #[inline]
    pub fn progress(&self) -> f64 {
        *self.progress.borrow()
    }
}

impl Drop for Download {
    #[inline]
    fn drop(&mut self) {
        self.aborter.abort();
    }
}

#[repr(transparent)]
pub(crate) struct Defer<F>(ManuallyDrop<F>)
where
    F: FnOnce();

impl<F> Defer<F>
where
    F: FnOnce(),
{
    #[inline]
    pub const fn new(f: F) -> Self {
        Self(ManuallyDrop::new(f))
    }

    #[inline]
    pub fn forget(self) {
        let mut this = ManuallyDrop::new(self);
        unsafe { ManuallyDrop::drop(&mut this.0) };
    }
}

impl<F> Drop for Defer<F>
where
    F: FnOnce(),
{
    #[inline]
    fn drop(&mut self) {
        (unsafe { ManuallyDrop::take(&mut self.0) })()
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
