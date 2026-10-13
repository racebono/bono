//! What Steam knows about the games it installs.

use std::path::PathBuf;
use tokio::sync::watch;

/// The folder the app is installed in, looked for in every Steam library.
pub fn app_install_dir(app_id: u32) -> Option<PathBuf> {
    let steam = steamlocate::SteamDir::locate().ok()?;
    let (app, library) = steam.find_app(app_id).ok()??;
    Some(library.resolve_app_dir(&app))
}

/// Whether Steam reports the app's process alive: its `Running` value, which Steam keeps in the
/// registry.
#[cfg(windows)]
pub fn is_app_running(app_id: u32) -> bool {
    use winreg::{RegKey, enums::HKEY_CURRENT_USER};

    RegKey::predef(HKEY_CURRENT_USER)
        .open_subkey(app_key(app_id))
        .is_ok_and(|key| read_running(&key))
}

/// Not known outside Windows: assumed running, so callers go by what the game leaves on disk.
#[cfg(not(windows))]
pub fn is_app_running(_app_id: u32) -> bool {
    true
}

/// Follows an app starting and stopping, as Steam reports it (see `is_app_running`). A thread
/// waits for Steam to change the app's registry key, and stops with the watcher.
pub struct AppWatcher {
    running: watch::Receiver<bool>,
    #[cfg(windows)]
    thread: windows::WatchThread,
}

impl AppWatcher {
    /// Fails if Steam has no registry key for the app (it was never installed), or on Windows'
    /// errors. Always fails outside Windows.
    pub fn new(app_id: u32) -> std::io::Result<Self> {
        #[cfg(windows)]
        {
            let (thread, running) = windows::WatchThread::spawn(app_id)?;
            Ok(Self { running, thread })
        }

        #[cfg(not(windows))]
        {
            let _ = app_id;
            Err(std::io::ErrorKind::Unsupported.into())
        }
    }

    /// Whether the app is running now.
    #[inline]
    pub fn is_running(&self) -> bool {
        *self.running.borrow()
    }

    /// Waits for the app to start or stop, and returns whether it's running now. Returns at once
    /// if it changed since the last call (or since `new`, on the first). `None` once the watch
    /// ended (Steam deleted the app's key, or Windows failed to report changes).
    pub async fn changed(&mut self) -> Option<bool> {
        self.running.changed().await.ok()?;
        Some(*self.running.borrow_and_update())
    }

    /// Another receiver of the changes, e.g. for another task. It outlives the watcher only as a
    /// closed channel.
    #[inline]
    pub fn subscribe(&self) -> watch::Receiver<bool> {
        self.running.clone()
    }
}

#[cfg(windows)]
fn app_key(app_id: u32) -> String {
    format!(r"Software\Valve\Steam\Apps\{app_id}")
}

#[cfg(windows)]
fn read_running(key: &winreg::RegKey) -> bool {
    key.get_value::<u32, _>("Running")
        .is_ok_and(|running| running != 0)
}

#[cfg(windows)]
mod windows {
    use super::{app_key, read_running};
    use std::{
        io,
        os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle},
        ptr,
        thread::JoinHandle,
    };
    use tokio::sync::watch;
    use windows_sys::Win32::{
        Foundation::{ERROR_SUCCESS, WAIT_OBJECT_0},
        System::{
            Registry::{
                KEY_NOTIFY, KEY_QUERY_VALUE, REG_NOTIFY_CHANGE_LAST_SET,
                REG_NOTIFY_THREAD_AGNOSTIC, RegNotifyChangeKeyValue,
            },
            Threading::{CreateEventW, INFINITE, SetEvent, WaitForMultipleObjects},
        },
    };
    use winreg::{RegKey, enums::HKEY_CURRENT_USER};

    pub struct WatchThread {
        stop: OwnedHandle,
        thread: Option<JoinHandle<()>>,
    }

    impl WatchThread {
        pub fn spawn(app_id: u32) -> io::Result<(Self, watch::Receiver<bool>)> {
            let key = RegKey::predef(HKEY_CURRENT_USER)
                .open_subkey_with_flags(app_key(app_id), KEY_NOTIFY | KEY_QUERY_VALUE)?;
            // Manual-reset, so it stays set for the thread however late it looks
            let stop = event(true)?;
            let changed = event(false)?;

            let (send, recv) = watch::channel(read_running(&key));
            // Handles aren't `Send`: the thread gets the stop event's value, kept alive by `Self`
            // until the thread is joined
            let stop_raw = stop.as_raw_handle() as usize;
            let thread = std::thread::Builder::new()
                .name(format!("steam app {app_id} watcher"))
                .spawn(move || watch(key, stop_raw as _, changed, send))?;

            Ok((
                Self {
                    stop,
                    thread: Some(thread),
                },
                recv,
            ))
        }
    }

    impl Drop for WatchThread {
        fn drop(&mut self) {
            unsafe { SetEvent(self.stop.as_raw_handle()) };
            if let Some(thread) = self.thread.take() {
                let _ = thread.join();
            }
        }
    }

    /// Until `stop` is set, or the key can't be watched any more. Dropping `send` then ends the
    /// receivers' `changed`.
    fn watch(
        key: RegKey,
        stop: *mut std::ffi::c_void,
        changed: OwnedHandle,
        send: watch::Sender<bool>,
    ) {
        let handles = [stop, changed.as_raw_handle()];
        loop {
            // Notifications are one-shot, so it's armed again after every change, and before
            // reading so a change in between isn't missed
            let status = unsafe {
                RegNotifyChangeKeyValue(
                    key.raw_handle(),
                    false.into(),
                    REG_NOTIFY_CHANGE_LAST_SET | REG_NOTIFY_THREAD_AGNOSTIC,
                    changed.as_raw_handle(),
                    true.into(),
                )
            };
            if status != ERROR_SUCCESS {
                break;
            }

            // Any value in the key changing fires it (e.g. "Updating"): only a flip of "Running"
            // reaches the receivers
            let running = read_running(&key);
            send.send_if_modified(|current| std::mem::replace(current, running) != running);

            let woken = unsafe {
                WaitForMultipleObjects(handles.len() as u32, handles.as_ptr(), 0, INFINITE)
            };
            if woken != WAIT_OBJECT_0 + 1 {
                break;
            }
        }
    }

    fn event(manual_reset: bool) -> io::Result<OwnedHandle> {
        let handle = unsafe { CreateEventW(ptr::null(), manual_reset.into(), 0, ptr::null()) };
        if handle.is_null() {
            return Err(io::Error::last_os_error());
        }
        Ok(unsafe { OwnedHandle::from_raw_handle(handle) })
    }
}
