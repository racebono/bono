//! The tables the model's `sql` reads besides the plugin's own: laps, corner passes and the current
//! state of every car, with the recording's traps already handled (session boundaries, wear
//! direction, Kelvin temperatures, lap distances jumping back).
//!
//! They live in an in-memory database with the recording attached read-only under it, on one
//! connection owned by a background thread, so the app's UI never waits for any of this and the
//! model's query can join both. The thread follows the recording's new rows: `lap_stats`,
//! `lap_wheel_stats`, `lap_sector_stats`, `corner_passes` and `corner_pass_wheels` are updated as
//! they arrive, a corner pass as soon as the car leaves the corner. The `*_now` tables are only worth anything when
//! they're read, so they're written just before each query.

use super::*;
use rusqlite::{
    hooks::{AuthAction, AuthContext, Authorization},
    limits::Limit,
    params_from_iter,
    types::ValueRef,
};
use std::{
    borrow::Cow,
    collections::BTreeSet,
    sync::mpsc::{self, RecvTimeoutError},
};

/// How often the thread looks for new rows
const TICK: Duration = Duration::from_millis(100);
/// How often the thread tries again while the recording can't be opened
const RETRY_OPEN: Duration = Duration::from_secs(1);
/// How often the track description is looked at again
const LAYOUT_CHECK: Duration = Duration::from_secs(2);
/// The most rows of a table read at a time: a query waits for no more than this
const CHUNK: i64 = 50_000;
/// How long a query waits for the tables to catch up with the recording. Past it, it's answered
/// with what's there and `race_state.derived_catching_up` says so
const CATCH_UP: Duration = Duration::from_secs(3);
/// Bound a model query's strings, blobs and rows inside SQLite, before result formatting.
const SQL_MAX_VALUE_BYTES: i32 = 1024 * 1024;
/// An official lap time belongs to the lap that ended this close to its scoring row
const OFFICIAL_MATCH: f64 = 3.0;
/// The longest race clock taken as one; anything past this is a bad input, not a race.
const MAX_RACE_TIME_LEFT: f64 = 48.0 * 3600.0;
/// Passes with fewer samples than this are marked as low precision
const FEW_SAMPLES: usize = 5;
/// Metres past a corner's exit searched for the throttle coming back
pub(super) const THROTTLE_SEARCH: f64 = 300.0;
/// A lap distance this far ahead of the car's last one, as a share of the lap, isn't believed:
/// the first rows of a lap can still carry the old lap's distance
pub(super) const IMPOSSIBLE_JUMP: f64 = 0.5;
/// Where a lap's first believable distance can be at most, as a share of the lap: past it, it's the
/// old lap's, or a car waiting just short of the line
const FIRST_DISTANCE: f64 = 0.9;
/// How far from the line a lap's rows must start to have seen all of it, as a share of its rows'
/// furthest distance
const SEEN_FROM_THE_START: f64 = 0.05;
/// A lap with more of it than this unseen, as a share of the lap, isn't relied on
pub(super) const WIDEST_GAP: f64 = 0.05;

const TABLES: &str = "
CREATE TABLE lap_stats (
    vehicle_id INTEGER NOT NULL, lap INTEGER NOT NULL, stint_id INTEGER, stint_boundary INTEGER,
    first_id INTEGER, last_id INTEGER, start_s REAL,
    time_s REAL, official_time_s REAL, top_speed_kmh REAL,
    complete INTEGER, invalid INTEGER, pit INTEGER, refuelled INTEGER, contact INTEGER,
    green INTEGER, track_limit_steps INTEGER,
    fuel_start_l REAL, fuel_end_l REAL, fuel_used_l REAL,
    energy_start_pct REAL, energy_end_pct REAL, energy_used_pct REAL,
    gap_ahead_end_s REAL, gap_behind_end_s REAL,
    PRIMARY KEY (vehicle_id, lap)
) WITHOUT ROWID;
CREATE TABLE lap_wheel_stats (
    vehicle_id INTEGER NOT NULL, lap INTEGER NOT NULL, wheel INTEGER NOT NULL, wheel_name TEXT,
    life_start_pct REAL, life_end_pct REAL,
    surface_avg_c REAL, surface_peak_c REAL, inner_avg_c REAL, middle_avg_c REAL, outer_avg_c REAL,
    optimal_c REAL, pressure_avg_kpa REAL, brake_avg_c REAL, brake_peak_c REAL,
    PRIMARY KEY (vehicle_id, lap, wheel)
) WITHOUT ROWID;
CREATE TABLE lap_sector_stats (
    vehicle_id INTEGER NOT NULL, lap INTEGER NOT NULL, sector INTEGER NOT NULL, time_s REAL,
    PRIMARY KEY (vehicle_id, lap, sector)
) WITHOUT ROWID;
CREATE TABLE corner_passes (
    vehicle_id INTEGER NOT NULL, lap INTEGER NOT NULL, turn_index INTEGER NOT NULL,
    stint_id INTEGER, stint_boundary INTEGER, turn_name TEXT, direction TEXT, scale INTEGER,
    first_id INTEGER, last_id INTEGER, entered_at_s REAL, exited_at_s REAL,
    boundary_m REAL, start_m REAL, apex_m REAL, end_m REAL, next_boundary_m REAL,
    entry_s REAL, apex_to_end_s REAL, straight_s REAL, total_s REAL,
    brake_m_before_apex REAL, min_speed_kmh REAL, min_speed_at_m REAL, exit_speed_kmh REAL,
    throttle_on_m_after_apex REAL, gear_at_min INTEGER,
    braking_source TEXT, sample_count INTEGER, low_precision INTEGER,
    lap_invalid INTEGER, in_pit INTEGER, contact INTEGER,
    PRIMARY KEY (vehicle_id, lap, turn_index)
) WITHOUT ROWID;
CREATE TABLE corner_pass_wheels (
    vehicle_id INTEGER NOT NULL, lap INTEGER NOT NULL, turn_index INTEGER NOT NULL,
    wheel INTEGER NOT NULL,
    surface_rise_c REAL, surface_rise_braking_c REAL, surface_rise_off_throttle_c REAL,
    surface_rise_full_throttle_c REAL, surface_end_c REAL,
    brake_rise_c REAL, brake_rise_braking_c REAL, brake_rise_off_throttle_c REAL,
    brake_rise_full_throttle_c REAL, brake_end_c REAL,
    PRIMARY KEY (vehicle_id, lap, turn_index, wheel)
) WITHOUT ROWID;
CREATE TABLE race_state (
    session_type TEXT, session_number INTEGER, phase TEXT, track TEXT, track_length_m REAL,
    max_laps INTEGER, session_time_s REAL, time_left_s REAL, leader_laps_completed INTEGER,
    player_vehicle_id INTEGER, session_first_id INTEGER, latest_telemetry_id INTEGER,
    data_age_s REAL, is_stale INTEGER, corner_source TEXT, derived_catching_up INTEGER,
    full_course_yellow TEXT, local_yellow INTEGER, sector_count INTEGER, start_light INTEGER,
    air_c REAL, track_c REAL, rain_pct REAL,
    wetness_avg_pct REAL, wetness_min_pct REAL, wetness_max_pct REAL, grip_level INTEGER,
    cloudiness_pct REAL, wind_speed_kmh REAL,
    pit_window_status TEXT, pit_window_start_lap REAL, pit_window_end_lap REAL,
    pit_window_opens_in_s REAL, pit_window_closes_in_s REAL, pit_speed_limit_kmh REAL,
    track_limit_steps_per_penalty INTEGER, track_limit_steps_per_point INTEGER,
    max_incident_points REAL
);
CREATE TABLE cars_now (
    vehicle_id INTEGER PRIMARY KEY, is_player INTEGER, current_stint_id INTEGER, team TEXT,
    car_number TEXT,
    vehicle TEXT, vehicle_class TEXT, telemetry_id INTEGER, sample_age_s REAL,
    position INTEGER, class_position INTEGER, position_estimated INTEGER,
    qualifying_position INTEGER, place_ahead_vehicle_id INTEGER, place_behind_vehicle_id INTEGER,
    lap INTEGER, laps_completed INTEGER, sector INTEGER, lap_distance_m REAL, lap_elapsed_s REAL,
    laps_behind_leader INTEGER, laps_behind_next INTEGER,
    gap_place_ahead_s REAL, gap_place_behind_s REAL, gap_car_ahead_s REAL, gap_car_behind_s REAL,
    time_behind_leader_s REAL, time_behind_next_s REAL,
    ahead_of_player_m REAL, right_of_player_m REAL,
    last_lap_s REAL, best_lap_s REAL, delta_best_s REAL, lap_invalidated INTEGER,
    speed_kmh REAL, gear INTEGER, fuel_l REAL, fuel_fraction REAL, energy_left_pct REAL,
    battery_pct REAL, front_compound TEXT, rear_compound TEXT,
    water_c REAL, oil_c REAL, engine_overheating INTEGER,
    tc INTEGER, tc_slip INTEGER, tc_cut INTEGER, abs INTEGER, engine_map INTEGER,
    front_brake_bias_pct REAL, brake_migration INTEGER, front_arb INTEGER, rear_arb INTEGER,
    limiter_on INTEGER,
    engine_damage REAL, transmission_damage REAL, aero_damage REAL, suspension_damage REAL,
    parts_detached INTEGER, dents TEXT, last_impact_ago_s REAL, last_impact_magnitude REAL,
    in_pit_lane INTEGER, in_pits INTEGER, in_garage INTEGER, pit_state TEXT, pit_stops INTEGER,
    scheduled_pit_stops INTEGER, pit_services_selected TEXT, pit_services_active TEXT,
    pit_min_duration_left_s REAL,
    flag TEXT, under_yellow INTEGER, finish_status TEXT, penalties INTEGER, penalty_type TEXT,
    penalty_reason TEXT, track_limit_steps INTEGER, track_limit_points_left INTEGER,
    incident_points REAL
);
CREATE TABLE wheels_now (
    vehicle_id INTEGER NOT NULL, wheel INTEGER NOT NULL, wheel_name TEXT,
    life_left_pct REAL, stint_wear_pct_per_lap REAL, compound, flat INTEGER, detached INTEGER,
    surface_inner_c REAL, surface_middle_c REAL, surface_outer_c REAL, core_c REAL, carcass_c REAL,
    optimal_c REAL, surface_vs_optimal_c REAL,
    pressure_kpa REAL, brake_c REAL, brake_optimal_c REAL, surface_type INTEGER,
    sliding_fraction REAL,
    PRIMARY KEY (vehicle_id, wheel)
) WITHOUT ROWID;
CREATE TABLE sectors_now (
    vehicle_id INTEGER NOT NULL, sector INTEGER NOT NULL,
    last_lap_s REAL, best_lap_s REAL, best_s REAL, current_lap_s REAL,
    PRIMARY KEY (vehicle_id, sector)
) WITHOUT ROWID;
CREATE TABLE drivers_now (
    driver_id INTEGER PRIMARY KEY, vehicle_id INTEGER, name TEXT,
    first_seen_s REAL, last_seen_s REAL
);
CREATE TABLE stints (
    stint_id INTEGER PRIMARY KEY, vehicle_id INTEGER NOT NULL, stint_number INTEGER NOT NULL,
    driver_id INTEGER, start_reason TEXT, start_from_s REAL, start_to_s REAL,
    start_standing_id INTEGER, start_lap INTEGER, end_lap INTEGER, laps_complete INTEGER,
    baseline_laps INTEGER, baseline_source TEXT, avg_lap_s REAL, pace_trend_s_per_lap REAL,
    fuel_per_lap_l REAL, energy_per_lap_pct REAL, calculated_at_s REAL, lap_fraction REAL,
    fuel_l REAL, fuel_laps_left REAL, fuel_runs_out_lap REAL, tank_capacity_l REAL,
    energy_left_pct REAL, energy_laps_left REAL, energy_runs_out_lap REAL,
    game_fuel_per_lap_l REAL, game_energy_per_lap_pct REAL,
    limited_by TEXT, laps_left REAL, last_full_lap INTEGER, laps_needed REAL,
    laps_needed_source TEXT, margin_laps REAL, save_pct_needed REAL
);
CREATE TABLE forecast_now (
    session_type TEXT, is_current_session INTEGER, session_fraction REAL, minutes_from_now REAL,
    part_of_session TEXT, sky TEXT, sky_text TEXT, rain_chance_pct REAL, air_c REAL,
    humidity_pct REAL, wind_speed_kmh REAL, wind_direction TEXT
);
";

const STREAM_TABLES: [&str; 5] = [
    "lap_stats",
    "lap_wheel_stats",
    "lap_sector_stats",
    "corner_passes",
    "corner_pass_wheels",
];
const NOW_TABLES: [&str; 7] = [
    "race_state",
    "cars_now",
    "drivers_now",
    "stints",
    "wheels_now",
    "sectors_now",
    "forecast_now",
];

type Columns = Vec<(&'static str, Value)>;
/// Every car's stints, by car
type CarStints = BTreeMap<i64, Vec<Segment>>;

fn real(v: Option<f64>) -> Value {
    v.filter(|v| v.is_finite()).map_or(Value::Null, Value::Real)
}

fn integer(v: Option<i64>) -> Value {
    v.map_or(Value::Null, Value::Integer)
}

fn flag(v: bool) -> Value {
    Value::Integer(i64::from(v))
}

fn text_or_null(v: Option<&str>) -> Value {
    v.map_or(Value::Null, |v| Value::Text(v.to_owned()))
}

fn copy(row: &Row, name: &str) -> Value {
    row.value(name).cloned().unwrap_or(Value::Null)
}

fn km_h(v: Option<f64>) -> Option<f64> {
    v.map(|v| v * 3.6)
}

fn percent(v: Option<f64>) -> Option<f64> {
    v.map(|v| v * 100.0)
}

/// A virtual energy reading as a percentage, when the simulator reports a fraction.
fn energy_percent(v: Option<f64>) -> Option<f64> {
    v.filter(|v| *v <= 1.0).map(|v| v * 100.0)
}

/// Some simulators report Kelvin
fn celsius(v: Option<f64>) -> Option<f64> {
    v.filter(|v| *v != 0.0)
        .map(|v| if v > 200.0 { v - 273.15 } else { v })
}

fn insert(conn: &Connection, table: &str, columns: Columns) -> rusqlite::Result<()> {
    let names = columns
        .iter()
        .map(|(name, _)| *name)
        .collect::<Vec<_>>()
        .join(", ");
    let marks = vec!["?"; columns.len()].join(", ");
    conn.prepare_cached(&format!(
        "INSERT OR REPLACE INTO {table} ({names}) VALUES ({marks})"
    ))?
    .execute(params_from_iter(columns.into_iter().map(|(_, v)| v)))?;
    Ok(())
}

// --------------------------------------------------------------------------- the thread

/// What's asked of the thread.
pub(super) enum Job {
    Sql {
        query: String,
        limit: Option<usize>,
    },
    Track,
    /// Waits for the tables to catch up with the recording
    #[cfg(test)]
    Sync,
}

pub(super) struct Request {
    pub job: Job,
    pub reply: tokio::sync::oneshot::Sender<anyhow::Result<String>>,
}

/// Starts the thread, which ends once the sender returned is dropped.
pub(super) fn spawn(shared: Arc<Shared>) -> mpsc::Sender<Request> {
    let (send, recv) = mpsc::channel();
    if let Err(e) = std::thread::Builder::new()
        .name("engineer data".into())
        .spawn(move || thread(&shared, &recv))
    {
        log::warn!("The engineer's data thread didn't start: {e}");
    }
    send
}

fn thread(shared: &Shared, requests: &mpsc::Receiver<Request>) {
    let mut data = None::<Data>;
    let mut failing = false;
    // Straight away: the sooner it starts following, the less there is to catch up with
    let mut started = false;
    loop {
        let wait = match &data {
            None if !std::mem::replace(&mut started, true) => Duration::ZERO,
            // Reading everything again as fast as it fails helps nobody
            Some(data) if data.stream.failed => RETRY_OPEN,
            Some(data) if data.stream.behind => Duration::ZERO,
            Some(_) => TICK,
            None => RETRY_OPEN,
        };
        let request = match requests.recv_timeout(wait) {
            Ok(request) => Some(request),
            Err(RecvTimeoutError::Timeout) => None,
            Err(RecvTimeoutError::Disconnected) => return,
        };
        if data.is_none() {
            match open(&shared.path) {
                Ok(conn) => {
                    data = Some(Data {
                        conn,
                        stream: Stream::default(),
                    });
                }
                Err(e) => {
                    log::debug!("The engineer can't open the recording yet: {e}");
                    if let Some(request) = request {
                        let _ = request.reply.send(
                            Err(anyhow::Error::new(e)).context("the recording can't be opened"),
                        );
                    }
                    continue;
                }
            }
        }
        let Some(data) = &mut data else {
            continue;
        };
        match request {
            Some(request) => {
                let result = match request.job {
                    Job::Sql { query, limit } => data.sql(shared, &query, limit),
                    Job::Track => track_json(&data.conn, shared, &mut data.stream),
                    #[cfg(test)]
                    Job::Sync => data.sync(shared).map(|()| String::new()),
                };
                let _ = request.reply.send(result);
            }
            None => match data.stream.tick(&data.conn, shared) {
                Ok(()) => failing = false,
                // The recorder is busy, or hasn't made its tables yet: the next tick tries again
                Err(e) => {
                    if !std::mem::replace(&mut failing, true) {
                        log::debug!("The engineer's tables didn't update: {e:#}");
                    }
                }
            },
        }
    }
}

/// `path` as a URI for `ATTACH`, opening it read-only.
fn file_uri(path: &Path) -> String {
    let mut uri = String::from("file://");
    let path = path.to_string_lossy().replace('\\', "/");
    if !path.starts_with('/') {
        uri.push('/');
    }
    for c in path.chars() {
        match c {
            '%' => uri.push_str("%25"),
            '?' => uri.push_str("%3f"),
            '#' => uri.push_str("%23"),
            c => uri.push(c),
        }
    }
    uri.push_str("?mode=ro");
    uri
}

fn open(path: &Path) -> rusqlite::Result<Connection> {
    // The default flags let `ATTACH` take a URI
    let conn = Connection::open_in_memory()?;
    // The writer can briefly hold the database (a checkpoint, clearing it): wait for it
    conn.busy_timeout(Duration::from_secs(1))?;
    conn.set_prepared_statement_cache_capacity(64);
    conn.execute("ATTACH DATABASE ?1 AS recording", [file_uri(path)])?;
    conn.execute_batch(TABLES)?;
    Ok(conn)
}

struct Data {
    conn: Connection,
    stream: Stream,
}

impl Data {
    fn catch_up(&mut self, shared: &Shared) {
        let started = Instant::now();
        loop {
            if let Err(e) = self.stream.tick(&self.conn, shared) {
                log::debug!("The engineer's tables didn't update: {e:#}");
                return;
            }
            if !self.stream.behind || started.elapsed() > CATCH_UP {
                return;
            }
        }
    }

    #[cfg(test)]
    fn sync(&mut self, shared: &Shared) -> anyhow::Result<()> {
        loop {
            self.stream.tick(&self.conn, shared)?;
            if !self.stream.behind {
                return Ok(());
            }
        }
    }

    fn sql(
        &mut self,
        shared: &Shared,
        query: &str,
        limit: Option<usize>,
    ) -> anyhow::Result<String> {
        let query = query.trim().trim_end_matches(';');
        let lower = without_leading_comments(query).to_lowercase();
        if !["select", "with", "pragma"]
            .iter()
            .any(|p| lower.starts_with(p))
        {
            bail!("only SELECT / WITH / PRAGMA queries are allowed");
        }

        self.catch_up(shared);
        // Queries of the recording's own tables still work without a player's car
        let note = match self.stream.refresh_now(&self.conn, shared) {
            Ok(note) => note,
            Err(e) => Some(format!("{e:#}")),
        };

        let conn = &self.conn;
        conn.authorizer(Some(authorize))?;
        let result = (|| {
            let previous = conn.set_limit(Limit::SQLITE_LIMIT_LENGTH, SQL_MAX_VALUE_BYTES)?;
            let result = run_query(conn, &self.stream, query, limit, note.as_deref());
            // The worker's own writes and subsequent queries retain their normal limits,
            // including when this query fails.
            conn.set_limit(Limit::SQLITE_LIMIT_LENGTH, previous)?;
            result
        })();
        conn.authorizer(None::<fn(AuthContext<'_>) -> Authorization>)?;
        result
    }
}

/// A query from its first word on.
fn without_leading_comments(mut query: &str) -> &str {
    loop {
        query = query.trim_start();
        if let Some(rest) = query.strip_prefix("--") {
            query = rest.split_once('\n').map_or("", |(_, rest)| rest);
        } else if let Some(rest) = query.strip_prefix("/*") {
            query = rest.split_once("*/").map_or("", |(_, rest)| rest);
        } else {
            return query;
        }
    }
}

/// What the model's query may do. Nothing but reading: not even in the in-memory tables, or
/// through a `PRAGMA` changing how the connection works, which this thread's writes depend on.
fn authorize(context: AuthContext<'_>) -> Authorization {
    match context.action {
        AuthAction::Select
        | AuthAction::Read { .. }
        | AuthAction::Function { .. }
        | AuthAction::Recursive => Authorization::Allow,
        AuthAction::Pragma { pragma_name, .. }
            if matches!(
                pragma_name,
                "table_info" | "table_xinfo" | "table_list" | "index_list" | "index_info"
            ) =>
        {
            Authorization::Allow
        }
        _ => Authorization::Deny,
    }
}

/// A number for the model: four significant digits, or to the millisecond for the larger ones (a
/// lap time, a session's clock, a distance), which those would blur.
fn real_text(x: f64) -> String {
    match x.abs() >= 10.0 && x.is_finite() {
        true => {
            let text = format!("{x:.3}");
            text.trim_end_matches('0').trim_end_matches('.').to_owned()
        }
        false => g4(x),
    }
}

fn run_query(
    conn: &Connection,
    stream: &Stream,
    query: &str,
    limit: Option<usize>,
    note: Option<&str>,
) -> anyhow::Result<String> {
    let limit = limit.unwrap_or(60).clamp(1, SQL_MAX_ROWS);
    let mut out = String::new();
    let latest = scalar::<i64>(conn, "SELECT MAX(id) FROM telemetry", [])
        .ok()
        .flatten()
        .unwrap_or(stream.cursor);
    say!(
        out,
        "-- player vehicle_id={}, session_first_id={}, latest_telemetry_id={latest}",
        int(stream.player),
        stream.session_start,
    );
    if stream.behind || stream.failed {
        say!(
            out,
            "-- the lap and corner tables are still reading the session (up to telemetry row {} of {latest}): their figures are partial",
            stream.cursor
        );
    }
    if let Some(note) = note {
        say!(out, "-- the current-state tables are incomplete: {note}");
    }

    let mut stmt = conn.prepare(query)?;
    if !stmt.readonly() {
        bail!("only queries that read are allowed");
    }

    // Stopped from inside SQLite: a runaway query would otherwise hold the connection
    let deadline = Instant::now() + SQL_TIME;
    conn.progress_handler(10_000, Some(move || Instant::now() > deadline))?;
    let result = (|| -> rusqlite::Result<()> {
        let names = stmt
            .column_names()
            .into_iter()
            .map(telemetry_text)
            .collect::<Vec<_>>();
        say!(out, "{}", names.join(" | "));
        if out.len() > SQL_MAX_BYTES {
            out.clear();
            say!(out, "... (SQL output exceeded byte limit)");
            return Ok(());
        }
        let columns = names.len();
        let mut rows = stmt.query([])?;
        let mut n = 0;
        while let Some(row) = rows.next()? {
            let values = (0..columns)
                .map(|i| {
                    Ok(match row.get_ref(i)? {
                        ValueRef::Null => "NULL".into(),
                        ValueRef::Integer(n) => n.to_string(),
                        ValueRef::Real(x) => real_text(x),
                        ValueRef::Text(text) => telemetry_text(
                            std::str::from_utf8(text)
                                .map_err(|e| rusqlite::Error::Utf8Error(i, e))?,
                        ),
                        ValueRef::Blob(blob) => format!("<{} bytes>", blob.len()),
                    })
                })
                .collect::<rusqlite::Result<Vec<_>>>()?;
            let line = values.join(" | ");
            // Leave space for the truncation notice, and never split a UTF-8 string.
            if out.len() + line.len() + 80 > SQL_MAX_BYTES {
                say!(out, "... (SQL output stopped at byte limit)");
                break;
            }
            say!(out, "{line}");
            n += 1;
            if n >= limit {
                say!(out, "... (stopped at {limit} rows)");
                break;
            }
        }
        Ok(())
    })();
    conn.progress_handler(0, None::<fn() -> bool>)?;

    match result {
        Err(rusqlite::Error::SqliteFailure(e, _))
            if e.code == rusqlite::ErrorCode::OperationInterrupted =>
        {
            bail!(
                "the query took longer than {} seconds and was stopped: scope it by id, or use the derived tables",
                SQL_TIME.as_secs()
            )
        }
        result => {
            result?;
            Ok(out)
        }
    }
}

// --------------------------------------------------------------------------- following the recording

/// One telemetry row, as far as laps and corners go.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) struct Sample {
    pub id: i64,
    pub vehicle: i64,
    pub is_player: bool,
    pub session_time: Option<f64>,
    pub lap: Option<i64>,
    pub lap_start: Option<f64>,
    pub lap_distance: Option<f64>,
    pub in_pit: bool,
    pub invalid: bool,
    pub speed: Option<f64>,
    pub brake: Option<f64>,
    pub throttle: Option<f64>,
    pub gear: Option<i64>,
    pub fuel: Option<f64>,
    pub energy: Option<f64>,
    pub gap_ahead: Option<f64>,
    pub gap_behind: Option<f64>,
}

pub(super) const SAMPLE_COLUMNS: &str = "id, vehicle_id, is_player, session_time, lap, \
     lap_start_time, lap_distance, in_pit_lane, lap_invalidated, speed, brake, throttle, gear, \
     fuel, virtual_energy, gap_to_place_ahead, gap_to_place_behind";

pub(super) fn sample(row: &rusqlite::Row<'_>) -> rusqlite::Result<Sample> {
    let on = |value: Option<f64>| value.is_some_and(|value| value != 0.0);
    Ok(Sample {
        id: row.get(0)?,
        vehicle: row.get::<_, Option<i64>>(1)?.unwrap_or(-1),
        is_player: on(row.get(2)?),
        session_time: row.get(3)?,
        lap: row.get(4)?,
        lap_start: row.get(5)?,
        lap_distance: row.get(6)?,
        in_pit: on(row.get(7)?),
        invalid: on(row.get(8)?),
        speed: row.get(9)?,
        brake: row.get(10)?,
        throttle: row.get(11)?,
        gear: row.get(12)?,
        fuel: row.get(13)?,
        energy: row.get(14)?,
        gap_ahead: row.get(15)?,
        gap_behind: row.get(16)?,
    })
}

/// One lap of one car in the current session.
#[derive(Clone)]
pub(super) struct Lap {
    pub lap: i64,
    pub first_id: i64,
    pub last_id: i64,
    pub start: Option<f64>,
    /// The next lap's start once it's known, or else the session time of its last row
    pub end: Option<f64>,
    pub time: Option<f64>,
    pub complete: bool,
    pub invalid: bool,
    pub pit: bool,
    pub refuelled: bool,
    pub contact: bool,
    pub green: bool,
    pub impact: Option<f64>,
    pub track_limit_steps: Option<i64>,
    pub top_speed: Option<f64>,
    pub fuel_start: Option<f64>,
    pub fuel_end: Option<f64>,
    pub fuel_used: Option<f64>,
    pub energy_start: Option<f64>,
    pub energy_end: Option<f64>,
    pub energy_used: Option<f64>,
    pub gap_ahead_end: Option<f64>,
    pub gap_behind_end: Option<f64>,
    pub wear_start: [Option<f64>; 4],
    pub wear_end: [Option<f64>; 4],
}

impl Lap {
    fn has_wear(wear: &[Option<f64>; 4]) -> bool {
        wear.iter().any(Option::is_some)
    }

    fn span(&self) -> LapSpan {
        LapSpan {
            lap: self.lap,
            start: self.start,
            end: self.end,
            stopped: self.pit || self.refuelled,
        }
    }
}

/// LMU reports tire_wear as the fraction REMAINING (1.0 = new) even though the schema says worn.
/// Decided from the data: if the value falls during a stint it's remaining. A lap in the pits says
/// nothing: new tyres go up either way.
fn wear_remaining(laps: &[Lap]) -> bool {
    for lap in laps.iter().filter(|lap| !lap.pit && !lap.refuelled) {
        if Lap::has_wear(&lap.wear_start) && Lap::has_wear(&lap.wear_end) {
            let d = (0..4)
                .filter_map(|w| Some(lap.wear_end[w]? - lap.wear_start[w]?))
                .collect::<Vec<_>>();
            let sum = d.iter().sum::<f64>();
            if !d.is_empty() && sum.abs() > 1e-4 {
                return sum < 0.0;
            }
        }
    }
    true
}

/// Representative laps for finding and aligning corners: clean first, with invalidated laps
/// still usable when no clean trace is available. Invalidation doesn't erase the geometry.
fn best_laps(laps: &[Lap]) -> Vec<&Lap> {
    let mut laps = laps
        .iter()
        .filter(|lap| lap.green && lap.time.is_some())
        .collect::<Vec<_>>();
    laps.sort_by(|a, b| {
        a.invalid
            .cmp(&b.invalid)
            .then_with(|| a.time.partial_cmp(&b.time).unwrap_or(Ordering::Equal))
    });
    laps
}

/// The preferred representative lap, including invalidated evidence if necessary.
fn best_lap(laps: &[Lap]) -> Option<&Lap> {
    best_laps(laps).first().copied()
}

/// A trace with usable braking zones, preferring clean laps over invalidated laps.
fn best_seen_lap<'a>(
    conn: &Connection,
    vehicle: i64,
    laps: &'a [Lap],
    length: f64,
    allow_invalid: bool,
) -> rusqlite::Result<Option<(&'a Lap, Trace)>> {
    for lap in best_laps(laps)
        .into_iter()
        .filter(|lap| allow_invalid || !lap.invalid)
    {
        let trace = Trace::read(conn, vehicle, lap, length)?;
        if trace.ok && !trace.braking_zones().is_empty() {
            return Ok(Some((lap, trace)));
        }
    }
    Ok(None)
}

/// Whether a row's lap distance `d` can follow the furthest one so far on its lap. The first rows of
/// a lap can still carry the old lap's distance, and a car waiting on the grid or in its pit box can
/// be just short of the line; a car joining midway really does start far on.
pub(super) fn believable(top: f64, d: f64, length: f64) -> bool {
    match top > 0.0 {
        true => d - top <= length * IMPOSSIBLE_JUMP,
        false => d <= length * FIRST_DISTANCE,
    }
}

/// What temperatures rose by in one corner section, for one wheel.
#[derive(Clone, Copy, Default)]
struct Rises {
    /// Tyre surface, brake: each by what the car was doing (braking, off the throttle, full
    /// throttle)
    by_phase: [[f64; 3]; 2],
    /// Rises without pedal data to tell what the car was doing
    other: [f64; 2],
    last: [Option<f64>; 2],
}

/// A lap being recorded.
struct LapBuild {
    lap: i64,
    first_id: i64,
    last_id: i64,
    start: Option<f64>,
    invalid: bool,
    pit: bool,
    top_speed: Option<f64>,
    fuel_start: Option<f64>,
    energy_start: Option<f64>,
    fuel_end: Option<f64>,
    energy_end: Option<f64>,
    gap_ahead_end: Option<f64>,
    gap_behind_end: Option<f64>,
    samples: Vec<Sample>,
    /// The furthest believable lap distance so far
    top: f64,
    /// The furthest lap distance of its rows
    furthest: Option<f64>,
    /// What's been written of each corner's pass: how many of its marks the car has passed
    marks_passed: Vec<u8>,
    /// The next distance at which something of a pass can be written
    next_mark: f64,
    /// By corner, by wheel
    rises: Vec<[Rises; 4]>,
}

impl LapBuild {
    fn new(sample: &Sample, lap: i64, layout: Option<&Layout>) -> Self {
        let corners = layout.map_or(0, |layout| layout.corners.len());
        Self {
            lap,
            first_id: sample.id,
            last_id: sample.id,
            start: None,
            invalid: false,
            pit: false,
            top_speed: None,
            fuel_start: sample.fuel,
            energy_start: sample.energy,
            fuel_end: None,
            energy_end: None,
            gap_ahead_end: None,
            gap_behind_end: None,
            samples: Vec::new(),
            top: 0.0,
            furthest: None,
            marks_passed: vec![0; corners],
            next_mark: layout.map_or(f64::INFINITY, |layout| next_mark(layout, 0.0)),
            rises: vec![[Rises::default(); 4]; corners],
        }
    }

    fn add(&mut self, sample: &Sample, length: Option<f64>) {
        self.last_id = sample.id;
        if sample
            .lap_start
            .is_some_and(|start| !self.start.is_some_and(|s| s >= start))
        {
            self.start = sample.lap_start;
        }
        self.invalid |= sample.invalid;
        self.pit |= sample.in_pit;
        if sample
            .speed
            .is_some_and(|speed| !self.top_speed.is_some_and(|top| top >= speed))
        {
            self.top_speed = sample.speed;
        }
        self.fuel_end = sample.fuel;
        self.energy_end = sample.energy;
        self.gap_ahead_end = sample.gap_ahead;
        self.gap_behind_end = sample.gap_behind;
        self.samples.push(*sample);
        if let Some(d) = sample.lap_distance {
            self.furthest = Some(self.furthest.map_or(d, |furthest| furthest.max(d)));
        }
        if let (Some(d), Some(length)) = (sample.lap_distance, length) {
            let d = d.min(length);
            if believable(self.top, d, length) {
                self.top = self.top.max(d);
            }
        }
    }

    /// How long it took, when it ended and what it used, `next` being the lap after it (if it's
    /// finished): consumption runs from the start of this lap to the start of the next (or else to
    /// its last row).
    fn timing(&self, next: Option<&LapBuild>) -> Timing {
        // With laps missing after it, the next lap's start says nothing about this one
        let next_lap = next.filter(|next| next.lap == self.lap + 1);
        let (fuel_1, energy_1) = match (next, next_lap) {
            (None, _) => (self.fuel_end, self.energy_end),
            (Some(_), Some(next)) => (next.fuel_start, next.energy_start),
            (Some(_), None) => (None, None),
        };
        let fuel_used = self.fuel_start.zip(fuel_1).map(|(a, b)| a - b);
        let energy_used = self.energy_start.zip(energy_1).map(|(a, b)| a - b);
        let next_start = next_lap.and_then(|next| next.start).filter(|s| *s != 0.0);
        let time = next_start
            .zip(self.start)
            .map(|(next, start)| next - start)
            .filter(|time| *time > 0.0);
        let end = match time {
            Some(_) => next_start,
            None => self.samples.last().and_then(|sample| sample.session_time),
        };
        Timing {
            next_lap: next_lap.is_some(),
            time,
            end,
            fuel_used,
            energy_used,
            refuelled: fuel_used.is_some_and(|f| f < -0.5)
                || energy_used.is_some_and(|v| v < -0.01),
        }
    }

    fn span(&self, next: Option<&LapBuild>) -> LapSpan {
        let timing = self.timing(next);
        LapSpan {
            lap: self.lap,
            start: self.start,
            end: timing.end,
            stopped: self.pit || timing.refuelled,
        }
    }

    /// The lap as it stands, `next` being the lap after it (if it's finished).
    fn lap(
        &self,
        conn: &Connection,
        vehicle: i64,
        next: Option<&LapBuild>,
        previous_impact: Option<f64>,
    ) -> rusqlite::Result<Lap> {
        let state = car_state(conn, vehicle, self.last_id)?;
        let impact = state.as_ref().and_then(|state| state.f("last_impact_time"));
        let wear = |id: i64| -> rusqlite::Result<[Option<f64>; 4]> {
            let mut wear = [None; 4];
            for (wheel, r) in wheels_at(conn, id)? {
                if let Some(w) = wear.get_mut(wheel) {
                    *w = r.f("tire_wear");
                }
            }
            Ok(wear)
        };
        let Timing {
            next_lap,
            time,
            end,
            fuel_used,
            energy_used,
            refuelled,
        } = self.timing(next);
        let complete = next.is_some();
        // A car joining midway used a fraction of a lap's fuel. The lap has to start near the line
        // and get back round: first rows of the old lap's distance don't count, and near the line
        // an estimated distance can wrap before the lap number does
        let seen_from_the_start = self.furthest.is_none_or(|furthest| {
            let mut rest = self
                .samples
                .iter()
                .filter_map(|sample| sample.lap_distance)
                .skip_while(|d| *d > furthest * FIRST_DISTANCE);
            rest.next()
                .is_some_and(|d| d <= furthest * SEEN_FROM_THE_START)
                && rest.any(|d| d >= furthest * FIRST_DISTANCE)
        });
        Ok(Lap {
            lap: self.lap,
            first_id: self.first_id,
            last_id: self.last_id,
            start: self.start,
            end,
            time,
            complete,
            invalid: self.invalid,
            pit: self.pit,
            refuelled,
            contact: impact.is_some_and(|impact| impact > 0.0) && impact != previous_impact,
            // A pit lap is an in or out lap: the pit lane flag was set at some point during the lap
            green: next_lap && seen_from_the_start && !self.pit && !refuelled,
            impact,
            track_limit_steps: state
                .as_ref()
                .and_then(|state| state.i("track_limits_steps")),
            top_speed: self.top_speed,
            fuel_start: self.fuel_start,
            fuel_end: self.fuel_end,
            fuel_used,
            energy_start: self.energy_start,
            energy_end: self.energy_end,
            energy_used,
            gap_ahead_end: self.gap_ahead_end,
            gap_behind_end: self.gap_behind_end,
            wear_start: wear(self.first_id)?,
            wear_end: wear(self.last_id)?,
        })
    }
}

/// One car's laps.
#[derive(Default)]
struct Car {
    finished: Vec<Lap>,
    /// Over, but the next lap's start time isn't in yet: the first rows of a lap can still carry
    /// the old one
    waiting: Option<LapBuild>,
    current: Option<LapBuild>,
    /// The last impact as of the last finished lap
    last_impact: Option<f64>,
    /// Wear has been seen to count up, and the life already written was put right
    wear_counts_up: bool,
    /// Its stints as its rows were last written with
    stints: Vec<Segment>,
}

impl Car {
    /// Every lap so far, the one being recorded too.
    fn laps(&self, conn: &Connection, vehicle: i64) -> rusqlite::Result<Vec<Lap>> {
        let mut laps = self.finished.clone();
        let mut impact = self.last_impact;
        if let Some(waiting) = &self.waiting {
            let lap = waiting.lap(conn, vehicle, self.current.as_ref(), impact)?;
            if lap.impact.is_some_and(|i| i != 0.0) {
                impact = lap.impact;
            }
            laps.push(lap);
        }
        if let Some(current) = &self.current {
            laps.push(current.lap(conn, vehicle, None, impact)?);
        }
        Ok(laps)
    }

    /// When every lap so far was driven, and which were stops: all its stints need.
    fn spans(&self) -> Vec<LapSpan> {
        let mut spans = self.finished.iter().map(Lap::span).collect::<Vec<_>>();
        if let Some(waiting) = &self.waiting {
            spans.push(waiting.span(self.current.as_ref()));
        }
        if let Some(current) = &self.current {
            spans.push(current.span(None));
        }
        spans
    }

    /// A row of an earlier lap than the one under way that's only a late one of a lap just ended,
    /// not another car in the slot.
    fn is_late_row(&self, sample: &Sample, lap: i64) -> bool {
        let same = |start: Option<f64>| sample.lap_start.is_none_or(|s| Some(s) == start);
        self.waiting
            .as_ref()
            .is_some_and(|waiting| waiting.lap == lap && same(waiting.start))
            || self
                .finished
                .last()
                .is_some_and(|finished| finished.lap == lap && same(finished.start))
    }
}

// --------------------------------------------------------------------------- drivers and stints

/// When a lap was driven, and whether it was a stop: all a car's stints need of it.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) struct LapSpan {
    pub lap: i64,
    pub start: Option<f64>,
    pub end: Option<f64>,
    /// In the pit lane at some point, or refuelled
    pub stopped: bool,
}

/// What a lap took and used, as `LapBuild::timing` works it out.
struct Timing {
    /// The lap after it is the next one, not one further on
    next_lap: bool,
    time: Option<f64>,
    end: Option<f64>,
    fuel_used: Option<f64>,
    energy_used: Option<f64>,
    refuelled: bool,
}

/// Someone the standings named as a car's driver.
struct Driver {
    vehicle: i64,
    name: String,
    first_seen: f64,
    last_seen: f64,
    /// Another car took the slot: they were in the one that left
    gone: bool,
}

/// A spell of one driver in a car, from the first standings row naming them to the last.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) struct Run {
    pub driver: i64,
    pub first: f64,
    pub last: f64,
    /// The standings row first naming them, if it's known which
    pub standing_id: Option<i64>,
}

/// The session's drivers. A name is all the simulator gives, so a driver is a name in a car: the
/// same name in another car is someone else.
#[derive(Default)]
struct Drivers {
    ids: HashMap<(i64, String), i64>,
    /// By id, from 1
    known: Vec<Driver>,
    /// By car, who drove it in turn
    runs: HashMap<i64, Vec<Run>>,
    /// By car, when another car took the slot: standings from before then are the last one's
    replaced: HashMap<i64, f64>,
}

impl Drivers {
    /// The standings name `name` the car's driver at `t`. Whether that starts a spell.
    fn seen(&mut self, vehicle: i64, name: &str, t: f64, standing_id: i64) -> bool {
        if self
            .replaced
            .get(&vehicle)
            .is_some_and(|replaced| t < *replaced)
        {
            return false;
        }
        let next = self.known.len() as i64 + 1;
        let id = *self.ids.entry((vehicle, name.to_owned())).or_insert(next);
        if id == next {
            self.known.push(Driver {
                vehicle,
                name: name.to_owned(),
                first_seen: t,
                last_seen: t,
                gone: false,
            });
        }
        let driver = &mut self.known[(id - 1) as usize];
        driver.first_seen = driver.first_seen.min(t);
        driver.last_seen = driver.last_seen.max(t);
        let runs = self.runs.entry(vehicle).or_default();
        match runs.last_mut() {
            Some(run) if run.driver == id => {
                run.last = run.last.max(t);
                false
            }
            _ => {
                runs.push(Run {
                    driver: id,
                    first: t,
                    last: t,
                    standing_id: Some(standing_id),
                });
                true
            }
        }
    }

    /// Another car took the slot at `t`: what the standings said before then was the last one's,
    /// read or still to be read. A spell going on past it (the same name in the new car) is the
    /// new one's from then on.
    fn replaced(&mut self, vehicle: i64, t: f64) {
        self.replaced.insert(vehicle, t);
        let runs = self.runs.entry(vehicle).or_default();
        runs.retain(|run| run.last >= t);
        for run in runs.iter_mut().filter(|run| run.first < t) {
            run.first = t;
            run.standing_id = None;
        }
        // When each driver staying was first seen in the new car
        let mut staying = HashMap::new();
        for run in runs.iter() {
            let first = staying.entry(run.driver).or_insert(run.first);
            *first = run.first.min(*first);
        }
        let known = &mut self.known;
        self.ids.retain(|(car, _), id| {
            let driver = &mut known[(*id - 1) as usize];
            match (*car == vehicle, staying.get(id)) {
                (false, _) => true,
                (true, Some(first)) => {
                    driver.first_seen = *first;
                    true
                }
                // Named again, they're someone new
                (true, None) => {
                    driver.gone = true;
                    false
                }
            }
        });
    }

    fn runs(&self, vehicle: i64) -> &[Run] {
        self.runs.get(&vehicle).map_or(&[], Vec::as_slice)
    }
}

/// A stint's id goes with its car and number: the same as long as no stint is found to have begun
/// before it.
#[derive(Default)]
struct StintIds(HashMap<(i64, i64), i64>);

impl StintIds {
    fn get(&mut self, vehicle: i64, number: i64) -> i64 {
        let next = self.0.len() as i64 + 1;
        *self.0.entry((vehicle, number)).or_insert(next)
    }
}

/// How a stint began.
#[derive(Clone, Debug, PartialEq)]
pub(super) struct StintStart {
    pub reason: &'static str,
    /// A stop's out lap: laps before it are the stint before's
    pub lap: Option<i64>,
    /// When it began, as observed: at the line between a stop's in and out laps, or between the
    /// last sighting of the driver before and the first of the next (the standings don't say just
    /// when they swapped)
    pub from: Option<f64>,
    pub to: Option<f64>,
    /// Someone else took over: which side of it a lap is on goes by when it was driven
    pub by_time: bool,
    /// The standings row first naming the new driver
    pub standing_id: Option<i64>,
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Side {
    Before,
    After,
    Across,
    Unknown,
}

impl StintStart {
    /// Where a stretch of lap `lap`, driven from `from` to `to`, is from this start.
    fn side(&self, lap: i64, from: Option<f64>, to: Option<f64>) -> Side {
        if self.by_time {
            return match (self.from, self.to, from, to) {
                (Some(start), _, _, Some(to)) if to <= start => Side::Before,
                (_, Some(end), Some(from), _) if from >= end => Side::After,
                (Some(_), Some(_), Some(_), Some(_)) => Side::Across,
                _ => Side::Unknown,
            };
        }
        match self.lap {
            Some(first) if lap < first => Side::Before,
            Some(_) => Side::After,
            None => Side::Unknown,
        }
    }
}

/// What a stint was on course for when last worked out while current, and which stint that was:
/// its number can change, when another is found to have begun before it.
struct Saved {
    start: StintStart,
    columns: Columns,
}

impl Saved {
    /// Whether it was worked out for this stint, whatever its number is now: when it began says
    /// which it is, though who drove it may only be known later.
    fn is(&self, stint: &Segment) -> bool {
        let (a, b) = (&self.start, &stint.start);
        (a.lap, a.from, a.to) == (b.lap, b.from, b.to)
    }
}

/// What a refresh of the tables works out once for all of them.
#[derive(Default)]
struct Fresh {
    stints: CarStints,
    /// Each car's laps, as read for it
    laps: HashMap<i64, Vec<Lap>>,
    /// What each car's current stint is on course for, and whether that's worth keeping once it's
    /// over: not from the pits, where it's being refuelled
    projections: HashMap<i64, (Columns, bool)>,
}

/// One of a car's stints.
#[derive(Clone, Debug, PartialEq)]
pub(super) struct Segment {
    pub id: i64,
    pub number: i64,
    pub driver: Option<i64>,
    pub start: StintStart,
}

/// Where a lap, or a stretch of one, belongs.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) enum Place {
    /// Wholly in this stint, by index
    Stint(usize),
    /// Across the start of one
    Across,
    /// There's no telling: when it was driven isn't known
    Unknown,
}

/// Where each of the laps belongs.
fn places(laps: &[Lap], stints: &[Segment]) -> Vec<Place> {
    laps.iter()
        .map(|lap| place(stints, lap.lap, lap.start, lap.end))
        .collect()
}

pub(super) fn place(stints: &[Segment], lap: i64, from: Option<f64>, to: Option<f64>) -> Place {
    if stints.is_empty() {
        return Place::Unknown;
    }
    let (mut index, mut unknown) = (0, false);
    for (i, stint) in stints.iter().enumerate().skip(1) {
        match stint.start.side(lap, from, to) {
            Side::After => index = i,
            Side::Before => {}
            Side::Across => return Place::Across,
            Side::Unknown => unknown = true,
        }
    }
    match unknown {
        true => Place::Unknown,
        false => Place::Stint(index),
    }
}

/// `stint_id` and `stint_boundary`: NULL and 1 across the start of a stint, NULL and 0 when there's
/// no telling.
fn stint_columns(stints: &[Segment], lap: i64, from: Option<f64>, to: Option<f64>) -> [Value; 2] {
    match place(stints, lap, from, to) {
        Place::Stint(i) => [Value::Integer(stints[i].id), flag(false)],
        Place::Across => [Value::Null, flag(true)],
        Place::Unknown => [Value::Null, flag(false)],
    }
}

/// When a corner pass was driven: from the start of its section to the end, or to the corner's
/// exit until the car gets there.
fn pass_span(
    entered: Option<f64>,
    exited: Option<f64>,
    total: Option<f64>,
) -> (Option<f64>, Option<f64>) {
    (entered, entered.zip(total).map(|(e, t)| e + t).or(exited))
}

/// A car's stints, with who drove each, from its laps and the drivers the standings named. A stop
/// starts one at the line between its in and out laps (leaving the garage isn't one), and so does
/// someone else taking over: during a stop, that's the same new stint.
pub(super) fn segment(spans: &[LapSpan], runs: &[Run]) -> Vec<(Option<i64>, StintStart)> {
    struct Stop {
        start: StintStart,
        /// From the start of its in lap to the end of its last lap in the pits
        window: Option<(f64, f64)>,
        driver: Option<i64>,
    }
    if spans.is_empty() && runs.is_empty() {
        return Vec::new();
    }
    let mut stops = Vec::new();
    // Leaving the garage isn't a stop
    let mut stopped = true;
    for (i, span) in spans.iter().enumerate() {
        // Both the in lap and the out lap show the pit lane when it crosses the line
        if span.stopped
            && !stopped
            && let Some(next) = spans.get(i + 1)
        {
            let at = next.start.or(span.end);
            let last = spans[i..]
                .iter()
                .take_while(|span| span.stopped)
                .last()
                .unwrap_or(span);
            stops.push(Stop {
                start: StintStart {
                    reason: "pit stop",
                    lap: Some(next.lap),
                    from: at,
                    to: at,
                    by_time: false,
                    standing_id: None,
                },
                window: span.start.or(span.end).zip(last.end),
                driver: None,
            });
        }
        stopped = span.stopped;
    }

    let mut starts = Vec::new();
    for pair in runs.windows(2) {
        let (before, after) = (pair[0], pair[1]);
        let (from, to) = (before.last, after.first);
        let stop = stops.iter_mut().find(|stop| {
            stop.window
                .is_some_and(|(start, end)| from <= end && to >= start)
        });
        match stop {
            Some(stop) => {
                let start = &mut stop.start;
                start.reason = "pit stop and driver change";
                start.from = Some(start.from.map_or(from, |f| f.min(from)));
                start.to = Some(start.to.map_or(to, |t| t.max(to)));
                start.by_time = true;
                start.standing_id = after.standing_id;
                stop.driver = Some(after.driver);
            }
            None => starts.push((
                Some(after.driver),
                StintStart {
                    reason: "driver change",
                    lap: None,
                    from: Some(from),
                    to: Some(to),
                    by_time: true,
                    standing_id: after.standing_id,
                },
            )),
        }
    }
    starts.extend(stops.into_iter().map(|stop| (stop.driver, stop.start)));
    starts.sort_by(|(_, a), (_, b)| {
        let at = |start: &StintStart| start.from.unwrap_or(f64::INFINITY);
        at(a).total_cmp(&at(b))
    });

    let first_lap = spans.first().and_then(|span| span.start.or(span.end));
    let first_seen = runs.first().map(|run| run.first);
    let from = match (first_lap, first_seen) {
        (Some(lap), Some(seen)) => Some(lap.min(seen)),
        (lap, seen) => lap.or(seen),
    };
    let mut driver = runs.first().map(|run| run.driver);
    let mut stints = vec![(
        driver,
        StintStart {
            reason: "first seen",
            lap: spans.first().map(|span| span.lap),
            from,
            to: from,
            by_time: false,
            standing_id: runs.first().and_then(|run| run.standing_id),
        },
    )];
    for (new_driver, start) in starts {
        driver = new_driver.or(driver);
        stints.push((driver, start));
    }
    stints
}

fn numbered(
    vehicle: i64,
    stints: Vec<(Option<i64>, StintStart)>,
    ids: &mut StintIds,
) -> Vec<Segment> {
    stints
        .into_iter()
        .zip(1..)
        .map(|((driver, start), number)| Segment {
            id: ids.get(vehicle, number),
            number,
            driver,
            start,
        })
        .collect()
}

/// Works out the car's stints again, and if they've changed, which of them its laps and corner
/// passes are in.
fn resegment(
    conn: &Connection,
    vehicle: i64,
    car: &mut Car,
    runs: &[Run],
    ids: &mut StintIds,
) -> rusqlite::Result<()> {
    let spans = car.spans();
    let stints = numbered(vehicle, segment(&spans, runs), ids);
    if stints == car.stints {
        return Ok(());
    }
    // Until its rows are all right: a failure leaves every one to be put right next time
    car.stints.clear();
    for span in &spans {
        let [id, across] = stint_columns(&stints, span.lap, span.start, span.end);
        conn.prepare_cached(
            "UPDATE lap_stats SET stint_id = ?, stint_boundary = ? WHERE vehicle_id = ? AND lap = ?",
        )?
        .execute(params![id, across, vehicle, span.lap])?;
        if let Place::Stint(_) = place(&stints, span.lap, span.start, span.end) {
            conn.prepare_cached(
                "UPDATE corner_passes SET stint_id = ?, stint_boundary = ? \
                 WHERE vehicle_id = ? AND lap = ?",
            )?
            .execute(params![id, across, vehicle, span.lap])?;
            continue;
        }
        // Each pass by when it was driven
        for r in rows(
            conn,
            "SELECT turn_index, entered_at_s, exited_at_s, total_s FROM corner_passes \
             WHERE vehicle_id = ? AND lap = ?",
            params![vehicle, span.lap],
        )? {
            let (from, to) = pass_span(r.f("entered_at_s"), r.f("exited_at_s"), r.f("total_s"));
            let [id, across] = stint_columns(&stints, span.lap, from, to);
            conn.prepare_cached(
                "UPDATE corner_passes SET stint_id = ?, stint_boundary = ? \
                 WHERE vehicle_id = ? AND lap = ? AND turn_index = ?",
            )?
            .execute(params![id, across, vehicle, span.lap, r.i("turn_index")])?;
        }
    }
    // Only once its rows are right: a failure leaves them to be put right next time
    car.stints = stints;
    Ok(())
}

/// An official lap time, from the standings.
#[derive(Clone)]
struct Official {
    at: f64,
    time: f64,
    /// Each sector's time from the line on, as many as the session has
    sectors: Vec<Option<f64>>,
}

/// Replaces a lap's sector times with the official ones known.
fn write_lap_sectors(
    conn: &Connection,
    vehicle: i64,
    lap: i64,
    sectors: &[Option<f64>],
) -> rusqlite::Result<()> {
    conn.prepare_cached("DELETE FROM lap_sector_stats WHERE vehicle_id = ? AND lap = ?")?
        .execute(params![vehicle, lap])?;
    for (sector, time) in sectors.iter().enumerate() {
        if time.is_none() {
            continue;
        }
        insert(
            conn,
            "lap_sector_stats",
            vec![
                ("vehicle_id", Value::Integer(vehicle)),
                ("lap", Value::Integer(lap)),
                ("sector", Value::Integer(sector as i64 + 1)),
                ("time_s", real(*time)),
            ],
        )?;
    }
    Ok(())
}

/// A turn's place on the lap, and the section of the lap it's measured over: every lap is split
/// at fixed boundaries, so passes compare across laps and cars, and add up to the lap time.
#[derive(Debug, Clone, PartialEq)]
pub(super) struct Corner {
    pub name: String,
    pub direction: Option<&'static str>,
    pub scale: Option<i64>,
    pub boundary: f64,
    pub start: f64,
    pub apex: f64,
    pub end: f64,
    pub next_boundary: f64,
    /// Where the corner ends for the pass: its end, if it's within the section
    pub exit: f64,
}

#[derive(Debug, Clone, PartialEq)]
pub(super) struct Layout {
    pub length: f64,
    pub source: &'static str,
    pub corners: Vec<Corner>,
}

/// The fixed sections of a lap. The first starts at the line and the last ends at it, so a pass
/// only ever needs its own lap's rows. A boundary between two turns is halfway from the end of one
/// to the start of the next, so braking falls in the next turn's section, and between the apexes
/// when turns overlap.
pub(super) fn layout(mut turns: Vec<Turn>, length: f64, source: &'static str) -> Layout {
    turns.sort_by(|a, b| a.apex.partial_cmp(&b.apex).unwrap_or(Ordering::Equal));
    let boundaries = (0..turns.len())
        .map(|k| match k {
            0 => 0.0,
            k => {
                let (before, turn) = (&turns[k - 1], &turns[k]);
                let from = match before.apex < before.end && before.end <= turn.apex {
                    true => before.end,
                    false => before.apex,
                };
                let to = match from <= turn.start && turn.start < turn.apex {
                    true => turn.start,
                    false => turn.apex,
                };
                (from + to) / 2.0
            }
        })
        .collect::<Vec<_>>();
    let corners = turns
        .iter()
        .enumerate()
        .map(|(k, turn)| {
            let next_boundary = boundaries.get(k + 1).copied().unwrap_or(length);
            Corner {
                name: turn.label.clone(),
                direction: turn.direction,
                scale: turn.scale,
                boundary: boundaries[k],
                start: turn.start,
                apex: turn.apex,
                end: turn.end,
                next_boundary,
                exit: match turn.apex < turn.end && turn.end <= next_boundary {
                    true => turn.end,
                    false => next_boundary,
                },
            }
        })
        .collect();
    Layout {
        length,
        source,
        corners,
    }
}

/// The distances at which more of a corner's pass can be written: its exit (the row appears), the
/// end of its section, and the end of the search for the throttle.
fn marks(corner: &Corner, length: f64) -> [f64; 3] {
    [
        corner.exit,
        corner.next_boundary,
        (corner.exit + THROTTLE_SEARCH).min(length),
    ]
}

fn next_mark(layout: &Layout, top: f64) -> f64 {
    layout
        .corners
        .iter()
        .flat_map(|corner| marks(corner, layout.length))
        .filter(|mark| *mark > top)
        .fold(f64::INFINITY, f64::min)
}

/// What's needed to write a lap.
struct Writer<'a> {
    conn: &'a Connection,
    layout: Option<&'a Layout>,
    official: &'a HashMap<i64, Vec<Official>>,
    player: Option<i64>,
}

impl Writer<'_> {
    fn sample_added(
        &self,
        vehicle: i64,
        build: &mut LapBuild,
        stints: &[Segment],
    ) -> rusqlite::Result<()> {
        let Some(layout) = self.layout else {
            return Ok(());
        };
        if build.top < build.next_mark {
            return Ok(());
        }
        let trace = Trace::build(&build.samples, build.start, None, layout.length);
        self.write_passes(vehicle, build, layout, &trace, true, false, stints)?;
        build.next_mark = next_mark(layout, build.top);
        Ok(())
    }

    /// Writes the passes `trace` adds something to.
    #[allow(clippy::too_many_arguments)]
    fn write_passes(
        &self,
        vehicle: i64,
        build: &mut LapBuild,
        layout: &Layout,
        trace: &Trace,
        reliable: bool,
        all: bool,
        stints: &[Segment],
    ) -> rusqlite::Result<()> {
        let top = trace.d.last().copied().unwrap_or(0.0);
        for (k, corner) in layout.corners.iter().enumerate() {
            let passed = marks(corner, layout.length)
                .into_iter()
                .filter(|mark| *mark <= top)
                .count() as u8;
            if top < corner.exit || !(all || passed > build.marks_passed[k]) {
                continue;
            }
            build.marks_passed[k] = passed;
            self.write_pass(vehicle, build, layout, k, trace, reliable, stints)?;
        }
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn write_pass(
        &self,
        vehicle: i64,
        build: &LapBuild,
        layout: &Layout,
        k: usize,
        trace: &Trace,
        reliable: bool,
        stints: &[Segment],
    ) -> rusqlite::Result<()> {
        let corner = &layout.corners[k];
        let top = trace.d.last().copied().unwrap_or(0.0);
        // The car joined after it: there's nothing to say about a corner it wasn't seen in
        if trace.seen(corner.boundary, corner.exit).is_empty() {
            return Ok(());
        }
        let metrics = trace.corner(corner.apex, corner.start, corner.exit);
        let entry = trace.span(corner.boundary, corner.apex);
        let apex_to_end = trace.span(corner.apex, corner.exit);
        let straight =
            (top >= corner.next_boundary).then(|| trace.span(corner.exit, corner.next_boundary));
        let (from, to) = (
            trace.index(corner.boundary),
            trace.index(corner.next_boundary.min(top)),
        );
        let (first_id, last_id) = (trace.id[from], trace.id[to]);
        let sample_count = trace
            .seen(corner.boundary, corner.next_boundary.min(top))
            .len();
        // Back to the section's start: a car joining midway through it never saw its entry
        let widest = trace.d[from..=to]
            .windows(2)
            .map(|pair| pair[1] - pair[0])
            .fold(trace.d[from] - corner.boundary, f64::max);
        let (entered, exited) = (trace.time_at(corner.boundary), trace.time_at(corner.exit));
        let contact = scalar::<i64>(
            self.conn,
            "SELECT 1 FROM car_state WHERE vehicle_id = ? AND telemetry_id BETWEEN ? AND ? \
             AND last_impact_time BETWEEN ? AND ? LIMIT 1",
            params![
                vehicle,
                first_id,
                last_id,
                entered,
                trace.time_at(corner.next_boundary.min(top))
            ],
        )?;
        let is_player = self.player == Some(vehicle);
        let total = straight.map(|s| entry + apex_to_end + s);
        let (driven_from, driven_to) = pass_span(Some(entered), Some(exited), total);
        let [stint_id, boundary] = stint_columns(stints, build.lap, driven_from, driven_to);
        insert(
            self.conn,
            "corner_passes",
            vec![
                ("vehicle_id", Value::Integer(vehicle)),
                ("lap", Value::Integer(build.lap)),
                ("turn_index", Value::Integer(k as i64)),
                ("stint_id", stint_id),
                ("stint_boundary", boundary),
                ("turn_name", Value::Text(corner.name.clone())),
                ("direction", text_or_null(corner.direction)),
                ("scale", integer(corner.scale)),
                ("first_id", Value::Integer(first_id)),
                ("last_id", Value::Integer(last_id)),
                ("entered_at_s", real(Some(entered))),
                ("exited_at_s", real(Some(exited))),
                ("boundary_m", real(Some(corner.boundary))),
                ("start_m", real(Some(corner.start))),
                ("apex_m", real(Some(corner.apex))),
                ("end_m", real(Some(corner.end))),
                ("next_boundary_m", real(Some(corner.next_boundary))),
                ("entry_s", real(Some(entry))),
                ("apex_to_end_s", real(Some(apex_to_end))),
                ("straight_s", real(straight)),
                ("total_s", real(total)),
                (
                    "brake_m_before_apex",
                    real(metrics.brake.map(|b| corner.apex - b)),
                ),
                ("min_speed_kmh", real(km_h(metrics.vmin))),
                ("min_speed_at_m", real(metrics.at_min)),
                ("exit_speed_kmh", real(km_h(metrics.vexit))),
                (
                    "throttle_on_m_after_apex",
                    real(metrics.throttle.map(|x| x - corner.apex)),
                ),
                ("gear_at_min", integer(metrics.gear)),
                (
                    "braking_source",
                    Value::Text(
                        match trace.has_pedals {
                            true => "pedal",
                            false => "deceleration",
                        }
                        .into(),
                    ),
                ),
                ("sample_count", Value::Integer(sample_count as i64)),
                (
                    "low_precision",
                    flag(
                        !reliable
                            || !is_player
                            || sample_count < FEW_SAMPLES
                            || widest > layout.length * WIDEST_GAP,
                    ),
                ),
                ("lap_invalid", flag(build.invalid)),
                ("in_pit", flag(trace.pit[from..=to].iter().any(|pit| *pit))),
                ("contact", flag(contact.is_some())),
            ],
        )?;
        if is_player {
            self.write_rises(vehicle, build, k)?;
        }
        Ok(())
    }

    fn write_rises(&self, vehicle: i64, build: &LapBuild, k: usize) -> rusqlite::Result<()> {
        for (wheel, rises) in build.rises[k].iter().enumerate() {
            let total = |kind: usize| rises.by_phase[kind].iter().sum::<f64>() + rises.other[kind];
            let phase = |kind: usize, phase: usize| real(Some(rises.by_phase[kind][phase]));
            insert(
                self.conn,
                "corner_pass_wheels",
                vec![
                    ("vehicle_id", Value::Integer(vehicle)),
                    ("lap", Value::Integer(build.lap)),
                    ("turn_index", Value::Integer(k as i64)),
                    ("wheel", Value::Integer(wheel as i64)),
                    ("surface_rise_c", real(Some(total(0)))),
                    ("surface_rise_braking_c", phase(0, 0)),
                    ("surface_rise_off_throttle_c", phase(0, 1)),
                    ("surface_rise_full_throttle_c", phase(0, 2)),
                    ("surface_end_c", real(rises.last[0])),
                    ("brake_rise_c", real(Some(total(1)))),
                    ("brake_rise_braking_c", phase(1, 0)),
                    ("brake_rise_off_throttle_c", phase(1, 1)),
                    ("brake_rise_full_throttle_c", phase(1, 2)),
                    ("brake_end_c", real(rises.last[1])),
                ],
            )?;
        }
        Ok(())
    }

    /// `lap` is over: its rows are written for good.
    fn finish(
        &self,
        vehicle: i64,
        car: &mut Car,
        mut build: LapBuild,
        next: &LapBuild,
    ) -> rusqlite::Result<()> {
        let lap = build.lap(self.conn, vehicle, Some(next), car.last_impact)?;
        if lap.impact.is_some_and(|impact| impact != 0.0) {
            car.last_impact = lap.impact;
        }
        // Only finished laps settle what the wear means
        car.finished.push(lap.clone());
        if !car.wear_counts_up && !wear_remaining(&car.finished) {
            car.wear_counts_up = true;
            self.conn.execute(
                "UPDATE lap_wheel_stats SET life_start_pct = 100 - life_start_pct, \
                 life_end_pct = 100 - life_end_pct WHERE vehicle_id = ?",
                [vehicle],
            )?;
        }
        self.write_lap(vehicle, car, &lap, true)?;

        if let Some(layout) = self.layout {
            let trace = Trace::build(&build.samples, build.start, lap.time, layout.length);
            // Not always a lap to rely on (the car joined midway, say): what it did get to
            let reliable = trace.ok;
            self.write_passes(
                vehicle,
                &mut build,
                layout,
                &trace,
                reliable,
                true,
                &car.stints,
            )?;
        }
        Ok(())
    }

    /// `heat`: whether to add what the tyres did over the lap, which takes a scan of its rows.
    fn write_lap(&self, vehicle: i64, car: &Car, lap: &Lap, heat: bool) -> rusqlite::Result<()> {
        let end = lap.start.zip(lap.time).map(|(start, time)| start + time);
        let official = end.and_then(|end| {
            self.official
                .get(&vehicle)?
                .iter()
                .filter(|official| (official.at - end).abs() < OFFICIAL_MATCH)
                .min_by(|a, b| {
                    (a.at - end)
                        .abs()
                        .partial_cmp(&(b.at - end).abs())
                        .unwrap_or(Ordering::Equal)
                })
        });
        let [stint_id, boundary] = stint_columns(&car.stints, lap.lap, lap.start, lap.end);
        insert(
            self.conn,
            "lap_stats",
            vec![
                ("vehicle_id", Value::Integer(vehicle)),
                ("lap", Value::Integer(lap.lap)),
                ("stint_id", stint_id),
                ("stint_boundary", boundary),
                ("first_id", Value::Integer(lap.first_id)),
                ("last_id", Value::Integer(lap.last_id)),
                ("start_s", real(lap.start)),
                ("time_s", real(lap.time)),
                ("official_time_s", real(official.map(|o| o.time))),
                ("top_speed_kmh", real(km_h(lap.top_speed))),
                ("complete", flag(lap.complete)),
                ("invalid", flag(lap.invalid)),
                ("pit", flag(lap.pit)),
                ("refuelled", flag(lap.refuelled)),
                ("contact", flag(lap.contact)),
                ("green", flag(lap.green)),
                ("track_limit_steps", integer(lap.track_limit_steps)),
                ("fuel_start_l", real(lap.fuel_start)),
                ("fuel_end_l", real(lap.fuel_end)),
                ("fuel_used_l", real(lap.fuel_used)),
                ("energy_start_pct", real(energy_percent(lap.energy_start))),
                ("energy_end_pct", real(energy_percent(lap.energy_end))),
                (
                    "energy_used_pct",
                    real(
                        lap.energy_used
                            .filter(|_| lap.energy_start.is_some_and(|e| e <= 1.0))
                            .map(|used| used * 100.0),
                    ),
                ),
                ("gap_ahead_end_s", real(lap.gap_ahead_end)),
                ("gap_behind_end_s", real(lap.gap_behind_end)),
            ],
        )?;
        let sectors = official.map_or(&[][..], |o| &o.sectors[..]);
        write_lap_sectors(self.conn, vehicle, lap.lap, sectors)?;

        if !heat {
            return Ok(());
        }
        // Not worth losing the laps over: a recording without some of these columns, say
        let heat = match lap_heat(self.conn, vehicle, lap.first_id, lap.last_id) {
            Ok(heat) => heat,
            Err(e) => {
                log::debug!("No tyre figures for car {vehicle}'s lap {}: {e}", lap.lap);
                return Ok(());
            }
        };
        let life = |wear: Option<f64>| {
            wear.map(|wear| match car.wear_counts_up {
                true => (1.0 - wear) * 100.0,
                false => wear * 100.0,
            })
        };
        for (wheel, h) in heat.iter().enumerate() {
            if h.is_empty() && lap.wear_end[wheel].is_none() {
                continue;
            }
            insert(
                self.conn,
                "lap_wheel_stats",
                vec![
                    ("vehicle_id", Value::Integer(vehicle)),
                    ("lap", Value::Integer(lap.lap)),
                    ("wheel", Value::Integer(wheel as i64)),
                    ("wheel_name", Value::Text(WHEELS[wheel].into())),
                    ("life_start_pct", real(life(lap.wear_start[wheel]))),
                    ("life_end_pct", real(life(lap.wear_end[wheel]))),
                    ("surface_avg_c", real(h.surface)),
                    ("surface_peak_c", real(h.peak)),
                    ("inner_avg_c", real(h.strips[0])),
                    ("middle_avg_c", real(h.strips[1])),
                    ("outer_avg_c", real(h.strips[2])),
                    ("optimal_c", real(celsius(h.optimal))),
                    ("pressure_avg_kpa", real(h.pressure)),
                    ("brake_avg_c", real(h.brake)),
                    ("brake_peak_c", real(h.brake_peak)),
                ],
            )?;
        }
        Ok(())
    }
}

/// Follows the recording, from its current session on.
#[derive(Default)]
pub(super) struct Stream {
    started: bool,
    /// The telemetry rows are behind the recording's, or the standings' are
    pub behind: bool,
    /// The last tick failed: the tables may be behind, whatever `behind` says
    pub failed: bool,
    pub session_start: i64,
    /// The last telemetry row read
    pub cursor: i64,
    /// (session_time, vehicle_id) of the row at `cursor`, to notice a recreated file
    last_row: Option<(Option<f64>, Option<i64>)>,
    player_t: Option<f64>,
    pub player: Option<i64>,
    cars: BTreeMap<i64, Car>,
    layout: Option<Layout>,
    checked_layout: Option<Instant>,
    /// How many laps the player had when the description was last lined up with them
    calibrated_laps: Option<(usize, usize)>,
    /// How many laps the player had when braking zones were last looked for (car and laps)
    zones_from: Option<(i64, usize)>,
    /// A fallback layout must be replaced when a usable clean lap becomes available.
    zones_from_invalid: bool,
    standings_cursor: i64,
    standings_t: Option<f64>,
    /// The standings' laps completed, by car, and when that last changed
    laps_completed: HashMap<i64, (i64, f64)>,
    official: HashMap<i64, Vec<Official>>,
    drivers: Drivers,
    stint_ids: StintIds,
    /// What each car's stints were on course for when last worked out, kept once they're over
    projections: HashMap<i64, Vec<Saved>>,
    /// The simulator's timing sectors' ends as last seen whole, and the session start, track and
    /// sector count they're for: a window of the laps can miss lines the session has crossed
    timing: Option<((i64, String, i64), Vec<f64>)>,
}

impl Stream {
    pub fn tick(&mut self, conn: &Connection, shared: &Shared) -> anyhow::Result<()> {
        let result = self.advance(conn, shared);
        self.failed = result.is_err();
        result
    }

    fn advance(&mut self, conn: &Connection, shared: &Shared) -> anyhow::Result<()> {
        self.behind = false;
        if !self.started {
            self.session_start = session_start(conn)?;
            self.cursor = self.session_start - 1;
            self.started = true;
        }
        self.notice_recreated_file(conn)?;
        if self
            .checked_layout
            .is_none_or(|checked| checked.elapsed() >= LAYOUT_CHECK)
        {
            self.check_layout(conn, shared)?;
        }

        // Read first: a failed read (the logger mid-checkpoint, say) changes nothing
        let samples = conn
            .prepare_cached(&format!(
                "SELECT {SAMPLE_COLUMNS} FROM telemetry WHERE id > ? ORDER BY id LIMIT ?"
            ))?
            .query_map(params![self.cursor, CHUNK], sample)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let tx = conn.unchecked_transaction()?;
        let standings_behind = match self.read_standings(conn) {
            Ok(behind) => behind,
            Err(e) => {
                // Stints worked out with rows put right that are now gone: they're all put right
                // again next time
                drop(tx);
                self.forget_stints();
                return Err(e.into());
            }
        };
        let telemetry_behind = match self.apply_telemetry(conn, &samples) {
            Ok(behind) => behind,
            Err(e) => {
                // What was applied of those rows is gone with the transaction: read them again
                drop(tx);
                self.rewind(conn)?;
                return Err(e.into());
            }
        };
        self.behind |= standings_behind || telemetry_behind;
        if let Err(e) = tx.commit() {
            // What was applied of those rows is gone: read them again
            self.rewind(conn)?;
            return Err(e.into());
        }

        if !self.behind {
            self.calibrate(conn, shared)?;
        }
        self.find_braking_zones(conn)?;
        Ok(())
    }

    /// The rows' stints may not be what the cars' say, the writes having been undone: they're all
    /// put right next time.
    fn forget_stints(&mut self) {
        for car in self.cars.values_mut() {
            car.stints.clear();
        }
    }

    fn rewind_memory(&mut self) {
        self.cars.clear();
        self.cursor = self.session_start - 1;
        self.last_row = None;
        self.player_t = None;
        self.behind = true;
    }

    fn clear_stream_tables(&self, conn: &Connection) -> rusqlite::Result<()> {
        for table in STREAM_TABLES {
            conn.execute(&format!("DELETE FROM {table}"), [])?;
        }
        Ok(())
    }

    /// Everything of the session is read again (the layout's changed, or it's another session).
    fn rewind(&mut self, conn: &Connection) -> rusqlite::Result<()> {
        self.rewind_memory();
        self.clear_stream_tables(conn)
    }

    fn start_session(&mut self, conn: &Connection, start: i64) -> rusqlite::Result<()> {
        self.session_start = start;
        self.stint_ids = StintIds::default();
        // Another car taking a slot was on the old session's clock
        self.drivers.replaced.clear();
        self.projections.clear();
        self.calibrated_laps = None;
        self.zones_from = None;
        self.zones_from_invalid = false;
        if self
            .layout
            .as_ref()
            .is_some_and(|layout| layout.source != TRACK_DATA)
        {
            self.layout = None;
        }
        self.rewind(conn)
    }

    /// The row last read must still be the same row, or the file's a new one.
    fn notice_recreated_file(&mut self, conn: &Connection) -> rusqlite::Result<()> {
        let top = |table: &str| {
            scalar::<i64>(conn, &format!("SELECT MAX(id) FROM {table}"), [])
                .map(|top| top.unwrap_or(0))
        };
        let same = match self.last_row {
            Some(last_row) => {
                let r = row(
                    conn,
                    "SELECT session_time, vehicle_id FROM telemetry WHERE id = ?",
                    [self.cursor],
                )?;
                r.map(|r| (r.f("session_time"), Some(r.i("vehicle_id").unwrap_or(-1))))
                    == Some(last_row)
            }
            // Nothing read since a rewind: there must still be rows that far on
            None => top("telemetry")? >= self.cursor,
        };
        if same && top("standings")? >= self.standings_cursor {
            return Ok(());
        }
        self.standings_cursor = 0;
        self.standings_t = None;
        self.laps_completed.clear();
        self.official.clear();
        self.drivers = Drivers::default();
        let start = session_start(conn)?;
        self.start_session(conn, start)
    }

    /// Reads the standings' new rows for official lap times and who's driving. Whether there are
    /// more to read.
    fn read_standings(&mut self, conn: &Connection) -> rusqlite::Result<bool> {
        let standings = rows(
            conn,
            "SELECT id, vehicle_id, session_time, laps_completed, last_lap_time, driver \
             FROM standings WHERE id > ? ORDER BY id LIMIT ?",
            params![self.standings_cursor, CHUNK],
        )?;
        let behind = standings.len() as i64 == CHUNK;
        // The cars someone else has taken over, or all of them
        let mut taken_over = BTreeSet::new();
        let mut everyone = false;
        for r in standings {
            self.standings_cursor = r.i("id").unwrap_or(self.standings_cursor);
            let t = r.f("session_time");
            // The clock going back: another session's
            if let (Some(t), Some(before)) = (t, self.standings_t)
                && t < before - 1.0
            {
                self.laps_completed.clear();
                self.official.clear();
                self.drivers = Drivers::default();
                everyone = true;
            }
            if t.is_some() {
                self.standings_t = t;
            }
            if let (Some(vehicle), Some(name), Some(t)) = (
                r.i("vehicle_id"),
                r.text("driver").filter(|name| !name.is_empty()),
                t,
            ) && self.drivers.seen(vehicle, name, t, self.standings_cursor)
            {
                taken_over.insert(vehicle);
            }

            let (Some(vehicle), Some(laps), Some(at)) =
                (r.i("vehicle_id"), r.i("laps_completed"), t)
            else {
                continue;
            };
            let before = self.laps_completed.get(&vehicle).copied();
            let new_lap = before.is_some_and(|(before, _)| before != laps);
            let ended = match new_lap {
                true => at,
                false => before.map_or(at, |(_, ended)| ended),
            };
            self.laps_completed.insert(vehicle, (laps, ended));
            // The first we hear of a car says nothing about a lap ending
            let Some(_) = before else {
                continue;
            };
            let Some(time) = r.f("last_lap_time").filter(|time| *time > 0.0) else {
                continue;
            };
            let entries = self.official.entry(vehicle).or_default();
            // The lap count and its time can come a row apart: the time goes with the count
            let same_lap = entries.last().is_some_and(|last| last.at == ended);
            let late = !new_lap
                && (at - ended) < OFFICIAL_MATCH
                && entries
                    .last()
                    .is_none_or(|last| last.at < ended || last.time != time);
            if !new_lap && !late {
                continue;
            }
            let mut sectors = Vec::new();
            for s in rows(
                conn,
                "SELECT sector, last_lap FROM standing_sector WHERE standing_id = ? \
                 ORDER BY sector",
                [self.standings_cursor],
            )? {
                let Some(sector) = s.i("sector").and_then(|s| usize::try_from(s).ok()) else {
                    continue;
                };
                if sectors.len() <= sector {
                    sectors.resize(sector + 1, None);
                }
                sectors[sector] = s.f("last_lap").filter(|time| *time > 0.0);
            }
            let official = Official {
                at: ended,
                time,
                sectors,
            };
            if same_lap {
                entries.pop();
            }
            // The telemetry may have seen the lap end first
            conn.prepare_cached(
                "UPDATE lap_stats SET official_time_s = ? WHERE vehicle_id = ? AND complete = 1 \
                 AND ABS(start_s + time_s - ?) < ?",
            )?
            .execute(params![time, vehicle, ended, OFFICIAL_MATCH])?;
            let laps = rows(
                conn,
                "SELECT lap FROM lap_stats WHERE vehicle_id = ? AND complete = 1 \
                 AND ABS(start_s + time_s - ?) < ?",
                params![vehicle, ended, OFFICIAL_MATCH],
            )?;
            for lap in laps.iter().filter_map(|r| r.i("lap")) {
                write_lap_sectors(conn, vehicle, lap, &official.sectors)?;
            }
            entries.push(official);
        }
        let Self {
            cars,
            drivers,
            stint_ids,
            ..
        } = self;
        for (vehicle, car) in cars
            .iter_mut()
            .filter(|(vehicle, _)| everyone || taken_over.contains(*vehicle))
        {
            resegment(conn, *vehicle, car, drivers.runs(*vehicle), stint_ids)?;
        }
        Ok(behind)
    }

    /// Takes in the telemetry's new rows. Whether there are more to read.
    fn apply_telemetry(&mut self, conn: &Connection, samples: &[Sample]) -> rusqlite::Result<bool> {
        let (Some(first), Some(last)) = (samples.first(), samples.last()) else {
            return Ok(false);
        };
        let (first_id, last_id) = (first.id, last.id);

        // Session boundaries belong to the real player
        let mut player_t = self.player_t;
        for sample in samples.iter().filter(|sample| sample.is_player) {
            if let (Some(t), Some(before)) = (sample.session_time, player_t)
                && t < before - 1.0
            {
                let start = frame_start(conn, sample.id, t)?;
                self.start_session(conn, start)?;
                return Ok(true);
            }
            if sample.session_time.is_some() {
                player_t = sample.session_time;
            }
            self.player = Some(sample.vehicle);
        }
        self.player_t = player_t;

        let temperatures = match (self.player, &self.layout) {
            (Some(player), Some(_)) => wheel_temperatures(conn, player, first_id, last_id)?,
            _ => HashMap::new(),
        };
        let Self {
            cars,
            layout,
            official,
            player,
            drivers,
            stint_ids,
            projections,
            ..
        } = self;
        let writer = Writer {
            conn,
            layout: layout.as_ref(),
            official,
            player: *player,
        };
        let length = writer.layout.map(|layout| layout.length);
        for sample in samples {
            let Some(lap) = sample.lap else {
                continue;
            };
            let car = cars.entry(sample.vehicle).or_default();
            // A lap's started or ended: its stints may have too
            let mut laps_changed = false;
            if car
                .current
                .as_ref()
                .is_some_and(|current| lap < current.lap)
            {
                if car.is_late_row(sample, lap) {
                    continue;
                }
                // Another car in the slot: nothing of the last one's is this one's
                for table in STREAM_TABLES {
                    conn.execute(
                        &format!("DELETE FROM {table} WHERE vehicle_id = ?"),
                        [sample.vehicle],
                    )?;
                }
                *car = Car::default();
                projections.remove(&sample.vehicle);
                // Nor are its drivers
                if let Some(t) = sample.session_time {
                    drivers.replaced(sample.vehicle, t);
                }
            }
            if car
                .current
                .as_ref()
                .is_none_or(|current| current.lap != lap)
            {
                let current = car.current.take();
                // The lap after it is over too, without ever saying when it started
                if let (Some(waiting), Some(next)) = (car.waiting.take(), &current) {
                    writer.finish(sample.vehicle, car, waiting, next)?;
                }
                car.waiting = current;
                car.current = Some(LapBuild::new(sample, lap, writer.layout));
                laps_changed = true;
            }
            let Some(mut current) = car.current.take() else {
                continue;
            };
            let was_invalid = current.invalid;
            current.add(sample, length);
            if current.invalid && !was_invalid {
                // Earlier corners may already be queryable before a later cut invalidates the
                // lap. Keep their flag current so they cannot look like clean references.
                conn.prepare_cached(
                    "UPDATE corner_passes SET lap_invalid = 1 WHERE vehicle_id = ? AND lap = ?",
                )?
                .execute(params![sample.vehicle, lap])?;
            }
            if let (Some(layout), Some(temperatures)) =
                (writer.layout, temperatures.get(&sample.id))
            {
                add_rises(&mut current, layout, temperatures, sample);
            }

            // The next lap's start says this one's time
            if let Some(waiting) = car.waiting.take() {
                let started = current
                    .start
                    .is_some_and(|start| !waiting.start.is_some_and(|before| before >= start));
                match started {
                    true => {
                        writer.finish(sample.vehicle, car, waiting, &current)?;
                        laps_changed = true;
                    }
                    false => car.waiting = Some(waiting),
                }
            }
            writer.sample_added(sample.vehicle, &mut current, &car.stints)?;
            car.current = Some(current);
            if laps_changed {
                resegment(
                    conn,
                    sample.vehicle,
                    car,
                    drivers.runs(sample.vehicle),
                    stint_ids,
                )?;
            }
        }

        self.cursor = last_id;
        self.last_row = samples
            .last()
            .map(|sample| (sample.session_time, Some(sample.vehicle)));
        Ok(samples.len() as i64 == CHUNK)
    }

    /// The description's layout, if it isn't the one in use: everything's read again with it.
    fn check_layout(&mut self, conn: &Connection, shared: &Shared) -> rusqlite::Result<()> {
        self.checked_layout = Some(Instant::now());
        let session = row(conn, "SELECT * FROM session ORDER BY id DESC LIMIT 1", [])?;
        let name = session.as_ref().and_then(|s| s.text("track"));
        let session_length = session.as_ref().and_then(|s| s.f("track_length"));
        // Without one, the layout comes from the player's lap (see `find_braking_zones`)
        let turns = read_track(&shared.path, name).and_then(|mut track| {
            let length = track_length(&track, session_length).filter(|l| *l > 0.0)?;
            if let Some(applied) = applied_shift(shared, &track) {
                shift_track(&mut track, applied / length);
            }
            let turns = track_turns(&track, length);
            (!turns.is_empty()).then_some((turns, length))
        });
        let Some((turns, length)) = turns else {
            if self.layout.as_ref().is_some_and(|l| l.source == TRACK_DATA) {
                self.layout = None;
                self.rewind(conn)?;
            }
            return Ok(());
        };
        let layout = layout(turns, length, TRACK_DATA);
        if self.layout.as_ref() != Some(&layout) {
            self.layout = Some(layout);
            self.rewind(conn)?;
        }
        Ok(())
    }

    /// Lines the description up with the laps, once the player has completed another: the layout
    /// moves if that takes more than a car's error in finding its apexes.
    fn calibrate(&mut self, conn: &Connection, shared: &Shared) -> anyhow::Result<()> {
        let player_laps = self
            .player
            .and_then(|player| self.cars.get(&player))
            .map_or(0, |car| car.finished.len());
        let all_laps = self
            .cars
            .values()
            .map(|car| car.finished.len())
            .sum::<usize>();
        // Once the player has laps, only theirs count; until then, anyone's might be usable
        let laps = (player_laps, if player_laps > 0 { 0 } else { all_laps });
        if all_laps == 0 || self.calibrated_laps.is_some_and(|before| before >= laps) {
            return Ok(());
        }
        self.calibrated_laps = Some(laps);

        let session = row(conn, "SELECT * FROM session ORDER BY id DESC LIMIT 1", [])?;
        let name = session.as_ref().and_then(|s| s.text("track"));
        let session_length = session.as_ref().and_then(|s| s.f("track_length"));
        let Some(track) = read_track(&shared.path, name) else {
            return Ok(());
        };
        let Some(length) = track_length(&track, session_length).filter(|l| *l > 0.0) else {
            return Ok(());
        };
        let Some(calibration) = self.fit_calibration(conn, shared, &track, length)? else {
            return Ok(());
        };
        let applied = applied_shift(shared, &track);
        if applied.is_some_and(|applied| (applied - calibration.shift).abs() < SHIFT_MIN) {
            return Ok(());
        }
        log::info!(
            "Track description moved {:+.0} m to line up with lap {} ({} of {} turns)",
            calibration.shift,
            calibration.lap,
            calibration.matched,
            calibration.turns
        );
        *lock(&shared.calibration) = Some(AppliedCalibration {
            track: track_key(&track),
            calibration,
        });
        self.check_layout(conn, shared)?;
        Ok(())
    }

    /// Lines the description's turns up with the corners of the player's best lap (or, before the
    /// player has one, the quickest cars'). `None` when it's lined up already, or there's no
    /// telling.
    fn fit_calibration(
        &self,
        conn: &Connection,
        shared: &Shared,
        track: &Json,
        length: f64,
    ) -> anyhow::Result<Option<Calibration>> {
        let apexes = track_turns(track, length)
            .into_iter()
            .map(|turn| turn.apex)
            .collect::<Vec<_>>();
        if apexes.len() < SHIFT_MIN_MATCHED {
            return Ok(None);
        }

        let mut vehicles = self.player.into_iter().collect::<Vec<_>>();
        let quickest = match Ctx::new(conn, shared) {
            Ok(ctx) => {
                let sc = ctx.scoring()?;
                let mut others = sc
                    .cars
                    .iter()
                    .filter(|(vehicle, _)| Some(**vehicle) != self.player)
                    .filter_map(|(vehicle, car)| {
                        Some((car.f("best_lap_time").filter(|t| *t > 0.0)?, *vehicle))
                    })
                    .collect::<Vec<_>>();
                others.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(Ordering::Equal));
                others.into_iter().take(3).map(|(_, v)| v).collect()
            }
            Err(_) => Vec::new(),
        };
        vehicles.extend(quickest);

        for vehicle in vehicles {
            let laps = self
                .cars
                .get(&vehicle)
                .map_or(&[][..], |car| &car.finished[..]);
            // A cut can distort the alignment of a published description. Keep calibration
            // clean-only; its uncalibrated description still supplies corner observations.
            let Some((lap, trace)) = best_seen_lap(conn, vehicle, laps, length, false)? else {
                continue;
            };
            let corners = trace
                .braking_zones()
                .into_iter()
                .map(|(_, apex)| apex)
                .collect::<Vec<_>>();
            return Ok(
                fit_shift(&apexes, &corners, length).map(|(shift, matched)| Calibration {
                    shift,
                    matched,
                    turns: apexes.len(),
                    vehicle,
                    lap: lap.lap,
                }),
            );
        }
        Ok(None)
    }

    /// Without a description, find corners from a representative player lap (invalidated is
    /// usable too): everything's read again with them, earlier laps included.
    fn find_braking_zones(&mut self, conn: &Connection) -> anyhow::Result<()> {
        if self
            .layout
            .as_ref()
            .is_some_and(|layout| layout.source != "braking zones" || !self.zones_from_invalid)
        {
            return Ok(());
        }
        let Some(vehicle) = self.player else {
            return Ok(());
        };
        let Some(car) = self.cars.get(&vehicle) else {
            return Ok(());
        };
        let Some(lap) = best_lap(&car.finished) else {
            return Ok(());
        };
        let tried = (vehicle, car.finished.len());
        if self.zones_from == Some(tried) {
            return Ok(());
        }
        self.zones_from = Some(tried);
        let session_length = scalar::<f64>(
            conn,
            "SELECT track_length FROM session ORDER BY id DESC LIMIT 1",
            [],
        )
        .ok()
        .flatten()
        .filter(|length| *length > 0.0);
        let length = match session_length {
            Some(length) => Some(length),
            None => scalar::<f64>(
                conn,
                "SELECT MAX(lap_distance) FROM telemetry WHERE vehicle_id = ? AND id BETWEEN ? AND ?",
                params![vehicle, lap.first_id, lap.last_id],
            )?,
        };
        let Some(length) = length.filter(|length| *length > 0.0) else {
            return Ok(());
        };
        let Some((lap, trace)) = best_seen_lap(conn, vehicle, &car.finished, length, true)? else {
            return Ok(());
        };
        // Keep the fallback stable while replaying it; replace it only with clean evidence.
        if self.layout.is_some() && lap.invalid {
            return Ok(());
        }
        let invalid = lap.invalid;
        let turns = trace
            .braking_zones()
            .into_iter()
            .enumerate()
            .map(|(k, (start, apex))| Turn {
                label: format!("braking zone {}", k + 1),
                direction: None,
                scale: None,
                start,
                apex,
                end: (apex + 60.0).min(length),
            })
            .collect::<Vec<_>>();
        if turns.is_empty() {
            return Ok(());
        }
        self.layout = Some(layout(turns, length, "braking zones"));
        self.zones_from_invalid = invalid;
        self.rewind(conn)?;
        Ok(())
    }

    /// Writes the tables of how things are now, and says what it couldn't: one car's trouble isn't
    /// the others', and a query is better off told than given an empty field.
    pub fn refresh_now(
        &mut self,
        conn: &Connection,
        shared: &Shared,
    ) -> anyhow::Result<Option<String>> {
        let tx = conn.unchecked_transaction()?;
        for table in NOW_TABLES {
            conn.execute(&format!("DELETE FROM {table}"), [])?;
        }
        let mut problems = Vec::new();
        let mut fresh = Fresh {
            stints: self.stints(conn, &mut problems),
            ..Fresh::default()
        };
        if let Err(e) = self.write_now(conn, shared, &mut fresh, &mut problems) {
            problems.push(format!("{e:#}"));
        }
        // Without a player's car too: they're the recording's
        self.write_stints(conn, &fresh, &mut problems);
        if let Err(e) = self.write_drivers(conn) {
            problems.push(format!("drivers_now: {e}"));
        }
        if let Err(e) = tx.commit() {
            self.forget_stints();
            return Err(e.into());
        }
        // Only what's worked out from all of it is kept: a car still being read could be in a stint
        // it's long finished
        if !self.behind && !self.failed {
            for (vehicle, (columns, keep)) in fresh.projections {
                if !keep {
                    continue;
                }
                if let Some(stint) = fresh.stints.get(&vehicle).and_then(|stints| stints.last()) {
                    let saved = self.projections.entry(vehicle).or_default();
                    saved.retain(|saved| !saved.is(stint));
                    saved.push(Saved {
                        start: stint.start.clone(),
                        columns,
                    });
                }
            }
        }
        Ok((!problems.is_empty()).then(|| problems.join("; ")))
    }

    /// Every car's stints as they stand, its rows put right if they've changed: a lap's gone on,
    /// and a stop with it. A car with no laps has them from its drivers alone.
    fn stints(&mut self, conn: &Connection, problems: &mut Vec<String>) -> CarStints {
        let Self {
            cars,
            drivers,
            stint_ids,
            ..
        } = self;
        let mut stints = CarStints::new();
        for (vehicle, car) in cars.iter_mut() {
            if let Err(e) = resegment(conn, *vehicle, car, drivers.runs(*vehicle), stint_ids) {
                problems.push(format!("car {vehicle}'s stints: {e}"));
            }
            stints.insert(*vehicle, car.stints.clone());
        }
        for (vehicle, runs) in &drivers.runs {
            stints
                .entry(*vehicle)
                .or_insert_with(|| numbered(*vehicle, segment(&[], runs), stint_ids));
        }
        stints
    }

    fn write_drivers(&self, conn: &Connection) -> rusqlite::Result<()> {
        for (id, driver) in (1..).zip(&self.drivers.known) {
            if driver.gone {
                continue;
            }
            insert(
                conn,
                "drivers_now",
                vec![
                    ("driver_id", Value::Integer(id)),
                    ("vehicle_id", Value::Integer(driver.vehicle)),
                    ("name", Value::Text(driver.name.clone())),
                    ("first_seen_s", real(Some(driver.first_seen))),
                    ("last_seen_s", real(Some(driver.last_seen))),
                ],
            )?;
        }
        Ok(())
    }

    /// Every stint so far: what it's done, and what it was on course for when last worked out.
    fn write_stints(&self, conn: &Connection, fresh: &Fresh, problems: &mut Vec<String>) {
        for (vehicle, stints) in &fresh.stints {
            let laps = match (fresh.laps.get(vehicle), self.cars.get(vehicle)) {
                (Some(laps), _) => Cow::Borrowed(laps.as_slice()),
                (None, Some(car)) => match car.laps(conn, *vehicle) {
                    Ok(laps) => Cow::Owned(laps),
                    Err(e) => {
                        problems.push(format!("car {vehicle}'s stints: {e}"));
                        continue;
                    }
                },
                (None, None) => Cow::Owned(Vec::new()),
            };
            let places = places(&laps, stints);
            for (i, segment) in stints.iter().enumerate() {
                let stint = Stint::new(&laps, &places, stints, i);
                let mut columns = stint_row(*vehicle, segment, &stint);
                let fresh = fresh
                    .projections
                    .get(vehicle)
                    .filter(|_| i + 1 == stints.len())
                    .map(|(columns, _)| columns);
                let saved = self
                    .projections
                    .get(vehicle)
                    .and_then(|saved| saved.iter().find(|saved| saved.is(segment)))
                    .map(|saved| &saved.columns);
                if let Some(projection) = fresh.or(saved) {
                    columns.extend(projection.iter().cloned());
                }
                if let Err(e) = insert(conn, "stints", columns) {
                    problems.push(format!("car {vehicle}'s stints: {e}"));
                }
            }
        }
    }

    fn write_now(
        &self,
        conn: &Connection,
        shared: &Shared,
        fresh: &mut Fresh,
        problems: &mut Vec<String>,
    ) -> anyhow::Result<()> {
        let ctx = Ctx::new(conn, shared)?;
        let sc = ctx.scoring()?;
        let frame = ctx.frame()?;
        let now = ctx.player.f("session_time");
        let (ids, place, exact) = positions(&ctx, &frame)?;

        if let Err(e) = self.write_race_state(&ctx, sc, now) {
            problems.push(format!("race_state: {e}"));
        }

        let mut vehicles = sc.cars.keys().copied().collect::<Vec<_>>();
        vehicles.extend(frame.keys().copied());
        vehicles.sort_unstable();
        vehicles.dedup();

        for vehicle in vehicles {
            let order = (ids.as_slice(), &place, exact);
            let laps = match self.cars.get(&vehicle) {
                Some(car) => match car.laps(conn, vehicle) {
                    Ok(laps) => laps,
                    Err(e) => {
                        problems.push(format!("car {vehicle}: {e}"));
                        continue;
                    }
                },
                None => Vec::new(),
            };
            let stints = fresh.stints.get(&vehicle).map_or(&[][..], Vec::as_slice);
            let projection =
                match self.write_car_now(&ctx, sc, &frame, vehicle, now, order, stints, &laps) {
                    Ok(projection) => projection,
                    Err(e) => {
                        problems.push(format!("car {vehicle}: {e}"));
                        None
                    }
                };
            if let Some(projection) = projection {
                fresh.projections.insert(vehicle, projection);
            }
            fresh.laps.insert(vehicle, laps);
        }
        if let Err(e) = write_forecast(conn, sc, now) {
            problems.push(format!("forecast_now: {e}"));
        }
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn write_car_now(
        &self,
        ctx: &Ctx,
        sc: &Scoring,
        frame: &BTreeMap<i64, Row>,
        vehicle: i64,
        now: Option<f64>,
        order: (&[i64], &HashMap<i64, usize>, bool),
        stints: &[Segment],
        laps: &[Lap],
    ) -> rusqlite::Result<Option<(Columns, bool)>> {
        let conn = ctx.conn;
        // The player's own row is the newest, with its car_state values
        let telemetry = match vehicle == ctx.vehicle {
            true => Some(ctx.player.clone()),
            false => frame
                .get(&vehicle)
                .map(|r| with_state(conn, r))
                .transpose()?,
        };
        let current = stints.last();
        self.write_car(
            ctx,
            sc,
            vehicle,
            telemetry.as_ref(),
            now,
            order,
            current.map(|stint| stint.id),
        )?;
        write_sectors(ctx, sc, vehicle)?;
        let Some(telemetry) = &telemetry else {
            return Ok(None);
        };
        let places = places(laps, stints);
        let stint = current.map(|_| Stint::new(laps, &places, stints, stints.len() - 1));
        let frac = lap_fraction(stint.as_ref().and_then(|stint| stint.avg_time), telemetry);
        write_wheels(ctx, vehicle, telemetry, laps, frac)?;
        let in_the_pits =
            telemetry.b("in_pit_lane") || laps.last().is_some_and(|lap| lap.pit || lap.refuelled);
        let projection = stint.filter(|_| !laps.is_empty()).map(|stint| {
            let columns = projection(sc, frame, vehicle, telemetry, &stint, frac, now);
            (columns, !in_the_pits)
        });

        // How the lap in progress stands
        if let Some(car) = self.cars.get(&vehicle) {
            let writer = Writer {
                conn,
                layout: self.layout.as_ref(),
                official: &self.official,
                player: self.player,
            };
            for lap in laps.iter().skip(car.finished.len()) {
                writer.write_lap(vehicle, car, lap, vehicle == ctx.vehicle)?;
            }
        }
        Ok(projection)
    }

    fn write_race_state(&self, ctx: &Ctx, sc: &Scoring, now: Option<f64>) -> rusqlite::Result<()> {
        // Without a session row yet, what's known without one is still known
        let empty = Row {
            columns: Arc::default(),
            values: Vec::new(),
        };
        let s = sc.session.as_ref().unwrap_or(&empty);
        let age = staleness(&ctx.shared.path);
        let (start_t, end_t) = (s.f("pit_window_start_time"), s.f("pit_window_end_time"));
        let opens = start_t
            .zip(now)
            .map(|(t, now)| t - now)
            .filter(|t| *t > 0.0);
        let closes = end_t.zip(now).map(|(t, now)| t - now).filter(|t| *t > 0.0);
        let wind = s.f("wind_x").zip(s.f("wind_z")).map(|(x, z)| x.hypot(z));
        insert(
            ctx.conn,
            "race_state",
            vec![
                ("session_type", copy(s, "session_type")),
                ("session_number", copy(s, "session_number")),
                ("phase", copy(s, "phase")),
                ("track", copy(s, "track")),
                ("track_length_m", copy(s, "track_length")),
                ("max_laps", copy(s, "max_laps")),
                ("session_time_s", real(now)),
                ("time_left_s", real(sc.time_left(now))),
                (
                    "leader_laps_completed",
                    integer(sc.order().first().and_then(|r| r.i("laps_completed"))),
                ),
                ("player_vehicle_id", Value::Integer(ctx.vehicle)),
                ("session_first_id", Value::Integer(self.session_start)),
                ("latest_telemetry_id", Value::Integer(ctx.last_id)),
                ("data_age_s", real(age)),
                ("is_stale", flag(age.is_some_and(|age| age > 10.0))),
                (
                    "corner_source",
                    text_or_null(self.layout.as_ref().map(|layout| layout.source)),
                ),
                ("derived_catching_up", flag(self.behind || self.failed)),
                ("full_course_yellow", copy(s, "full_course_yellow")),
                ("local_yellow", flag(!sc.yellow_sectors.is_empty())),
                ("sector_count", copy(s, "sector_count")),
                ("start_light", copy(s, "start_light")),
                ("air_c", copy(s, "ambient_temperature")),
                ("track_c", copy(s, "track_temperature")),
                ("rain_pct", real(percent(s.f("rain")))),
                (
                    "wetness_avg_pct",
                    real(percent(s.f("track_wetness_average"))),
                ),
                ("wetness_min_pct", real(percent(s.f("track_wetness_min")))),
                ("wetness_max_pct", real(percent(s.f("track_wetness_max")))),
                ("grip_level", copy(s, "track_grip_level")),
                ("cloudiness_pct", real(percent(s.f("cloudiness")))),
                ("wind_speed_kmh", real(km_h(wind))),
                ("pit_window_status", copy(s, "pit_window_status")),
                ("pit_window_start_lap", copy(s, "pit_window_start_lap")),
                ("pit_window_end_lap", copy(s, "pit_window_end_lap")),
                ("pit_window_opens_in_s", real(opens)),
                ("pit_window_closes_in_s", real(closes)),
                ("pit_speed_limit_kmh", real(km_h(s.f("pit_speed_limit")))),
                (
                    "track_limit_steps_per_penalty",
                    copy(s, "track_limits_steps_per_penalty"),
                ),
                (
                    "track_limit_steps_per_point",
                    copy(s, "track_limits_steps_per_point"),
                ),
                ("max_incident_points", copy(s, "max_incident_points")),
            ],
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn write_car(
        &self,
        ctx: &Ctx,
        sc: &Scoring,
        vehicle: i64,
        telemetry: Option<&Row>,
        now: Option<f64>,
        (ids, place, exact): (&[i64], &HashMap<i64, usize>, bool),
        current_stint: Option<i64>,
    ) -> rusqlite::Result<()> {
        let empty = Row {
            columns: Arc::default(),
            values: Vec::new(),
        };
        let t = telemetry.unwrap_or(&empty);
        let st = sc.cars.get(&vehicle).unwrap_or(&empty);
        let position = place.get(&vehicle).copied();
        let neighbour =
            |p: Option<usize>| integer(p.and_then(|p| ids.get(p.checked_sub(1)?)).copied());
        let relative = telemetry
            .filter(|_| vehicle != ctx.vehicle)
            .and_then(|r| rel(&ctx.player, r));
        let elapsed = t
            .f("session_time")
            .zip(t.f("lap_start_time"))
            .map(|(t, s)| t - s);
        let impact = t.f("last_impact_time").filter(|t| *t != 0.0);
        let dents = (0..8)
            .map(|i| t.i(&format!("dent_severity_{i}")))
            .collect::<Vec<_>>();
        let dents = dents.iter().any(Option::is_some).then(|| {
            dents
                .iter()
                .map(|d| d.unwrap_or(0).to_string())
                .collect::<Vec<_>>()
                .join(",")
        });
        let (selected, active) = pit_services(st);
        insert(
            ctx.conn,
            "cars_now",
            vec![
                ("vehicle_id", Value::Integer(vehicle)),
                ("is_player", flag(vehicle == ctx.vehicle)),
                ("current_stint_id", integer(current_stint)),
                ("team", copy(st, "team")),
                ("car_number", copy(st, "car_number")),
                ("vehicle", copy(st, "vehicle")),
                ("vehicle_class", copy(st, "vehicle_class")),
                ("telemetry_id", copy(t, "id")),
                (
                    "sample_age_s",
                    real(now.zip(t.f("session_time")).map(|(now, t)| now - t)),
                ),
                (
                    "position",
                    integer(st.i("position").or(position.map(|p| p as i64))),
                ),
                (
                    "class_position",
                    integer(
                        st.i("class_position")
                            .or_else(|| sc.class_position(st).map(|p| p as i64)),
                    ),
                ),
                ("position_estimated", flag(!exact)),
                ("qualifying_position", copy(st, "qualifying_position")),
                (
                    "place_ahead_vehicle_id",
                    neighbour(position.and_then(|p| p.checked_sub(1))),
                ),
                (
                    "place_behind_vehicle_id",
                    neighbour(position.map(|p| p + 1)),
                ),
                ("lap", copy(t, "lap")),
                ("laps_completed", copy(st, "laps_completed")),
                ("sector", integer(t.i("sector").map(|sector| sector + 1))),
                ("lap_distance_m", copy(t, "lap_distance")),
                ("lap_elapsed_s", real(elapsed)),
                ("laps_behind_leader", copy(st, "laps_behind_leader")),
                ("laps_behind_next", copy(st, "laps_behind_next")),
                ("gap_place_ahead_s", copy(t, "gap_to_place_ahead")),
                ("gap_place_behind_s", copy(t, "gap_to_place_behind")),
                ("gap_car_ahead_s", copy(t, "gap_to_car_ahead")),
                ("gap_car_behind_s", copy(t, "gap_to_car_behind")),
                ("time_behind_leader_s", copy(st, "time_behind_leader")),
                ("time_behind_next_s", copy(st, "time_behind_next")),
                ("ahead_of_player_m", real(relative.map(|(ahead, _)| ahead))),
                ("right_of_player_m", real(relative.map(|(_, right)| right))),
                ("last_lap_s", copy(st, "last_lap_time")),
                ("best_lap_s", copy(st, "best_lap_time")),
                ("delta_best_s", copy(t, "delta_best")),
                ("lap_invalidated", copy(t, "lap_invalidated")),
                ("speed_kmh", real(km_h(t.f("speed")))),
                ("gear", copy(t, "gear")),
                ("fuel_l", copy(t, "fuel")),
                ("fuel_fraction", copy(st, "fuel_fraction")),
                (
                    "energy_left_pct",
                    real(energy_percent(t.f("virtual_energy"))),
                ),
                ("battery_pct", real(percent(t.f("battery_charge")))),
                ("front_compound", copy(st, "front_tire_compound_name")),
                ("rear_compound", copy(st, "rear_tire_compound_name")),
                ("water_c", copy(t, "engine_water_temperature")),
                ("oil_c", copy(t, "engine_oil_temperature")),
                ("engine_overheating", copy(t, "engine_overheating")),
                ("tc", copy(t, "traction_control")),
                ("tc_slip", copy(t, "traction_control_slip")),
                ("tc_cut", copy(t, "traction_control_cut")),
                ("abs", copy(t, "abs")),
                ("engine_map", copy(t, "engine_map")),
                (
                    "front_brake_bias_pct",
                    real(t.f("rear_brake_bias").map(|b| (1.0 - b) * 100.0)),
                ),
                ("brake_migration", copy(t, "brake_migration")),
                ("front_arb", copy(t, "front_anti_roll_bar")),
                ("rear_arb", copy(t, "rear_anti_roll_bar")),
                ("limiter_on", copy(t, "speed_limiter_on")),
                ("engine_damage", copy(t, "engine_damage")),
                ("transmission_damage", copy(t, "transmission_damage")),
                ("aero_damage", copy(t, "aerodynamics_damage")),
                ("suspension_damage", copy(t, "suspension_damage")),
                ("parts_detached", copy(t, "parts_detached")),
                ("dents", text_or_null(dents.as_deref())),
                (
                    "last_impact_ago_s",
                    real(now.zip(impact).map(|(now, impact)| now - impact)),
                ),
                ("last_impact_magnitude", copy(t, "last_impact_magnitude")),
                ("in_pit_lane", copy(t, "in_pit_lane")),
                ("in_pits", copy(st, "in_pits")),
                ("in_garage", copy(st, "in_garage")),
                ("pit_state", copy(st, "pit_state")),
                ("pit_stops", copy(st, "pit_stops")),
                ("scheduled_pit_stops", copy(t, "scheduled_pit_stops")),
                ("pit_services_selected", text_or_null(selected.as_deref())),
                ("pit_services_active", text_or_null(active.as_deref())),
                ("pit_min_duration_left_s", copy(st, "pit_min_duration_left")),
                ("flag", copy(st, "flag")),
                ("under_yellow", copy(st, "under_yellow")),
                ("finish_status", copy(st, "finish_status")),
                ("penalties", copy(st, "penalties")),
                ("penalty_type", copy(st, "penalty_type")),
                ("penalty_reason", copy(st, "penalty_reason")),
                ("track_limit_steps", copy(t, "track_limits_steps")),
                (
                    "track_limit_points_left",
                    integer(
                        sc.track_limits(t.i("track_limits_steps"))
                            .map(|(_, _, left)| left),
                    ),
                ),
                ("incident_points", copy(t, "incident_points")),
            ],
        )
    }
}

const TRACK_DATA: &str = "track data";

const PIT_SERVICES: [(&str, &str); 9] = [
    ("penalty", "penalty"),
    ("driver_change", "driver change"),
    ("refuel", "refuel"),
    ("front_tires", "front tires"),
    ("rear_tires", "rear tires"),
    ("body_repair", "body repair"),
    ("front_wing", "front wing"),
    ("rear_wing", "rear wing"),
    ("suspension", "suspension"),
];

/// The pit services selected and those underway, as lists: "none" when the simulator says so, and
/// nothing when it doesn't say.
fn pit_services(st: &Row) -> (Option<String>, Option<String>) {
    let list = |suffix: &str| {
        let column = |service: &str| format!("pit_service_{service}_{suffix}");
        let services = PIT_SERVICES
            .iter()
            .filter(|(service, _)| st.b(&column(service)))
            .map(|(_, name)| *name)
            .collect::<Vec<_>>();
        match services.is_empty() {
            false => Some(services.join(", ")),
            true => PIT_SERVICES
                .iter()
                .any(|(service, _)| st.i(&column(service)).is_some())
                .then(|| "none".to_owned()),
        }
    };
    (list("selected"), list("active"))
}

/// One stint's laps, and the green ones its averages come from.
struct Stint<'a> {
    /// The laps wholly in it
    own: Vec<&'a Lap>,
    baseline: Vec<&'a Lap>,
    /// Whose laps the baseline is
    baseline_source: Option<&'static str>,
    avg_time: Option<f64>,
}

impl<'a> Stint<'a> {
    /// Stint `index` of `stints`. With too few green laps of its own, it borrows its driver's from
    /// before, or else anyone's in the car before it; a lap across a change of stint is no one's.
    /// `places`: where each lap belongs.
    fn new(laps: &'a [Lap], places: &[Place], stints: &[Segment], index: usize) -> Self {
        let placed = laps
            .iter()
            .zip(places)
            .filter_map(|(lap, place)| match place {
                Place::Stint(i) if *i <= index => Some((lap, *i)),
                _ => None,
            })
            .collect::<Vec<_>>();
        let driver = stints[index].driver;
        let same_driver = |i: usize| driver.is_some() && stints[i].driver == driver;
        let last_green = |of: &dyn Fn(usize) -> bool| {
            let green = placed
                .iter()
                .copied()
                .filter(|(lap, i)| lap.green && of(*i))
                .collect::<Vec<_>>();
            green[green.len().saturating_sub(5)..].to_vec()
        };
        let mut baseline = last_green(&|i| i == index);
        if baseline.len() < 2 && driver.is_some() {
            baseline = last_green(&same_driver);
        }
        if baseline.len() < 2 {
            baseline = last_green(&|_| true);
        }
        let baseline_source = (!baseline.is_empty()).then(|| {
            if baseline.iter().all(|(_, i)| *i == index) {
                "this stint"
            } else if baseline.iter().all(|(_, i)| same_driver(*i)) {
                "this driver"
            } else {
                "any driver"
            }
        });
        let baseline = baseline.into_iter().map(|(lap, _)| lap).collect::<Vec<_>>();
        let times = baseline
            .iter()
            .filter_map(|lap| lap.time)
            .collect::<Vec<_>>();
        let avg_time = (!times.is_empty()).then(|| times.iter().sum::<f64>() / times.len() as f64);
        Self {
            own: placed
                .iter()
                .filter(|(_, i)| *i == index)
                .map(|(lap, _)| *lap)
                .collect(),
            baseline,
            baseline_source,
            avg_time,
        }
    }

    fn average_use(&self, used: impl Fn(&Lap) -> Option<f64>) -> Option<f64> {
        let values = self
            .baseline
            .iter()
            .filter_map(|lap| used(lap))
            .filter(|v| *v > 0.0)
            .collect::<Vec<_>>();
        (!values.is_empty()).then(|| values.iter().sum::<f64>() / values.len() as f64)
    }
}

/// How far into its lap the car is, by its stint's average lap.
fn lap_fraction(avg_time: Option<f64>, telemetry: &Row) -> f64 {
    let elapsed = telemetry
        .f("session_time")
        .zip(telemetry.f("lap_start_time"))
        .map(|(t, s)| t - s);
    match (avg_time, elapsed) {
        (Some(avg), Some(elapsed)) if avg != 0.0 => (elapsed / avg).clamp(0.0, 0.99),
        _ => 0.0,
    }
}

/// Steps after `first`, `step` apart, until reaching `target`: the smallest `k >= 0` with
/// `first + k * step >= target`. Closed form, so a huge `target` can't spin forever.
fn steps_to(first: f64, step: f64, target: f64) -> f64 {
    if first >= target {
        0.0
    } else {
        ((target - first) / step).ceil()
    }
}

/// Laps a car still has to drive (including the rest of this one), and where that came from.
fn race_need(
    sc: &Scoring,
    frame: &BTreeMap<i64, Row>,
    vehicle: i64,
    now: Option<f64>,
    frac: f64,
    avg_time: Option<f64>,
) -> (Option<f64>, String) {
    let s = sc.session.as_ref();
    let Some(s) = s else {
        return (None, "unknown (no session data)".into());
    };
    let note = s
        .text("session_type")
        .filter(|kind| *kind != "race")
        .map(|kind| format!("this is {kind}, not a race"));

    let mut needs = Vec::<(f64, String)>::new();
    if let (Some(max_laps), Some(me)) = (s.i("max_laps").filter(|n| *n != 0), sc.cars.get(&vehicle))
        && let Some(completed) = me.i("laps_completed")
    {
        // A lapped car finishes when it crosses the line after the leader has done max_laps
        let n = (max_laps - me.i("laps_behind_leader").unwrap_or(0) - completed) as f64 - frac;
        needs.push((n.max(0.0), format!("{max_laps} lap race")));
    }

    let time_left = sc
        .time_left(now)
        .filter(|t| t.is_finite() && *t <= MAX_RACE_TIME_LEFT);
    let lap_ok = |t: &f64| t.is_finite() && *t > 0.0;
    if let (Some(time_left), Some(avg), Some(now)) = (time_left, avg_time.filter(lap_ok), now) {
        // The flag falls when the leader crosses the line after the clock runs out; you then
        // finish your lap
        let mut t_flag = now + time_left;
        match sc.order().first().copied() {
            Some(leader) if leader.i("vehicle_id") != Some(vehicle) => {
                let leader_row = leader.i("vehicle_id").and_then(|v| frame.get(&v));
                let leader_lap = leader
                    .f("last_lap_time")
                    .filter(lap_ok)
                    .or_else(|| leader.f("estimated_lap_time").filter(lap_ok))
                    .unwrap_or(avg);
                if let Some(start) = leader_row
                    .and_then(|r| r.f("lap_start_time"))
                    .filter(|t| t.is_finite())
                {
                    let clock_end = s.f("session_end_time").unwrap_or(now + time_left);
                    t_flag = match start >= clock_end {
                        // Started this lap with the clock run out: that was the flag
                        true => start,
                        false => {
                            let first = start + leader_lap;
                            first + steps_to(first, leader_lap, now + time_left) * leader_lap
                        }
                    };
                }
            }
            _ => {
                let first = now + (1.0 - frac) * avg;
                t_flag = first + steps_to(first, avg, now + time_left) * avg;
            }
        }

        let n = 1.0 - frac + steps_to(now + (1.0 - frac) * avg, avg, t_flag - 0.5);
        needs.push((
            n,
            format!(
                "{} on the clock, plus the lap to the flag",
                clock(time_left)
            ),
        ));
    }

    let Some((need, source)) = needs
        .into_iter()
        .min_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(Ordering::Equal))
    else {
        return (None, note.unwrap_or_else(|| "unknown".into()));
    };
    let note = note.map(|note| format!("; {note}")).unwrap_or_default();
    (Some(need), format!("{source}{note}"))
}

/// Laps a level lasts, and on which lap (lap + fraction of it) it runs out.
fn range(
    level: Option<f64>,
    per_lap: Option<f64>,
    lap: Option<i64>,
    frac: f64,
) -> (Option<f64>, Option<f64>) {
    match (level, per_lap.filter(|v| *v != 0.0)) {
        (Some(level), Some(per_lap)) => {
            let left = level / per_lap;
            (Some(left), Some(lap.unwrap_or(0) as f64 + frac + left))
        }
        _ => (None, None),
    }
}

/// What a stint is and has done, from its laps.
fn stint_row(vehicle: i64, segment: &Segment, stint: &Stint) -> Columns {
    let times = stint
        .baseline
        .iter()
        .filter_map(|l| l.time)
        .collect::<Vec<_>>();
    let trend = match (times.first(), times.last()) {
        (Some(first), Some(last)) if times.len() > 1 => {
            Some((last - first) / (times.len() - 1) as f64)
        }
        _ => None,
    };
    let energy_is_fraction = stint
        .baseline
        .iter()
        .any(|lap| lap.energy_start.is_some_and(|energy| energy <= 1.0));
    let energy_use = stint.average_use(|lap| lap.energy_used);
    let start = &segment.start;
    vec![
        ("stint_id", Value::Integer(segment.id)),
        ("vehicle_id", Value::Integer(vehicle)),
        ("stint_number", Value::Integer(segment.number)),
        ("driver_id", integer(segment.driver)),
        ("start_reason", Value::Text(start.reason.into())),
        ("start_from_s", real(start.from)),
        ("start_to_s", real(start.to)),
        ("start_standing_id", integer(start.standing_id)),
        ("start_lap", integer(stint.own.first().map(|lap| lap.lap))),
        ("end_lap", integer(stint.own.last().map(|lap| lap.lap))),
        (
            "laps_complete",
            Value::Integer(stint.own.iter().filter(|lap| lap.complete).count() as i64),
        ),
        ("baseline_laps", Value::Integer(stint.baseline.len() as i64)),
        ("baseline_source", text_or_null(stint.baseline_source)),
        ("avg_lap_s", real(stint.avg_time)),
        ("pace_trend_s_per_lap", real(trend)),
        (
            "fuel_per_lap_l",
            real(stint.average_use(|lap| lap.fuel_used)),
        ),
        (
            "energy_per_lap_pct",
            real(energy_use.filter(|_| energy_is_fraction).map(|e| e * 100.0)),
        ),
    ]
}

/// What the car's current stint is on course for, as of `now`: kept with the stint once it's over.
fn projection(
    sc: &Scoring,
    frame: &BTreeMap<i64, Row>,
    vehicle: i64,
    p: &Row,
    stint: &Stint,
    frac: f64,
    now: Option<f64>,
) -> Columns {
    let lap_now = p.i("lap");
    let fuel = p.f("fuel");
    let energy = p.f("virtual_energy");
    let fuel_use = stint.average_use(|lap| lap.fuel_used);
    let energy_use = stint.average_use(|lap| lap.energy_used);
    let (fuel_laps, fuel_out) = range(fuel, fuel_use, lap_now, frac);
    // Without any use seen, a level of zero only means the simulator has no such thing
    let (energy_laps, energy_out) = range(energy, energy_use, lap_now, frac);
    let limit = match (energy_laps, fuel_laps) {
        (Some(energy), Some(fuel)) if energy <= fuel => Some(("energy", energy)),
        (Some(energy), None) => Some(("energy", energy)),
        (_, Some(fuel)) => Some(("fuel", fuel)),
        (None, None) => None,
    };
    let (need, source) = race_need(sc, frame, vehicle, now, frac, stint.avg_time);
    let margin = limit.zip(need).map(|((_, left), need)| left - need);
    let save = margin
        .zip(need)
        .filter(|(margin, _)| *margin < 0.0)
        .map(|(margin, need)| -margin / need.max(0.1) * 100.0);
    vec![
        ("calculated_at_s", real(now)),
        ("lap_fraction", real(Some(frac))),
        ("fuel_l", real(fuel)),
        ("fuel_laps_left", real(fuel_laps)),
        ("fuel_runs_out_lap", real(fuel_out)),
        ("tank_capacity_l", copy(p, "fuel_capacity")),
        ("energy_left_pct", real(energy_percent(energy))),
        ("energy_laps_left", real(energy_laps)),
        ("energy_runs_out_lap", real(energy_out)),
        ("game_fuel_per_lap_l", copy(p, "fuel_per_lap")),
        (
            "game_energy_per_lap_pct",
            real(percent(p.f("virtual_energy_per_lap"))),
        ),
        ("limited_by", text_or_null(limit.map(|(which, _)| which))),
        ("laps_left", real(limit.map(|(_, left)| left))),
        (
            "last_full_lap",
            // Lap N spans [N, N + 1), so reaching N completes lap N - 1.
            integer(
                limit.map(|(_, left)| {
                    (lap_now.unwrap_or(0) as f64 + frac + left).floor() as i64 - 1
                }),
            ),
        ),
        ("laps_needed", real(need)),
        ("laps_needed_source", Value::Text(source)),
        ("margin_laps", real(margin)),
        ("save_pct_needed", real(save)),
    ]
}

/// The car's sector times as of its newest standing.
fn write_sectors(ctx: &Ctx, sc: &Scoring, vehicle: i64) -> rusqlite::Result<()> {
    let Some(id) = sc.cars.get(&vehicle).and_then(|st| st.i("id")) else {
        return Ok(());
    };
    for s in rows(
        ctx.conn,
        "SELECT * FROM standing_sector WHERE standing_id = ? ORDER BY sector",
        [id],
    )? {
        let Some(sector) = s.i("sector") else {
            continue;
        };
        let time = |name| real(s.f(name).filter(|time| *time > 0.0));
        insert(
            ctx.conn,
            "sectors_now",
            vec![
                ("vehicle_id", Value::Integer(vehicle)),
                ("sector", Value::Integer(sector + 1)),
                ("last_lap_s", time("last_lap")),
                ("best_lap_s", time("best_lap")),
                ("best_s", time("best")),
                ("current_lap_s", time("current_lap")),
            ],
        )?;
    }
    Ok(())
}

fn write_wheels(
    ctx: &Ctx,
    vehicle: i64,
    telemetry: &Row,
    laps: &[Lap],
    frac: f64,
) -> rusqlite::Result<()> {
    let Some(id) = telemetry.i("id") else {
        return Ok(());
    };
    let remaining = wear_remaining(laps);
    // The tyres' laps: since the last refuel or pit
    let start = laps
        .iter()
        .rposition(|lap| lap.refuelled || lap.pit)
        .map_or(0, |i| i + 1);
    let stint_laps = &laps[start..];
    let done = stint_laps
        .iter()
        .filter(|lap| lap.complete && Lap::has_wear(&lap.wear_end))
        .count();
    let n = done as f64 + frac;
    for (wheel, r) in wheels_at(ctx.conn, id)? {
        let Some(name) = WHEELS.get(wheel) else {
            continue;
        };
        let strips = inner_middle_outer(
            wheel,
            [
                r.f("tire_surface_temperature_left"),
                r.f("tire_surface_temperature_center"),
                r.f("tire_surface_temperature_right"),
            ],
        );
        let optimal = celsius(r.f("tire_optimal_temperature"));
        let surface = match strips {
            [Some(a), Some(b), Some(c)] => Some((a + b + c) / 3.0),
            _ => None,
        };
        let first = stint_laps.first().and_then(|lap| lap.wear_start[wheel]);
        let last = laps.last().and_then(|lap| lap.wear_end[wheel]);
        let rate = (done > 0 && n > 0.0)
            .then(|| {
                first
                    .zip(last)
                    .map(|(first, last)| (first - last).abs() / n * 100.0)
            })
            .flatten();
        let life = r.f("tire_wear").map(|wear| match remaining {
            true => wear * 100.0,
            false => (1.0 - wear) * 100.0,
        });
        insert(
            ctx.conn,
            "wheels_now",
            vec![
                ("vehicle_id", Value::Integer(vehicle)),
                ("wheel", Value::Integer(wheel as i64)),
                ("wheel_name", Value::Text((*name).into())),
                ("life_left_pct", real(life)),
                ("stint_wear_pct_per_lap", real(rate)),
                ("compound", copy(&r, "tire_compound")),
                ("flat", copy(&r, "tire_flat")),
                ("detached", copy(&r, "wheel_detached")),
                ("surface_inner_c", real(strips[0])),
                ("surface_middle_c", real(strips[1])),
                ("surface_outer_c", real(strips[2])),
                ("core_c", copy(&r, "tire_inner_temperature_center")),
                ("carcass_c", copy(&r, "tire_carcass_temperature")),
                ("optimal_c", real(optimal)),
                (
                    "surface_vs_optimal_c",
                    real(
                        surface
                            .zip(optimal)
                            .map(|(surface, optimal)| surface - optimal),
                    ),
                ),
                ("pressure_kpa", copy(&r, "tire_pressure")),
                ("brake_c", copy(&r, "brake_temperature")),
                ("brake_optimal_c", copy(&r, "brake_optimal_temperature")),
                ("surface_type", copy(&r, "surface_type")),
                ("sliding_fraction", copy(&r, "sliding_fraction")),
            ],
        )?;
    }
    Ok(())
}

/// The forecast for this session, and the race's if this isn't it.
fn write_forecast(conn: &Connection, sc: &Scoring, now: Option<f64>) -> rusqlite::Result<()> {
    let Some(s) = &sc.session else {
        return Ok(());
    };
    let Some(kind) = s.text("session_type") else {
        return Ok(());
    };
    let end = s.f("session_end_time").filter(|end| *end > 0.0);
    let mut kinds = vec![kind];
    if kind != "race" {
        kinds.push("race");
    }
    for forecast_kind in kinds {
        let current = forecast_kind == kind;
        for r in Scoring::forecast(conn, forecast_kind)? {
            let fraction = r.f("session_fraction").unwrap_or(0.0);
            let sky = r
                .i("sky")
                .and_then(|sky| usize::try_from(sky).ok())
                .and_then(|sky| SKY.get(sky).copied());
            let minutes = end
                .zip(now)
                .filter(|_| current)
                .map(|(end, now)| (fraction * end - now) / 60.0);
            insert(
                conn,
                "forecast_now",
                vec![
                    ("session_type", Value::Text(forecast_kind.to_owned())),
                    ("is_current_session", flag(current)),
                    ("session_fraction", real(Some(fraction))),
                    ("minutes_from_now", real(minutes)),
                    ("part_of_session", Value::Text(part_of(fraction).to_owned())),
                    ("sky", text_or_null(sky)),
                    ("sky_text", copy(&r, "sky_text")),
                    ("rain_chance_pct", real(percent(r.f("rain_chance")))),
                    ("air_c", copy(&r, "temperature")),
                    ("humidity_pct", real(percent(r.f("humidity")))),
                    ("wind_speed_kmh", real(km_h(r.f("wind_speed")))),
                    ("wind_direction", copy(&r, "wind_direction")),
                ],
            )?;
        }
    }
    Ok(())
}

/// Where the newest session starts: the player's clock last went back, on a scan of the whole
/// file, only done once.
fn session_start(conn: &Connection) -> rusqlite::Result<i64> {
    let top = scalar::<i64>(conn, "SELECT MAX(id) FROM telemetry", [])?.unwrap_or(0);
    let (mut start, mut player_t, mut lo) = (1, None::<f64>, 1);
    while lo <= top {
        let hi = top.min(lo + 200_000 - 1);
        for r in rows(
            conn,
            "SELECT id, session_time FROM telemetry WHERE is_player = 1 AND id BETWEEN ? AND ? ORDER BY id",
            params![lo, hi],
        )? {
            let (Some(id), Some(t)) = (r.i("id"), r.f("session_time")) else {
                continue;
            };
            if player_t.is_some_and(|before| t < before - 1.0) {
                start = frame_start(conn, id, t)?;
            }
            player_t = Some(t);
        }
        lo = hi + 1;
    }
    Ok(start)
}

/// The first row of the frame the player's first row of a session belongs to: after the last row
/// around it whose clock is the old session's. Never before the player's last row of that session,
/// or the clock would be seen going back again from there.
fn frame_start(conn: &Connection, id: i64, t: f64) -> rusqlite::Result<i64> {
    let old = scalar::<i64>(
        conn,
        "SELECT MAX(id) FROM telemetry WHERE id BETWEEN ? AND ? AND session_time > ?",
        params![id - FRAME_WINDOW, id - 1, t + 0.5],
    )?;
    let from = old.map_or(id - FRAME_WINDOW, |old| old + 1);
    Ok(scalar::<i64>(
        conn,
        "SELECT MIN(id) FROM telemetry WHERE id BETWEEN ? AND ?",
        params![from, id],
    )?
    .unwrap_or(id))
}

/// Each player row's tyre surface mean and brake temperature, by wheel.
fn wheel_temperatures(
    conn: &Connection,
    vehicle: i64,
    from: i64,
    to: i64,
) -> rusqlite::Result<HashMap<i64, [[Option<f64>; 2]; 4]>> {
    let mut temperatures = HashMap::<i64, [[Option<f64>; 2]; 4]>::new();
    let mut stmt = conn.prepare_cached(
        "SELECT t.id, w.wheel, w.tire_surface_temperature_left, \
                w.tire_surface_temperature_center, w.tire_surface_temperature_right, \
                w.brake_temperature \
         FROM telemetry t JOIN telemetry_wheel w ON w.telemetry_id = t.id \
         WHERE t.vehicle_id = ? AND t.id BETWEEN ? AND ?",
    )?;
    let mut rows = stmt.query(params![vehicle, from, to])?;
    while let Some(r) = rows.next()? {
        let Some(wheel) = usize::try_from(r.get::<_, i64>(1)?).ok().filter(|w| *w < 4) else {
            continue;
        };
        let strips: [Option<f64>; 3] = [r.get(2)?, r.get(3)?, r.get(4)?];
        let surface = match strips {
            [Some(l), Some(c), Some(r)] => Some((l + c + r) / 3.0),
            _ => None,
        };
        temperatures.entry(r.get(0)?).or_default()[wheel] = [surface, r.get(5)?];
    }
    Ok(temperatures)
}

/// Adds a player's sample's temperature rises to the section of the lap it's in. Only rises: heat
/// that builds in one place shows there even if it's shed again on the next straight. What it rose
/// from is the car's last reading before this sample, whichever corner that was in.
fn add_rises(
    build: &mut LapBuild,
    layout: &Layout,
    temperatures: &[[Option<f64>; 2]; 4],
    sample: &Sample,
) {
    let Some(section) = layout
        .corners
        .iter()
        .rposition(|corner| corner.boundary <= build.top)
    else {
        return;
    };
    let phase = match (sample.brake, sample.throttle) {
        (Some(b), _) if b >= BRAKE_ON => Some(0),
        (_, Some(t)) if t >= THROTTLE_ON => Some(2),
        (None, None) => None,
        _ => Some(1),
    };
    let last_reading = |build: &LapBuild, wheel: usize, kind: usize| {
        build.rises[..=section]
            .iter()
            .rev()
            .find_map(|rises| rises[wheel].last[kind])
    };
    for (wheel, readings) in temperatures.iter().enumerate() {
        for (kind, now) in readings.iter().enumerate() {
            let Some(now) = *now else {
                continue;
            };
            let before = last_reading(build, wheel, kind);
            let rises = &mut build.rises[section][wheel];
            if let Some(before) = before
                && now > before
            {
                match phase {
                    Some(phase) => rises.by_phase[kind][phase] += now - before,
                    None => rises.other[kind] += now - before,
                }
            }
            rises.last[kind] = Some(now);
        }
    }
}

// --------------------------------------------------------------------------- the track description

/// A key for the description, to tell whether a calibration is for it.
pub(super) fn track_key(track: &Json) -> String {
    track
        .get("source_track")
        .or_else(|| track.get("name"))
        .and_then(Json::as_str)
        .unwrap_or_default()
        .to_owned()
}

/// The shift the description has been moved by, if it has.
fn applied_shift(shared: &Shared, track: &Json) -> Option<f64> {
    lock(&shared.calibration)
        .as_ref()
        .filter(|applied| applied.track == track_key(track))
        .map(|applied| applied.calibration.shift)
}

/// The description's turns, in metres. A turn may run across the line.
pub(super) fn track_turns(track: &Json, length: f64) -> Vec<Turn> {
    json_list(track, "turn")
        .into_iter()
        .filter_map(|t| {
            let (start, end) = (
                num(t, "start")?.rem_euclid(1.0),
                num(t, "end")?.rem_euclid(1.0),
            );
            let apex = num(t, "marker")
                .unwrap_or_else(|| start + (end - start).rem_euclid(1.0) / 2.0)
                .rem_euclid(1.0);
            Some(Turn {
                label: turn_label(t),
                direction: match t.get("direction").and_then(Json::as_i64) {
                    Some(0) => Some("left"),
                    Some(1) => Some("right"),
                    _ => None,
                },
                scale: t.get("scale").and_then(Json::as_i64),
                start: start * length,
                apex: apex * length,
                end: end * length,
            })
        })
        .collect()
}

/// `track`: the description as it's published, its positions lined up with the laps.
fn track_json(conn: &Connection, shared: &Shared, stream: &mut Stream) -> anyhow::Result<String> {
    let session = row(conn, "SELECT * FROM session ORDER BY id DESC LIMIT 1", [])?;
    let name = session.as_ref().and_then(|s| s.text("track"));
    let session_length = session.as_ref().and_then(|s| s.f("track_length"));
    let Some(mut track) = read_track(&shared.path, name) else {
        return Ok(format!(
            "TRACK no description for {} (there is one only when lovely-track-data has the track, or the same layout in another simulator, and the app saved it for this session)",
            telemetry_text(name.unwrap_or("this track"))
        ));
    };
    let length = track_length(&track, session_length).filter(|l| *l > 0.0);
    let applied = lock(&shared.calibration)
        .as_ref()
        .filter(|applied| applied.track == track_key(&track))
        .map(|applied| applied.calibration);
    if let (Some(length), Some(calibration)) = (length, applied) {
        shift_track(&mut track, calibration.shift / length);
        track["calibration"] = serde_json::json!({
            "shift_m": calibration.shift,
            "matched_turns": calibration.matched,
            "turns": calibration.turns,
            "vehicle": calibration.vehicle,
            "lap": calibration.lap,
        });
    }
    if let Some(length) = length {
        track["lap_length_m"] = length.into();
    }
    // The simulator's own timing lines are already where its laps are: after the shift
    let count = session
        .as_ref()
        .and_then(|s| s.i("sector_count"))
        .filter(|n| *n > 0);
    let timing = match (count, length) {
        (Some(count), Some(length)) => {
            let from = stream.session_start;
            let key = (from, name.unwrap_or_default().to_owned(), count);
            // The lines don't move during a session: once seen whole, they're not looked for again
            match &stream.timing {
                Some((seen, ends)) if *seen == key => Some(ends.clone()),
                _ => {
                    let ends = timing_sector_ends(conn, count, length, from)?;
                    if let Some(ends) = &ends {
                        stream.timing = Some((key, ends.clone()));
                    }
                    ends
                }
            }
        }
        _ => None,
    };
    let described = track.get("sector").and_then(Json::as_array).map(Vec::len);
    let source = match (timing, count) {
        (Some(ends), _) => {
            track["sector"] = ends
                .iter()
                .enumerate()
                .map(|(i, end)| serde_json::json!({"name": (i + 1).to_string(), "marker": end}))
                .collect();
            "timing"
        }
        // Another simulator's, or another split of the lap: not the sectors the times are for
        (None, Some(count)) if described.is_some_and(|n| n as i64 != count) => {
            if let Some(object) = track.as_object_mut() {
                object.remove("sector");
            }
            "none"
        }
        _ if described.is_some() => "track data",
        _ => "none",
    };
    track["sector_source"] = source.into();
    track["attribution"] = TRACK_SOURCE.into();
    Ok(serde_json::to_string(&track)?)
}

/// The most recent player rows looked through for where the timing sectors change: minutes of
/// driving, whatever the grid.
const TIMING_ROWS: i64 = 60_000;

/// Where each of the simulator's `count` timing sectors ends, as fractions of the lap: where the
/// player's recent laps (since the row `from`, the session's first) went from one sector to the
/// next (the median of them), the last at the line. `None` until every one of those lines has been
/// crossed, or if they don't come one after another round the lap; and with a single sector, which
/// has no line to see.
fn timing_sector_ends(
    conn: &Connection,
    count: i64,
    length: f64,
    from: i64,
) -> rusqlite::Result<Option<Vec<f64>>> {
    let Some(count) = usize::try_from(count).ok().filter(|count| *count > 1) else {
        return Ok(None);
    };
    // A crossing is a row in the next sector after one of the same car in the sector before, both
    // out of the pit lane; the clock going back is another session, whose crossings say nothing of
    // this one's
    let marks = rows(
        conn,
        "SELECT id, sector, lap_distance, session_time < before_t - 1.0 AS restart FROM (
             SELECT id, sector, lap_distance, in_pit_lane, session_time,
                    LAG(sector) OVER w AS before, LAG(session_time) OVER w AS before_t,
                    LAG(in_pit_lane) OVER w AS before_pit
             FROM (
                 SELECT id, vehicle_id, sector, lap_distance, in_pit_lane, session_time
                 FROM telemetry WHERE is_player = 1 AND id >= ? ORDER BY id DESC LIMIT ?
             )
             WINDOW w AS (PARTITION BY vehicle_id ORDER BY id)
         )
         WHERE (sector = before + 1 AND NOT coalesce(in_pit_lane, 0)
                AND NOT coalesce(before_pit, 0))
            OR session_time < before_t - 1.0
         ORDER BY id",
        params![from, TIMING_ROWS],
    )?;
    let mut crossings = vec![Vec::new(); count.saturating_sub(1)];
    for mark in marks {
        if mark.b("restart") {
            crossings.iter_mut().for_each(Vec::clear);
            continue;
        }
        let (Some(sector), Some(distance)) = (mark.i("sector"), mark.f("lap_distance")) else {
            continue;
        };
        // Into sector k (zero-based) is where sector k - 1 ends
        if let Some(ends) = usize::try_from(sector - 1)
            .ok()
            .and_then(|i| crossings.get_mut(i))
            && (0.0..=length).contains(&distance)
        {
            ends.push(distance);
        }
    }
    let mut ends = Vec::with_capacity(count);
    for mut seen in crossings {
        if seen.is_empty() {
            return Ok(None);
        }
        seen.sort_by(|a, b| a.partial_cmp(b).unwrap_or(Ordering::Equal));
        ends.push(seen[seen.len() / 2] / length);
    }
    ends.push(1.0);
    // Stray crossings (a rewind, say) can outnumber the real ones over a lap or two
    let in_order = ends.first().is_some_and(|first| *first > 0.0)
        && ends.windows(2).all(|pair| pair[0] < pair[1]);
    Ok(in_order.then_some(ends))
}

#[cfg(test)]
mod tests {
    use super::{super::tests::*, *};
    use bono_sim::recorder::DATABASE;

    async fn query(engineer: &Engineer, query: &str) -> Vec<Vec<String>> {
        let out = run(
            engineer,
            serde_json::json!({"command": "sql", "query": query, "limit": 200}),
        )
        .await;
        out.lines()
            .skip(2)
            .filter(|line| !line.starts_with("..."))
            .map(|line| line.split(" | ").map(str::to_owned).collect())
            .collect()
    }

    async fn number(engineer: &Engineer, sql: &str) -> f64 {
        let rows = query(engineer, sql).await;
        rows.first()
            .and_then(|row| row.first())
            .unwrap_or_else(|| panic!("no rows: {sql}"))
            .parse()
            .unwrap_or_else(|_| panic!("not a number: {rows:?} {sql}"))
    }

    #[test]
    fn a_query_can_start_with_comments() {
        assert_eq!(without_leading_comments("SELECT 1"), "SELECT 1");
        assert_eq!(
            without_leading_comments("-- a\n  /* b */\n--c\nWITH x AS (SELECT 1)"),
            "WITH x AS (SELECT 1)"
        );
        assert_eq!(without_leading_comments("-- only"), "");
    }

    #[test]
    fn the_flag_having_fallen_leaves_only_the_lap_under_way() {
        let conn = Connection::open_in_memory().unwrap();
        let session = row(
            &conn,
            "SELECT 1000.0 AS session_end_time, 'race' AS session_type, 1007.0 AS session_time",
            [],
        )
        .unwrap()
        .unwrap();
        let leader = row(
            &conn,
            "SELECT 2 AS vehicle_id, 1 AS position, 100.0 AS last_lap_time",
            [],
        )
        .unwrap()
        .unwrap();
        let me = row(&conn, "SELECT 1 AS vehicle_id, 2 AS position", [])
            .unwrap()
            .unwrap();
        let sc = Scoring {
            session: Some(session),
            yellow_sectors: Vec::new(),
            cars: BTreeMap::from([(1, me.clone()), (2, leader)]),
            player: Some(me),
        };
        let at = |start: f64| {
            let sql = format!("SELECT {start} AS lap_start_time");
            BTreeMap::from([(2, row(&conn, &sql, []).unwrap().unwrap())])
        };
        // The leader crossed the line at 1005, after the clock ran out at 1000
        let (need, _) = race_need(&sc, &at(1005.0), 1, Some(1007.0), 0.92, Some(100.0));
        assert!((need.unwrap() - 0.08).abs() < 1e-9, "{need:?}");
        // Before the clock's end, that lap's end is the flag: one more for us after this one
        let (need, _) = race_need(&sc, &at(960.0), 1, Some(1007.0), 0.92, Some(100.0));
        assert!((need.unwrap() - 1.08).abs() < 1e-9, "{need:?}");
    }

    #[test]
    fn steps_to_matches_stepping_and_stays_bounded() {
        assert_eq!(steps_to(10.0, 5.0, 3.0), 0.0);
        assert_eq!(steps_to(10.0, 5.0, 10.0), 0.0);
        assert_eq!(steps_to(10.0, 5.0, 10.1), 1.0);
        assert_eq!(steps_to(10.0, 5.0, 20.0), 2.0);
        assert_eq!(steps_to(10.0, 5.0, 20.1), 3.0);
        // Used to loop (near) forever
        assert!(steps_to(0.0, 90.0, 1e19).is_finite());
    }

    #[test]
    fn a_path_becomes_a_read_only_uri() {
        assert_eq!(
            file_uri(Path::new("/a b/c#1?.db")),
            "file:///a b/c%231%3f.db?mode=ro"
        );
        assert_eq!(
            file_uri(Path::new(r"D:\Games\100%\telemetry.db")).replace('\\', "/"),
            "file:///D:/Games/100%25/telemetry.db?mode=ro"
        );
    }

    #[test]
    fn sections_partition_the_lap_without_crossing_the_line() {
        let turn = |start: f64, apex: f64, end: f64| Turn {
            label: String::new(),
            direction: None,
            scale: None,
            start,
            apex,
            end,
        };
        let layout = layout(
            vec![
                // Out of order, and the last runs across the line
                turn(4900.0, 4950.0, 100.0),
                // Before the line
                turn(4950.0, 50.0, 150.0),
                turn(900.0, 1000.0, 1100.0),
                // Overlaps the one before it
                turn(1050.0, 1200.0, 1300.0),
            ],
            5000.0,
            TRACK_DATA,
        );
        let corners = &layout.corners;
        assert_eq!(corners[0].boundary, 0.0);
        assert_eq!(corners[0].exit, 150.0);
        // Halfway from the end of one to the start of the next
        assert_eq!(corners[1].boundary, 525.0);
        // Between the end of the one before and the apex
        assert_eq!(corners[2].boundary, 1150.0);
        // Its end is on the next lap: it's over at the line
        assert_eq!(corners[3].next_boundary, 5000.0);
        assert_eq!(corners[3].exit, 5000.0);
        for pair in corners.windows(2) {
            assert_eq!(pair[0].next_boundary, pair[1].boundary);
            assert!(pair[0].apex <= pair[0].next_boundary);
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn lap_stats_come_from_the_recording() {
        let (_dir, engineer) = race();
        let rows = query(
            &engineer,
            "SELECT lap, time_s, complete, green, \
             (SELECT stint_number FROM stints t WHERE t.stint_id = l.stint_id), stint_boundary \
             FROM lap_stats l WHERE vehicle_id = 1 ORDER BY lap",
        )
        .await;
        assert_eq!(
            rows,
            [
                ["0", "100", "1", "1", "1", "0"],
                ["1", "100", "1", "1", "1", "0"],
                ["2", "100", "1", "1", "1", "0"],
                ["3", "NULL", "0", "0", "1", "0"],
            ]
        );
        assert_eq!(
            number(
                &engineer,
                "SELECT official_time_s FROM lap_stats WHERE vehicle_id = 2 AND lap = 0"
            )
            .await,
            99.8
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn wheel_stats_read_the_tyres_inner_to_outer() {
        let (_dir, engineer) = race();
        let rows = query(
            &engineer,
            "SELECT wheel_name, ROUND(inner_avg_c - outer_avg_c), ROUND(optimal_c), ROUND(life_start_pct) \
             FROM lap_wheel_stats WHERE vehicle_id = 1 AND lap = 0 ORDER BY wheel",
        )
        .await;
        assert_eq!(rows.len(), 4);
        // The left edge is the hot one: outside on a left tyre, inside on a right tyre
        assert_eq!(rows[0], ["\"FL\"", "-8", "85", "100"]);
        assert_eq!(rows[3], ["\"RR\"", "8", "85", "100"]);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn corner_passes_add_up_to_the_lap() {
        let (_dir, engineer) = shifted_ring();
        let rows = query(
            &engineer,
            "SELECT COUNT(*), turn_name FROM corner_passes WHERE vehicle_id = 1 AND lap = 1 GROUP BY turn_index ORDER BY turn_index",
        )
        .await;
        assert_eq!(rows.len(), 5, "{rows:?}");
        assert!(rows[0][1].contains("T1"), "{rows:?}");
        let total = number(
            &engineer,
            "SELECT SUM(total_s) FROM corner_passes WHERE vehicle_id = 1 AND lap = 1",
        )
        .await;
        let lap = number(
            &engineer,
            "SELECT time_s FROM lap_stats WHERE vehicle_id = 1 AND lap = 1",
        )
        .await;
        assert!((total - lap).abs() < 0.01, "{total} {lap}");
        // Braking 100 m before each apex, and the apex where the car is slowest
        let (brake, speed, at) = (
            number(
                &engineer,
                "SELECT brake_m_before_apex FROM corner_passes WHERE lap = 1 AND turn_index = 0",
            )
            .await,
            number(
                &engineer,
                "SELECT min_speed_kmh FROM corner_passes WHERE lap = 1 AND turn_index = 0",
            )
            .await,
            number(
                &engineer,
                "SELECT min_speed_at_m FROM corner_passes WHERE lap = 1 AND turn_index = 0",
            )
            .await,
        );
        assert!((brake - 100.0).abs() < 10.0, "{brake}");
        assert!((speed - 72.0).abs() < 3.0, "{speed}");
        assert!((at - 300.0).abs() < 10.0, "{at}");
    }

    fn lap_with(pit: bool, refuelled: bool, wear: (f64, f64)) -> Lap {
        Lap {
            lap: 0,
            first_id: 0,
            last_id: 0,
            start: None,
            end: None,
            time: None,
            complete: true,
            invalid: false,
            pit,
            refuelled,
            contact: false,
            green: !pit && !refuelled,
            impact: None,
            track_limit_steps: None,
            top_speed: None,
            fuel_start: None,
            fuel_end: None,
            fuel_used: None,
            energy_start: None,
            energy_end: None,
            energy_used: None,
            gap_ahead_end: None,
            gap_behind_end: None,
            wear_start: [Some(wear.0); 4],
            wear_end: [Some(wear.1); 4],
        }
    }

    #[test]
    fn representative_laps_prefer_clean_but_keep_invalidated_fallbacks() {
        let mut clean = lap_with(false, false, (1.0, 0.9));
        clean.lap = 1;
        clean.time = Some(105.0);
        let mut invalid = clean.clone();
        invalid.lap = 2;
        invalid.invalid = true;
        invalid.time = Some(100.0);
        let mut pit = clean.clone();
        pit.lap = 3;
        pit.green = false;
        pit.pit = true;
        pit.time = Some(90.0);
        let laps = [invalid.clone(), pit, clean];
        assert_eq!(
            best_laps(&laps)
                .iter()
                .map(|lap| lap.lap)
                .collect::<Vec<_>>(),
            [1, 2]
        );
        assert_eq!(best_lap(&[invalid]).unwrap().lap, 2);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn reference_trace_uses_invalidated_only_when_no_clean_trace_is_usable() {
        let (dir, _engineer) = race();
        let conn = Connection::open(dir.path().join(DATABASE)).unwrap();
        let mut invalid = lap_with(false, false, (1.0, 0.9));
        invalid.lap = 2;
        invalid.start = Some(0.0);
        invalid.time = Some(100.0);
        invalid.first_id = 1;
        invalid.last_id = 400;
        invalid.invalid = true;
        let mut missing = invalid.clone();
        missing.lap = 1;
        missing.invalid = false;
        missing.first_id = 999999;
        missing.last_id = 999999;
        let laps = [missing.clone(), invalid.clone()];
        assert_eq!(
            best_seen_lap(&conn, 1, &laps, 1000.0, true)
                .unwrap()
                .unwrap()
                .0
                .lap,
            2
        );
        assert!(
            best_seen_lap(&conn, 1, &laps, 1000.0, false)
                .unwrap()
                .is_none()
        );

        let mut no_zones = invalid.clone();
        no_zones.invalid = false;
        no_zones.lap = 1;
        no_zones.first_id = 401;
        no_zones.last_id = 800;
        no_zones.start = Some(100.0);
        conn.execute(
            "UPDATE telemetry SET brake = 0 WHERE vehicle_id = 1 AND lap = 1",
            [],
        )
        .unwrap();
        let laps = [no_zones, invalid.clone()];
        assert!(
            best_seen_lap(&conn, 1, &laps, 1000.0, true)
                .unwrap()
                .unwrap()
                .0
                .invalid
        );

        let mut clean = invalid.clone();
        clean.lap = 4;
        clean.invalid = false;
        clean.time = Some(101.0);
        // Exhaust usable clean candidates before falling back, rather than stopping after
        // the first few clean laps have missing recordings.
        let laps = [missing.clone(), missing.clone(), missing, invalid, clean];
        assert_eq!(
            best_seen_lap(&conn, 1, &laps, 1000.0, true)
                .unwrap()
                .unwrap()
                .0
                .lap,
            4
        );
    }

    #[test]
    fn the_improvement_query_prefers_clean_and_exposes_an_invalidated_fallback() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(TABLES).unwrap();
        conn.execute_batch(
            "INSERT INTO corner_passes (vehicle_id, lap, turn_index, turn_name, total_s, \
             lap_invalid, in_pit, contact, low_precision, brake_m_before_apex, min_speed_kmh, exit_speed_kmh) VALUES \
             (1, 0, 0, 'T1', 9.0, 0, 0, 0, 0, 100, 80, 100), \
             (1, 1, 0, 'T1', 10.5, 0, 0, 0, 0, 100, 80, 100), \
             (1, 2, 0, 'T1', 10.0, 1, 0, 0, 0, 100, 80, 100), \
             (1, 2, 10, 'T11', 0.5, 0, 0, 0, 0, 100, 80, 100);",
        )
        .unwrap();
        let sql = SKILL
            .split_once("```sql")
            .unwrap()
            .1
            .split_once("```")
            .unwrap()
            .0
            .split(';')
            .nth(2)
            .unwrap()
            .replace(":P", "1")
            .replace(":TURN_INDEX", "0")
            .replace(":FROM_LAP", "1")
            .replace(":TO_LAP", "2");
        let selected = |observed: i64| {
            let sql = sql.replace(":OBSERVED_LAP", &observed.to_string());
            conn.query_row(&sql, [], |row| {
                Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(6)?))
            })
            .unwrap()
        };
        assert_eq!(selected(3), (1, 0));
        conn.execute("UPDATE corner_passes SET total_s = NULL WHERE lap = 1", [])
            .unwrap();
        // An older clean pass outside the requested window must not suppress the fallback.
        assert_eq!(selected(3), (2, 1));
        conn.execute(
            "UPDATE corner_passes SET total_s = 10.5, brake_m_before_apex = NULL WHERE lap = 1",
            [],
        )
        .unwrap();
        assert_eq!(selected(3), (2, 1));
        conn.execute(
            "UPDATE corner_passes SET brake_m_before_apex = 100 WHERE lap = 1",
            [],
        )
        .unwrap();
        assert_eq!(selected(1), (2, 1));
        // The same exact index also works for inferred braking-zone names.
        conn.execute(
            "UPDATE corner_passes SET turn_name = 'braking zone 1' WHERE turn_index = 0",
            [],
        )
        .unwrap();
        assert_eq!(selected(1), (2, 1));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn clean_evidence_replaces_an_invalidated_braking_zone_layout_once() {
        let (dir, original) = race();
        drop(original);
        let conn = Connection::open(dir.path().join(DATABASE)).unwrap();
        conn.execute_batch(TABLES).unwrap();
        conn.execute("UPDATE telemetry SET brake = CASE WHEN lap_distance BETWEEN 100 AND 145 THEN 1 ELSE 0 END WHERE vehicle_id = 1 AND lap = 0", []).unwrap();
        let mut invalid = lap_with(false, false, (1.0, 0.9));
        invalid.invalid = true;
        invalid.first_id = 1;
        invalid.last_id = 400;
        invalid.start = Some(0.0);
        invalid.time = Some(100.0);
        let car = |laps| Car {
            finished: laps,
            ..Car::default()
        };
        let mut second_invalid = invalid.clone();
        second_invalid.lap = 1;
        second_invalid.first_id = 401;
        second_invalid.last_id = 800;
        second_invalid.start = Some(100.0);
        let mut stream = Stream {
            player: Some(1),
            cars: BTreeMap::from([(1, car(vec![invalid.clone(), second_invalid.clone()]))]),
            ..Stream::default()
        };
        stream.find_braking_zones(&conn).unwrap();
        assert!(stream.zones_from_invalid);
        let fallback = stream.layout.as_ref().unwrap().corners[0].start;
        // Replaying the same invalid evidence doesn't regenerate/rewind the layout.
        stream.cars.insert(1, car(vec![invalid.clone()]));
        stream.find_braking_zones(&conn).unwrap();
        assert_eq!(stream.cars[&1].finished.len(), 1);
        let mut clean = invalid.clone();
        clean.invalid = false;
        clean.lap = 2;
        clean.first_id = 801;
        clean.last_id = 1200;
        clean.start = Some(200.0);
        stream
            .cars
            .insert(1, car(vec![invalid, second_invalid, clean]));
        stream.find_braking_zones(&conn).unwrap();
        assert!(!stream.zones_from_invalid);
        assert!(stream.layout.as_ref().unwrap().corners[0].start > fallback + 100.0);
        assert!(stream.cars.is_empty());
        // A clean-sourced layout stays stable after the rebuild.
        stream.find_braking_zones(&conn).unwrap();
        assert!(!stream.zones_from_invalid);
    }

    #[test]
    fn informational_comparisons_do_not_compare_a_fast_invalid_lap_with_itself() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(TABLES).unwrap();
        conn.execute_batch("INSERT INTO lap_stats (vehicle_id, lap, complete, green, invalid, time_s) VALUES (1, 1, 1, 1, 0, 105), (1, 2, 1, 1, 1, 100);
            INSERT INTO corner_passes (vehicle_id, lap, turn_index, turn_name, total_s, entry_s, lap_invalid) VALUES (1, 1, 0, 'T1', 10.5, 5.5, 0), (1, 2, 0, 'T1', 10, 5, 1);").unwrap();
        let sql = SKILL
            .split_once("```sql")
            .unwrap()
            .1
            .split_once("```")
            .unwrap()
            .0
            .split(';')
            .next()
            .unwrap()
            .replace(":P", "1");
        let (lost, observed, reference) = conn
            .query_row(&sql, [], |r| {
                Ok((
                    r.get::<_, f64>(1)?,
                    r.get::<_, i64>(5)?,
                    r.get::<_, i64>(6)?,
                ))
            })
            .unwrap();
        assert_eq!((lost, observed, reference), (-0.5, 1, 0));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn invalidated_laps_still_supply_corners_without_a_track_description() {
        let (dir, original) = race();
        drop(original);
        let conn = Connection::open(dir.path().join(DATABASE)).unwrap();
        conn.execute("UPDATE telemetry SET lap_invalidated = 1", [])
            .unwrap();
        let engineer = reopen(&dir);
        let passes = query(
            &engineer,
            "SELECT lap, lap_invalid FROM corner_passes WHERE vehicle_id = 1 ORDER BY lap",
        )
        .await;
        assert_eq!(passes, [["0", "1"], ["1", "1"], ["2", "1"]]);
    }

    #[test]
    fn invalidation_updates_corners_already_written_on_the_current_lap() {
        let conn = Connection::open_in_memory().unwrap();
        bono_sim::sql::create_tables(&conn).unwrap();
        conn.execute_batch(TABLES).unwrap();
        let mut stream = Stream {
            player: Some(1),
            layout: Some(layout(
                vec![Turn {
                    label: "T1".into(),
                    direction: None,
                    scale: None,
                    start: 450.0,
                    apex: 500.0,
                    end: 550.0,
                }],
                2000.0,
                TRACK_DATA,
            )),
            ..Stream::default()
        };
        let samples = partial(1900.0);
        stream.apply_telemetry(&conn, &samples).unwrap();
        let invalid = || scalar::<i64>(&conn, "SELECT lap_invalid FROM corner_passes", []).unwrap();
        assert_eq!(invalid(), Some(0));
        let mut late = *samples.last().unwrap();
        late.id += 1;
        late.session_time = late.session_time.map(|t| t + 0.1);
        late.lap_distance = Some(1905.0);
        late.invalid = true;
        stream.apply_telemetry(&conn, &[late]).unwrap();
        assert_eq!(invalid(), Some(1));
    }

    /// Laps of 100 s from lap 0, each `(pit, refuelled)`.
    fn spans_of(laps: &[(bool, bool)]) -> Vec<LapSpan> {
        laps.iter()
            .zip(0..)
            .map(|((pit, refuelled), lap)| LapSpan {
                lap,
                start: Some(lap as f64 * 100.0),
                end: Some(lap as f64 * 100.0 + 100.0),
                stopped: *pit || *refuelled,
            })
            .collect()
    }

    fn spell(driver: i64, first: f64, last: f64) -> Run {
        Run {
            driver,
            first,
            last,
            standing_id: Some((first * 10.0) as i64),
        }
    }

    fn stints_of(spans: &[LapSpan], runs: &[Run]) -> Vec<Segment> {
        numbered(1, segment(spans, runs), &mut StintIds::default())
    }

    /// Each lap's stint number, 0 across the start of one.
    fn numbers(stints: &[Segment], spans: &[LapSpan]) -> Vec<i64> {
        spans
            .iter()
            .map(|span| match place(stints, span.lap, span.start, span.end) {
                Place::Stint(i) => stints[i].number,
                Place::Across => 0,
                Place::Unknown => -1,
            })
            .collect()
    }

    #[test]
    fn one_stop_is_one_more_stint() {
        let run = |laps: &[(bool, bool)]| {
            let spans = spans_of(laps);
            numbers(&stints_of(&spans, &[]), &spans)
        };
        let (no, pit, refuel) = ((false, false), (true, false), (false, true));
        // The in lap and the out lap both show the pit lane
        assert_eq!(run(&[no, pit, pit, no, no]), [1, 1, 2, 2, 2]);
        // A pit lane that doesn't cross the line
        assert_eq!(run(&[no, pit, no, no]), [1, 1, 2, 2]);
        // Leaving the garage isn't a stop
        assert_eq!(run(&[pit, no, no]), [1, 1, 1]);
        assert_eq!(run(&[no, refuel, no]), [1, 1, 2]);
        // The in lap still under way: no stop yet
        assert_eq!(run(&[no, pit]), [1, 1]);
    }

    #[test]
    fn a_driver_change_in_a_stop_is_the_stops_stint() {
        let (no, pit) = ((false, false), (true, false));
        let spans = spans_of(&[no, no, pit, pit, no]);
        // Last seen at the line, the next driver ten seconds into the out lap
        let stints = stints_of(&spans, &[spell(1, 0.0, 300.0), spell(2, 310.0, 450.0)]);
        assert_eq!(stints.len(), 2);
        assert_eq!(stints[1].start.reason, "pit stop and driver change");
        assert_eq!(stints[1].start.lap, Some(3));
        assert_eq!(
            (stints[1].start.from, stints[1].start.to),
            (Some(300.0), Some(310.0))
        );
        assert_eq!(stints[1].start.standing_id, Some(3100));
        assert_eq!((stints[0].driver, stints[1].driver), (Some(1), Some(2)));
        // The out lap had both of them: it's no one stint's
        assert_eq!(numbers(&stints, &spans), [1, 1, 1, 0, 2]);
        // So are its corners up to the handover, but not those after it
        assert_eq!(place(&stints, 3, Some(300.0), Some(305.0)), Place::Across);
        assert_eq!(place(&stints, 3, Some(320.0), Some(330.0)), Place::Stint(1));
        assert_eq!(place(&stints, 2, Some(250.0), Some(260.0)), Place::Stint(0));
        // When it was driven isn't known
        assert_eq!(place(&stints, 4, None, None), Place::Unknown);
    }

    #[test]
    fn a_returning_driver_starts_a_new_stint_as_themselves() {
        let (no, pit) = ((false, false), (true, false));
        let spans = spans_of(&[no, pit, pit, no, pit, pit, no]);
        // Each last seen at the line, the next driver a little into the out lap
        let runs = [
            spell(1, 0.0, 200.0),
            spell(2, 205.0, 500.0),
            spell(1, 505.0, 690.0),
        ];
        let stints = stints_of(&spans, &runs);
        let drivers = stints.iter().map(|stint| stint.driver).collect::<Vec<_>>();
        assert_eq!(drivers, [Some(1), Some(2), Some(1)]);
        assert_eq!(numbers(&stints, &spans), [1, 1, 0, 2, 2, 0, 3]);
    }

    #[test]
    fn a_driver_change_away_from_a_stop_is_a_stint_of_its_own() {
        let no = (false, false);
        let spans = spans_of(&[no, no, no]);
        let stints = stints_of(&spans, &[spell(1, 0.0, 150.0), spell(2, 160.0, 290.0)]);
        assert_eq!(stints[1].start.reason, "driver change");
        assert_eq!(numbers(&stints, &spans), [1, 0, 2]);
        // Its drivers alone, for a car without laps
        let stints = stints_of(&[], &[spell(1, 0.0, 150.0), spell(2, 160.0, 290.0)]);
        assert_eq!(stints.len(), 2);
        assert_eq!(stints[1].driver, Some(2));
    }

    #[test]
    fn another_car_in_the_slot_leaves_its_drivers_behind() {
        let mut drivers = Drivers::default();
        // The last car's driver, still named a little after its last telemetry
        for t in [0.0, 450.0, 900.0, 900.2] {
            drivers.seen(2, "Ana Lopes", t, t as i64);
        }
        // The new car's telemetry shows up at 910 s, its driver named a little before
        drivers.seen(2, "Bea Costa", 905.0, 905);
        drivers.seen(2, "Bea Costa", 950.0, 950);
        drivers.replaced(2, 910.0);
        // The last car's standings read late
        assert!(!drivers.seen(2, "Ana Lopes", 900.5, 901));
        let runs = drivers.runs(2).to_vec();
        assert_eq!(
            runs,
            [Run {
                driver: 2,
                first: 910.0,
                last: 950.0,
                standing_id: None
            }]
        );
        let stints = stints_of(&[], &runs);
        assert_eq!(stints.len(), 1);
        assert_eq!(stints[0].driver, Some(2));
        // The same name in the new car, named before its telemetry shows up
        drivers.seen(3, "Ana Lopes", 0.0, 1);
        drivers.seen(3, "Ana Lopes", 950.0, 950);
        drivers.replaced(3, 910.0);
        let runs = drivers.runs(3);
        assert_eq!(runs.len(), 1);
        assert_eq!((runs[0].first, runs[0].last), (910.0, 950.0));
        // The last car's driver is gone, and someone new if the name's seen again
        let gone = |drivers: &Drivers, name: &str| {
            drivers
                .known
                .iter()
                .filter(|driver| driver.vehicle == 2 && driver.name == name)
                .map(|driver| driver.gone)
                .collect::<Vec<_>>()
        };
        assert_eq!(gone(&drivers, "Ana Lopes"), [true]);
        assert_eq!(gone(&drivers, "Bea Costa"), [false]);
        drivers.seen(2, "Ana Lopes", 990.0, 990);
        assert_eq!(gone(&drivers, "Ana Lopes"), [true, false]);
    }

    #[test]
    fn a_short_stint_borrows_its_drivers_laps_first() {
        let (no, pit) = ((false, false), (true, false));
        let flags = [no, no, no, pit, pit, no, pit, pit, no];
        let spans = spans_of(&flags);
        let laps = flags
            .iter()
            .zip(&spans)
            .map(|((pit, refuelled), span)| Lap {
                lap: span.lap,
                start: span.start,
                end: span.end,
                time: Some(100.0 + span.lap as f64),
                ..lap_with(*pit, *refuelled, (1.0, 1.0))
            })
            .collect::<Vec<_>>();
        let runs = [
            spell(1, 0.0, 395.0),
            spell(2, 405.0, 695.0),
            spell(1, 705.0, 890.0),
        ];
        let stints = stints_of(&spans, &runs);
        assert_eq!(stints.len(), 3);
        // One green lap of their own, none from before: anyone's
        let second = Stint::new(&laps, &places(&laps, &stints), &stints, 1);
        assert_eq!(second.baseline_source, Some("any driver"));
        // One green lap of their own, and this driver's from before
        let third = Stint::new(&laps, &places(&laps, &stints), &stints, 2);
        assert_eq!(third.baseline_source, Some("this driver"));
        let used = third.baseline.iter().map(|lap| lap.lap).collect::<Vec<_>>();
        assert_eq!(used, [0, 1, 2, 8]);
        assert_eq!(third.own.iter().map(|lap| lap.lap).collect::<Vec<_>>(), [8]);
        // Enough of its own
        assert_eq!(
            Stint::new(&laps, &places(&laps, &stints), &stints, 0).baseline_source,
            Some("this stint")
        );
    }

    /// Five laps of a 1000 m track at 10 m/s, a stop between laps 2 and 3, and one driver handing
    /// over to another during it, as the standings saw it.
    fn relay(standings_first: bool) -> (Connection, Stream) {
        let conn = Connection::open_in_memory().unwrap();
        bono_sim::sql::create_tables(&conn).unwrap();
        conn.execute_batch(TABLES).unwrap();
        let mut stream = Stream {
            player: Some(1),
            layout: Some(layout(
                vec![Turn {
                    label: "T1".into(),
                    direction: None,
                    scale: None,
                    start: 450.0,
                    apex: 500.0,
                    end: 550.0,
                }],
                1000.0,
                TRACK_DATA,
            )),
            ..Stream::default()
        };
        let samples = (0..=460)
            .map(|i| {
                let t = i as f64;
                let lap = i / 100;
                let d = t * 10.0 - lap as f64 * 1000.0;
                Sample {
                    id: i + 1,
                    vehicle: 1,
                    is_player: true,
                    session_time: Some(t),
                    lap: Some(lap),
                    lap_start: Some(lap as f64 * 100.0),
                    lap_distance: Some(d),
                    in_pit: (lap == 2 && d >= 900.0) || (lap == 3 && d < 100.0),
                    invalid: false,
                    speed: Some(10.0),
                    brake: Some(f64::from((400.0..450.0).contains(&d))),
                    throttle: Some(f64::from(!(400.0..500.0).contains(&d))),
                    gear: Some(3),
                    fuel: Some(50.0 - t * 0.02),
                    energy: None,
                    gap_ahead: None,
                    gap_behind: None,
                }
            })
            .collect::<Vec<_>>();
        let standings = |conn: &Connection| {
            for (t, driver) in [
                (0.0, "Ana Lopes"),
                (100.0, "Ana Lopes"),
                (200.0, "Ana Lopes"),
                (300.0, "Ana Lopes"),
                (310.0, "Bea Costa"),
                (400.0, "Bea Costa"),
            ] {
                conn.execute(
                    "INSERT INTO standings (session_time, vehicle_id, is_player, driver) \
                     VALUES (?, 1, 1, ?)",
                    params![t, driver],
                )
                .unwrap();
            }
        };
        if standings_first {
            standings(&conn);
            stream.read_standings(&conn).unwrap();
            stream.apply_telemetry(&conn, &samples).unwrap();
        } else {
            stream.apply_telemetry(&conn, &samples).unwrap();
            standings(&conn);
            stream.read_standings(&conn).unwrap();
        }
        (conn, stream)
    }

    #[test]
    fn a_handover_in_the_pits_splits_laps_and_corners_by_stint() {
        for standings_first in [true, false] {
            let (conn, stream) = relay(standings_first);
            let car = &stream.cars[&1];
            let reasons = car
                .stints
                .iter()
                .map(|stint| (stint.start.reason, stint.driver))
                .collect::<Vec<_>>();
            assert_eq!(
                reasons,
                [
                    ("first seen", Some(1)),
                    ("pit stop and driver change", Some(2))
                ]
            );
            let names = stream
                .drivers
                .known
                .iter()
                .map(|driver| driver.name.as_str())
                .collect::<Vec<_>>();
            assert_eq!(names, ["Ana Lopes", "Bea Costa"]);
            let (first, second) = (car.stints[0].id, car.stints[1].id);
            let laps = rows(
                &conn,
                "SELECT lap, stint_id, stint_boundary FROM lap_stats ORDER BY lap",
                [],
            )
            .unwrap()
            .iter()
            .map(|r| (r.i("lap"), r.i("stint_id"), r.i("stint_boundary")))
            .collect::<Vec<_>>();
            assert_eq!(
                laps,
                [
                    (Some(0), Some(first), Some(0)),
                    (Some(1), Some(first), Some(0)),
                    (Some(2), Some(first), Some(0)),
                    // The out lap: one driver, then the other
                    (Some(3), None, Some(1)),
                ],
                "standings first: {standings_first}"
            );
            let passes = rows(
                &conn,
                "SELECT lap, stint_id, stint_boundary FROM corner_passes ORDER BY lap",
                [],
            )
            .unwrap()
            .iter()
            .map(|r| (r.i("lap"), r.i("stint_id"), r.i("stint_boundary")))
            .collect::<Vec<_>>();
            assert_eq!(
                passes,
                [
                    (Some(0), Some(first), Some(0)),
                    (Some(1), Some(first), Some(0)),
                    (Some(2), Some(first), Some(0)),
                    // Its section starts at the line, before the handover
                    (Some(3), None, Some(1)),
                    (Some(4), Some(second), Some(0)),
                ],
                "standings first: {standings_first}"
            );
        }
    }

    #[test]
    fn new_tyres_say_nothing_about_what_wear_means() {
        // Worn tyres replaced on the first lap with any change
        let laps = [
            lap_with(true, false, (0.2, 1.0)),
            lap_with(false, false, (1.0, 0.98)),
        ];
        assert!(wear_remaining(&laps));
        // Wear that really counts up
        assert!(!wear_remaining(&[lap_with(false, false, (0.0, 0.02))]));
        assert!(wear_remaining(&[lap_with(true, false, (0.2, 1.0))]));
    }

    #[test]
    fn a_new_session_starts_after_the_old_one_however_short() {
        let conn = Connection::open_in_memory().unwrap();
        bono_sim::sql::create_tables(&conn).unwrap();
        for (id, time) in [(1, 0.0), (2, 0.4), (3, 1.0), (4, 1.6), (5, 0.0), (6, 0.1)] {
            conn.execute(
                "INSERT INTO telemetry (id, vehicle_id, is_player, session_time) VALUES (?, 1, 1, ?)",
                params![id, time],
            )
            .unwrap();
        }
        assert_eq!(frame_start(&conn, 5, 0.0).unwrap(), 5);
        // Everything before it is of the same frame: the rows of other cars of the new session
        conn.execute("DELETE FROM telemetry WHERE id < 4", [])
            .unwrap();
        conn.execute(
            "INSERT INTO telemetry (id, vehicle_id, is_player, session_time) VALUES (10, 2, 0, 0.2), (11, 1, 1, 0.2)",
            [],
        )
        .unwrap();
        assert_eq!(frame_start(&conn, 11, 0.2).unwrap(), 5);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_short_session_doesnt_stop_the_tables() {
        let (dir, engineer) = race();
        let conn = Connection::open(dir.path().join(DATABASE)).unwrap();
        // A session of just over a second, then another
        for t in [0.0, 0.5, 1.6, 0.0, 0.5] {
            conn.execute(
                "INSERT INTO telemetry (vehicle_id, is_player, session_time, lap, lap_start_time) VALUES (1, 1, ?, 0, 0)",
                [t],
            )
            .unwrap();
        }
        tokio::time::timeout(Duration::from_secs(20), engineer.sync())
            .await
            .expect("the tables never caught up")
            .unwrap();
        let top: i64 = conn
            .query_row("SELECT MAX(id) FROM telemetry", [], |r| r.get(0))
            .unwrap();
        assert_eq!(
            number(&engineer, "SELECT session_first_id FROM race_state").await,
            (top - 1) as f64
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_lap_number_going_back_is_a_late_row_or_another_car() {
        let (dir, engineer) = race();
        let count = "SELECT COUNT(*) FROM lap_stats WHERE vehicle_id = 2";
        assert_eq!(number(&engineer, count).await, 4.0);
        let conn = Connection::open(dir.path().join(DATABASE)).unwrap();
        // A late row of the lap just ended changes nothing
        conn.execute(
            "INSERT INTO telemetry (vehicle_id, is_player, session_time, lap, lap_start_time, lap_distance) VALUES (2, 0, 311, 2, 200, 999)",
            [],
        )
        .unwrap();
        assert_eq!(number(&engineer, count).await, 4.0);
        // Someone else in the slot, on their first laps: none of the last car's is theirs
        conn.execute(
            "INSERT INTO telemetry (vehicle_id, is_player, session_time, lap, lap_start_time, lap_distance) VALUES (2, 0, 311.5, 1, 290, 100)",
            [],
        )
        .unwrap();
        assert_eq!(number(&engineer, count).await, 1.0);
        assert_eq!(
            number(
                &engineer,
                "SELECT COUNT(*) FROM corner_passes WHERE vehicle_id = 2"
            )
            .await,
            0.0
        );
        // The other car's untouched
        assert_eq!(
            number(
                &engineer,
                "SELECT COUNT(*) FROM lap_stats WHERE vehicle_id = 1"
            )
            .await,
            4.0
        );
    }

    #[test]
    fn large_numbers_keep_their_precision() {
        assert_eq!(real_text(3605.2), "3605.2");
        assert_eq!(real_text(12345.678), "12345.678");
        assert_eq!(real_text(1000.0), "1000");
        assert_eq!(real_text(99.8), "99.8");
        // Lap times keep their milliseconds, whatever the track
        assert_eq!(real_text(101.234), "101.234");
        assert_eq!(real_text(95.123), "95.123");
        assert_eq!(real_text(2.977), "2.977");
        assert_eq!(real_text(-2.5), "-2.5");
    }

    #[test]
    fn a_first_distance_is_believed_unless_its_the_old_laps() {
        assert!(believable(0.0, 10.0, 2000.0));
        // Joined midway
        assert!(believable(0.0, 1200.0, 2000.0));
        // The old lap's, or waiting just short of the line
        assert!(!believable(0.0, 1990.0, 2000.0));
        assert!(believable(1200.0, 1300.0, 2000.0));
        assert!(!believable(100.0, 1200.0, 2000.0));
    }

    fn build_of(lap: i64, samples: &[Sample], start: f64) -> LapBuild {
        let mut build = LapBuild::new(&samples[0], lap, None);
        for sample in samples {
            build.add(sample, None);
        }
        build.start = Some(start);
        build
    }

    #[test]
    fn a_lap_is_only_green_with_all_of_it_and_the_next_one_seen() {
        let conn = Connection::open_in_memory().unwrap();
        bono_sim::sql::create_tables(&conn).unwrap();
        let whole = partial(2000.0);
        let lap = |number: i64, next: i64, samples: &[Sample]| {
            let next = build_of(next, &whole, 100.0);
            build_of(number, samples, 60.0)
                .lap(&conn, 1, Some(&next), None)
                .unwrap()
        };
        let clean = lap(1, 2, &whole);
        assert!(clean.green);
        assert_eq!(clean.time, Some(40.0));
        assert_eq!(clean.fuel_used, Some(0.0));

        // Laps missing after it: nothing says how long this one took or what it used
        let skipped = lap(1, 3, &whole);
        assert!(skipped.complete && !skipped.green);
        assert_eq!((skipped.time, skipped.fuel_used), (None, None));

        // Joined in the last stretch of the lap, with or without a distance that wraps early
        let last = whole
            .iter()
            .filter(|s| s.lap_distance.unwrap() >= 1900.0)
            .copied()
            .collect::<Vec<_>>();
        assert!(!lap(1, 2, &last).green);
        let mut last_wrapped = last.clone();
        let mut row = *last_wrapped.last().unwrap();
        row.lap_distance = Some(1.0);
        last_wrapped.push(row);
        assert!(!lap(1, 2, &last_wrapped).green);

        // Joined midway
        let joined = whole
            .iter()
            .filter(|s| s.lap_distance.unwrap() >= 1200.0)
            .copied()
            .collect::<Vec<_>>();
        assert!(!lap(1, 2, &joined).green);
        // Nor does a distance wrapping before the lap number make it a whole lap
        let mut wrapped = joined.clone();
        let mut row = *wrapped.last().unwrap();
        row.lap_distance = Some(1.0);
        wrapped.push(row);
        assert!(!lap(1, 2, &wrapped).green);
        // And rows of the old lap's distance don't stop one being one
        let mut stale = whole.clone();
        stale[0].lap_distance = Some(1999.0);
        assert!(lap(1, 2, &stale).green);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_query_is_told_when_the_tables_are_behind() {
        let (_dir, engineer) = race();
        let out = run(
            &engineer,
            serde_json::json!({"command": "sql", "query": "SELECT 1"}),
        )
        .await;
        assert!(!out.contains("still reading"), "{out}");
    }

    #[test]
    fn a_trace_only_trusts_what_the_car_was_seen_doing() {
        let whole = partial(2000.0);
        // Seen all the way round
        assert!(Trace::build(&whole, Some(0.0), Some(40.0), 2000.0).ok);
        // Joined 700 m in: the lap's end doesn't make it a whole lap
        let joined = whole
            .iter()
            .filter(|s| s.lap_distance.unwrap() >= 700.0)
            .copied()
            .collect::<Vec<_>>();
        let trace = Trace::build(&joined, Some(0.0), Some(40.0), 2000.0);
        assert!(!trace.ok);
        assert!(trace.seen(0.0, 600.0).is_empty());
        assert_eq!(trace.seen(700.0, 800.0).len(), 21);
        // Past halfway too: the corners it did get to count
        let late = whole
            .iter()
            .filter(|s| s.lap_distance.unwrap() >= 1200.0)
            .copied()
            .collect::<Vec<_>>();
        let trace = Trace::build(&late, Some(0.0), Some(40.0), 2000.0);
        assert!(!trace.ok);
        assert_eq!(trace.seen(1200.0, 1300.0).len(), 21);
        // A gap in the recording
        let gap = whole
            .iter()
            .filter(|s| !(800.0..1100.0).contains(&s.lap_distance.unwrap()))
            .copied()
            .collect::<Vec<_>>();
        assert!(!Trace::build(&gap, Some(0.0), Some(40.0), 2000.0).ok);

        // Waiting on the grid just short of the line, then driving off
        let mut grid = whole.clone();
        for sample in grid.iter_mut().take(15) {
            sample.lap_distance = Some(1990.0);
        }
        assert!(Trace::build(&grid, Some(0.0), Some(40.0), 2000.0).ok);

        // The distance reaching the line early doesn't end the lap early
        let trace = Trace::build(&whole, Some(0.0), Some(40.0), 2000.0);
        assert_eq!(trace.time_at(2000.0), 40.0);
        let mut early = whole.clone();
        early.last_mut().unwrap().session_time = Some(33.0);
        assert_eq!(
            Trace::build(&early, Some(0.0), Some(40.0), 2000.0).time_at(2000.0),
            40.0
        );
    }

    #[test]
    fn a_corner_the_car_wasnt_seen_in_gets_no_pass() {
        let conn = Connection::open_in_memory().unwrap();
        bono_sim::sql::create_tables(&conn).unwrap();
        conn.execute_batch(TABLES).unwrap();
        let turn = |apex: f64| Turn {
            label: format!("T{apex}"),
            direction: None,
            scale: None,
            start: apex - 50.0,
            apex,
            end: apex + 50.0,
        };
        let layout = layout(vec![turn(500.0), turn(1500.0)], 2000.0, TRACK_DATA);
        let official = HashMap::new();
        let writer = Writer {
            conn: &conn,
            layout: Some(&layout),
            official: &official,
            player: Some(1),
        };
        let joined = partial(2000.0)
            .into_iter()
            .filter(|s| s.lap_distance.unwrap() >= 900.0)
            .collect::<Vec<_>>();
        let mut build = LapBuild::new(&joined[0], 0, Some(&layout));
        for sample in &joined {
            build.add(sample, Some(2000.0));
        }
        build.start = Some(0.0);
        let mut next = LapBuild::new(&joined[0], 1, Some(&layout));
        next.start = Some(33.0);
        writer.finish(1, &mut Car::default(), build, &next).unwrap();
        let rows = conn
            .prepare("SELECT turn_index, low_precision FROM corner_passes")
            .unwrap()
            .query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?)))
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap();
        assert_eq!(rows, [(1, 1)]);
    }

    #[test]
    fn joining_midway_through_a_corners_section_is_low_precision_at_once() {
        let conn = Connection::open_in_memory().unwrap();
        bono_sim::sql::create_tables(&conn).unwrap();
        conn.execute_batch(TABLES).unwrap();
        let layout = layout(
            vec![Turn {
                label: "T1".into(),
                direction: None,
                scale: None,
                start: 1400.0,
                apex: 1500.0,
                end: 1600.0,
            }],
            2000.0,
            TRACK_DATA,
        );
        let official = HashMap::new();
        let writer = Writer {
            conn: &conn,
            layout: Some(&layout),
            official: &official,
            player: Some(1),
        };
        let joined = partial(1650.0)
            .into_iter()
            .filter(|s| s.lap_distance.unwrap() >= 1200.0)
            .collect::<Vec<_>>();
        let mut build = LapBuild::new(&joined[0], 0, Some(&layout));
        for sample in &joined {
            build.add(sample, Some(2000.0));
            writer.sample_added(1, &mut build, &[]).unwrap();
        }
        let low: i64 = conn
            .query_row("SELECT low_precision FROM corner_passes", [], |r| r.get(0))
            .unwrap();
        assert_eq!(low, 1);
    }

    #[test]
    fn range_runs_out_at_zero_only_when_the_use_is_known() {
        assert_eq!(
            range(Some(0.0), Some(0.05), Some(3), 0.5),
            (Some(0.0), Some(3.5))
        );
        assert_eq!(range(Some(0.0), None, Some(3), 0.5), (None, None));
    }

    #[test]
    fn a_pit_service_list_says_when_nothing_is_selected() {
        let conn = Connection::open_in_memory().unwrap();
        let services = |sql: &str| pit_services(&row(&conn, sql, []).unwrap().unwrap());
        assert_eq!(services("SELECT 1 AS other"), (None, None));
        assert_eq!(
            services("SELECT 0 AS pit_service_refuel_selected"),
            (Some("none".to_owned()), None)
        );
        assert_eq!(
            services(
                "SELECT 1 AS pit_service_refuel_selected, 1 AS pit_service_front_tires_active"
            ),
            (Some("refuel".to_owned()), Some("front tires".to_owned()))
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_query_is_told_when_the_current_state_tables_are_empty() {
        let (dir, engineer) = race();
        let conn = Connection::open(dir.path().join(DATABASE)).unwrap();
        conn.execute("UPDATE telemetry SET is_player = 0", [])
            .unwrap();
        let out = run(
            &engineer,
            serde_json::json!({"command": "sql", "query": "SELECT COUNT(*) AS n FROM telemetry"}),
        )
        .await;
        assert!(
            out.contains("-- the current-state tables are incomplete: no rows flagged"),
            "{out}"
        );
        assert!(out.contains("\"n\"\n1242"), "{out}");
    }

    /// A lap of the ring as far as `distance`, for a car at 60 m/s.
    fn partial(distance: f64) -> Vec<Sample> {
        (0..)
            .map(|i| i as f64 * 5.0)
            .take_while(|d| *d <= distance)
            .enumerate()
            .map(|(i, d)| Sample {
                id: i as i64 + 1,
                vehicle: 1,
                is_player: true,
                session_time: Some(d / 60.0),
                lap: Some(0),
                lap_start: Some(0.0),
                lap_distance: Some(d),
                in_pit: false,
                invalid: false,
                speed: Some(60.0),
                brake: Some(0.0),
                throttle: Some(1.0),
                gear: Some(5),
                fuel: Some(50.0),
                energy: Some(0.5),
                gap_ahead: None,
                gap_behind: None,
            })
            .collect()
    }

    #[test]
    fn a_pass_appears_at_the_corners_exit_and_fills_in_later() {
        let conn = Connection::open_in_memory().unwrap();
        bono_sim::sql::create_tables(&conn).unwrap();
        conn.execute_batch(TABLES).unwrap();
        let turn = |apex: f64| Turn {
            label: format!("T{apex}"),
            direction: None,
            scale: None,
            start: apex - 50.0,
            apex,
            end: apex + 50.0,
        };
        let layout = layout(vec![turn(500.0), turn(1500.0)], 2000.0, TRACK_DATA);
        let official = HashMap::new();
        let writer = Writer {
            conn: &conn,
            layout: Some(&layout),
            official: &official,
            player: Some(1),
        };
        let samples = partial(1990.0);
        let mut build = LapBuild::new(&samples[0], 0, Some(&layout));
        let pass = |conn: &Connection, column: &str| -> Option<Option<f64>> {
            let sql = format!("SELECT {column} FROM corner_passes WHERE turn_index = 0");
            conn.query_row(&sql, [], |r| r.get(0)).ok()
        };
        for sample in samples {
            build.add(&sample, Some(2000.0));
            writer.sample_added(1, &mut build, &[]).unwrap();
            let d = sample.lap_distance.unwrap();
            let row = pass(&conn, "entry_s");
            // The boundary with the next turn is at 1000 m, and the throttle's search ends 300 m
            // past the exit at 550 m
            assert_eq!(row.is_some(), d >= 550.0, "{d}");
            if d >= 550.0 {
                assert!(row.unwrap().is_some());
                let straight = pass(&conn, "straight_s").unwrap();
                assert_eq!(straight.is_some(), d >= 1000.0, "{d}");
                assert_eq!(
                    pass(&conn, "total_s").unwrap().is_some(),
                    d >= 1000.0,
                    "{d}"
                );
            }
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn current_lap_passes_are_there_before_the_lap_ends() {
        let (_dir, engineer) = race();
        // Lap 3 is under way at 100 m of a 1000 m track, before its one braking zone
        let rows = query(
            &engineer,
            "SELECT lap, turn_name, total_s IS NOT NULL FROM corner_passes WHERE vehicle_id = 1 ORDER BY lap",
        )
        .await;
        assert_eq!(rows.len(), 3, "{rows:?}");
        assert!(rows.iter().all(|row| row[1].contains("braking zone 1")));
        // Without a description the corners were found on the first clean lap, then read again
        assert_eq!(rows[0][0], "0");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn heat_shows_in_the_corner_it_builds_in() {
        let (_dir, engineer) = race();
        let fl = query(
            &engineer,
            "SELECT surface_rise_c, surface_rise_braking_c, brake_rise_braking_c \
             FROM corner_pass_wheels WHERE vehicle_id = 1 AND lap = 1 AND wheel = 0",
        )
        .await;
        assert_eq!(fl.len(), 1, "{fl:?}");
        let values = fl[0]
            .iter()
            .map(|v| v.parse::<f64>().unwrap())
            .collect::<Vec<_>>();
        assert!(values[0] >= 9.0, "{values:?}");
        // Most of it under braking
        assert!(values[1] / values[0] > 0.85, "{values:?}");
        assert!(values[2] >= 180.0, "{values:?}");
        // Only the player's car
        assert_eq!(
            number(
                &engineer,
                "SELECT COUNT(*) FROM corner_pass_wheels WHERE vehicle_id = 2"
            )
            .await,
            0.0
        );
        // The rear right never warms mid-lap
        assert_eq!(
            number(
                &engineer,
                "SELECT brake_rise_c FROM corner_pass_wheels WHERE lap = 1 AND wheel = 3"
            )
            .await,
            0.0
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn rivals_are_marked_as_less_precise() {
        let (_dir, engineer) = race();
        for (vehicle, expected) in [(1, 0.0), (2, 1.0)] {
            let sql = format!(
                "SELECT low_precision FROM corner_passes WHERE vehicle_id = {vehicle} AND lap = 1"
            );
            assert_eq!(number(&engineer, &sql).await, expected);
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn now_tables_describe_this_moment() {
        let (_dir, engineer) = race();
        let state = query(
            &engineer,
            "SELECT session_type, player_vehicle_id, session_first_id, track_length_m, derived_catching_up FROM race_state",
        )
        .await;
        assert_eq!(state, [["\"race\"", "1", "1", "1000", "0"]]);

        let cars = query(
            &engineer,
            "SELECT c.vehicle_id, is_player, d.name, position, ROUND(ahead_of_player_m), ROUND(right_of_player_m), place_behind_vehicle_id \
             FROM cars_now c JOIN stints s ON s.stint_id = c.current_stint_id JOIN drivers_now d USING (driver_id) \
             ORDER BY c.vehicle_id",
        )
        .await;
        assert_eq!(cars[0][..4], ["1", "1", "\"Ana Lopes\"", "1"]);
        assert_eq!(cars[0][4], "NULL");
        assert_eq!(cars[0][6], "2");
        // Alongside on the right
        assert_eq!(cars[1][2], "\"Lena Fischer\"");
        assert_eq!(cars[1][4], "-3");
        assert_eq!(cars[1][5], "3");

        let wheels = query(
            &engineer,
            "SELECT wheel_name, ROUND(optimal_c), ROUND(pressure_kpa) FROM wheels_now WHERE vehicle_id = 1 ORDER BY wheel",
        )
        .await;
        assert_eq!(wheels[0], ["\"FL\"", "85", "170"]);
        assert_eq!(wheels.len(), 4);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn the_current_stint_gives_the_verdict() {
        let (_dir, engineer) = race();
        let rows = query(
            &engineer,
            "SELECT limited_by, ROUND(laps_left, 1), ROUND(laps_needed), last_full_lap, margin_laps < 0, \
             save_pct_needed > 0, ROUND(avg_lap_s), baseline_source, start_reason, start_lap, \
             calculated_at_s = (SELECT session_time_s FROM race_state) \
             FROM stints s JOIN cars_now c ON s.stint_id = c.current_stint_id WHERE c.vehicle_id = 1",
        )
        .await;
        // 1.0 - t * 0.0005 energy over 100 s laps: 0.05 a lap, at 0.845 left after 310 s
        // 60 - t * 0.02 fuel: 2 L a lap, 53.8 L left
        assert_eq!(
            rows,
            [[
                "\"energy\"",
                "16.9",
                "15",
                "18",
                "0",
                "NULL",
                "100",
                "\"this stint\"",
                "\"first seen\"",
                "0",
                "1"
            ]],
            "{rows:?}"
        );
        // A rival's too, from what the recording has of it
        assert_eq!(number(&engineer, "SELECT COUNT(*) FROM stints").await, 2.0);
        assert_eq!(
            number(&engineer, "SELECT COUNT(*) FROM drivers_now").await,
            2.0
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn queries_can_join_both_kinds_of_tables_but_never_write() {
        let (_dir, engineer) = race();
        let rows = query(
            &engineer,
            "SELECT COUNT(*) > 0 FROM lap_stats l JOIN telemetry t ON t.id = l.first_id",
        )
        .await;
        assert_eq!(rows, [["1"]]);

        for (sql, expected) in [
            ("DELETE FROM session", "only SELECT"),
            (
                "WITH x AS (SELECT 1) DELETE FROM lap_stats",
                "not authorized",
            ),
            ("PRAGMA query_only = 1", "not authorized"),
            (
                "SELECT * FROM lap_stats; DROP TABLE lap_stats",
                "not authorized",
            ),
        ] {
            let error = engineer
                .run(
                    serde_json::from_value(serde_json::json!({"command": "sql", "query": sql}))
                        .unwrap(),
                )
                .await
                .unwrap_err();
            assert!(format!("{error:#}").contains(expected), "{sql}: {error:#}");
        }
        // And nothing was changed for the thread's own writes
        assert_eq!(
            number(&engineer, "SELECT COUNT(*) FROM cars_now").await,
            2.0
        );
        let table_info = query(&engineer, "PRAGMA table_info(lap_stats)").await;
        assert!(table_info.iter().any(|row| row[1] == "\"stint_id\""));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn the_skill_documents_every_column_and_its_queries_run() {
        let (_dir, engineer) = race();
        for table in STREAM_TABLES.into_iter().chain(NOW_TABLES) {
            let columns = query(&engineer, &format!("PRAGMA table_info({table})")).await;
            assert!(!columns.is_empty(), "{table}");
            for column in columns {
                let name = column[1].trim_matches('"');
                assert!(SKILL.contains(&format!("`{name}`")), "{table}.{name}");
            }
        }

        let (_, block) = SKILL.split_once("```sql").unwrap();
        let (block, _) = block.split_once("```").unwrap();
        // As they're written, comments and all
        let queries = block
            .split(';')
            .map(str::trim)
            .filter(|query| !query.is_empty())
            .collect::<Vec<_>>();
        assert_eq!(queries.len(), 5);
        for sql in queries {
            let sql = sql
                .replace(":P", "1")
                .replace(":TURN_INDEX", "0")
                .replace(":OBSERVED_LAP", "3")
                .replace(":FROM_LAP", "0")
                .replace(":TO_LAP", "1000");
            run(
                &engineer,
                serde_json::json!({"command": "sql", "query": sql}),
            )
            .await;
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn sql_rejects_large_values_and_recovers_after_errors() {
        let (_dir, engineer) = race();
        for sql in ["SELECT randomblob(2097152)", "SELECT hex(zeroblob(600000))"] {
            let error = engineer
                .run(
                    serde_json::from_value(serde_json::json!({
                        "command": "sql", "query": sql
                    }))
                    .unwrap(),
                )
                .await
                .unwrap_err();
            assert!(format!("{error:#}").contains("too big"), "{sql}: {error:#}");
        }
        // Authorizer and limits are restored after failure: current-state writes and
        // ordinary model queries still work on this same worker connection.
        assert_eq!(
            number(&engineer, "SELECT COUNT(*) FROM cars_now").await,
            2.0
        );
        assert_eq!(
            query(&engineer, "SELECT zeroblob(16)").await,
            [["<16 bytes>"]]
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn sql_has_hard_row_and_byte_limits() {
        let (_dir, engineer) = race();
        let rows = run(&engineer, serde_json::json!({
            "command": "sql", "limit": 1000000,
            "query": "WITH RECURSIVE n(x) AS (VALUES(1) UNION ALL SELECT x+1 FROM n WHERE x<1000) SELECT x FROM n"
        })).await;
        assert!(rows.contains("stopped at 200 rows"), "{rows}");
        let bytes = run(&engineer, serde_json::json!({
            "command": "sql", "limit": 200,
            "query": "WITH RECURSIVE n(x) AS (VALUES(1) UNION ALL SELECT x+1 FROM n WHERE x<200) SELECT char(1,2,3,4,5,6,7,8,9,10,11,12,13,14,15,16,17,18,19,20,21,22,23,24,25,26,27,28,29,30,31) FROM n"
        })).await;
        assert!(bytes.len() <= SQL_MAX_BYTES, "{}", bytes.len());
        assert!(bytes.contains("byte limit"), "{bytes}");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_new_session_starts_the_tables_over() {
        let (dir, engineer) = race();
        assert_eq!(
            number(
                &engineer,
                "SELECT COUNT(*) FROM lap_stats WHERE vehicle_id = 1"
            )
            .await,
            4.0
        );
        let conn = Connection::open(dir.path().join(DATABASE)).unwrap();
        // An opponent's clock alone doesn't start one
        conn.execute(
            "INSERT INTO telemetry (vehicle_id, is_player, session_time, lap) VALUES (2, 0, 0, 0)",
            [],
        )
        .unwrap();
        assert_eq!(
            number(
                &engineer,
                "SELECT COUNT(*) FROM lap_stats WHERE vehicle_id = 1"
            )
            .await,
            4.0
        );
        // The player's does
        conn.execute(
            "INSERT INTO telemetry (vehicle_id, is_player, session_time, lap, lap_start_time) VALUES (1, 1, 0, 0, 0)",
            [],
        )
        .unwrap();
        let first = number(&engineer, "SELECT session_first_id FROM race_state").await;
        assert!(first > 1.0, "{first}");
        assert_eq!(
            number(
                &engineer,
                "SELECT COUNT(*) FROM lap_stats WHERE vehicle_id = 1"
            )
            .await,
            1.0
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_session_in_the_file_before_this_one_is_skipped() {
        let (dir, engineer) = race();
        drop(engineer);
        let conn = Connection::open(dir.path().join(DATABASE)).unwrap();
        conn.execute(
            "INSERT INTO telemetry (vehicle_id, is_player, session_time, lap, lap_start_time) VALUES (1, 1, 0, 0, 0)",
            [],
        )
        .unwrap();
        drop(conn);
        let engineer = reopen(&dir);
        assert_eq!(
            number(&engineer, "SELECT COUNT(*) FROM lap_stats").await,
            1.0
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn sectors_are_as_many_as_the_simulator_times() {
        let (dir, engineer) = race();
        assert_eq!(
            number(&engineer, "SELECT sector_count FROM race_state").await,
            3.0
        );
        // Each complete lap's official sectors, which add up to its official time
        assert_eq!(
            number(
                &engineer,
                "SELECT COUNT(*) FROM lap_sector_stats WHERE vehicle_id = 2"
            )
            .await,
            9.0
        );
        let lap = "SELECT SUM(s.time_s) - l.official_time_s FROM lap_sector_stats s \
                   JOIN lap_stats l USING (vehicle_id, lap) WHERE vehicle_id = 2 AND lap = 1";
        assert!(number(&engineer, lap).await.abs() < 1e-9);
        assert_eq!(
            number(
                &engineer,
                "SELECT time_s FROM lap_sector_stats WHERE vehicle_id = 2 AND lap = 1 AND sector = 2"
            )
            .await,
            35.0
        );
        // The newest standing's, one row a sector
        assert_eq!(
            number(
                &engineer,
                "SELECT COUNT(*) FROM sectors_now WHERE vehicle_id = 1"
            )
            .await,
            3.0
        );
        let last = number(
            &engineer,
            "SELECT last_lap_s FROM sectors_now WHERE vehicle_id = 2 AND sector = 3",
        )
        .await;
        assert!((last - 34.8).abs() < 1e-9, "{last}");

        // A local yellow in the second sector
        let conn = Connection::open(dir.path().join(DATABASE)).unwrap();
        conn.execute(
            "UPDATE session_sector SET local_yellow = 1 \
             WHERE session_id = (SELECT MAX(id) FROM session) AND sector = 1",
            [],
        )
        .unwrap();
        drop(conn);
        assert_eq!(
            number(&engineer, "SELECT local_yellow FROM race_state").await,
            1.0
        );
        let quick = run(&engineer, serde_json::json!({"command": "quick"})).await;
        assert!(
            quick.contains("sector index 2 as the sim reports it"),
            "{quick}"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn the_tracks_sectors_are_the_simulators() {
        let (dir, engineer) = race();
        // Another split of the lap than the simulator's
        std::fs::write(
            dir.path().join(crate::track::TRACK_FILE),
            serde_json::json!({
                "name": "Test Ring",
                "length": 1000.0,
                "source_track": "Test Ring",
                "sector": [
                    {"name": "1", "marker": 0.2}, {"name": "2", "marker": 0.5},
                    {"name": "3", "marker": 0.8}, {"name": "4", "marker": 1.0},
                ],
            })
            .to_string(),
        )
        .unwrap();
        // The session's start known, as it is once the tables have read the recording
        engineer.sync().await.unwrap();
        let track = run(&engineer, serde_json::json!({"command": "track"})).await;
        let track: Json = serde_json::from_str(&track).unwrap();
        assert_eq!(track["sector_source"], "timing", "{track}");
        let ends = track["sector"]
            .as_array()
            .unwrap()
            .iter()
            .map(|s| s["marker"].as_f64().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(ends, [0.3, 0.65, 1.0]);

        // Lines out of the window, or never crossed: what was seen of them this session stands
        let conn = Connection::open(dir.path().join(DATABASE)).unwrap();
        conn.execute("UPDATE telemetry SET sector = 0", []).unwrap();
        drop(conn);
        let track = run(&engineer, serde_json::json!({"command": "track"})).await;
        let track: Json = serde_json::from_str(&track).unwrap();
        assert_eq!(track["sector_source"], "timing", "{track}");

        // Without the lines ever seen, the description's four can't stand for the three
        drop(engineer);
        let engineer = reopen(&dir);
        engineer.sync().await.unwrap();
        let track = run(&engineer, serde_json::json!({"command": "track"})).await;
        let track: Json = serde_json::from_str(&track).unwrap();
        assert_eq!(track["sector_source"], "none", "{track}");
        assert!(track.get("sector").is_none(), "{track}");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn the_track_is_its_description_lined_up_with_the_laps() {
        let (_dir, engineer) = shifted_ring();
        engineer.sync().await.unwrap();
        let track = run(&engineer, serde_json::json!({"command": "track"})).await;
        let track: Json = serde_json::from_str(&track).unwrap();
        assert_eq!(track["name"], "Shifted Ring");
        assert_eq!(track["lap_length_m"], 2000.0);
        assert!(
            track["attribution"]
                .as_str()
                .unwrap()
                .contains("CC BY-NC-SA")
        );
        assert!(
            (track["calibration"]["shift_m"].as_f64().unwrap() + 150.0).abs() < 5.0,
            "{track}"
        );
        // Where the car slows
        let t1 = &track["turn"][0];
        assert!(
            (t1["marker"].as_f64().unwrap() * 2000.0 - 300.0).abs() < 5.0,
            "{track}"
        );
        assert!(
            (track["pitentry"].as_f64().unwrap() * 2000.0 - 1750.0).abs() < 5.0,
            "{track}"
        );

        // Quick and the corners use the same correction
        let quick = run(&engineer, serde_json::json!({"command": "quick"})).await;
        assert!(quick.contains("TRACK"), "{quick}");
        let apex = number(
            &engineer,
            "SELECT apex_m FROM corner_passes WHERE lap = 1 AND turn_index = 0",
        )
        .await;
        assert!((apex - 300.0).abs() < 5.0, "{apex}");
        assert_eq!(
            number(
                &engineer,
                "SELECT corner_source = 'track data' FROM race_state"
            )
            .await,
            1.0
        );
    }
}
