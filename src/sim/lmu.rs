//! Le Mans Ultimate, through Steam. Its plugin (`lmu/`) writes `UserData\lambiase\telemetry.db`
//! in the game's folder while the game runs, a weekend at a time, and deletes it when the game
//! closes. It can't while this app still has it open (Windows doesn't delete open files), and the app
//! only lets go once Steam reports the game closed, which is after: so the app deletes it then. A
//! file left behind anyway (a crash, or another program had it open) only counts while Steam reports
//! the game running, and the plugin replaces it the next time the pilot drives.

use crate::{
    sim::{Simulator, SimulatorInfo, SimulatorSettings},
    sqlite::{Database, pool::Pool},
    steam::{self},
};
use lambiase_sim::recorder::{DATABASE, SETTINGS, try_remove_database};
use serde_json::{Map, Value};
use std::{
    num::NonZeroUsize,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

const STEAM_APP_ID: u32 = 2399420;
/// The plugin's folder, in the game's UserData folder.
const FOLDER: &str = "lambiase";

const MAX_IDLE: NonZeroUsize = NonZeroUsize::new(2).unwrap();
const MAX_OPEN: NonZeroUsize = NonZeroUsize::new(4).unwrap();

/// Once the game closes, how long the database is left for the connections still reading it to
/// finish (an answer being written, say) before giving up on deleting it.
const REMOVE_FOR: Duration = Duration::from_secs(60);
const REMOVE_EVERY: Duration = Duration::from_secs(1);

pub struct LeMansUltimate {
    /// Found once; looked for again on every call while the game isn't installed
    install_dir: Option<PathBuf>,
    /// The database the plugin is writing right now, read-only
    live: Option<Pool>,
}

impl LeMansUltimate {
    pub fn new() -> Self {
        Self {
            install_dir: None,
            live: None,
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

impl SimulatorInfo for LeMansUltimate {
    const NAME: &'static str = "Le Mans Ultimate";
    const TRACK_DATA_ID: &'static str = "lmu";
    const APP_ID: u32 = STEAM_APP_ID;
}

impl Simulator for LeMansUltimate {
    /// The database the plugin is writing right now, or `None` when the game isn't installed, isn't
    /// running, or hasn't written one yet (the pilot hasn't loaded a session).
    fn database(&mut self, running: bool) -> Option<&Pool> {
        let path = self
            .folder()
            .map(|folder| folder.join(DATABASE))
            .filter(|path| running && path.is_file());

        let Some(path) = path else {
            // The game closed: our connections close, and the file the plugin couldn't delete while
            // they were open goes
            if let Some(pool) = self.live.take()
                && !running
                && let Some(dir) = pool.database().path().parent()
            {
                tokio::spawn(remove_when_closed(dir.to_owned(), pool));
            }
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

/// Deletes the database in `dir` once nothing has it open: the pool's connections close as the ones
/// in use are returned (the engineer's last answer, say). Stops if the game starts again, since its
/// plugin replaces the file itself.
async fn remove_when_closed(dir: PathBuf, pool: Pool) {
    drop(pool);

    let mut waited = Duration::ZERO;
    loop {
        tokio::time::sleep(REMOVE_EVERY).await;
        waited += REMOVE_EVERY;
        if steam::is_app_running(STEAM_APP_ID) {
            return;
        }

        match try_remove_database(&dir) {
            Ok(()) => {
                log::debug!("Removed the recording Le Mans Ultimate left in {}", dir.display());
                return;
            }
            Err(e) if waited >= REMOVE_FOR => {
                log::warn!(
                    "Couldn't remove the recording Le Mans Ultimate left in {} (is another program \
                     reading it?): {e}. The game replaces it the next time it records",
                    dir.display()
                );
                return;
            }
            Err(_) => {}
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
