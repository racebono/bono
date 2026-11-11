use crate::{
    process::{ProcessWatcher, Program, Programs},
    sqlite::pool::Pool,
    steam::{self, AppWatcher},
    task::MaybeDone,
    track::TrackData,
};
use futures::future::select_all;
use serde::{Deserialize, Serialize};
use std::{
    fmt,
    future::Future,
    num::NonZeroU32,
    ops::{Deref, DerefMut},
    path::{Path, PathBuf},
    pin::Pin,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};
use tokio::sync::{Mutex, watch};

#[cfg(windows)]
mod lmu;
#[cfg(any(windows, test))]
mod probe;
#[cfg(windows)]
mod rrre;

pub struct Sim<T: ?Sized = dyn Simulator> {
    name: &'static str,
    /// e.g. `lmu`
    id: &'static str,
    /// Where the game is, shared with the simulator's background tasks
    game: Game,
    /// Whether Steam reports the game running
    running: watch::Receiver<bool>,
    /// Whether one of the game's processes is running, however it was installed. Closed if the
    /// processes can't be looked at
    process: watch::Receiver<Program>,
    /// Shared with the task that keeps the session's track description up to date
    track_data: Arc<Mutex<TrackData>>,
    /// Kept for its thread. Without Steam's key for the game (it's not installed), a sender kept
    /// so `running` stays false instead of closing
    _watcher: Result<AppWatcher, watch::Sender<bool>>,
    /// Following the game again, started in the background by `ActiveSimulator::changed`
    retrying: Option<tokio::task::JoinHandle<Option<AppWatcher>>>,
    /// Whether it was running, whether Steam reported it, and whether its process was, at the last
    /// `started`
    was_running: (bool, bool, bool),
    simulator: T,
}

impl<T> Sim<T> {
    /// `game` is the one the simulator was made with.
    pub fn new(simulator: T, game: Game) -> Self
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
            process: game.0.process.clone(),
            game,
            running,
            track_data: Arc::new(Mutex::new(TrackData::new(T::TRACK_DATA_ID))),
            _watcher: watcher,
            retrying: None,
            was_running: (false, false, false),
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

    /// Looks for the game's folder, if it's installed, in the background (see
    /// `Game::install_dir`).
    pub fn install_dir(&self) -> MaybeDone<Option<PathBuf>> {
        let game = self.game.clone();
        MaybeDone::spawn_blocking(move || game.install_dir())
    }

    /// Whether the game is running now: Steam reports it, or one of its processes is running.
    #[inline]
    pub fn is_running(&self) -> bool {
        *self.running.borrow() || process_running(&self.process)
    }

    /// Whether the game started running since the last call (or since it was made, on the first),
    /// whether or not it's the current simulator. Its process being seen counts as a start too,
    /// though Steam may already report it running: Steam's flag can lag behind one copy closing
    /// as another starts. Only reads channels, so it's cheap every frame.
    ///
    /// Also tells `Game::install_dir` which of them reported the game last, so it's called every
    /// frame (by `update_simulators`).
    pub fn started(&mut self) -> bool {
        let steam = *self.running.borrow();
        let process = process_running(&self.process);
        let now = (steam || process, steam, process);
        let (was_running, was_steam, was_process) = std::mem::replace(&mut self.was_running, now);

        // In this order: a process seen in the same frame tells the copy for sure
        if steam && !was_steam {
            self.game.0.steam_first.store(true, Ordering::Relaxed);
        }
        if process && !was_process {
            self.game.0.steam_first.store(false, Ordering::Relaxed);
        }
        (now.0 && !was_running) || (process && !was_process)
    }

    /// Waits for Steam or the processes to report the game starting or stopping. Closed channels
    /// aren't waited on: `None` if both are.
    fn wait_for_change(&mut self) -> Option<Pin<Box<dyn Future<Output = ()> + '_>>> {
        let steam = self.running.has_changed().is_ok();
        let process = self.process.has_changed().is_ok();
        let (running, processes) = (&mut self.running, &mut self.process);
        Some(match (steam, process) {
            (true, true) => Box::pin(async move {
                tokio::select! {
                    _ = running.changed() => {},
                    _ = processes.changed() => {},
                }
            }),
            (true, false) => Box::pin(async move {
                let _ = running.changed().await;
            }),
            (false, true) => Box::pin(async move {
                let _ = processes.changed().await;
            }),
            (false, false) => return None,
        })
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
    /// Steam's id for the game
    const APP_ID: u32;
    /// The game's executables, by file name (compared ignoring ASCII case): any of them running is
    /// the game running, however it was installed
    const EXECUTABLES: &'static [&'static str];

    /// The game's folder, given the path of one of its `EXECUTABLES`.
    fn install_dir_of(exe: &Path) -> Option<PathBuf>;
}

/// Whether the process watcher reports the program running. Not once its channel closed: the last
/// value it sent would stay forever.
fn process_running(process: &watch::Receiver<Program>) -> bool {
    process.has_changed().is_ok() && process.borrow().running
}

/// Where a game is installed and whether it's running, as Steam and the processes running tell:
/// Steam only knows the games it installed, and only while they're started through it. Cheap to
/// clone, for the simulator's background tasks.
#[derive(Clone)]
pub struct Game(Arc<GameInner>);

struct GameInner {
    app_id: u32,
    process: watch::Receiver<Program>,
    install_dir_of: fn(&Path) -> Option<PathBuf>,
    /// Whether Steam reported the game starting since one of its processes was last seen starting
    /// (`Sim::started` keeps it): its copy may not be the one last seen then
    steam_first: AtomicBool,
}

impl Game {
    /// `process` follows `T::EXECUTABLES` (see `Programs::watch`).
    pub fn new<T: SimulatorInfo>(process: watch::Receiver<Program>) -> Self {
        Self(Arc::new(GameInner {
            app_id: T::APP_ID,
            process,
            install_dir_of: T::install_dir_of,
            steam_first: AtomicBool::new(false),
        }))
    }

    /// Whether one of the game's processes is running, or Steam reports it running. Reads Steam's
    /// registry key: brief, but not for every frame (`Sim::is_running` is).
    pub fn is_running(&self) -> bool {
        process_running(&self.0.process) || steam::is_app_running(self.0.app_id)
    }

    /// The game's folder, if it's installed: the one it was last seen running from (since the app
    /// started), otherwise the one Steam installed it in. Steam's comes first if Steam reported the
    /// game starting since its process was last seen starting: the copy last seen may be another
    /// one. Steam's flag outlasting a process doesn't count, so the folder of a copy that just closed
    /// stays, for its recording to be removed. Blocks: it may look through Steam's libraries.
    pub fn install_dir(&self) -> Option<PathBuf> {
        // Cloned, so the channel isn't held while looking at the disk
        let exe = self.0.process.borrow().exe.clone();
        let seen = || {
            exe.as_deref()
                .and_then(self.0.install_dir_of)
                .filter(|dir| dir.is_dir())
        };
        let steam = || steam::app_install_dir(self.0.app_id).filter(|dir| dir.is_dir());

        if self.0.steam_first.load(Ordering::Relaxed) {
            steam().or_else(seen)
        } else {
            seen().or_else(steam)
        }
    }

    #[cfg(test)]
    pub(crate) fn fake() -> Self {
        Self(Arc::new(GameInner {
            app_id: 0,
            process: watch::channel(Program::default()).1,
            install_dir_of: |_| None,
            steam_first: AtomicBool::new(false),
        }))
    }
}

pub trait Simulator: Send {
    /// The database the simulator's plugin is writing right now, given whether the game is
    /// running (`Sim::database` passes it). Not running, it lets go of the database.
    fn database(&mut self, running: bool) -> Option<&Pool>;
    fn apply(&mut self, settings: &SimulatorSettings);
    fn clear(&mut self);

    /// The plugin the game needs installed to be recorded, for games that need one: `None` for
    /// the others.
    fn plugin(&mut self) -> Option<MaybeDone<PluginStatus>> {
        None
    }

    /// Installs (or updates) the game's plugin, the copy that comes with the app, and enables it,
    /// in the background.
    fn install_plugin(&mut self) -> MaybeDone<anyhow::Result<()>> {
        MaybeDone::Done(Err(anyhow::anyhow!("This game has no plugin")))
    }
}

/// What a game's plugin looks like on this computer.
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
    /// Following the simulators' processes, for their `process` channels
    processes: Option<ProcessWatcher>,
}

impl ActiveSimulator {
    /// Joins watchers and closes idle database connections away from SDL's event thread.
    pub fn shutdown(&mut self) -> tokio::task::JoinHandle<()> {
        let simulators = std::mem::take(&mut self.simulators);
        let processes = self.processes.take();
        self.current = None;
        tokio::task::spawn_blocking(move || drop((simulators, processes)))
    }

    fn new(simulators: Vec<Box<Sim<dyn Simulator>>>, processes: Option<ProcessWatcher>) -> Self {
        let mut this = Self {
            simulators,
            current: None,
            last_watch_retry: Instant::now(),
            processes,
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
            // Opening Steam's registry key and starting a watch thread happen in the background
            let due = self.last_watch_retry.elapsed() >= Duration::from_secs(2);
            if due {
                self.last_watch_retry = Instant::now();
            }
            for sim in &mut self.simulators {
                poll_retry(sim, due);
            }
            let next = self.pick();
            if next != self.current {
                self.current = next;
                break;
            }

            // A retry in flight is looked at again soon
            let mut wait = Duration::from_secs(2).saturating_sub(self.last_watch_retry.elapsed());
            if self.simulators.iter().any(|sim| sim.retrying.is_some()) {
                wait = wait.min(Duration::from_millis(100));
            }

            // Closed channels aren't waited on: the next retry reconnects Steam's.
            let waits = self
                .simulators
                .iter_mut()
                .filter_map(|sim| sim.wait_for_change())
                .collect::<Vec<_>>();

            let retry = tokio::time::sleep(wait);
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
        let is_running = |i: usize| self.simulators.get(i).is_some_and(|sim| sim.is_running());

        self.current
            .filter(|&current| is_running(current))
            .or_else(|| (0..self.simulators.len()).find(|&i| is_running(i)))
    }
}

/// Takes a finished background retry of following the game, and starts one if `due` and needed.
/// Never waits for one.
fn poll_retry(sim: &mut Sim<dyn Simulator>, due: bool) {
    if sim.retrying.as_ref().is_some_and(|task| task.is_finished())
        && let Some(task) = sim.retrying.take()
    {
        // Finished, so this doesn't wait
        let next = futures::FutureExt::now_or_never(tokio::task::coop::unconstrained(task))
            .and_then(Result::ok)
            .flatten();
        // A closed watcher being replaced already ended its thread, so dropping it doesn't wait
        retry_watcher(&mut sim._watcher, &mut sim.running, || {
            let watcher = next?;
            let running = watcher.subscribe();
            Some((watcher, running))
        });
    }

    if due
        && sim.retrying.is_none()
        && (sim._watcher.is_err() || sim.running.has_changed().is_err())
    {
        let app_id = sim.game.0.app_id;
        sim.retrying = Some(tokio::task::spawn_blocking(move || {
            AppWatcher::new(app_id).ok()
        }));
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
    /// Blocks: opens Steam's registry keys and starts threads.
    fn default() -> Self {
        let mut programs = Programs::default();
        let simulators = simulators(&mut programs);
        let processes = programs.spawn().unwrap_or_else(|e| {
            // The `process` channels close with `programs`: only Steam's games are found
            log::warn!("Can't follow the simulators' processes: {e}");
            None
        });
        Self::new(simulators, processes)
    }
}

#[cfg_attr(not(windows), allow(unused_variables, unused_mut))]
fn simulators(programs: &mut Programs) -> Vec<Box<Sim<dyn Simulator>>> {
    let mut simulators = Vec::new();
    #[cfg(windows)]
    {
        add(&mut simulators, programs, lmu::LeMansUltimate::new);
        add(&mut simulators, programs, |_| rrre::RaceRoom::new());
    }
    simulators
}

/// Adds the simulator `make` makes with its game, whose processes `programs` follows.
#[cfg_attr(not(windows), allow(dead_code))]
fn add<T: SimulatorInfo + 'static>(
    simulators: &mut Vec<Box<Sim<dyn Simulator>>>,
    programs: &mut Programs,
    make: impl FnOnce(Game) -> T,
) {
    let game = Game::new::<T>(programs.watch(T::EXECUTABLES));
    simulators.push(Box::new(Sim::new(make(game.clone()), game)));
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
        game: Game::fake(),
        running,
        process: watch::channel(Program::default()).1,
        track_data: Arc::new(Mutex::new(TrackData::new("test".into()))),
        _watcher: Err(send),
        retrying: None,
        was_running: (false, false, false),
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
            game: Game::fake(),
            running,
            process: watch::channel(Program::default()).1,
            track_data: Arc::new(Mutex::new(TrackData::new(name.into()))),
            // Not the test's sender, so the test can close the channel
            _watcher: Err(watch::channel(false).0),
            retrying: None,
            was_running: (false, false, false),
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
        let mut active = ActiveSimulator::new(vec![a, b], None);
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
        let mut active = ActiveSimulator::new(vec![a, b], None);
        drop(a_running);
        no_change(&mut active).await;

        b_running.send(true).unwrap();
        assert_eq!(next(&mut active).await, Some("b"));
    }

    #[tokio::test]
    async fn waits_when_all_watcher_channels_are_closed() {
        let (sim, running) = fake("a", false);
        drop(running);
        let mut active = ActiveSimulator::new(vec![sim], None);
        no_change(&mut active).await;
    }

    #[test]
    fn reports_each_start_once() {
        let (mut sim, steam) = fake("a", false);
        assert!(!sim.started());
        steam.send(true).unwrap();
        assert!(sim.started());
        assert!(!sim.started());
        steam.send(false).unwrap();
        assert!(!sim.started());
        steam.send(true).unwrap();
        assert!(sim.started());

        // Another copy starting while Steam still reports the first one running
        let (process, receiver) = watch::channel(Program::default());
        sim.process = receiver;
        assert!(!sim.started());
        process.send_modify(|program| program.running = true);
        assert!(sim.started());
        assert!(!sim.started());
    }

    #[test]
    fn install_dir_follows_whichever_reported_the_game_last() {
        let (mut sim, steam) = fake("a", false);
        let (process, receiver) = watch::channel(Program::default());
        sim.process = receiver;
        let steam_first = |sim: &Sim| sim.game.0.steam_first.load(Ordering::Relaxed);
        sim.started();
        assert!(!steam_first(&sim));

        // Steam starting a copy: it may not be the one last seen
        steam.send(true).unwrap();
        sim.started();
        assert!(steam_first(&sim));
        process.send_modify(|program| program.running = true);
        sim.started();
        assert!(!steam_first(&sim));

        // Steam's flag outlasting the process keeps the copy seen
        process.send_modify(|program| program.running = false);
        sim.started();
        assert!(!steam_first(&sim));

        // Both in the same frame: the process tells the copy
        steam.send(false).unwrap();
        sim.started();
        steam.send(true).unwrap();
        process.send_modify(|program| program.running = true);
        sim.started();
        assert!(!steam_first(&sim));
    }

    #[tokio::test]
    async fn follows_a_game_running_without_steam() {
        let (mut sim, _steam) = fake("a", false);
        let (process, receiver) = watch::channel(Program::default());
        sim.process = receiver;
        let mut active = ActiveSimulator::new(vec![sim], None);
        assert!(active.current().is_none());

        process.send_modify(|program| program.running = true);
        assert_eq!(next(&mut active).await, Some("a"));
        assert!(active.current().is_some_and(|sim| sim.is_running()));

        process.send_modify(|program| program.running = false);
        assert_eq!(next(&mut active).await, None);

        // The watcher stopping while the game runs doesn't leave it running forever
        process.send_modify(|program| program.running = true);
        assert_eq!(next(&mut active).await, Some("a"));
        drop(process);
        assert_eq!(next(&mut active).await, None);
    }

    #[tokio::test]
    async fn either_steam_or_the_process_keeps_a_game_running() {
        let (mut sim, steam) = fake("a", true);
        let (process, receiver) = watch::channel(Program {
            running: true,
            exe: None,
        });
        sim.process = receiver;
        let mut active = ActiveSimulator::new(vec![sim], None);

        // Steam lags behind the process exiting
        process.send_modify(|program| program.running = false);
        no_change(&mut active).await;
        steam.send(false).unwrap();
        assert_eq!(next(&mut active).await, None);

        // The process closing its channel leaves Steam's to follow
        drop(process);
        steam.send(true).unwrap();
        assert_eq!(next(&mut active).await, Some("a"));
    }

    struct Named;

    impl Simulator for Named {
        fn database(&mut self, _: bool) -> Option<&Pool> {
            None
        }
        fn apply(&mut self, _: &SimulatorSettings) {}
        fn clear(&mut self) {}
    }

    impl SimulatorInfo for Named {
        const NAME: &'static str = "Named";
        const TRACK_DATA_ID: &'static str = "named";
        const APP_ID: u32 = 0;
        const EXECUTABLES: &'static [&'static str] = &["Named.exe"];

        fn install_dir_of(exe: &Path) -> Option<PathBuf> {
            exe.parent()?.parent().map(Path::to_owned)
        }
    }

    #[test]
    fn install_dir_is_where_the_game_last_ran_from() {
        let dir = tempfile::tempdir().unwrap();
        let exe = dir.path().join("bin").join("Named.exe");
        let (process, receiver) = watch::channel(Program::default());
        let game = Game::new::<Named>(receiver);

        process.send_replace(Program {
            running: true,
            exe: Some(exe.clone()),
        });
        assert_eq!(game.install_dir().as_deref(), Some(dir.path()));
        assert!(game.is_running());

        // Remembered once it stops
        process.send_replace(Program {
            running: false,
            exe: Some(exe),
        });
        assert_eq!(game.install_dir().as_deref(), Some(dir.path()));
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
