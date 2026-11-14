//! Recording a game that has no plugins, but publishes its state in shared memory (RaceRoom,
//! iRacing): a thread of ours reads it while the game runs and records it like LMU's plugin does
//! (`bono_sim::recorder`), into a folder of the app's own. The recording is deleted once the game
//! closes.
//!
//! Each game reads its memory with a [`Source`]; the rest (the reader thread, the recorder, the
//! settings the app passes it, cleaning up) is the same for all of them.

use crate::{
    sim::{SimulatorSettings, probe::Probe},
    sqlite::{Database, pool::Pool},
};
use bono_sim::{
    recorder::{DATABASE, Recorder, SETTINGS, try_remove_database},
    sys::{bono_session_info, bono_standing, bono_telemetry},
};
use serde_json::{Map, Value};
use std::{
    num::NonZeroUsize,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};
use tokio::sync::mpsc;

const MAX_IDLE: NonZeroUsize = NonZeroUsize::new(2).unwrap();
const MAX_OPEN: NonZeroUsize = NonZeroUsize::new(4).unwrap();

/// How often the shared memory is read: the games update it every frame (or 60 times a second).
pub(super) const POLL: Duration = Duration::from_millis(16);
/// How often the session and the standings are recorded (LMU's scoring rate).
pub(super) const SCORING_EVERY: Duration = Duration::from_millis(200);
/// How long before looking for the shared memory again, while the game hasn't made it yet.
pub(super) const RETRY: Duration = Duration::from_secs(1);

/// Once the game closes, how long the database is left for the connections still reading it.
const REMOVE_FOR: Duration = Duration::from_secs(60);
const REMOVE_EVERY: Duration = Duration::from_secs(1);
/// While the game runs, how often the recording's folder is looked at again: for the earlier
/// recording to remove before reading, then for the new one.
const LOOK_EVERY: Duration = Duration::from_secs(1);

/// How a game's shared memory is read, on the reader thread. Made anew for each run of the game.
pub(super) trait Source: Send + 'static {
    /// Reads the game once, at `now`.
    fn read(&mut self, now: Instant) -> Read;
}

/// What a [`Source`] read.
pub(super) enum Read {
    /// The game's data isn't there, or can't be recorded: looked for again after [`RETRY`]
    Unavailable,
    /// Nothing new to record (menus, a pause, the same frame): read again after [`POLL`]
    Idle,
    Update(Update),
}

/// One read of the game, for the recorder.
pub(super) struct Update {
    pub telemetry: Vec<bono_telemetry>,
    /// Every `SCORING_EVERY`
    pub scoring: Option<(bono_session_info, Vec<bono_standing>)>,
    /// Another event (track, layout) since the last update
    pub new_event: bool,
}

/// A game recorded from its shared memory, while it runs.
pub(super) struct Recording {
    /// The game's name, for the log
    name: &'static str,
    /// Where the recording goes
    dir: PathBuf,
    /// Makes the source each reader reads the game with
    source: fn() -> Box<dyn Source>,
    /// Reading the game while it runs
    reader: Option<Reader>,
    /// Set once the last stopped reader has let go of the recording
    stopped: Option<Arc<AtomicBool>>,
    /// Deleting the last stopped recording, cancelled when recording restarts so it can't delete
    /// the new one
    cleanup: Option<tokio::task::JoinHandle<()>>,
    /// The database being recorded, read-only
    live: Option<Pool>,
    /// Hands changes to settings.json to the task writing them. Started with the first change
    settings: Option<mpsc::UnboundedSender<SettingsChange>>,
    /// Whether an earlier run's recording was removed (in the background), before reading
    cleaned: Probe<bool>,
    /// Whether the recording is there, as last looked up in the background since reading started
    ready: Probe<bool>,
}

impl Recording {
    /// Records `name` into `folder`, under the temporary directory, reading it with what `source`
    /// makes.
    pub fn new(name: &'static str, folder: &str, source: fn() -> Box<dyn Source>) -> Self {
        Self::in_dir(name, std::env::temp_dir().join("bono").join(folder), source)
    }

    fn in_dir(name: &'static str, dir: PathBuf, source: fn() -> Box<dyn Source>) -> Self {
        Self {
            name,
            dir,
            source,
            reader: None,
            stopped: None,
            cleanup: None,
            live: None,
            settings: None,
            cleaned: Probe::new(LOOK_EVERY),
            ready: Probe::new(LOOK_EVERY),
        }
    }

    /// Changes settings.json in the background, keeping what else is in it (the recorder acts on
    /// any value that changes). The changes are made one at a time and in order, by a task started
    /// with the first: two "clear" requests at once would otherwise both read the same count.
    fn update_settings(
        &mut self,
        purpose: String,
        change: impl FnOnce(&mut Map<String, Value>) + Send + 'static,
    ) {
        let settings = self.settings.get_or_insert_with(|| {
            let (send, mut recv) = mpsc::unbounded_channel::<SettingsChange>();
            let dir = self.dir.clone();
            tokio::spawn(async move {
                while let Some(SettingsChange { change, purpose }) = recv.recv().await {
                    if let Err(e) = write_settings(&dir, change).await {
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

    /// The database being recorded while the game runs (once it's on track), started on the first
    /// call it does. Not running, the reader stops and the recording is deleted.
    pub fn database(&mut self, running: bool) -> Option<&Pool> {
        if !running {
            // Stopped without waiting: the recorder writes what it has and lets go of the file on
            // its own thread, and nothing deletes or restarts the recording until it has.
            if let Some(reader) = self.reader.take() {
                self.stopped = Some(reader.stop());
            }
            self.cleaned.reset_when_idle();
            self.ready.reset();
            if let Some(pool) = self.live.take() {
                let stopped = self.stopped.clone();
                if let Some(old) = self.cleanup.replace(tokio::spawn(remove_when_closed(
                    self.name,
                    self.dir.clone(),
                    pool,
                    stopped,
                ))) {
                    old.abort();
                }
            }
            return None;
        }

        if self.reader.is_none() {
            // The previous reader is still finishing: try again on a later call
            if self
                .stopped
                .as_ref()
                .is_some_and(|done| !done.load(Ordering::Acquire))
            {
                return None;
            }
            self.stopped = None;
            // Removed below instead, before the new recording starts. A removal already under way
            // only stops at its next await, so try again on a later call until it has.
            if let Some(cleanup) = &self.cleanup {
                cleanup.abort();
                if !cleanup.is_finished() {
                    return None;
                }
                self.cleanup = None;
            }
            // Cleanup must complete before recording starts, and never runs on the UI thread.
            let (dir, name) = (self.dir.clone(), self.name);
            let cleaned = self.cleaned.get(move || {
                try_remove_database(&dir)
                    .inspect_err(|e| log::warn!("Couldn't remove {name}'s previous recording: {e}"))
                    .is_ok()
            });
            if cleaned != Some(&true) {
                return None;
            }
            self.reader = Some(Reader::start(self.name, self.dir.clone(), (self.source)()));
        }

        let path = self.dir.join(DATABASE);
        let ready = self.ready.get({
            let path = path.clone();
            move || path.is_file()
        });
        if ready != Some(&true) {
            return None;
        }
        Some(self.live.get_or_insert_with(|| {
            Pool::new(Database::read_only(Arc::new(path)), MAX_IDLE, MAX_OPEN)
        }))
    }

    /// The recorder reads settings.json when it starts and whenever it changes.
    pub fn apply(&mut self, settings: &SimulatorSettings) {
        let frequency = settings.opponent_frequency.get();
        self.update_settings(
            format!("passing the recording settings to {}'s recorder", self.name),
            move |json| {
                json.insert("opponent_frequency".into(), frequency.into());
            },
        );
    }

    /// The recorder clears its data when the "clear" count in settings.json changes.
    pub fn clear(&mut self) {
        self.update_settings(
            format!("asking {}'s recorder to clear its data", self.name),
            |json| {
                let clear = json.get("clear").and_then(Value::as_i64).unwrap_or(0);
                json.insert("clear".into(), (clear + 1).into());
            },
        );
    }
}

/// A change to settings.json, and what it's for (for the log, if it fails).
struct SettingsChange {
    change: Box<dyn FnOnce(&mut Map<String, Value>) + Send>,
    purpose: String,
}

/// Changes settings.json in `dir`, keeping what else is in it.
async fn write_settings(
    dir: &Path,
    change: impl FnOnce(&mut Map<String, Value>),
) -> anyhow::Result<()> {
    let path = dir.join(SETTINGS);
    let mut settings = tokio::fs::read(&path)
        .await
        .ok()
        .and_then(|bytes| serde_json::from_slice::<Map<String, Value>>(&bytes).ok())
        .unwrap_or_default();
    change(&mut settings);

    tokio::fs::create_dir_all(dir).await?;
    // Written whole, so the recorder never reads half a file
    let temp = path.with_extension("json.tmp");
    tokio::fs::write(&temp, serde_json::to_vec_pretty(&settings)?).await?;
    tokio::fs::rename(&temp, &path).await?;
    Ok(())
}

/// Deletes the database in `dir` once nothing has it open, after the reader that recorded it has
/// stopped (`stopped`).
async fn remove_when_closed(
    name: &'static str,
    dir: PathBuf,
    pool: Pool,
    stopped: Option<Arc<AtomicBool>>,
) {
    drop(pool);
    if let Some(stopped) = stopped {
        while !stopped.load(Ordering::Acquire) {
            tokio::time::sleep(REMOVE_EVERY).await;
        }
    }

    let mut waited = Duration::ZERO;
    loop {
        tokio::time::sleep(REMOVE_EVERY).await;
        waited += REMOVE_EVERY;
        match try_remove_database(&dir) {
            Ok(()) => return,
            Err(e) if waited >= REMOVE_FOR => {
                log::warn!(
                    "Couldn't remove {name}'s recording in {} (is another program reading \
                     it?): {e}. It's replaced the next time the game records",
                    dir.display()
                );
                return;
            }
            Err(_) => {}
        }
    }
}

/// The thread reading the game into the recorder, told to stop when dropped. Never joined: it
/// can be sleeping for up to [`RETRY`] and then closing the recorder, so callers on the UI thread
/// watch [`Reader::stop`]'s flag instead.
struct Reader {
    stop: Arc<AtomicBool>,
    /// Set by the thread once it has returned and dropped its recorder (or never started)
    done: Arc<AtomicBool>,
}

impl Reader {
    fn start(name: &'static str, dir: PathBuf, source: Box<dyn Source>) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let done = Arc::new(AtomicBool::new(false));
        let spawned = std::thread::Builder::new()
            .name(format!("bono {}", name.to_lowercase()))
            .spawn({
                let stop = stop.clone();
                let done = SetOnDrop(done.clone());
                move || {
                    let _done = done;
                    read(name, &dir, &stop, source);
                }
            });
        if let Err(e) = spawned {
            log::error!("Error starting {name}'s reader: {e}");
            done.store(true, Ordering::Release);
        }
        Self { stop, done }
    }

    /// Tells the thread to stop without waiting, returning the flag set once it has.
    fn stop(self) -> Arc<AtomicBool> {
        self.done.clone()
    }
}

impl Drop for Reader {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

/// Sets the flag when dropped, also when the thread panics.
struct SetOnDrop(Arc<AtomicBool>);

impl Drop for SetOnDrop {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Release);
    }
}

/// Reads the game with `source` until `stop`, recording what's new.
fn read(name: &str, dir: &Path, stop: &AtomicBool, mut source: Box<dyn Source>) {
    let mut recorder = None::<Recorder>;
    let mut retry_at = Instant::now();
    let mut retry_delay = RETRY;
    let mut warned_recorder = false;
    let mut healthy_since = None::<Instant>;

    while !stop.load(Ordering::Relaxed) {
        let now = Instant::now();
        if recorder.as_ref().is_some_and(Recorder::failed) && healthy_since.take().is_some() {
            retry_at = now + retry_delay;
            retry_delay = (retry_delay * 2).min(Duration::from_secs(60));
            if !warned_recorder {
                log::error!("Recording {name} stopped on an error, retrying with backoff");
                warned_recorder = true;
            }
        }
        if now < retry_at {
            std::thread::sleep(POLL);
            continue;
        }

        let update = match source.read(now) {
            Read::Unavailable => {
                std::thread::sleep(RETRY);
                continue;
            }
            Read::Idle => {
                std::thread::sleep(POLL);
                continue;
            }
            Read::Update(update) => update,
        };

        // Only once there's something to record, so menus leave no empty database.
        // A failed writer resumes the same recording, preserving earlier sessions.
        if recorder.as_ref().is_none_or(Recorder::failed) {
            let result = match &mut recorder {
                Some(recorder) => recorder.restart(None),
                None => Recorder::new(dir, None).map(|new| recorder = Some(new)),
            };
            if let Err(e) = result {
                if !warned_recorder {
                    log::error!("Error starting to record {name}: {e}");
                    warned_recorder = true;
                }
                retry_at = now + retry_delay;
                retry_delay = (retry_delay * 2).min(Duration::from_secs(60));
                std::thread::sleep(POLL);
                continue;
            }
            healthy_since = Some(now);
        }
        // Opening a writer thread isn't success yet: only reset after it stays alive.
        if healthy_since.is_some_and(|since| now.duration_since(since) >= Duration::from_secs(60)) {
            retry_delay = RETRY;
            warned_recorder = false;
        }
        if let Some(recorder) = &recorder {
            record(recorder, update);
        }
        std::thread::sleep(POLL);
    }
}

/// Hands an update to the recorder.
pub(super) fn record(recorder: &Recorder, update: Update) {
    // Another event (track, layout): what was recorded is about something else
    if update.new_event {
        recorder.clear();
    }
    // Scoring first: it says which car is the player's, which the telemetry needs
    if let Some((session, standings)) = update.scoring {
        recorder.push_scoring(session, standings);
    }
    for telemetry in &update.telemetry {
        let vehicle = telemetry.vehicle_id.get().unwrap_or(-1);
        let time = telemetry.session_time.get().unwrap_or(0.0);
        let speed = telemetry.speed.get().unwrap_or(0.0);
        if recorder.wants_telemetry(vehicle, time, speed) {
            recorder.push_telemetry(telemetry);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::num::NonZeroU32;

    /// A game that never makes its memory.
    struct Absent;

    impl Source for Absent {
        fn read(&mut self, _: Instant) -> Read {
            Read::Unavailable
        }
    }

    fn recording(dir: &Path) -> Recording {
        Recording::in_dir("Test", dir.to_path_buf(), || Box::new(Absent))
    }

    async fn wait_for_reader(sim: &mut Recording) {
        tokio::time::timeout(Duration::from_secs(5), async {
            while sim.reader.is_none() {
                sim.database(true);
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("reader never started after cleanup");
    }

    #[tokio::test]
    async fn settings_changes_are_written_in_order_keeping_the_rest() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(SETTINGS), r#"{"other": true, "clear": 3}"#).unwrap();
        let mut sim = recording(dir.path());

        sim.clear();
        sim.apply(&SimulatorSettings {
            opponent_frequency: NonZeroU32::new(7).unwrap(),
        });
        sim.clear();
        // Changes run in order, so this one runs once the others are written
        let (done, wait) = tokio::sync::oneshot::channel();
        sim.update_settings("test".into(), move |_| {
            let _ = done.send(());
        });
        wait.await.unwrap();

        let json: Map<String, Value> =
            serde_json::from_slice(&std::fs::read(dir.path().join(SETTINGS)).unwrap()).unwrap();
        assert_eq!(json["clear"], 5);
        assert_eq!(json["other"], true);
        assert_eq!(json["opponent_frequency"], 7);
    }

    #[tokio::test]
    async fn removes_previous_recording_before_starting_reader() {
        let dir = tempfile::tempdir().unwrap();
        for suffix in ["", "-wal", "-shm"] {
            std::fs::write(dir.path().join(format!("{DATABASE}{suffix}")), "old run").unwrap();
        }
        let mut sim = recording(dir.path());

        // Removed in the background: nothing's served meanwhile
        assert!(sim.database(true).is_none());
        assert!(sim.reader.is_none());
        while sim.reader.is_none() {
            assert!(sim.database(true).is_none());
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert!(sim.live.is_none());
        for suffix in ["", "-wal", "-shm"] {
            assert!(!dir.path().join(format!("{DATABASE}{suffix}")).exists());
        }
    }

    #[tokio::test]
    async fn does_not_start_reader_when_previous_recording_cannot_be_removed() {
        let dir = tempfile::tempdir().unwrap();
        // A directory at the database path makes removal fail without relying on file locks.
        std::fs::create_dir(dir.path().join(DATABASE)).unwrap();
        let mut sim = recording(dir.path());

        for _ in 0..20 {
            assert!(sim.database(true).is_none());
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert!(sim.reader.is_none());
        assert!(sim.live.is_none());
    }

    /// A reader whose thread is still finishing until `done` is set.
    fn finishing_reader() -> (Reader, Arc<AtomicBool>) {
        let done = Arc::new(AtomicBool::new(false));
        let reader = Reader {
            stop: Arc::new(AtomicBool::new(false)),
            done: done.clone(),
        };
        (reader, done)
    }

    #[tokio::test]
    async fn stopping_tells_the_reader_without_waiting_and_restarts_once_it_has_stopped() {
        let dir = tempfile::tempdir().unwrap();
        let mut sim = recording(dir.path());
        let (reader, done) = finishing_reader();
        let stop = reader.stop.clone();
        sim.reader = Some(reader);

        // Returns while the thread is still finishing
        assert!(sim.database(false).is_none());
        assert!(stop.load(Ordering::Relaxed));
        assert!(sim.reader.is_none());

        // The old recorder may still have the file: it isn't removed nor recorded over yet
        std::fs::write(dir.path().join(DATABASE), "old run").unwrap();
        assert!(sim.database(true).is_none());
        assert!(sim.reader.is_none());
        assert!(dir.path().join(DATABASE).exists());

        done.store(true, Ordering::Release);
        assert!(sim.database(true).is_none());
        wait_for_reader(&mut sim).await;
        assert!(sim.reader.is_some());
        assert!(sim.stopped.is_none());
        assert!(!dir.path().join(DATABASE).exists());
        sim.database(false);
    }

    #[test]
    fn a_stopped_reader_flags_once_its_thread_returns() {
        let dir = tempfile::tempdir().unwrap();
        let reader = Reader::start("Test", dir.path().to_path_buf(), Box::new(Absent));
        let done = reader.stop();
        let start = Instant::now();
        while !done.load(Ordering::Acquire) {
            assert!(start.elapsed() < RETRY * 5, "reader didn't stop");
            std::thread::yield_now();
        }
    }

    #[tokio::test]
    async fn restarting_cancels_the_cleanup_of_the_last_recording() {
        let dir = tempfile::tempdir().unwrap();
        let mut sim = recording(dir.path());
        let cleanup = tokio::spawn(std::future::pending());
        let aborted = cleanup.abort_handle();
        sim.cleanup = Some(cleanup);

        // Not started until the cleanup has actually stopped
        assert!(sim.database(true).is_none());
        assert!(sim.reader.is_none());
        while !aborted.is_finished() {
            tokio::task::yield_now().await;
        }
        assert!(sim.database(true).is_none());
        assert!(sim.cleanup.is_none());
        wait_for_reader(&mut sim).await;
        assert!(sim.reader.is_some());
        sim.database(false);
    }

    #[tokio::test(start_paused = true)]
    async fn removes_the_recording_only_after_the_reader_stopped() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(DATABASE);
        std::fs::write(&path, "recording").unwrap();
        let pool = Pool::new(
            Database::read_only(Arc::new(path.clone())),
            MAX_IDLE,
            MAX_OPEN,
        );
        let done = Arc::new(AtomicBool::new(false));
        let task = tokio::spawn(remove_when_closed(
            "Test",
            dir.path().to_path_buf(),
            pool,
            Some(done.clone()),
        ));

        tokio::time::sleep(REMOVE_EVERY * 5).await;
        assert!(path.exists());

        done.store(true, Ordering::Release);
        task.await.unwrap();
        assert!(!path.exists());
    }
}
