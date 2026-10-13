//! Records a simulator's updates into the database (see `sql`), for the plugins running inside the
//! simulator. The game's threads only copy updates; a thread of its own writes them, so the game
//! never waits on the database, and a failure stops recording instead of crashing the game.
//!
//! The database lives in a folder the app and the race-engineer skill's scripts know, next to a
//! `settings.json` the app writes to ask things of the recorder.

use crate::{
    sql::{self, ForecastWriter, ScoringWriter, TelemetryWriter},
    sys::{lambiase_forecast_point, lambiase_session_info, lambiase_standing, lambiase_telemetry},
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
    time::{Duration, Instant, SystemTime},
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
pub type ForecastSource = Box<dyn FnMut(f64) -> Option<Vec<lambiase_forecast_point>> + Send>;

/// Deletes the database in `dir`, with its WAL companions.
pub fn remove_database(dir: &Path) {
    for suffix in ["", "-wal", "-shm"] {
        let _ = fs::remove_file(dir.join(format!("{DATABASE}{suffix}")));
    }
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
        // A database left behind by a crash would mix two runs
        remove_database(&dir);

        let settings = Settings::read(&dir.join(SETTINGS));
        let shared = Arc::new(Shared {
            queue: Mutex::new(Queue::default()),
            wake: Condvar::new(),
            opponent_frequency: AtomicU32::new(settings.opponent_frequency),
            player_id: AtomicI64::new(-1),
            session_time: AtomicU64::new(0f64.to_bits()),
            failed: AtomicBool::new(false),
        });

        let thread = std::thread::Builder::new().name("lambiase recorder".into()).spawn({
            let shared = shared.clone();
            let mut writer = Writer {
                database: dir.join(DATABASE),
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
        tracking.advance(vehicle_id, session_time, speed);
        if vehicle_id == self.shared.player_id.load(Ordering::Relaxed) {
            return true;
        }

        let frequency = self.shared.opponent_frequency.load(Ordering::Relaxed).max(1);
        let updates = tracking.opponent_updates.entry(vehicle_id).or_default();
        let keep = *updates % frequency == 0;
        *updates = updates.wrapping_add(1);
        return keep;
    }

    /// Queues a telemetry update `wants_telemetry` kept. Fills in `is_player`, and `lap_distance`
    /// when the simulator doesn't give it, estimated from the scoring updates.
    pub fn push_telemetry(&self, telemetry: &lambiase_telemetry) {
        let mut telemetry = *telemetry;
        let vehicle = telemetry.vehicle_id.get();
        let player = self.shared.player_id.load(Ordering::Relaxed);
        telemetry.is_player = vehicle.map(|vehicle| vehicle == player).into();
        if telemetry.lap_distance.get().is_none()
            && let Some(vehicle) = vehicle
        {
            let tracking = lock(&self.tracking);
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
        queue.telemetry.push(telemetry);
        drop(queue);
        self.shared.wake.notify_one();
    }

    /// Queues a scoring update: the session and every car's standing.
    pub fn push_scoring(&self, session: lambiase_session_info, standings: Vec<lambiase_standing>) {
        if self.failed() {
            return;
        }

        let session_time = session.session_time.get().unwrap_or(0.0);
        self.shared
            .session_time
            .store(session_time.to_bits(), Ordering::Relaxed);

        {
            // Where each car is now, for the telemetry updates until the next scoring update
            let mut tracking = lock(&self.tracking);
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

        if let Some(player) = standings
            .iter()
            .find(|standing| standing.is_player.get() == Some(true))
            .and_then(|standing| standing.vehicle_id.get())
        {
            self.shared.player_id.store(player, Ordering::Relaxed);
        }

        let mut queue = lock(&self.shared.queue);
        if queue.scoring.len() >= MAX_QUEUED_SCORING {
            return;
        }
        queue.scoring.push((session, standings));
        drop(queue);
        self.shared.wake.notify_one();
    }

    /// Deletes everything recorded so far (on the writer thread, soon after), e.g. when a session
    /// ends.
    pub fn clear(&self) {
        let mut queue = lock(&self.shared.queue);
        queue.telemetry.clear();
        queue.scoring.clear();
        queue.clear = true;
        drop(queue);
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
}

#[derive(Default)]
struct Queue {
    telemetry: Vec<lambiase_telemetry>,
    scoring: Vec<(lambiase_session_info, Vec<lambiase_standing>)>,
    /// `clear` was called: the writer deletes what was recorded
    clear: bool,
    stop: bool,
}

impl Queue {
    fn is_empty(&self) -> bool {
        self.telemetry.is_empty() && self.scoring.is_empty()
    }
}

/// Telemetry may have no distance around the lap (LMU's doesn't), scoring (five times a second)
/// does. A telemetry update's is then the car's last scoring distance plus speed x time for every
/// telemetry update since.
#[derive(Default)]
struct Tracking {
    track_length: f64,
    lap_distances: HashMap<i64, LapDistance>,
    /// Updates seen per opponent, for keeping every Nth
    opponent_updates: HashMap<i64, u32>,
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
        sql::create_tables(&conn)?;

        let mut telemetry_writer = TelemetryWriter::new(&conn)?;
        let mut scoring_writer = ScoringWriter::new(&conn)?;
        let mut forecast_writer = ForecastWriter::new(&conn)?;
        let mut next_forecast = Instant::now();

        let mut telemetry = Vec::new();
        let mut scoring = Vec::new();
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
                std::mem::take(&mut queue.clear)
            };

            // Asked by the app: what's waiting was recorded before it asked
            if self.check_settings(shared) {
                telemetry.clear();
                scoring.clear();
                clear = true;
            }

            if clear {
                sql::clear_tables(&conn)?;
                telemetry_writer.reset();
                scoring_writer.reset();
                forecast_writer.reset();
                next_forecast = Instant::now();
            }

            if !telemetry.is_empty() || !scoring.is_empty() {
                // Everything that arrived since the last write goes into one transaction
                let tx = conn.unchecked_transaction()?;
                for update in telemetry.drain(..) {
                    telemetry_writer.write(&update)?;
                }
                for (session, standings) in scoring.drain(..) {
                    scoring_writer.write(session, standings)?;
                }
                tx.commit()?;
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

        let Ok(modified) = fs::metadata(&self.settings_path).and_then(|meta| meta.modified()) else {
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
    use crate::sys::lambiase_forecast_point;

    fn standing(vehicle_id: i64, is_player: bool, lap_distance: f64) -> lambiase_standing {
        let mut standing = lambiase_standing::default();
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
            let mut point = lambiase_forecast_point::default();
            point.session_time = Some(session_time).into();
            point.sky_text = Some("Clear".to_owned()).into();
            Some(vec![point])
        });
        let recorder = Recorder::new(dir.path(), Some(forecast)).unwrap();

        let mut session = lambiase_session_info::default();
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
        let mut telemetry = lambiase_telemetry::default();
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
            conn.query_row(&format!("SELECT count(*) FROM {table}"), [], |row| row.get(0))
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

    #[test]
    fn clears_what_was_recorded() {
        let dir = tempfile::tempdir().unwrap();
        let recorder = Recorder::new(dir.path(), None).unwrap();
        recorder.push_scoring(lambiase_session_info::default(), vec![standing(1, true, 0.0)]);
        recorder.clear();
        recorder.push_scoring(lambiase_session_info::default(), vec![standing(2, true, 0.0)]);
        drop(recorder);

        let conn = rusqlite::Connection::open(dir.path().join(DATABASE)).unwrap();
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
    fn readers_see_what_is_recorded() {
        let dir = tempfile::tempdir().unwrap();
        let recorder = Recorder::new(dir.path(), None).unwrap();
        recorder.push_scoring(lambiase_session_info::default(), vec![standing(1, true, 0.0)]);

        // As the app opens it: read-only, once the file is there, while the recorder is writing
        let deadline = Instant::now() + Duration::from_secs(5);
        let path = dir.path().join(DATABASE);
        while !path.is_file() {
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(10));
        }
        let reader =
            rusqlite::Connection::open_with_flags(&path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
                .unwrap();
        reader.busy_timeout(Duration::from_secs(1)).unwrap();

        loop {
            let rows = reader.query_row("SELECT count(*) FROM standings", [], |row| row.get::<_, i64>(0));
            if rows.as_ref().is_ok_and(|rows| *rows == 1) {
                break;
            }
            assert!(Instant::now() < deadline, "{rows:?}");
            std::thread::sleep(Duration::from_millis(10));
        }
        drop(recorder);
    }
}
