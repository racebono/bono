//! Le Mans Ultimate, through Steam. Its plugin (`lmu/`) writes `UserData\lambiase\telemetry.db`
//! in the game's folder while the game runs, and deletes it when the game closes. A file left behind
//! by a crash is only replaced the next time the pilot drives, so it only counts while Steam reports
//! the game running.

use crate::{
    sim::{Simulator, SimulatorSettings},
    sqlite::{Database, pool::Pool},
    steam::{self, AppWatcher},
};
use lambiase_sim::recorder::{DATABASE, SETTINGS};
use serde_json::{Map, Value};
use std::{
    num::NonZeroUsize,
    path::{Path, PathBuf},
    sync::Arc,
};
use tokio::sync::watch;

const STEAM_APP_ID: u32 = 2399420;
/// The plugin's folder, in the game's UserData folder.
const FOLDER: &str = "lambiase";

const MAX_IDLE: NonZeroUsize = NonZeroUsize::new(2).unwrap();
const MAX_OPEN: NonZeroUsize = NonZeroUsize::new(4).unwrap();

pub struct LeMansUltimate {
    /// Found once; looked for again on every call while the game isn't installed
    install_dir: Option<PathBuf>,
    /// The database the plugin is writing right now, read-only
    live: Option<Pool>,
    /// Whether Steam reports the game running
    running: watch::Receiver<bool>,
    /// Kept for its thread. Without Steam's key for the game (it's not installed), a sender kept
    /// so `running` stays false instead of closing
    _watcher: Result<AppWatcher, watch::Sender<bool>>,
}

impl LeMansUltimate {
    pub fn new() -> Self {
        let (watcher, running) = match AppWatcher::new(STEAM_APP_ID) {
            Ok(watcher) => {
                let running = watcher.subscribe();
                (Ok(watcher), running)
            }
            Err(e) => {
                log::debug!("Not following Le Mans Ultimate starting and stopping: {e}");
                let (send, running) = watch::channel(false);
                (Err(send), running)
            }
        };

        Self {
            install_dir: None,
            live: None,
            running,
            _watcher: watcher,
        }
    }

    /// `UserData\lambiase`, if the game is installed.
    fn folder(&mut self) -> Option<PathBuf> {
        if self.install_dir.is_none() {
            self.install_dir = steam::app_install_dir(STEAM_APP_ID);
        }
        Some(self.install_dir.as_ref()?.join("UserData").join(FOLDER))
    }

    /// Changes settings.json, keeping what else is in it (the plugin acts on any value that
    /// changes). Does nothing if the game isn't installed.
    fn update_settings(
        &mut self,
        change: impl FnOnce(&mut Map<String, Value>),
    ) -> anyhow::Result<()> {
        let Some(folder) = self.folder() else {
            return Ok(());
        };

        let path = folder.join(SETTINGS);
        let mut settings = std::fs::read(&path)
            .ok()
            .and_then(|bytes| serde_json::from_slice::<Map<String, Value>>(&bytes).ok())
            .unwrap_or_default();
        change(&mut settings);

        std::fs::create_dir_all(&folder)?;
        let mut json = serde_json::to_vec_pretty(&settings)?;
        json.push(b'\n');
        write_replacing(&path, &json)?;
        Ok(())
    }
}

impl Simulator for LeMansUltimate {
    #[inline]
    fn name(&self) -> &'static str {
        "Le Mans Ultimate"
    }

    #[inline]
    fn track_data_id(&self) -> &'static str {
        "lmu"
    }

    #[inline]
    fn running(&self) -> watch::Receiver<bool> {
        self.running.clone()
    }

    /// The database the plugin is writing right now, or `None` when the game isn't installed, isn't
    /// running, or hasn't written one yet (the pilot hasn't loaded a session).
    fn database(&mut self) -> Option<&Pool> {
        let running = *self.running.borrow();
        let path = self
            .folder()
            .map(|folder| folder.join(DATABASE))
            .filter(|path| running && path.is_file());

        let Some(path) = path else {
            // Closes the connections, so the plugin can delete the file
            self.live = None;
            return None;
        };

        Some(self.live.get_or_insert_with(|| {
            Pool::new(Database::read_only(Arc::new(path)), MAX_IDLE, MAX_OPEN)
        }))
    }

    /// The plugin reads settings.json when it starts recording and whenever it changes.
    fn apply(&mut self, settings: &SimulatorSettings) {
        let frequency = settings.opponent_frequency.get();
        if let Err(e) = self.update_settings(|json| {
            json.insert("opponent_frequency".into(), frequency.into());
        }) {
            log::error!("Error passing the recording settings to Le Mans Ultimate: {e}");
        }
    }

    /// The plugin clears its data when the "clear" count in settings.json changes.
    fn clear(&mut self) {
        if let Err(e) = self.update_settings(|json| {
            let clear = json.get("clear").and_then(Value::as_i64).unwrap_or(0);
            json.insert("clear".into(), (clear + 1).into());
        }) {
            log::error!("Error asking Le Mans Ultimate to clear its data: {e}");
        }
    }
}

/// Writes a temporary file and moves it over `path`, so the plugin never reads half a file: a
/// half-read "clear" count would look like a new request.
fn write_replacing(path: &Path, contents: &[u8]) -> std::io::Result<()> {
    let temp = path.with_extension("json.tmp");
    std::fs::write(&temp, contents)?;
    std::fs::rename(&temp, path)
}
