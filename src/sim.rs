use crate::{
    sqlite::pool::Pool,
    steam::{self, AppWatcher},
    track::TrackData,
};
use futures::future::select_all;
use serde::{Deserialize, Serialize};
use std::{
    fmt,
    num::NonZeroU32,
    ops::{Deref, DerefMut},
    path::PathBuf,
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::sync::{Mutex, watch};

#[cfg(windows)]
mod lmu;
#[cfg(windows)]
mod rrre;

pub struct Sim<T: ?Sized = dyn Simulator> {
    name: &'static str,
    /// e.g. `lmu`
    id: &'static str,
    app_id: u32,
    /// Whether Steam reports the game running
    running: watch::Receiver<bool>,
    /// Shared with the task that keeps the session's track description up to date
    track_data: Arc<Mutex<TrackData>>,
    /// Kept for its thread. Without Steam's key for the game (it's not installed), a sender kept
    /// so `running` stays false instead of closing
    _watcher: Result<AppWatcher, watch::Sender<bool>>,
    simulator: T,
}

impl<T> Sim<T> {
    pub fn new(simulator: T) -> Self
    where
        T: SimulatorInfo,
    {
        let (watcher, running) = match AppWatcher::new(T::APP_ID) {
            Ok(watcher) => {
                let running = watcher.subscribe();
                (Ok(watcher), running)
            }
            Err(e) => {
                log::debug!("Not following {} starting and stopping: {e}", T::NAME);
                let (send, running) = watch::channel(false);
                (Err(send), running)
            }
        };

        Self {
            name: T::NAME,
            id: T::TRACK_DATA_ID,
            app_id: T::APP_ID,
            running,
            track_data: Arc::new(Mutex::new(TrackData::new(T::TRACK_DATA_ID))),
            _watcher: watcher,
            simulator,
        }
    }
}

impl<T> Sim<T>
where
    T: ?Sized,
{
    #[inline]
    pub fn name(&self) -> &'static str {
        self.name
    }

    /// The app's identifier for the simulator, e.g. `lmu` or `rrre`.
    #[inline]
    pub fn id(&self) -> &'static str {
        self.id
    }

    /// The folder Steam installed the game in, if it's installed. Looks through Steam's libraries
    /// on every call.
    #[inline]
    pub fn install_dir(&self) -> Option<PathBuf> {
        steam::app_install_dir(self.app_id)
    }

    /// Whether Steam reports the game running now.
    #[inline]
    pub fn is_running(&self) -> bool {
        *self.running.borrow()
    }

    /// The simulator's track descriptions.
    #[inline]
    pub fn track_data(&self) -> &Arc<Mutex<TrackData>> {
        &self.track_data
    }
}

impl<T> Sim<T>
where
    T: ?Sized + Simulator,
{
    /// The database the simulator's plugin is writing right now, if it's running and has written
    /// one.
    #[inline]
    pub fn database(&mut self) -> Option<&Pool> {
        let running = self.is_running();
        self.simulator.database(running)
    }

    /// Once the game stops, closes the database's connections (when nothing else still holds
    /// the pool), so the plugin can delete the file as the game closes.
    #[inline]
    pub fn release_if_stopped(&mut self) {
        if !self.is_running() {
            self.simulator.database(false);
        }
    }
}

impl<T> Deref for Sim<T>
where
    T: ?Sized,
{
    type Target = T;

    #[inline]
    fn deref(&self) -> &Self::Target {
        &self.simulator
    }
}

impl<T> DerefMut for Sim<T>
where
    T: ?Sized,
{
    #[inline]
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.simulator
    }
}

pub trait SimulatorInfo: Simulator {
    const NAME: &'static str;
    const TRACK_DATA_ID: &'static str;
    const APP_ID: u32;
}

pub trait Simulator: Send {
    /// The database the simulator's plugin is writing right now, given whether the game is
    /// running (`Sim::database` passes it). Not running, it lets go of the database.
    fn database(&mut self, running: bool) -> Option<&Pool>;
    fn apply(&mut self, settings: &SimulatorSettings);
    fn clear(&mut self);

    /// The plugin the game needs installed to be recorded, for games that need one: `None` for
    /// the others.
    fn plugin(&mut self) -> Option<PluginStatus> {
        None
    }

    /// Installs (or updates) the game's plugin, the copy that comes with the app, and enables it.
    fn install_plugin(&mut self) -> anyhow::Result<()> {
        anyhow::bail!("This game has no plugin")
    }
}

/// What a game's plugin looks like on this computer.
#[derive(Debug, Clone, Default)]
pub struct PluginStatus {
    /// The copy in the game's folder: `None` if there isn't one, `Some(None)` if it doesn't say its
    /// version (it's older than the versioned ones)
    pub installed: Option<Option<Version>>,
    /// Whether the game loads it (its plugin settings may disable it)
    pub enabled: bool,
    /// The copy that comes with the app, which `install_plugin` installs: `None` if it's missing,
    /// `Some(None)` if it doesn't say its version
    pub bundled: Option<Option<Version>>,
    /// Whether the installed copy's contents hash the same as the one that comes with the app:
    /// `None` if either is missing or couldn't be read
    pub same_as_bundled: Option<bool>,
}

impl PluginStatus {
    /// The version of the copy that comes with the app, if it's there and says it.
    pub fn bundled_version(&self) -> Option<&Version> {
        self.bundled.as_ref()?.as_ref()
    }

    /// Whether the installed copy says it's the same version as the one that comes with the app,
    /// but its code isn't (a local build, or a damaged or modified file). Other versions are
    /// expected to differ.
    pub fn unexpected_code(&self) -> bool {
        self.same_as_bundled == Some(false)
            && self.installed.is_some()
            && self.installed == self.bundled
    }
}

/// A plugin's version, as its file says.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Version {
    pub major: u16,
    pub minor: u16,
    pub patch: u16,
    /// The build metadata after `+`: the commit, in nightly builds
    pub build: Option<String>,
}

impl Version {
    /// Whether this (the copy that comes with the app) should replace `installed`: it's newer, or
    /// it's the same version from another build, since nightlies only differ in their commit.
    pub fn replaces(&self, installed: &Version) -> bool {
        let number = |v: &Version| (v.major, v.minor, v.patch);
        match number(self).cmp(&number(installed)) {
            std::cmp::Ordering::Greater => true,
            std::cmp::Ordering::Equal => self.build != installed.build,
            std::cmp::Ordering::Less => false,
        }
    }
}

impl fmt::Display for Version {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}.{}", self.major, self.minor, self.patch)?;
        if let Some(build) = &self.build {
            write!(f, "+{build}")?;
        }
        Ok(())
    }
}

#[derive(Debug, Serialize, Deserialize)]
pub struct SimulatorSettings {
    pub opponent_frequency: NonZeroU32,
}

/// Follows which simulator is running. The first one found running stays current for as long as
/// it runs, even if another starts meanwhile; when it stops, the next one running (if any) takes
/// over.
pub struct ActiveSimulator {
    simulators: Vec<Box<Sim<dyn Simulator>>>,
    current: Option<usize>,
    last_watch_retry: Instant,
}

impl ActiveSimulator {
    fn new(simulators: Vec<Box<Sim<dyn Simulator>>>) -> Self {
        let mut this = Self {
            simulators,
            current: None,
            last_watch_retry: Instant::now(),
        };
        this.current = this.pick();
        this
    }

    /// The simulator running now, if any.
    #[inline]
    pub fn current(&self) -> Option<&Sim<dyn Simulator>> {
        Some(self.simulators.get(self.current?)?.as_ref())
    }

    /// The simulator running now, if any.
    #[inline]
    pub fn current_mut(&mut self) -> Option<&mut Sim<dyn Simulator>> {
        Some(self.simulators.get_mut(self.current?)?.as_mut())
    }

    pub fn simulators(&mut self) -> impl Iterator<Item = &mut Sim<dyn Simulator + 'static>> {
        self.simulators.iter_mut().map(|sim| sim.as_mut())
    }

    /// Waits for the current simulator to change, and returns the new one (`None` when none is
    /// running any more). Simulators starting and stopping while the current one runs on aren't
    /// reported. Waits forever if no simulator can start or stop any more.
    pub async fn changed(&mut self) -> Option<&mut Sim<dyn Simulator>> {
        loop {
            if self.last_watch_retry.elapsed() >= Duration::from_secs(2) {
                self.last_watch_retry = Instant::now();
                for sim in &mut self.simulators {
                    retry_watcher(&mut sim._watcher, &mut sim.running, || {
                        let watcher = AppWatcher::new(sim.app_id).ok()?;
                        let running = watcher.subscribe();
                        Some((watcher, running))
                    });
                }
            }
            let next = self.pick();
            if next != self.current {
                self.current = next;
                break;
            }

            // Closed channels aren't waited on: the next retry reconnects them.
            let waits = self
                .simulators
                .iter_mut()
                .filter(|sim| sim.running.has_changed().is_ok())
                .map(|sim| Box::pin(sim.running.changed()))
                .collect::<Vec<_>>();

            let retry = tokio::time::sleep(
                Duration::from_secs(2).saturating_sub(self.last_watch_retry.elapsed()),
            );
            tokio::select! {
                _ = retry => {},
                _ = async {
                    if waits.is_empty() {
                        std::future::pending::<()>().await;
                    } else {
                        let _ = select_all(waits).await;
                    }
                } => {},
            }
        }

        self.current_mut()
    }

    /// The current simulator while it runs, otherwise the first one running.
    fn pick(&self) -> Option<usize> {
        let is_running = |i: usize| {
            self.simulators
                .get(i)
                .is_some_and(|sim| *sim.running.borrow())
        };

        self.current
            .filter(|&current| is_running(current))
            .or_else(|| (0..self.simulators.len()).find(|&i| is_running(i)))
    }
}

/// Keeps the fallback channel until Steam can be followed, then takes its current state too.
fn retry_watcher<W>(
    watcher: &mut Result<W, watch::Sender<bool>>,
    running: &mut watch::Receiver<bool>,
    create: impl FnOnce() -> Option<(W, watch::Receiver<bool>)>,
) {
    if (watcher.is_err() || running.has_changed().is_err())
        && let Some((next, receiver)) = create()
    {
        *running = receiver;
        *watcher = Ok(next);
    }
}

impl Default for ActiveSimulator {
    fn default() -> Self {
        Self::new(simulators())
    }
}

fn simulators() -> Vec<Box<Sim<dyn Simulator>>> {
    return [
        #[cfg(windows)]
        (Box::new(Sim::new(lmu::LeMansUltimate::new())) as Box<Sim<dyn Simulator>>),
        #[cfg(windows)]
        (Box::new(Sim::new(rrre::RaceRoom::new())) as Box<Sim<dyn Simulator>>),
    ]
    .into_iter()
    .collect::<Vec<_>>();
}

/// A running simulator whose recording is `pool`'s database, for tests.
#[cfg(test)]
pub(crate) fn recording(pool: Pool) -> Box<Sim> {
    struct Recording(Pool);

    impl Simulator for Recording {
        fn database(&mut self, running: bool) -> Option<&Pool> {
            running.then_some(&self.0)
        }
        fn apply(&mut self, _: &SimulatorSettings) {}
        fn clear(&mut self) {}
    }

    let (send, running) = watch::channel(true);
    Box::new(Sim {
        name: "Test",
        id: "test",
        app_id: 0,
        running,
        track_data: Arc::new(Mutex::new(TrackData::new("test".into()))),
        _watcher: Err(send),
        simulator: Recording(pool),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use tokio::time::timeout;

    #[test]
    fn retries_missing_watcher_and_adopts_its_running_state() {
        let (send, mut running) = watch::channel(false);
        let mut watcher: Result<(), _> = Err(send);
        retry_watcher(&mut watcher, &mut running, || None);
        assert!(watcher.is_err());
        assert!(!*running.borrow());

        let (send, receiver) = watch::channel(true);
        retry_watcher(&mut watcher, &mut running, || Some(((), receiver)));
        assert!(watcher.is_ok());
        assert!(*running.borrow());
        retry_watcher(&mut watcher, &mut running, || panic!("already watching"));
        send.send(false).unwrap();
        assert!(!*running.borrow());
    }

    #[test]
    fn reconnects_a_closed_watcher() {
        let (send, mut running) = watch::channel(true);
        drop(send);
        let mut watcher = Ok(());
        let (_send, receiver) = watch::channel(false);
        retry_watcher(&mut watcher, &mut running, || Some(((), receiver)));
        assert!(!*running.borrow());
        assert!(running.has_changed().is_ok());
    }

    struct Fake;

    impl Simulator for Fake {
        fn database(&mut self, _: bool) -> Option<&Pool> {
            None
        }
        fn apply(&mut self, _: &SimulatorSettings) {}
        fn clear(&mut self) {}
    }

    /// A simulator whose running state the test sets.
    fn fake(name: &'static str, running: bool) -> (Box<Sim>, watch::Sender<bool>) {
        let (send, running) = watch::channel(running);
        let sim = Box::new(Sim {
            name,
            id: name,
            app_id: 0,
            running,
            track_data: Arc::new(Mutex::new(TrackData::new(name.into()))),
            // Not the test's sender, so the test can close the channel
            _watcher: Err(watch::channel(false).0),
            simulator: Fake,
        });
        (sim, send)
    }

    async fn next(active: &mut ActiveSimulator) -> Option<&'static str> {
        timeout(Duration::from_secs(1), active.changed())
            .await
            .expect("no change")
            .map(|sim| sim.name())
    }

    async fn no_change(active: &mut ActiveSimulator) {
        assert!(
            timeout(Duration::from_millis(50), active.changed())
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn keeps_the_current_simulator_while_it_runs() {
        let (a, a_running) = fake("a", false);
        let (b, b_running) = fake("b", true);
        let mut active = ActiveSimulator::new(vec![a, b]);
        assert_eq!(active.current_mut().map(|sim| sim.name()), Some("b"));

        // Another starting doesn't take over
        a_running.send(true).unwrap();
        no_change(&mut active).await;

        // Until the current one stops
        b_running.send(false).unwrap();
        assert_eq!(next(&mut active).await, Some("a"));

        b_running.send(true).unwrap();
        no_change(&mut active).await;

        a_running.send(false).unwrap();
        assert_eq!(next(&mut active).await, Some("b"));

        b_running.send(false).unwrap();
        assert_eq!(next(&mut active).await, None);
    }

    #[tokio::test]
    async fn ignores_simulators_that_cant_be_followed() {
        let (a, a_running) = fake("a", false);
        let (b, b_running) = fake("b", false);
        let mut active = ActiveSimulator::new(vec![a, b]);
        drop(a_running);
        no_change(&mut active).await;

        b_running.send(true).unwrap();
        assert_eq!(next(&mut active).await, Some("b"));
    }

    #[tokio::test]
    async fn waits_when_all_watcher_channels_are_closed() {
        let (sim, running) = fake("a", false);
        drop(running);
        let mut active = ActiveSimulator::new(vec![sim]);
        no_change(&mut active).await;
    }

    #[test]
    fn bundled_plugin_replaces_older_versions_and_other_builds() {
        let version = |patch, build: Option<&str>| Version {
            major: 0,
            minor: 1,
            patch,
            build: build.map(str::to_owned),
        };

        assert!(version(1, None).replaces(&version(0, None)));
        assert!(!version(0, None).replaces(&version(1, None)));
        assert!(!version(0, None).replaces(&version(0, None)));
        assert!(!version(0, Some("abc1234")).replaces(&version(0, Some("abc1234"))));
        assert!(version(0, Some("def5678")).replaces(&version(0, Some("abc1234"))));
        assert!(version(0, Some("abc1234")).replaces(&version(0, None)));
        assert!(version(0, None).replaces(&version(0, Some("abc1234"))));
        // A newer number wins whatever the build
        assert!(!version(0, Some("def5678")).replaces(&version(1, Some("abc1234"))));
    }

    #[test]
    fn only_same_version_plugins_with_other_code_are_unexpected() {
        let version = |patch| {
            Some(Some(Version {
                major: 0,
                minor: 1,
                patch,
                build: None,
            }))
        };
        let status = |installed, bundled, same_as_bundled| PluginStatus {
            installed,
            enabled: true,
            bundled,
            same_as_bundled,
        };

        assert!(status(version(0), version(0), Some(false)).unexpected_code());
        assert!(!status(version(0), version(0), Some(true)).unexpected_code());
        assert!(!status(version(0), version(0), None).unexpected_code());
        // Another version is expected to be other code
        assert!(!status(version(0), version(1), Some(false)).unexpected_code());
        assert!(!status(None, version(0), None).unexpected_code());
    }
}
