//! Le Mans Ultimate, through Steam. Its plugin (`lmu/`) writes `UserData\bono\telemetry.db`
//! in the game's folder while the game runs, a weekend at a time, and deletes it when the game
//! closes. It can't while this app still has it open (Windows doesn't delete open files), and the app
//! only lets go once Steam reports the game closed, which is after: so the app deletes it then. A
//! file left behind anyway (a crash, or another program had it open) only counts while Steam reports
//! the game running, and the plugin replaces it the next time the pilot drives.

use crate::{
    sim::{PluginStatus, Simulator, SimulatorInfo, SimulatorSettings, probe::Probe},
    sqlite::{Database, pool::Pool},
    steam::{self},
    task::MaybeDone,
};
use bono_sim::recorder::{DATABASE, SETTINGS, try_remove_database};
use serde_json::{Map, Value};
use std::{
    num::NonZeroUsize,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};
use tokio::sync::mpsc;

mod plugin;

const STEAM_APP_ID: u32 = 2399420;
/// The plugin's folder, in the game's UserData folder.
const FOLDER: &str = "bono";

const MAX_IDLE: NonZeroUsize = NonZeroUsize::new(2).unwrap();
const MAX_OPEN: NonZeroUsize = NonZeroUsize::new(4).unwrap();

/// Once the game closes, how long the database is left for the connections still reading it to
/// finish (an answer being written, say) before giving up on deleting it.
const REMOVE_FOR: Duration = Duration::from_secs(60);
const REMOVE_EVERY: Duration = Duration::from_secs(1);
/// While the game runs, how often Steam's libraries and the plugin's folder are looked at again for
/// the database.
const LOOK_EVERY: Duration = Duration::from_secs(1);

pub struct LeMansUltimate {
    /// The database the plugin is writing right now, read-only
    live: Option<Pool>,
    /// Where the plugin's database is, if it's there, as last looked up in the background while
    /// the game runs
    ready: Probe<Option<PathBuf>>,
    /// Hands changes to settings.json to the task writing them, which makes them one at a time and
    /// in order (two "clear" requests at once would otherwise both read the same count). Started
    /// with the first change
    settings: Option<mpsc::UnboundedSender<SettingsChange>>,
}

/// A change to settings.json in a folder, and what it's for (for the log, if it fails).
struct SettingsChange {
    folder: PathBuf,
    change: Box<dyn FnOnce(&mut Map<String, Value>) + Send>,
    purpose: &'static str,
}

impl LeMansUltimate {
    pub fn new() -> Self {
        Self {
            live: None,
            ready: Probe::new(LOOK_EVERY),
            settings: None,
        }
    }

    /// The game's folder, if it's installed.
    fn install_dir(&self) -> Option<PathBuf> {
        install_dir()
    }

    /// `UserData\bono`, if the game is installed.
    fn folder(&mut self) -> Option<PathBuf> {
        Some(self.install_dir()?.join("UserData").join(FOLDER))
    }

    /// Changes settings.json in the background, keeping what else is in it (the plugin acts on any
    /// value that changes). Does nothing if the game isn't installed.
    fn update_settings(
        &mut self,
        purpose: &'static str,
        change: impl FnOnce(&mut Map<String, Value>) + Send + 'static,
    ) {
        let Some(folder) = self.folder() else {
            return;
        };

        let settings = self.settings.get_or_insert_with(|| {
            let (send, mut recv) = mpsc::unbounded_channel::<SettingsChange>();
            tokio::spawn(async move {
                while let Some(SettingsChange {
                    folder,
                    change,
                    purpose,
                }) = recv.recv().await
                {
                    if let Err(e) = write_settings(&folder, change).await {
                        log::error!("Error {purpose}: {e}");
                    }
                }
            });
            send
        });
        // Only fails if the task panicked
        let _ = settings.send(SettingsChange {
            folder,
            change: Box::new(change),
            purpose,
        });
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
        // Looked up in the background: finding the game searches Steam's libraries
        let path = if running {
            self.ready.get(database_path).cloned().flatten()
        } else {
            self.ready.reset();
            None
        };

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

        if self
            .live
            .as_ref()
            .is_some_and(|pool| pool.database().path() != &path)
        {
            self.live = None;
        }
        Some(self.live.get_or_insert_with(|| {
            Pool::new(Database::read_only(Arc::new(path)), MAX_IDLE, MAX_OPEN)
        }))
    }

    /// The plugin reads settings.json when it starts recording and whenever it changes.
    fn apply(&mut self, settings: &SimulatorSettings) {
        let frequency = settings.opponent_frequency.get();
        self.update_settings(
            "passing the recording settings to Le Mans Ultimate",
            move |json| {
                json.insert("opponent_frequency".into(), frequency.into());
            },
        );
    }

    /// The plugin clears its data when the "clear" count in settings.json changes.
    fn clear(&mut self) {
        self.update_settings("asking Le Mans Ultimate to clear its data", |json| {
            let clear = json.get("clear").and_then(Value::as_i64).unwrap_or(0);
            json.insert("clear".into(), (clear + 1).into());
        });
    }

    fn plugin(&mut self) -> Option<MaybeDone<PluginStatus>> {
        Some(MaybeDone::spawn(async {
            // Finding the game searches Steam's libraries
            let install_dir = tokio::task::spawn_blocking(install_dir)
                .await
                .ok()
                .flatten();
            plugin::status(install_dir).await
        }))
    }

    fn install_plugin(&mut self) -> MaybeDone<anyhow::Result<()>> {
        MaybeDone::spawn(async {
            let result = async {
                if steam::is_app_running(STEAM_APP_ID) {
                    anyhow::bail!("Le Mans Ultimate is running: close it first");
                }
                let Some(install_dir) = install_dir() else {
                    anyhow::bail!("Le Mans Ultimate isn't installed");
                };
                plugin::install(&install_dir).await?;
                log::info!(
                    "Installed the Le Mans Ultimate plugin in {}",
                    install_dir.display()
                );
                Ok(())
            }
            .await;
            if let Err(e) = &result {
                log::error!("Error installing the Le Mans Ultimate plugin: {e}");
            }
            result
        })
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
                log::debug!(
                    "Removed the recording Le Mans Ultimate left in {}",
                    dir.display()
                );
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

/// The game's folder, if it's installed.
fn install_dir() -> Option<PathBuf> {
    steam::app_install_dir(STEAM_APP_ID).filter(|dir| dir.is_dir())
}

/// The plugin's database, if the game is installed and the file is there. Blocks: searches
/// Steam's libraries.
fn database_path() -> Option<PathBuf> {
    Some(install_dir()?.join("UserData").join(FOLDER).join(DATABASE)).filter(|path| path.is_file())
}

/// Changes settings.json in `folder`, keeping what else is in it.
async fn write_settings(
    folder: &Path,
    change: impl FnOnce(&mut Map<String, Value>),
) -> anyhow::Result<()> {
    let path = folder.join(SETTINGS);
    let mut settings = tokio::fs::read(&path)
        .await
        .ok()
        .and_then(|bytes| serde_json::from_slice::<Map<String, Value>>(&bytes).ok())
        .unwrap_or_default();
    change(&mut settings);

    tokio::fs::create_dir_all(folder).await?;
    let mut json = serde_json::to_vec_pretty(&settings)?;
    json.push(b'\n');
    write_replacing(&path, &json).await?;
    Ok(())
}

/// Writes a temporary file and moves it over `path`, so the plugin never reads half a file: a
/// half-read "clear" count would look like a new request. The game neither (half a DLL).
async fn write_replacing(path: &Path, contents: &[u8]) -> std::io::Result<()> {
    let mut temp = path.as_os_str().to_owned();
    temp.push(".tmp");
    let temp = PathBuf::from(temp);
    tokio::fs::write(&temp, contents).await?;
    tokio::fs::rename(&temp, path).await
}
