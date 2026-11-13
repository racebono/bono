//! Records a simulator's updates into the database (see `sql`), for the plugins running inside the
//! simulator. The game's threads only copy updates; a thread of its own writes them, so the game
//! never waits on the database, and a failure stops recording instead of crashing the game.
//!
//! The database lives in a folder the app and the race-engineer skill's scripts know, next to a
//! `settings.json` the app writes to ask things of the recorder.
//!
//! Recording is split into epochs (see `sql`). Adapters start one where they know the timeline
//! breaks (`begin_epoch`), and the recorder starts one itself where the scoring shows it: the
//! session's identity changing, its clock going back, or a replay ending somewhere the live
//! timeline doesn't carry on from. Every update is tagged with the epoch when it's queued, and
//! the writer writes the epochs in order, so an update can't land on the wrong side of a boundary.
//! A writer restarted after an error carries on in the same epoch.

use crate::{
    sql::{self, EpochReason, ForecastWriter, ScoringWriter, TelemetryWriter},
    sys::{bono_forecast_point, bono_session_info, bono_standing, bono_telemetry},
};
use std::{
    collections::HashMap,
    fs,
    panic::{AssertUnwindSafe, catch_unwind},
    path::{Path, PathBuf},
    sync::{
        Arc, Condvar, Mutex, MutexGuard, PoisonError,
        atomic::{AtomicBool, AtomicI64, AtomicU32, AtomicU64, Ordering},
    },
    thread::JoinHandle,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

pub const DATABASE: &str = "telemetry.db";
pub const SETTINGS: &str = "settings.json";

/// Opponents are recorded at every Nth update of each car (the player's car at every update), to keep
/// the database small. The app writes the pilot's choice to settings.json.
pub const DEFAULT_OPPONENT_FREQUENCY: u32 = 5;

/// Drops updates rather than growing without limit if the writer ever falls behind.
const MAX_QUEUED_TELEMETRY: usize = 50_000;
const MAX_QUEUED_SCORING: usize = 100;
/// How often the forecast is read.
const FORECAST_CHECK: Duration = Duration::from_secs(30);
/// How often the writer looks for a new settings.json.
const SETTINGS_CHECK: Duration = Duration::from_secs(2);

/// Reads the forecast on the writer thread, given the session time of the last scoring update.
/// `None` when it can't be read this time.
pub type ForecastSource = Box<dyn FnMut(f64) -> Option<Vec<bono_forecast_point>> + Send>;

/// Deletes the database in `dir`, with its WAL companions, if it can.
pub fn remove_database(dir: &Path) {
    let _ = try_remove_database(dir);
}

/// Deletes the database in `dir`, with its WAL companions. Fails while anything still has it open:
/// Windows doesn't delete open files, and SQLite doesn't let others delete the files it opens. A
/// database that's missing already counts as deleted.
///
/// The database goes first, and its WAL only once it's gone: a WAL left without its database is
/// fine (a new database replaces it, see `Recorder::new`), but a database left without its WAL
/// would lose whatever wasn't checkpointed yet.
pub fn try_remove_database(dir: &Path) -> std::io::Result<()> {
    let mut result = Ok(());
    for suffix in ["", "-wal", "-shm"] {
        match fs::remove_file(dir.join(format!("{DATABASE}{suffix}"))) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) if suffix.is_empty() => return Err(e),
            Err(e) => {
                if result.is_ok() {
                    result = Err(e);
                }
            }
        }
    }
    result
}

pub struct Recorder {
    dir: PathBuf,
    shared: Arc<Shared>,
    tracking: Mutex<Tracking>,
    thread: Option<JoinHandle<()>>,
}

impl Recorder {
    /// Starts recording into a new database in `dir`, replacing one left by an earlier run.
    pub fn new(dir: impl Into<PathBuf>, forecast: Option<ForecastSource>) -> std::io::Result<Self> {
        let dir = dir.into();
        fs::create_dir_all(&dir)?;
        // A database left behind (the game crashed, or something still had it open when it closed)
        // would mix two runs. If it's still open, it's emptied instead, once the writer has it
        let left_over = try_remove_database(&dir).is_err();

        Self::start(dir, forecast, left_over, None)
    }

    /// Restarts a stopped writer without deleting earlier sessions.
    pub fn restart(&mut self, forecast: Option<ForecastSource>) -> std::io::Result<()> {
        if !self.failed() {
            return Ok(());
        }
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
        // The same session carries on: only the writer failed. What's known of it stays (the
        // player, where the timeline is, a replay playing), and so does what was waiting to be
        // done (a clear, the epochs started)
        let queue = {
            let old = lock(&self.shared.queue);
            Queue {
                epoch: old.epoch,
                boundaries: old.boundaries.clone(),
                clear: old.clear,
                ..Queue::default()
            }
        };
        let mut restarted = Self::start(
            self.dir.clone(),
            forecast,
            self.shared.reset_pending.load(Ordering::Relaxed),
            Some(queue),
        )?;
        std::mem::swap(
            restarted
                .tracking
                .get_mut()
                .unwrap_or_else(PoisonError::into_inner),
            self.tracking
                .get_mut()
                .unwrap_or_else(PoisonError::into_inner),
        );
        restarted.shared.player_id.store(
            self.shared.player_id.load(Ordering::Relaxed),
            Ordering::Relaxed,
        );
        *self = restarted;
        Ok(())
    }

    /// Carries on from `queue` if given (its epoch, and what was waiting to be done), or starts an
    /// epoch.
    fn start(
        dir: PathBuf,
        forecast: Option<ForecastSource>,
        left_over: bool,
        queue: Option<Queue>,
    ) -> std::io::Result<Self> {
        let settings = Settings::read(&dir.join(SETTINGS));
        let queue = match queue {
            Some(queue) => queue,
            None => {
                let epoch = next_epoch(0);
                Queue {
                    epoch,
                    boundaries: vec![(epoch, EpochReason::Start)],
                    ..Queue::default()
                }
            }
        };
        let shared = Arc::new(Shared {
            queue: Mutex::new(queue),
            wake: Condvar::new(),
            opponent_frequency: AtomicU32::new(settings.opponent_frequency),
            player_id: AtomicI64::new(-1),
            session_time: AtomicU64::new(0f64.to_bits()),
            failed: AtomicBool::new(false),
            reset_pending: AtomicBool::new(left_over),
        });

        let thread = std::thread::Builder::new()
            .name("bono recorder".into())
            .spawn({
                let shared = shared.clone();
                let mut writer = Writer {
                    database: dir.join(DATABASE),
                    left_over,
                    settings_path: dir.join(SETTINGS),
                    clear_requests: settings.clear,
                    settings_modified: None,
                    next_settings_check: Instant::now() + SETTINGS_CHECK,
                    forecast,
                };
                move || {
                    let result = catch_unwind(AssertUnwindSafe(|| writer.run(&shared)));
                    if !matches!(result, Ok(Ok(()))) {
                        shared.failed.store(true, Ordering::Relaxed);
                    }
                }
            })?;

        Ok(Self {
            dir,
            shared,
            tracking: Mutex::new(Tracking::default()),
            thread: Some(thread),
        })
    }

    #[inline]
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Whether the writer stopped on an error: nothing more is recorded.
    #[inline]
    pub fn failed(&self) -> bool {
        self.shared.failed.load(Ordering::Relaxed)
    }

    /// Called for every telemetry update of every car, before converting it: moves the car on, and
    /// returns whether this update is recorded (opponents are kept at every Nth update). If it is,
    /// pass it to `push_telemetry` next.
    pub fn wants_telemetry(&self, vehicle_id: i64, session_time: f64, speed: f64) -> bool {
        if self.failed() {
            return false;
        }

        let mut tracking = lock(&self.tracking);
        // A replay isn't recorded: see `push_scoring`. Nor is telemetry from well before the
        // scoring: a replay's before its scoring says so, or a restart's before its scoring starts
        // the epoch it belongs in
        if tracking.replay
            || tracking
                .scoring_time
                .is_some_and(|scoring| session_time < scoring - 1.0)
        {
            return false;
        }
        tracking.advance(vehicle_id, session_time, speed);
        if vehicle_id == self.shared.player_id.load(Ordering::Relaxed) {
            return true;
        }

        let frequency = self
            .shared
            .opponent_frequency
            .load(Ordering::Relaxed)
            .max(1);
        let updates = tracking.opponent_updates.entry(vehicle_id).or_default();
        let keep = *updates % frequency == 0;
        *updates = updates.wrapping_add(1);
        return keep;
    }

    /// Queues a telemetry update `wants_telemetry` kept. Fills in `is_player`, and `lap_distance`
    /// when the simulator doesn't give it, estimated from the scoring updates.
    pub fn push_telemetry(&self, telemetry: &bono_telemetry) {
        // Held until it's queued: an epoch started meanwhile (on another thread) would have it
        // tagged with the new epoch but filled in from the old one
        let tracking = lock(&self.tracking);
        if tracking.replay {
            return;
        }
        let mut telemetry = *telemetry;
        let vehicle = telemetry.vehicle_id.get();
        let player = self.shared.player_id.load(Ordering::Relaxed);
        telemetry.is_player = vehicle.map(|vehicle| vehicle == player).into();
        if telemetry.lap_distance.get().is_none()
            && let Some(vehicle) = vehicle
        {
            telemetry.lap_distance = tracking
                .lap_distances
                .get(&vehicle)
                .map(|lap| lap.distance)
                .into();
        }

        let mut queue = lock(&self.shared.queue);
        if queue.telemetry.len() >= MAX_QUEUED_TELEMETRY {
            return;
        }
        let epoch = queue.epoch;
        queue.telemetry.push((epoch, telemetry));
        drop(queue);
        drop(tracking);
        self.shared.wake.notify_one();
    }

    /// Queues a scoring update: the session and every car's standing.
    ///
    /// Starts an epoch when the session's identity (track, type and number) changes, or its clock
    /// goes back more than a second. A replay (a car under replay control) isn't recorded; once it
    /// ends, recording carries on in the same epoch only if the session is the same and its clock
    /// hasn't gone back, and starts another otherwise.
    pub fn push_scoring(&self, session: bono_session_info, standings: Vec<bono_standing>) {
        if self.failed() {
            return;
        }

        let session_time = session.session_time.get().unwrap_or(0.0);
        let replay = standings
            .iter()
            .any(|standing| standing.control.as_deref() == Some("replay"));

        // Held until it's queued: an epoch started meanwhile (on another thread) would have this
        // session's scoring open it, and its player taken for the new session's
        let mut tracking = lock(&self.tracking);
        {
            if replay {
                tracking.replay = true;
                return;
            }
            let mut key = SessionKey::of(&session);
            let other_session = tracking
                .session_key
                .as_ref()
                .is_some_and(|last| last.differs(&key));
            let went_back = session
                .session_time
                .get()
                .zip(tracking.scoring_time)
                .is_some_and(|(time, last)| time < last - 1.0);
            // A field an update leaves out is still what it was, for telling the next one apart
            if !other_session && let Some(last) = &tracking.session_key {
                key.fill_from(last);
            }
            let after_replay = std::mem::take(&mut tracking.replay);
            let reason = if other_session {
                Some(EpochReason::Session)
            } else if went_back && after_replay {
                Some(EpochReason::Replay)
            } else if went_back {
                Some(EpochReason::ClockRollback)
            } else {
                None
            };
            if let Some(reason) = reason {
                let mut queue = lock(&self.shared.queue);
                self.start_epoch(&mut tracking, &mut queue, reason);
            }
            tracking.session_key = Some(key);
            if let Some(time) = session.session_time.get() {
                tracking.scoring_time = Some(tracking.scoring_time.map_or(time, |t| t.max(time)));
            }

            // Where each car is now, for the telemetry updates until the next scoring update
            tracking.track_length = session.track_length.get().unwrap_or(0.0);
            for standing in &standings {
                if let (Some(vehicle), Some(distance)) =
                    (standing.vehicle_id.get(), standing.lap_distance.get())
                {
                    let time = standing.session_time.get().unwrap_or(session_time);
                    tracking
                        .lap_distances
                        .insert(vehicle, LapDistance { distance, time });
                }
            }
        }

        self.shared
            .session_time
            .store(session_time.to_bits(), Ordering::Relaxed);

        // No car is the player's (spectating, a replay): no telemetry is flagged as theirs, rather
        // than the car that has the player's last id
        if !standings.is_empty() {
            let player = standings
                .iter()
                .find(|standing| standing.is_player.get() == Some(true))
                .and_then(|standing| standing.vehicle_id.get())
                .unwrap_or(-1);
            self.shared.player_id.store(player, Ordering::Relaxed);
        }

        let mut queue = lock(&self.shared.queue);
        if queue.scoring.len() >= MAX_QUEUED_SCORING {
            return;
        }
        let epoch = queue.epoch;
        queue.scoring.push((epoch, session, standings));
        drop(queue);
        drop(tracking);
        self.shared.wake.notify_one();
    }

    /// Starts an epoch: the updates queued from now on are another session's, or another part of
    /// the timeline. For adapters that know (a session starting, the clock jumping); the recorder
    /// also starts one itself from the scoring, see `push_scoring`. What's known about the cars
    /// goes, as the new session's cars aren't the old one's until scoring says so.
    pub fn begin_epoch(&self, reason: EpochReason) {
        let mut tracking = lock(&self.tracking);
        let mut queue = lock(&self.shared.queue);
        self.start_epoch(&mut tracking, &mut queue, reason);
        drop(queue);
        drop(tracking);
        self.shared.wake.notify_one();
    }

    /// The epoch updates are queued in now.
    pub fn epoch(&self) -> i64 {
        lock(&self.shared.queue).epoch
    }

    /// With both locks held (tracking's, then the queue's), so nothing is queued half in the old
    /// epoch and half in the new. The caller wakes the writer once they're released.
    fn start_epoch(&self, tracking: &mut Tracking, queue: &mut Queue, reason: EpochReason) {
        // A replay still playing still isn't recorded, whatever starts
        *tracking = Tracking {
            replay: tracking.replay,
            ..Tracking::default()
        };
        self.shared.player_id.store(-1, Ordering::Relaxed);
        queue.epoch = next_epoch(queue.epoch);
        let epoch = queue.epoch;
        queue.boundaries.push((epoch, reason));
    }

    /// Deletes everything recorded so far (on the writer thread, soon after), e.g. when a session
    /// ends. What's known about the cars goes too: until the next scoring update, the new
    /// session's cars aren't taken for the old one's (the player's id, where each car was).
    ///
    /// Starts an epoch too, so readers following the last one notice.
    pub fn clear(&self) {
        let mut tracking = lock(&self.tracking);
        let mut queue = lock(&self.shared.queue);
        queue.telemetry.clear();
        queue.scoring.clear();
        queue.boundaries.clear();
        queue.clear = true;
        // In the same breath as the clear: the writer never clears without the epoch that follows
        self.start_epoch(&mut tracking, &mut queue, EpochReason::Cleared);
        drop(queue);
        drop(tracking);
        self.shared.wake.notify_one();
    }
}

impl Drop for Recorder {
    /// Writes what's queued, then stops.
    fn drop(&mut self) {
        lock(&self.shared.queue).stop = true;
        self.shared.wake.notify_one();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

struct Shared {
    queue: Mutex<Queue>,
    wake: Condvar,
    opponent_frequency: AtomicU32,
    /// -1 before the first scoring update with the player's car
    player_id: AtomicI64,
    /// The session time of the last scoring update (an `f64`), for when the forecast was read
    session_time: AtomicU64,
    failed: AtomicBool,
    reset_pending: AtomicBool,
}

/// What's waiting to be written, each update with the epoch it was queued in.
#[derive(Default)]
struct Queue {
    /// The epoch updates are queued in now
    epoch: i64,
    /// Epochs started since the last write, and why
    boundaries: Vec<(i64, EpochReason)>,
    telemetry: Vec<(i64, bono_telemetry)>,
    scoring: Vec<(i64, bono_session_info, Vec<bono_standing>)>,
    /// `clear` was called: the writer deletes what was recorded
    clear: bool,
    stop: bool,
}

impl Queue {
    fn is_empty(&self) -> bool {
        self.telemetry.is_empty() && self.scoring.is_empty() && self.boundaries.is_empty()
    }
}

/// The epoch after `previous`. From the clock, so a recording made again (the game restarted) is
/// unlikely to reuse a reader's last epoch, and never going back within a recording.
fn next_epoch(previous: i64) -> i64 {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| {
            i64::try_from(since.as_micros()).unwrap_or(i64::MAX - 1)
        });
    now.max(previous.saturating_add(1))
}

/// What identifies a session in its scoring. Fields a simulator leaves out (or doesn't fill yet)
/// don't count as a change.
#[derive(Clone, Debug, Default, PartialEq)]
struct SessionKey {
    track: Option<String>,
    session_type: Option<String>,
    session_number: Option<i64>,
}

impl SessionKey {
    fn of(session: &bono_session_info) -> Self {
        Self {
            // Empty is as good as left out (a session still loading)
            track: session
                .track
                .as_deref()
                .filter(|track| !track.is_empty())
                .map(str::to_owned),
            session_type: session
                .session_type
                .as_deref()
                .filter(|kind| !kind.is_empty())
                .map(str::to_owned),
            session_number: session.session_number.get(),
        }
    }

    /// Takes what this leaves out from `last`.
    fn fill_from(&mut self, last: &Self) {
        self.track = self.track.take().or_else(|| last.track.clone());
        self.session_type = self
            .session_type
            .take()
            .or_else(|| last.session_type.clone());
        self.session_number = self.session_number.or(last.session_number);
    }

    fn differs(&self, other: &Self) -> bool {
        fn differ<T: PartialEq>(a: &Option<T>, b: &Option<T>) -> bool {
            matches!((a, b), (Some(a), Some(b)) if a != b)
        }
        differ(&self.track, &other.track)
            || differ(&self.session_type, &other.session_type)
            || differ(&self.session_number, &other.session_number)
    }
}

/// What the recorder follows of the epoch being recorded.
///
/// Telemetry may have no distance around the lap (LMU's doesn't), scoring (five times a second)
/// does. A telemetry update's is then the car's last scoring distance plus speed x time for every
/// telemetry update since.
#[derive(Default)]
struct Tracking {
    track_length: f64,
    lap_distances: HashMap<i64, LapDistance>,
    /// Updates seen per opponent, for keeping every Nth
    opponent_updates: HashMap<i64, u32>,
    /// The session the epoch's scoring is about
    session_key: Option<SessionKey>,
    /// The latest session time the epoch's scoring has reached
    scoring_time: Option<f64>,
    /// A replay is playing: nothing is recorded until the scoring is live again
    replay: bool,
}

struct LapDistance {
    distance: f64,
    /// The session time it was true at
    time: f64,
}

impl Tracking {
    fn advance(&mut self, vehicle_id: i64, session_time: f64, speed: f64) {
        let Some(lap) = self.lap_distances.get_mut(&vehicle_id) else {
            return;
        };

        // Telemetry a little older than the scoring update keeps its distance
        let dt = session_time - lap.time;
        if dt > 0.0 {
            lap.distance += speed * dt;
            lap.time = session_time;
            // Past the line before scoring says so
            if self.track_length > 0.0 && lap.distance >= self.track_length {
                lap.distance -= self.track_length;
            }
        }
    }
}

/// What the app asks of the recorder, in settings.json.
struct Settings {
    opponent_frequency: u32,
    /// The app adds one to this to have the recorded data cleared
    clear: i64,
}

impl Settings {
    /// With defaults for what's missing or invalid.
    fn read(path: &Path) -> Self {
        let json = fs::read(path)
            .ok()
            .and_then(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).ok());

        let field = |name: &str| json.as_ref().and_then(|json| json.get(name)?.as_i64());
        Self {
            opponent_frequency: field("opponent_frequency")
                .map_or(DEFAULT_OPPONENT_FREQUENCY, |n| n.clamp(1, 1000) as u32),
            clear: field("clear").unwrap_or(0),
        }
    }
}

/// The writer thread's state.
struct Writer {
    database: PathBuf,
    /// The database is an earlier run's that couldn't be deleted: it's emptied first
    left_over: bool,
    settings_path: PathBuf,
    /// The app's last clear request seen in settings.json
    clear_requests: i64,
    settings_modified: Option<SystemTime>,
    next_settings_check: Instant,
    forecast: Option<ForecastSource>,
}

impl Writer {
    fn run(&mut self, shared: &Shared) -> rusqlite::Result<()> {
        let conn = rusqlite::Connection::open(&self.database)?;
        // Readers on other connections can briefly hold the database: wait for them instead of failing
        conn.busy_timeout(Duration::from_secs(1))?;
        // WAL lets readers (the app, the race-engineer skill) run alongside this writer without ever
        // blocking a commit. NORMAL sync is still corruption-safe in WAL mode and skips an fsync per frame
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        if self.left_over {
            sql::recreate_tables(&conn)?;
            shared.reset_pending.store(false, Ordering::Relaxed);
        } else {
            sql::create_tables(&conn)?;
        }

        let mut telemetry_writer = TelemetryWriter::new(&conn)?;
        let mut scoring_writer = ScoringWriter::new(&conn)?;
        let mut forecast_writer = ForecastWriter::new(&conn)?;
        let mut next_forecast = Instant::now();

        let mut telemetry = Vec::new();
        let mut scoring = Vec::new();
        let mut boundaries = Vec::new();
        // The epoch the rows are being written in, whose row is written: `None` until the first
        let mut written = None::<i64>;
        loop {
            let mut clear = {
                let queue = lock(&shared.queue);
                // Wakes now and then even with nothing to write, to look at settings.json
                let (mut queue, _) = shared
                    .wake
                    .wait_timeout_while(queue, SETTINGS_CHECK, |queue| {
                        !queue.stop && !queue.clear && queue.is_empty()
                    })
                    .unwrap_or_else(PoisonError::into_inner);

                // Stopped, and nothing left to write
                if queue.stop && queue.is_empty() {
                    return Ok(());
                }

                std::mem::swap(&mut telemetry, &mut queue.telemetry);
                std::mem::swap(&mut scoring, &mut queue.scoring);
                std::mem::swap(&mut boundaries, &mut queue.boundaries);
                std::mem::take(&mut queue.clear)
            };

            // Asked by the app: what's waiting was recorded before it asked. Readers following the
            // last epoch are told by a new one
            if self.check_settings(shared) {
                telemetry.clear();
                scoring.clear();
                boundaries.clear();
                let mut queue = lock(&shared.queue);
                // Including what was queued since the batch was taken, and a clear asked for
                // meanwhile: this clear is the newer, and its epoch the active one
                queue.telemetry.clear();
                queue.scoring.clear();
                queue.boundaries.clear();
                queue.clear = false;
                queue.epoch = next_epoch(queue.epoch);
                boundaries.push((queue.epoch, EpochReason::Cleared));
                clear = true;
            }

            if clear {
                if let Err(e) = sql::clear_tables(&conn) {
                    // Still to do, by a restarted writer, with the epoch that follows it
                    let mut queue = lock(&shared.queue);
                    queue.clear = true;
                    queue.boundaries.splice(0..0, boundaries.drain(..));
                    return Err(e);
                }
                telemetry_writer.reset();
                scoring_writer.reset();
                forecast_writer.reset();
                next_forecast = Instant::now();
                written = None;
            }

            if !telemetry.is_empty() || !scoring.is_empty() || !boundaries.is_empty() {
                // Everything that arrived since the last write goes into one transaction, epoch by
                // epoch, each starting before its rows
                let tx = conn.unchecked_transaction()?;
                let mut epochs = boundaries
                    .iter()
                    .map(|(epoch, _)| *epoch)
                    .chain(telemetry.iter().map(|(epoch, _)| *epoch))
                    .chain(scoring.iter().map(|(epoch, ..)| *epoch))
                    .collect::<Vec<_>>();
                epochs.sort_unstable();
                epochs.dedup();
                for epoch in epochs {
                    // Queued before an epoch already written: it would land in that one
                    if written.is_some_and(|written| epoch < written) {
                        continue;
                    }
                    if written != Some(epoch) {
                        let reason = boundaries
                            .iter()
                            .find(|(start, _)| *start == epoch)
                            .map_or(EpochReason::Resumed, |(_, reason)| *reason);
                        sql::begin_epoch(&conn, epoch, reason)?;
                        // Each epoch has every car's state and standing of its own
                        telemetry_writer.reset();
                        scoring_writer.reset();
                        written = Some(epoch);
                    }
                    for (_, update) in telemetry.iter().filter(|(of, _)| *of == epoch) {
                        telemetry_writer.write(epoch, update)?;
                    }
                    for (_, session, standings) in
                        scoring.iter_mut().filter(|(of, ..)| *of == epoch)
                    {
                        scoring_writer.write(
                            epoch,
                            std::mem::take(session),
                            std::mem::take(standings),
                        )?;
                    }
                }
                tx.commit()?;
                telemetry.clear();
                scoring.clear();
                boundaries.clear();
            }

            // The forecast, now and then, after what's waiting has been written. A source that doesn't
            // answer only skips it this time
            if let Some(forecast) = &mut self.forecast
                && Instant::now() >= next_forecast
            {
                next_forecast = Instant::now() + FORECAST_CHECK;
                let session_time = f64::from_bits(shared.session_time.load(Ordering::Relaxed));
                if let Some(points) = forecast(session_time) {
                    forecast_writer.write(points)?;
                }
            }
        }
    }

    /// Picks up a changed settings.json while recording. Returns whether the app asked for the data
    /// to be cleared since the last look.
    fn check_settings(&mut self, shared: &Shared) -> bool {
        let now = Instant::now();
        if now < self.next_settings_check {
            return false;
        }
        self.next_settings_check = now + SETTINGS_CHECK;

        let Ok(modified) = fs::metadata(&self.settings_path).and_then(|meta| meta.modified())
        else {
            return false;
        };
        if self.settings_modified == Some(modified) {
            return false;
        }
        self.settings_modified = Some(modified);

        let settings = Settings::read(&self.settings_path);
        shared
            .opponent_frequency
            .store(settings.opponent_frequency, Ordering::Relaxed);
        return std::mem::replace(&mut self.clear_requests, settings.clear) != settings.clear;
    }
}

/// A panic on another thread mustn't stop recording: the data behind these locks stays valid.
#[inline]
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sys::bono_forecast_point;

    fn standing(vehicle_id: i64, is_player: bool, lap_distance: f64) -> bono_standing {
        let mut standing = bono_standing::default();
        standing.session_time = Some(10.0).into();
        standing.vehicle_id = Some(vehicle_id).into();
        standing.is_player = Some(is_player).into();
        standing.driver = Some(format!("Driver {vehicle_id}")).into();
        standing.lap_distance = Some(lap_distance).into();
        standing
    }

    #[test]
    fn records_into_the_database() {
        let dir = tempfile::tempdir().unwrap();
        let forecast: ForecastSource = Box::new(|session_time| {
            let mut point = bono_forecast_point::default();
            point.session_time = Some(session_time).into();
            point.sky_text = Some("Clear".to_owned()).into();
            Some(vec![point])
        });
        let recorder = Recorder::new(dir.path(), Some(forecast)).unwrap();

        let mut session = bono_session_info::default();
        session.session_time = Some(10.0).into();
        session.track = Some("Circuit de la Sarthe".to_owned()).into();
        session.track_length = Some(1000.0).into();
        // The same scoring twice: written once
        for _ in 0..2 {
            recorder.push_scoring(
                session.clone(),
                vec![standing(1, true, 990.0), standing(2, false, 100.0)],
            );
        }

        // The player's car, half a second after scoring at 40 m/s: past the line
        assert!(recorder.wants_telemetry(1, 10.5, 40.0));
        let mut telemetry = bono_telemetry::default();
        telemetry.vehicle_id = Some(1).into();
        telemetry.session_time = Some(10.5).into();
        recorder.push_telemetry(&telemetry);

        // Opponents at every 5th update
        let kept = (0..10)
            .filter(|i| recorder.wants_telemetry(2, 10.0 + *i as f64 * 0.01, 0.0))
            .count();
        assert_eq!(kept, 2);

        drop(recorder);
        let conn = rusqlite::Connection::open(dir.path().join(DATABASE)).unwrap();
        let count = |table: &str| -> i64 {
            conn.query_row(&format!("SELECT count(*) FROM {table}"), [], |row| {
                row.get(0)
            })
            .unwrap()
        };
        assert_eq!(count("session"), 1);
        assert_eq!(count("standings"), 2);
        assert_eq!(count("telemetry"), 1);
        assert_eq!(count("telemetry_wheel"), 4);
        assert_eq!(count("car_state"), 1);
        assert_eq!(count("forecast"), 1);

        let (is_player, lap_distance): (bool, f64) = conn
            .query_row("SELECT is_player, lap_distance FROM telemetry", [], |row| {
                Ok((row.get(0)?, row.get(1)?))
            })
            .unwrap();
        assert!(is_player);
        assert!((lap_distance - 10.0).abs() < 1e-9, "{lap_distance}");

        let track: String = conn
            .query_row("SELECT track FROM session", [], |row| row.get(0))
            .unwrap();
        assert_eq!(track, "Circuit de la Sarthe");
        let sky: String = conn
            .query_row("SELECT sky_text FROM forecast", [], |row| row.get(0))
            .unwrap();
        assert_eq!(sky, "Clear");
    }

    /// Waits for the writer to have written everything queued so far.
    fn written(recorder: &Recorder) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while !lock(&recorder.shared.queue).is_empty() {
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    fn restarting_a_failed_writer_preserves_earlier_sessions() {
        let dir = tempfile::tempdir().unwrap();
        // Forecast runs after queued scoring has committed.
        let mut recorder =
            Recorder::new(dir.path(), Some(Box::new(|_| panic!("forecast failed")))).unwrap();
        recorder.push_scoring(bono_session_info::default(), vec![standing(1, true, 0.0)]);
        let deadline = Instant::now() + Duration::from_secs(5);
        while !recorder.failed() {
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(10));
        }
        recorder.restart(None).unwrap();
        recorder.push_scoring(bono_session_info::default(), vec![standing(2, true, 0.0)]);
        drop(recorder);

        let conn = rusqlite::Connection::open(dir.path().join(DATABASE)).unwrap();
        let vehicles = conn
            .prepare("SELECT vehicle_id FROM standings ORDER BY id")
            .unwrap()
            .query_map([], |row| row.get::<_, i64>(0))
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap();
        assert_eq!(vehicles, [1, 2]);
        // Only the writer failed: the session carries on in the same epoch
        let epochs = conn
            .prepare("SELECT DISTINCT epoch FROM standings")
            .unwrap()
            .query_map([], |row| row.get::<_, i64>(0))
            .unwrap()
            .count();
        assert_eq!(epochs, 1);
        assert_eq!(
            reasons(&conn),
            ["start"],
            "the restarted writer found its epoch already there"
        );
    }

    #[test]
    fn a_restarted_writer_carries_on_from_what_was_known() {
        let dir = tempfile::tempdir().unwrap();
        let mut recorder =
            Recorder::new(dir.path(), Some(Box::new(|_| panic!("forecast failed")))).unwrap();
        recorder.push_scoring(session("Spa", "race", 500.0), vec![standing(1, true, 0.0)]);
        let deadline = Instant::now() + Duration::from_secs(5);
        while !recorder.failed() {
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(10));
        }
        recorder.restart(None).unwrap();
        // Still the player's car, before any scoring since
        recorder.push_telemetry(&telemetry(1, 500.0));
        // A restart of the race the writer failed in, with no adapter to say so
        recorder.push_scoring(session("Spa", "race", 3.0), vec![standing(1, true, 0.0)]);
        drop(recorder);

        let conn = rusqlite::Connection::open(dir.path().join(DATABASE)).unwrap();
        assert_eq!(reasons(&conn), ["start", "clock_rollback"]);
        let first = conn
            .query_row("SELECT min(id) FROM epoch", [], |row| row.get::<_, i64>(0))
            .unwrap();
        assert_eq!(telemetry_rows(&conn), [(first, 1, true)]);
    }

    fn session(track: &str, kind: &str, time: f64) -> bono_session_info {
        let mut session = bono_session_info::default();
        session.track = Some(track.to_owned()).into();
        session.session_type = Some(kind.to_owned()).into();
        session.session_number = Some(1).into();
        session.session_time = Some(time).into();
        session
    }

    fn telemetry(vehicle: i64, time: f64) -> bono_telemetry {
        let mut telemetry = bono_telemetry::default();
        telemetry.vehicle_id = Some(vehicle).into();
        telemetry.session_time = Some(time).into();
        telemetry
    }

    fn reasons(conn: &rusqlite::Connection) -> Vec<String> {
        conn.prepare("SELECT reason FROM epoch ORDER BY id")
            .unwrap()
            .query_map([], |row| row.get(0))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap()
    }

    /// (epoch, vehicle_id, is_player) of every telemetry row, in order.
    fn telemetry_rows(conn: &rusqlite::Connection) -> Vec<(i64, i64, bool)> {
        conn.prepare("SELECT epoch, vehicle_id, is_player FROM telemetry ORDER BY id")
            .unwrap()
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap()
    }

    #[test]
    fn a_new_session_starts_an_epoch_without_a_player_row_or_the_clock_going_back() {
        let dir = tempfile::tempdir().unwrap();
        let recorder = Recorder::new(dir.path(), None).unwrap();
        recorder.push_scoring(
            session("Spa", "practice", 100.0),
            vec![standing(1, true, 0.0)],
        );
        recorder.push_telemetry(&telemetry(1, 100.0));
        // Qualifying, later on the clock, spectating: nobody's car is the player's
        recorder.push_scoring(
            session("Spa", "qualifying", 200.0),
            vec![standing(1, false, 0.0)],
        );
        recorder.push_telemetry(&telemetry(1, 200.0));
        drop(recorder);

        let conn = rusqlite::Connection::open(dir.path().join(DATABASE)).unwrap();
        assert_eq!(reasons(&conn), ["start", "session"]);
        let epoch = sql::active_epoch(&conn).unwrap();
        let rows = telemetry_rows(&conn);
        assert_eq!(rows.len(), 2);
        assert!(rows[0].0 < rows[1].0);
        // The new epoch's rows are the last one's, and car 1 isn't the player's in it
        assert_eq!(rows[1], (epoch.id.unwrap(), 1, false));
        assert_eq!(epoch.telemetry_from, 2);
        assert_eq!(epoch.standings_from, 2);
        assert_eq!(epoch.session_from, 2);
    }

    #[test]
    fn a_session_change_is_seen_through_an_update_that_leaves_the_track_out() {
        let dir = tempfile::tempdir().unwrap();
        let recorder = Recorder::new(dir.path(), None).unwrap();
        recorder.push_scoring(session("Spa", "race", 10.0), vec![standing(1, true, 0.0)]);
        let mut unknown = session("Spa", "race", 11.0);
        unknown.track = None::<String>.into();
        recorder.push_scoring(unknown, vec![standing(1, true, 0.0)]);
        recorder.push_scoring(session("Monza", "race", 12.0), vec![standing(1, true, 0.0)]);
        drop(recorder);

        let conn = rusqlite::Connection::open(dir.path().join(DATABASE)).unwrap();
        assert_eq!(reasons(&conn), ["start", "session"]);
    }

    #[test]
    fn telemetry_from_well_before_the_scoring_is_not_recorded() {
        let dir = tempfile::tempdir().unwrap();
        let recorder = Recorder::new(dir.path(), None).unwrap();
        recorder.push_scoring(session("Spa", "race", 500.0), vec![standing(1, true, 0.0)]);
        assert!(recorder.wants_telemetry(1, 499.5, 10.0));
        // A replay's, or a restart's, before its scoring says so
        assert!(!recorder.wants_telemetry(1, 20.0, 10.0));
    }

    #[test]
    fn a_replay_playing_through_an_epoch_still_is_not_recorded() {
        let mut replaying = standing(1, true, 0.0);
        replaying.control = Some("replay".to_owned()).into();
        let dir = tempfile::tempdir().unwrap();
        let recorder = Recorder::new(dir.path(), None).unwrap();
        recorder.push_scoring(session("Spa", "race", 100.0), vec![standing(1, true, 0.0)]);
        recorder.push_scoring(session("Spa", "race", 5.0), vec![replaying]);
        recorder.clear();
        recorder.begin_epoch(EpochReason::Session);
        assert!(!recorder.wants_telemetry(1, 5.0, 10.0));
        recorder.push_scoring(session("Spa", "race", 6.0), vec![standing(1, true, 0.0)]);
        assert!(recorder.wants_telemetry(1, 6.0, 10.0));
    }

    #[test]
    fn a_clock_going_back_starts_an_epoch() {
        let dir = tempfile::tempdir().unwrap();
        let recorder = Recorder::new(dir.path(), None).unwrap();
        recorder.push_scoring(session("Spa", "race", 500.0), vec![standing(1, true, 0.0)]);
        // Less than a second back is the same session
        recorder.push_scoring(session("Spa", "race", 499.5), vec![standing(1, true, 0.0)]);
        // A restart
        recorder.push_scoring(session("Spa", "race", 3.0), vec![standing(1, true, 0.0)]);
        drop(recorder);

        let conn = rusqlite::Connection::open(dir.path().join(DATABASE)).unwrap();
        assert_eq!(reasons(&conn), ["start", "clock_rollback"]);
    }

    #[test]
    fn updates_stay_in_the_epoch_they_were_queued_in() {
        let dir = tempfile::tempdir().unwrap();
        let recorder = Recorder::new(dir.path(), None).unwrap();
        // Scoring and telemetry queued on both sides of a boundary, however the writer batches them
        recorder.push_scoring(session("Spa", "race", 10.0), vec![standing(1, true, 0.0)]);
        recorder.push_telemetry(&telemetry(1, 10.0));
        recorder.begin_epoch(EpochReason::Session);
        // The slot is reused by someone else, whose state must be written again
        recorder.push_telemetry(&telemetry(1, 0.5));
        recorder.push_scoring(session("Spa", "race", 1.0), vec![standing(1, true, 0.0)]);
        recorder.push_telemetry(&telemetry(1, 1.0));
        drop(recorder);

        let conn = rusqlite::Connection::open(dir.path().join(DATABASE)).unwrap();
        let epoch = sql::active_epoch(&conn).unwrap();
        let rows = telemetry_rows(&conn);
        let first = rows[0].0;
        let second = epoch.id.unwrap();
        // Before the new session's scoring, car 1 is nobody's
        assert_eq!(
            rows,
            [(first, 1, true), (second, 1, false), (second, 1, true)]
        );
        assert_eq!(epoch.telemetry_from, 2);
        let states = conn
            .query_row("SELECT count(*) FROM car_state", [], |row| {
                row.get::<_, i64>(0)
            })
            .unwrap();
        assert_eq!(states, 2, "each epoch has the car's state of its own");
        let standings_epochs = conn
            .prepare("SELECT epoch FROM standings ORDER BY id")
            .unwrap()
            .query_map([], |row| row.get::<_, i64>(0))
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap();
        assert_eq!(standings_epochs, [first, second]);
    }

    #[test]
    fn a_replay_is_not_recorded() {
        let replaying = |vehicle: i64| {
            let mut standing = standing(vehicle, true, 0.0);
            standing.control = Some("replay".to_owned()).into();
            standing
        };
        let dir = tempfile::tempdir().unwrap();
        let recorder = Recorder::new(dir.path(), None).unwrap();
        recorder.push_scoring(session("Spa", "race", 100.0), vec![standing(1, true, 0.0)]);

        // Watching the replay from the start
        recorder.push_scoring(session("Spa", "race", 5.0), vec![replaying(1)]);
        assert!(!recorder.wants_telemetry(1, 5.0, 10.0));
        // Live again where it left off: the same epoch
        recorder.push_scoring(session("Spa", "race", 101.0), vec![standing(1, true, 0.0)]);
        assert!(recorder.wants_telemetry(1, 101.0, 10.0));

        // Another replay, and back live with the clock earlier than before it: a new epoch
        recorder.push_scoring(session("Spa", "race", 50.0), vec![replaying(1)]);
        recorder.push_scoring(session("Spa", "race", 60.0), vec![standing(1, true, 0.0)]);
        drop(recorder);

        let conn = rusqlite::Connection::open(dir.path().join(DATABASE)).unwrap();
        assert_eq!(reasons(&conn), ["start", "replay"]);
        let times = conn
            .prepare("SELECT session_time FROM session ORDER BY id")
            .unwrap()
            .query_map([], |row| row.get::<_, f64>(0))
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap();
        assert_eq!(times, [100.0, 60.0]);
    }

    #[test]
    fn clearing_starts_an_epoch() {
        let dir = tempfile::tempdir().unwrap();
        let recorder = Recorder::new(dir.path(), None).unwrap();
        recorder.push_scoring(session("Spa", "race", 10.0), vec![standing(1, true, 0.0)]);
        written(&recorder);
        let before = recorder.epoch();
        recorder.clear();
        let after = recorder.epoch();
        assert!(after > before);
        drop(recorder);

        let conn = rusqlite::Connection::open(dir.path().join(DATABASE)).unwrap();
        assert_eq!(reasons(&conn), ["cleared"]);
        assert_eq!(sql::active_epoch(&conn).unwrap().id, Some(after));
    }

    #[test]
    fn forgets_the_player_when_cleared_or_spectating() {
        let dir = tempfile::tempdir().unwrap();
        let recorder = Recorder::new(dir.path(), None).unwrap();
        let telemetry = |vehicle: i64| {
            let mut telemetry = bono_telemetry::default();
            telemetry.vehicle_id = Some(vehicle).into();
            telemetry
        };

        recorder.push_scoring(bono_session_info::default(), vec![standing(1, true, 0.0)]);
        recorder.push_telemetry(&telemetry(1));
        written(&recorder);

        // A new weekend: car 1 is someone else's until scoring says otherwise
        recorder.clear();
        recorder.push_telemetry(&telemetry(1));
        written(&recorder);

        // Spectating: nobody is the player
        recorder.push_scoring(bono_session_info::default(), vec![standing(1, false, 0.0)]);
        recorder.push_telemetry(&telemetry(1));
        drop(recorder);

        let conn = rusqlite::Connection::open(dir.path().join(DATABASE)).unwrap();
        let flags = conn
            .prepare("SELECT is_player FROM telemetry ORDER BY id")
            .unwrap()
            .query_map([], |row| row.get::<_, bool>(0))
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap();
        assert_eq!(flags, [false, false]);
    }

    #[test]
    fn clears_what_was_recorded() {
        let dir = tempfile::tempdir().unwrap();
        let recorder = Recorder::new(dir.path(), None).unwrap();
        recorder.push_scoring(bono_session_info::default(), vec![standing(1, true, 0.0)]);
        // Telemetry too, which the wheel and car state rows reference
        assert!(recorder.wants_telemetry(1, 10.0, 0.0));
        let mut telemetry = bono_telemetry::default();
        telemetry.vehicle_id = Some(1).into();
        recorder.push_telemetry(&telemetry);
        written(&recorder);

        recorder.clear();
        recorder.push_scoring(bono_session_info::default(), vec![standing(2, true, 0.0)]);
        drop(recorder);

        let conn = rusqlite::Connection::open(dir.path().join(DATABASE)).unwrap();
        let telemetry = conn
            .query_row("SELECT count(*) FROM telemetry", [], |row| {
                row.get::<_, i64>(0)
            })
            .unwrap();
        assert_eq!(telemetry, 0);
        let vehicles = conn
            .prepare("SELECT vehicle_id FROM standings")
            .unwrap()
            .query_map([], |row| row.get::<_, i64>(0))
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap();
        assert_eq!(vehicles, [2]);
    }

    #[test]
    fn replaces_a_database_still_open_elsewhere() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(DATABASE);
        let count = |conn: &rusqlite::Connection| {
            conn.query_row("SELECT count(*) FROM standings", [], |row| {
                row.get::<_, i64>(0)
            })
        };

        let recorder = Recorder::new(dir.path(), None).unwrap();
        recorder.push_scoring(bono_session_info::default(), vec![standing(1, true, 0.0)]);
        drop(recorder);

        // A reader still has the last run's database open, so it can't be deleted (on Windows)
        let reader = rusqlite::Connection::open(&path).unwrap();
        assert_eq!(count(&reader).unwrap(), 1);
        let recorder = Recorder::new(dir.path(), None).unwrap();

        // The new run starts empty all the same
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let conn = rusqlite::Connection::open(&path).unwrap();
            if count(&conn).is_ok_and(|rows| rows == 0) {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "the last run's rows are still there"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        drop((reader, recorder));
    }

    #[test]
    fn removing_reports_open_databases() {
        let dir = tempfile::tempdir().unwrap();
        // Nothing to delete
        try_remove_database(dir.path()).unwrap();

        let reader = rusqlite::Connection::open(dir.path().join(DATABASE)).unwrap();
        reader.execute_batch("CREATE TABLE t (x)").unwrap();
        if cfg!(windows) {
            assert!(try_remove_database(dir.path()).is_err());
            assert!(dir.path().join(DATABASE).is_file());
        }

        drop(reader);
        try_remove_database(dir.path()).unwrap();
        assert!(!dir.path().join(DATABASE).exists());
    }

    #[test]
    fn readers_see_what_is_recorded() {
        let dir = tempfile::tempdir().unwrap();
        let recorder = Recorder::new(dir.path(), None).unwrap();
        recorder.push_scoring(bono_session_info::default(), vec![standing(1, true, 0.0)]);

        // As the app opens it: read-only, once the file is there, while the recorder is writing
        let deadline = Instant::now() + Duration::from_secs(5);
        let path = dir.path().join(DATABASE);
        while !path.is_file() {
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(10));
        }
        let reader = rusqlite::Connection::open_with_flags(
            &path,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )
        .unwrap();
        reader.busy_timeout(Duration::from_secs(1)).unwrap();

        loop {
            let rows = reader.query_row("SELECT count(*) FROM standings", [], |row| {
                row.get::<_, i64>(0)
            });
            if rows.as_ref().is_ok_and(|rows| *rows == 1) {
                break;
            }
            assert!(Instant::now() < deadline, "{rows:?}");
            std::thread::sleep(Duration::from_millis(10));
        }
        drop(recorder);
    }
}
