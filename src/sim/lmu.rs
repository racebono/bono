//! Le Mans Ultimate, installed by Steam. Its plugin (`lmu/`) writes
//! `UserData\bono\telemetry.db` in the game's folder while the game runs, a weekend at a time, and
//! deletes it when the game closes. It can't while this app still has it open (Windows doesn't
//! delete open files), and the app only lets go once it sees the game closed, which is after: so the
//! app deletes it then. A file left behind anyway (a crash, or another program had it open) only
//! counts while the game runs, and the plugin replaces it the next time the pilot drives.

use crate::{
    sim::{
        Game, PluginStatus, Simulator, SimulatorInfo, SimulatorSettings, probe::Probe,
        steam_install_dir,
    },
    sqlite::{Database, pool::Pool},
    task::MaybeDone,
};
use bono_sim::recorder::{DATABASE, SETTINGS, try_remove_database};
use futures::FutureExt;
use serde_json::{Map, Value};
use std::{
    num::NonZeroUsize,
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::{
    sync::mpsc,
    task::{JoinHandle, coop::unconstrained},
};

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
/// While the game runs, how often the game's folder (which may mean Steam's libraries) and the
/// plugin's are looked at again for the database.
const LOOK_EVERY: Duration = Duration::from_secs(1);
/// While the game runs, how often the plugin is looked at again: once a run would do, as the game
/// keeps the one it loaded, but the app may not see the game stop between two runs.
const PLUGIN_LOOK_EVERY: Duration = Duration::from_secs(30);

pub struct LeMansUltimate {
    /// Where the game is, and whether it's running
    game: Game,
    /// The database the plugin is writing right now, read-only
    live: Option<Pool>,
    /// Where the plugin's database is, if it's there, as last looked up in the background while
    /// the game runs
    ready: Probe<Option<PathBuf>>,
    /// Hands changes to settings.json to the task writing them, which makes them one at a time and
    /// in order (two "clear" requests at once would otherwise both read the same count). Started
    /// with the first change
    settings: Option<mpsc::UnboundedSender<SettingsChange>>,
    /// Looking at the plugin the game runs
    plugin_check: Option<JoinHandle<PluginStatus>>,
    /// When the plugin was last looked at this run
    plugin_looked: Option<Instant>,
    /// Whether the plugin the game runs is outdated, once looked at this run
    plugin_outdated: Option<bool>,
    /// Whether the plugin's version has been read this run
    plugin_version_read: bool,
}

/// A change to settings.json in a folder, and what it's for (for the log, if it fails).
struct SettingsChange {
    change: Box<dyn FnOnce(&mut Map<String, Value>) + Send>,
    purpose: &'static str,
}

impl LeMansUltimate {
    pub fn new(game: Game) -> Self {
        Self {
            game,
            live: None,
            ready: Probe::new(LOOK_EVERY),
            settings: None,
            plugin_check: None,
            plugin_looked: None,
            plugin_outdated: None,
            plugin_version_read: false,
        }
    }

    /// Whether the plugin the game runs is known to be up to date, as last looked at in the
    /// background (every `PLUGIN_LOOK_EVERY`, keeping the last answer meanwhile).
    fn plugin_current(&mut self) -> bool {
        match &self.plugin_check {
            Some(check) if check.is_finished() => {
                let check = self.plugin_check.take();
                // Finished, so this doesn't wait
                match check.and_then(|check| unconstrained(check).now_or_never()) {
                    // The game's plugin not found while the game runs, the app's copy's version not
                    // read, or the plugin's no longer read once it was: the lookup failed, which says
                    // nothing new about the plugin loaded
                    Some(Ok(status))
                        if status.installed.is_none()
                            || status.bundled_version().is_none()
                            || (self.plugin_version_read
                                && matches!(status.installed, Some(None))) =>
                    {
                        self.plugin_outdated.get_or_insert(false);
                    }
                    Some(Ok(status)) => {
                        self.plugin_version_read |= matches!(status.installed, Some(Some(_)));
                        let outdated = status.outdated();
                        if outdated && self.plugin_outdated != Some(true) {
                            log::warn!(
                                "The Le Mans Ultimate plugin is outdated: its sessions aren't \
                                 followed until it's updated"
                            );
                        }
                        self.plugin_outdated = Some(outdated);
                    }
                    // Not knowing isn't knowing it's outdated: the last answer stands, if any
                    Some(Err(e)) => {
                        log::error!("Error looking at the Le Mans Ultimate plugin: {e}");
                        self.plugin_outdated.get_or_insert(false);
                    }
                    None => {}
                }
            }
            Some(_) => {}
            None if self
                .plugin_looked
                .is_none_or(|at| at.elapsed() >= PLUGIN_LOOK_EVERY) =>
            {
                self.plugin_looked = Some(Instant::now());
                self.plugin_check = Some(tokio::spawn(plugin_status(self.game.clone())));
            }
            None => {}
        }
        self.plugin_outdated == Some(false)
    }

    /// Changes settings.json in the background, keeping what else is in it (the plugin acts on any
    /// value that changes). Does nothing if the game isn't installed.
    fn update_settings(
        &mut self,
        purpose: &'static str,
        change: impl FnOnce(&mut Map<String, Value>) + Send + 'static,
    ) {
        let settings = self.settings.get_or_insert_with(|| {
            let (send, mut recv) = mpsc::unbounded_channel::<SettingsChange>();
            let game = self.game.clone();
            tokio::spawn(async move {
                while let Some(SettingsChange { change, purpose }) = recv.recv().await {
                    // Discovery may read Steam's library files, so it belongs on a worker too.
                    let game = game.clone();
                    let folder = tokio::task::spawn_blocking(move || {
                        Some(game.install_dir()?.join("UserData").join(FOLDER))
                    })
                    .await;
                    let Ok(Some(folder)) = folder else { continue };
                    if let Err(e) = write_settings(&folder, change).await {
                        log::error!("Error {purpose}: {e}");
                    }
                }
            });
            send
        });
        // Only fails if the task panicked
        let _ = settings.send(SettingsChange {
            change: Box::new(change),
            purpose,
        });
    }
}

impl SimulatorInfo for LeMansUltimate {
    const NAME: &'static str = "Le Mans Ultimate";
    const TRACK_DATA_ID: &'static str = "lmu";
    const APP_ID: Option<u32> = Some(STEAM_APP_ID);
    const EXECUTABLES: &'static [&'static str] = &["Le Mans Ultimate.exe"];
    /// Always installed through Steam, even when started outside it
    const INSTALL_DIR: Option<fn() -> Option<PathBuf>> = Some(steam_install_dir::<Self>);
}

impl Simulator for LeMansUltimate {
    /// The database the plugin is writing right now, or `None` when the game isn't installed, isn't
    /// running, or hasn't written one yet (the pilot hasn't loaded a session).
    fn database(&mut self, running: bool) -> Option<&Pool> {
        // Looked up in the background: finding the game may search Steam's libraries. With an
        // outdated plugin, or before it's known not to be, there's no session to follow
        let path = if running {
            let current = self.plugin_current();
            let game = self.game.clone();
            self.ready
                .get(move || database_path(&game))
                .cloned()
                .flatten()
                .filter(|_| current)
        } else {
            self.ready.reset();
            // The next run may load another plugin: detached, its answer is dropped
            self.plugin_check = None;
            self.plugin_looked = None;
            self.plugin_outdated = None;
            self.plugin_version_read = false;
            None
        };

        let Some(path) = path else {
            // The game closed: our connections close, and the file the plugin couldn't delete while
            // they were open goes
            if let Some(pool) = self.live.take() {
                if !running && let Some(dir) = pool.database().path().parent() {
                    let game = self.game.clone();
                    tokio::spawn(remove_when_closed(dir.to_owned(), pool, move || {
                        game.is_running()
                    }));
                } else {
                    // The database can disappear while the game still runs. Closing the last
                    // pool reference joins its SQLite workers, so this branch needs a worker too.
                    tokio::task::spawn_blocking(move || drop(pool));
                }
            }
            return None;
        };

        if self
            .live
            .as_ref()
            .is_some_and(|pool| pool.database().path() != &path)
        {
            // Steam's installation folder changed. Closing joins the connections' threads;
            // release them off the UI thread without deleting a possibly active recording.
            if let Some(pool) = self.live.take() {
                tokio::task::spawn_blocking(move || drop(pool));
            }
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
        Some(MaybeDone::spawn(plugin_status(self.game.clone())))
    }

    fn install_plugin(&mut self) -> MaybeDone<anyhow::Result<()>> {
        let game = self.game.clone();
        MaybeDone::spawn(async move {
            let result = async {
                // Both read the registry, and finding the game may search Steam's libraries
                let (running, install_dir) =
                    tokio::task::spawn_blocking(move || (game.is_running(), game.install_dir()))
                        .await?;
                if running {
                    anyhow::bail!("Le Mans Ultimate is running: close it first");
                }
                let Some(install_dir) = install_dir else {
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

    fn plugin_outdated(&self) -> bool {
        self.plugin_outdated == Some(true)
    }
}

/// Deletes the database in `dir` once nothing has it open: the pool's connections close as the ones
/// in use are returned (the engineer's last answer, say). Stops once `in_use` (e.g. the game started
/// again, since its plugin replaces the file itself).
async fn remove_when_closed(dir: PathBuf, pool: Pool, in_use: impl Fn() -> bool) {
    drop(pool);

    let mut waited = Duration::ZERO;
    loop {
        tokio::time::sleep(REMOVE_EVERY).await;
        waited += REMOVE_EVERY;
        if in_use() {
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

/// The plugin in the game's folder, and the one that comes with the app.
async fn plugin_status(game: Game) -> PluginStatus {
    // Finding the game may search Steam's libraries
    let install_dir = tokio::task::spawn_blocking(move || game.install_dir())
        .await
        .ok()
        .flatten();
    plugin::status(install_dir).await
}

/// The plugin's database, if the game is installed and the file is there. Blocks: may search
/// Steam's libraries.
fn database_path(game: &Game) -> Option<PathBuf> {
    Some(
        game.install_dir()?
            .join("UserData")
            .join(FOLDER)
            .join(DATABASE),
    )
    .filter(|path| path.is_file())
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

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn an_outdated_plugin_has_no_session_to_follow() {
        let mut lmu = LeMansUltimate::new(Game::fake());
        lmu.plugin_outdated = Some(true);
        assert!(lmu.database(true).is_none());
        assert!(lmu.plugin_outdated());

        // The next run may load another plugin: it's looked at again
        assert!(lmu.database(false).is_none());
        assert!(!lmu.plugin_outdated());
        assert!(lmu.plugin_outdated.is_none());
    }

    /// The verdict after a look at the plugin that found `status`.
    async fn looked_at(lmu: &mut LeMansUltimate, status: PluginStatus) -> bool {
        let check = tokio::spawn(async move { status });
        while !check.is_finished() {
            tokio::task::yield_now().await;
        }
        lmu.plugin_check = Some(check);
        lmu.plugin_looked = Some(Instant::now());
        lmu.plugin_current()
    }

    #[tokio::test]
    async fn a_failed_look_says_nothing_new_about_the_plugin() {
        let version = |patch| {
            Some(Some(crate::sim::Version {
                major: 0,
                minor: 2,
                patch,
                build: None,
            }))
        };
        let status = |installed, bundled| PluginStatus {
            installed,
            enabled: true,
            bundled,
            same_as_bundled: None,
        };

        let mut lmu = LeMansUltimate::new(Game::fake());
        assert!(!looked_at(&mut lmu, status(version(0), version(1))).await);
        assert!(lmu.plugin_outdated());
        // The plugin not found, the app's copy's version not read, the plugin's no longer read
        for failed in [
            status(None, version(1)),
            status(version(0), None),
            status(version(0), Some(None)),
            status(Some(None), version(1)),
        ] {
            assert!(!looked_at(&mut lmu, failed).await);
            assert!(lmu.plugin_outdated());
        }
        // A look that tells
        assert!(looked_at(&mut lmu, status(version(1), version(1))).await);
        assert!(!lmu.plugin_outdated());

        // Without a version on the first look, the plugin's too old to say it
        let mut lmu = LeMansUltimate::new(Game::fake());
        assert!(!looked_at(&mut lmu, status(Some(None), version(1))).await);
        assert!(lmu.plugin_outdated());

        // Likewise after a first look that didn't find it
        let mut lmu = LeMansUltimate::new(Game::fake());
        assert!(looked_at(&mut lmu, status(None, version(1))).await);
        assert!(!looked_at(&mut lmu, status(Some(None), version(1))).await);
        assert!(lmu.plugin_outdated());
    }

    #[tokio::test]
    async fn no_session_is_followed_before_the_plugin_is_looked_at() {
        let mut lmu = LeMansUltimate::new(Game::fake());
        assert!(lmu.database(true).is_none());
        assert!(lmu.plugin_check.is_some());
        assert!(!lmu.plugin_outdated());
    }
}
