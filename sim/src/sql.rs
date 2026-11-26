//! The database the simulator plugins record into, with one table per struct in `include/bono`
//! and a column per field: `telemetry`, `telemetry_wheel`, `car_state`, `session`,
//! `session_sector`, `standings`, `standing_sector`, `forecast`, `event_session` and
//! `event_rule`. The columns come from the
//! headers' column lists (see `build.rs`).
//!
//! A recording is split into epochs (the `epoch` table): a session, a restart or anything else that
//! breaks the timeline starts a new one. The `telemetry`, `session` and `standings` rows carry the
//! epoch they were recorded in, and each epoch's rows come after every earlier epoch's, so an
//! epoch's rows are those from its `*_from` ids on. Readers look at the newest epoch (see
//! [`active_epoch`]) to keep one session's laps, standings and car state from mixing with
//! another's.

use crate::{
    setup::{Setting, Setup},
    sys::{
        bono_car_state, bono_event_rule, bono_event_session, bono_forecast_point,
        bono_session_info, bono_session_sector, bono_standing, bono_standing_sector, bono_telemetry,
        bono_wheel_telemetry,
    },
};
use rusqlite::{Connection, PrepFlags, Statement};
use std::collections::HashMap;

include!(concat!(env!("OUT_DIR"), "/columns.rs"));

/// A struct stored as columns of a table.
pub trait Columns {
    /// `, name TYPE` for each column.
    const DEFINITIONS: &'static str;
    const NAMES: &'static [&'static str];

    /// Binds each column to the statement's parameters, starting at the 1-based `first`.
    fn bind(&self, stmt: &mut Statement<'_>, first: usize) -> rusqlite::Result<()>;

    /// Whether any column flagged as compared differs (all of them, in lists without the flag).
    fn changed(&self, other: &Self) -> bool;
}

macro_rules! sql_type {
    (f64) => {
        "REAL"
    };
    (i64) => {
        "INTEGER"
    };
    (boolean) => {
        "INTEGER"
    };
    (string) => {
        "TEXT"
    };
}

macro_rules! columns {
    ($struct:ident [$(($ty:ident, $name:ident, $compared:literal)),* $(,)?]) => {
        impl Columns for $struct {
            const DEFINITIONS: &'static str = concat!($(", ", stringify!($name), " ", sql_type!($ty)),*);
            const NAMES: &'static [&'static str] = &[$(stringify!($name)),*];

            fn bind(&self, stmt: &mut Statement<'_>, first: usize) -> rusqlite::Result<()> {
                let mut index = first;
                $(
                    stmt.raw_bind_parameter(index, &self.$name)?;
                    index += 1;
                )*
                let _ = index;
                Ok(())
            }

            #[allow(clippy::nonminimal_bool)]
            fn changed(&self, other: &Self) -> bool {
                false $(|| ($compared && self.$name != other.$name))*
            }
        }
    };
}

sim_forecast_columns!(columns!(bono_forecast_point));
sim_event_session_columns!(columns!(bono_event_session));
sim_event_rule_columns!(columns!(bono_event_rule));
sim_session_columns!(columns!(bono_session_info));
sim_standing_columns!(columns!(bono_standing));
sim_session_sector_columns!(columns!(bono_session_sector));
sim_standing_sector_columns!(columns!(bono_standing_sector));
sim_telemetry_columns!(columns!(bono_telemetry));
sim_car_state_columns!(columns!(bono_car_state));
sim_wheel_telemetry_columns!(columns!(bono_wheel_telemetry));

pub fn create_tables(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(&format!(
        "-- An epoch's rows are each table's from its *_from id on (until the next epoch's)
        CREATE TABLE IF NOT EXISTS epoch (
            id INTEGER PRIMARY KEY, reason TEXT NOT NULL, telemetry_from INTEGER NOT NULL,
            session_from INTEGER NOT NULL, standings_from INTEGER NOT NULL
        );
        CREATE TABLE IF NOT EXISTS telemetry (id INTEGER PRIMARY KEY, epoch INTEGER{telemetry});

        -- wheel: 0 = front left, 1 = front right, 2 = rear left, 3 = rear right
        CREATE TABLE IF NOT EXISTS telemetry_wheel (
            telemetry_id INTEGER NOT NULL REFERENCES telemetry(id), wheel INTEGER NOT NULL{wheel},
            PRIMARY KEY (telemetry_id, wheel)
        ) WITHOUT ROWID;

        -- A car's state at a telemetry row is its latest car_state row at or before it, found through the index
        CREATE TABLE IF NOT EXISTS car_state (
            id INTEGER PRIMARY KEY, telemetry_id INTEGER NOT NULL REFERENCES telemetry(id),
            vehicle_id INTEGER, session_time REAL{state}
        );
        CREATE INDEX IF NOT EXISTS car_state_by_vehicle ON car_state (vehicle_id, telemetry_id);

        -- sector: zero-based, as in telemetry, a row per sector of the session's sector_count
        CREATE TABLE IF NOT EXISTS session (id INTEGER PRIMARY KEY, epoch INTEGER{session});
        CREATE TABLE IF NOT EXISTS session_sector (
            session_id INTEGER NOT NULL REFERENCES session(id), sector INTEGER NOT NULL{session_sector},
            PRIMARY KEY (session_id, sector)
        ) WITHOUT ROWID;
        CREATE TABLE IF NOT EXISTS standings (id INTEGER PRIMARY KEY, epoch INTEGER{standing});
        CREATE TABLE IF NOT EXISTS standing_sector (
            standing_id INTEGER NOT NULL REFERENCES standings(id), sector INTEGER NOT NULL{standing_sector},
            PRIMARY KEY (standing_id, sector)
        ) WITHOUT ROWID;
        CREATE TABLE IF NOT EXISTS forecast (id INTEGER PRIMARY KEY, generation INTEGER{forecast});

        -- The event's sessions and rules (see event.h): a set of both at a time, sharing a generation
        CREATE TABLE IF NOT EXISTS event_session (id INTEGER PRIMARY KEY, generation INTEGER{event_session});
        CREATE TABLE IF NOT EXISTS event_rule (id INTEGER PRIMARY KEY, generation INTEGER{event_rule});

        -- The player's garage setup (see setup.rs): a set each time it changes, with only the
        -- settings that differ from the car's last set (all of them in its first). value NULL: the
        -- setting is gone. step to saved_value are LMU's only. telemetry_id is the latest row of
        -- any car when it was written, player_telemetry_id the player's, and session_time,
        -- session_type and session_number the session's as the last scoring update gave it
        CREATE TABLE IF NOT EXISTS setup_set (
            id INTEGER PRIMARY KEY, telemetry_id INTEGER REFERENCES telemetry(id),
            player_telemetry_id INTEGER REFERENCES telemetry(id), vehicle_id INTEGER,
            session_time REAL, file_name TEXT, modified INTEGER, source TEXT, session_type TEXT,
            session_number INTEGER
        );
        CREATE TABLE IF NOT EXISTS setup (
            setup_set_id INTEGER NOT NULL REFERENCES setup_set(id), name TEXT NOT NULL, value TEXT,
            step INTEGER, min_step INTEGER, max_step INTEGER, saved_value TEXT,
            PRIMARY KEY (setup_set_id, name)
        ) WITHOUT ROWID;",
        telemetry = bono_telemetry::DEFINITIONS,
        wheel = bono_wheel_telemetry::DEFINITIONS,
        state = bono_car_state::DEFINITIONS,
        session = bono_session_info::DEFINITIONS,
        session_sector = bono_session_sector::DEFINITIONS,
        standing = bono_standing::DEFINITIONS,
        standing_sector = bono_standing_sector::DEFINITIONS,
        forecast = bono_forecast_point::DEFINITIONS,
        event_session = bono_event_session::DEFINITIONS,
        event_rule = bono_event_rule::DEFINITIONS,
    ))?;
    Ok(())
}

/// Why an epoch began, in the `epoch` table's `reason`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EpochReason {
    /// Recording started
    Start,
    /// What was recorded was deleted (another event, or the app asked)
    Cleared,
    /// The simulator reported another session (its identity changed)
    Session,
    /// The session's clock went back: a restart, or a jump the timeline can't follow
    ClockRollback,
    /// Back from a replay without the live timeline carrying on where it left off
    Replay,
    /// A writer restarted after an error found its epoch's row missing
    Resumed,
}

impl EpochReason {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Start => "start",
            Self::Cleared => "cleared",
            Self::Session => "session",
            Self::ClockRollback => "clock_rollback",
            Self::Replay => "replay",
            Self::Resumed => "resumed",
        }
    }
}

/// Starts epoch `id`: its rows are those written from now on. Nothing happens if it's already
/// there (a writer restarted in the same epoch).
pub fn begin_epoch(conn: &Connection, id: i64, reason: EpochReason) -> rusqlite::Result<()> {
    conn.prepare_cached(
        "INSERT OR IGNORE INTO epoch (id, reason, telemetry_from, session_from, standings_from) \
         VALUES (?1, ?2, (SELECT coalesce(max(id), 0) + 1 FROM telemetry), \
         (SELECT coalesce(max(id), 0) + 1 FROM session), \
         (SELECT coalesce(max(id), 0) + 1 FROM standings))",
    )?
    .execute(rusqlite::params![id, reason.as_str()])?;
    Ok(())
}

/// The newest epoch of a recording: where its rows start in each table.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Epoch {
    /// `None` for recordings made before epochs, or before the first one's row is written: their
    /// rows all count, from 1
    pub id: Option<i64>,
    pub telemetry_from: i64,
    pub session_from: i64,
    pub standings_from: i64,
}

impl Epoch {
    /// The whole recording, for one without epochs.
    pub const ALL: Self = Self {
        id: None,
        telemetry_from: 1,
        session_from: 1,
        standings_from: 1,
    };
}

impl Default for Epoch {
    fn default() -> Self {
        Self::ALL
    }
}

/// The newest epoch, or [`Epoch::ALL`] for recordings without any. The recording may be attached
/// to `conn` under another schema name.
pub fn active_epoch(conn: &Connection) -> rusqlite::Result<Epoch> {
    let exists = conn
        .prepare_cached("SELECT 1 FROM pragma_table_list WHERE name = 'epoch' AND type = 'table'")?
        .exists([])?;
    if !exists {
        return Ok(Epoch::ALL);
    }
    let epoch = conn
        .prepare_cached(
            "SELECT id, telemetry_from, session_from, standings_from FROM epoch \
             ORDER BY id DESC LIMIT 1",
        )?
        .query_row([], |row| {
            Ok(Epoch {
                id: Some(row.get(0)?),
                telemetry_from: row.get(1)?,
                session_from: row.get(2)?,
                standings_from: row.get(3)?,
            })
        });
    match epoch {
        Ok(epoch) => Ok(epoch),
        Err(rusqlite::Error::QueryReturnedNoRows) => Ok(Epoch::ALL),
        Err(e) => Err(e),
    }
}

/// Indexes for reading a recording once it's exported (see https://github.com/racebono/bono-export),
/// which a recording does without to keep writing cheap: a car's rows in a range of ids, as the
/// engineer reads laps.
pub const EXPORT_SCHEMA: &str =
    "CREATE INDEX IF NOT EXISTS telemetry_by_vehicle ON telemetry (vehicle_id);";

/// Replaces a previous run's schema, including tables from older versions.
pub fn recreate_tables(conn: &Connection) -> rusqlite::Result<()> {
    let tables = conn
        .prepare("SELECT name FROM sqlite_master WHERE type = 'table' AND name NOT LIKE 'sqlite_%' ORDER BY rowid DESC")?
        .query_map([], |row| row.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let tx = conn.unchecked_transaction()?;
    tx.pragma_update(None, "defer_foreign_keys", true)?;
    for table in tables {
        tx.execute(
            &format!("DROP TABLE \"{}\"", table.replace('"', "\"\"")),
            [],
        )?;
    }
    create_tables(&tx)?;
    tx.commit()
}

/// Deletes every row of every table.
/// Starts epoch `cleared` in the same breath, if given: a reader never sees the emptied
/// recording without an epoch.
pub fn clear_tables(conn: &Connection, cleared: Option<i64>) -> rusqlite::Result<()> {
    // Newest first: the tables referencing `telemetry` (`telemetry_wheel`, `car_state`) are made
    // after it, and each `*_sector` table after the one it belongs to, so they're emptied first. Foreign keys are on (rusqlite's bundled SQLite), and
    // each `telemetry` row deleted while `car_state` still has rows is looked up in it, which has
    // no index by `telemetry_id`: a whole scan per row, minutes for a weekend
    let tables = conn
        .prepare(
            "SELECT name FROM sqlite_master WHERE type = 'table' AND name NOT LIKE 'sqlite_%' \
             ORDER BY rowid DESC",
        )?
        .query_map([], |row| row.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;

    let tx = conn.unchecked_transaction()?;
    // Checked at the commit instead, once they're all empty, whatever order they're in. Ends with it
    tx.pragma_update(None, "defer_foreign_keys", true)?;
    for table in tables {
        tx.execute(
            &format!("DELETE FROM \"{}\"", table.replace('"', "\"\"")),
            [],
        )?;
    }
    if let Some(id) = cleared {
        begin_epoch(&tx, id, EpochReason::Cleared)?;
    }
    tx.commit()?;

    // Shrinking is a bonus: a reader in the middle of a query can stop it, and the rows are gone anyway
    let _ = conn.execute_batch("VACUUM; PRAGMA wal_checkpoint(TRUNCATE);");
    Ok(())
}

/// An INSERT into `table` of the `leading` columns, then `T`'s.
fn prepare_insert<'conn, T: Columns>(
    conn: &'conn Connection,
    table: &str,
    leading: &[&str],
) -> rusqlite::Result<Statement<'conn>> {
    let names = leading.iter().chain(T::NAMES).copied().collect::<Vec<_>>();
    let values = vec!["?"; names.len()];
    conn.prepare_with_flags(
        &format!(
            "INSERT INTO {table} ({}) VALUES ({})",
            names.join(", "),
            values.join(", ")
        ),
        PrepFlags::SQLITE_PREPARE_PERSISTENT,
    )
}

pub struct TelemetryWriter<'conn> {
    conn: &'conn Connection,
    insert_car: Statement<'conn>,
    insert_wheel: Statement<'conn>,
    insert_state: Statement<'conn>,
    /// Each car's last state written, and the session time it was last seen at.
    last_states: HashMap<i64, (bono_car_state, f64)>,
    /// The player's latest row
    last_player_row: Option<i64>,
}

impl<'conn> TelemetryWriter<'conn> {
    pub fn new(conn: &'conn Connection) -> rusqlite::Result<Self> {
        Ok(Self {
            conn,
            insert_car: prepare_insert::<bono_telemetry>(conn, "telemetry", &["epoch"])?,
            insert_wheel: prepare_insert::<bono_wheel_telemetry>(
                conn,
                "telemetry_wheel",
                &["telemetry_id", "wheel"],
            )?,
            insert_state: prepare_insert::<bono_car_state>(
                conn,
                "car_state",
                &["telemetry_id", "vehicle_id", "session_time"],
            )?,
            last_states: HashMap::new(),
            // Rows written before a restart count too
            last_player_row: conn.query_row(
                "SELECT max(id) FROM telemetry WHERE is_player = 1",
                [],
                |row| row.get(0),
            )?,
        })
    }

    /// For another epoch: each has every car's state of its own. The player's latest row stays: it
    /// still dates a setup (one in another session is told apart by its session)
    pub fn reset(&mut self) {
        self.last_states.clear();
    }

    /// The rows are gone: the player's latest row with them.
    pub fn cleared(&mut self) {
        self.reset();
        self.last_player_row = None;
    }

    /// The player's latest row, written now or before.
    pub fn last_player_row(&self) -> Option<i64> {
        self.last_player_row
    }

    /// Writes an update of epoch `epoch`. Call `reset` when the epoch changes, so each epoch has
    /// its cars' state of its own.
    pub fn write(&mut self, epoch: i64, telemetry: &bono_telemetry) -> rusqlite::Result<()> {
        self.insert_car.raw_bind_parameter(1, epoch)?;
        telemetry.bind(&mut self.insert_car, 2)?;
        self.insert_car.raw_execute()?;
        let telemetry_id = self.conn.last_insert_rowid();
        if telemetry.is_player.get() == Some(true) {
            self.last_player_row = Some(telemetry_id);
        }

        for (wheel, w) in telemetry.wheels.iter().enumerate() {
            self.insert_wheel.raw_bind_parameter(1, telemetry_id)?;
            self.insert_wheel.raw_bind_parameter(2, wheel as i64)?;
            w.bind(&mut self.insert_wheel, 3)?;
            self.insert_wheel.raw_execute()?;
        }

        // A new session (the clock going back) writes the state again, so each session has its own
        let vehicle = telemetry.vehicle_id.get().unwrap_or(-1);
        let time = telemetry.session_time.get().unwrap_or(0.0);
        if let Some((state, last_time)) = self.last_states.get_mut(&vehicle)
            && !state.changed(&telemetry.state)
            && time >= *last_time - 1.0
        {
            *last_time = time;
            return Ok(());
        }

        self.insert_state.raw_bind_parameter(1, telemetry_id)?;
        self.insert_state
            .raw_bind_parameter(2, &telemetry.vehicle_id)?;
        self.insert_state
            .raw_bind_parameter(3, &telemetry.session_time)?;
        telemetry.state.bind(&mut self.insert_state, 4)?;
        self.insert_state.raw_execute()?;
        self.last_states.insert(vehicle, (telemetry.state, time));
        Ok(())
    }
}

/// Writes scoring only when it changes: see scoring.h.
pub struct ScoringWriter<'conn> {
    conn: &'conn Connection,
    insert_session: Statement<'conn>,
    insert_session_sector: Statement<'conn>,
    insert_standing: Statement<'conn>,
    insert_standing_sector: Statement<'conn>,
    last_session: Option<bono_session_info>,
    last_standings: HashMap<i64, bono_standing>,
}

/// How many of a session's sectors are written: its `sector_count`, as far as there's room.
fn sector_count(session: &bono_session_info) -> usize {
    let count = session.sector_count.get().unwrap_or(0).max(0);
    usize::try_from(count).map_or(session.sectors.len(), |count| {
        count.min(session.sectors.len())
    })
}

/// Whether any of the first `count` sectors differs.
fn sectors_changed<T: Columns>(sectors: &[T], other: &[T], count: usize) -> bool {
    sectors[..count]
        .iter()
        .zip(&other[..count])
        .any(|(sector, other)| sector.changed(other))
}

/// Writes the first `count` sectors, as rows of the row `parent`.
fn write_sectors<T: Columns>(
    insert: &mut Statement<'_>,
    parent: i64,
    sectors: &[T],
    count: usize,
) -> rusqlite::Result<()> {
    for (sector, s) in sectors[..count].iter().enumerate() {
        insert.raw_bind_parameter(1, parent)?;
        insert.raw_bind_parameter(2, sector as i64)?;
        s.bind(insert, 3)?;
        insert.raw_execute()?;
    }
    Ok(())
}

impl<'conn> ScoringWriter<'conn> {
    pub fn new(conn: &'conn Connection) -> rusqlite::Result<Self> {
        Ok(Self {
            conn,
            insert_session: prepare_insert::<bono_session_info>(conn, "session", &["epoch"])?,
            insert_session_sector: prepare_insert::<bono_session_sector>(
                conn,
                "session_sector",
                &["session_id", "sector"],
            )?,
            insert_standing: prepare_insert::<bono_standing>(conn, "standings", &["epoch"])?,
            insert_standing_sector: prepare_insert::<bono_standing_sector>(
                conn,
                "standing_sector",
                &["standing_id", "sector"],
            )?,
            last_session: None,
            last_standings: HashMap::new(),
        })
    }

    pub fn reset(&mut self) {
        self.last_session = None;
        self.last_standings.clear();
    }

    /// Writes an update of epoch `epoch`. Call `reset` when the epoch changes, so each epoch starts
    /// with every car's standing.
    pub fn write(
        &mut self,
        epoch: i64,
        session: bono_session_info,
        standings: Vec<bono_standing>,
    ) -> rusqlite::Result<()> {
        // `sector_count` is compared, so a row it changes on is written whatever its sectors say,
        // and so is every standing: the last ones have the old count's sectors
        let count = sector_count(&session);
        if self
            .last_session
            .as_ref()
            .is_some_and(|last| sector_count(last) != count)
        {
            // Cars that aren't in this update too, once they're back
            self.last_standings.clear();
        }
        if self.last_session.as_ref().is_none_or(|last| {
            session.changed(last) || sectors_changed(&session.sectors, &last.sectors, count)
        }) {
            self.insert_session.raw_bind_parameter(1, epoch)?;
            session.bind(&mut self.insert_session, 2)?;
            self.insert_session.raw_execute()?;
            let id = self.conn.last_insert_rowid();
            write_sectors(&mut self.insert_session_sector, id, &session.sectors, count)?;
            self.last_session = Some(session);
        }

        for standing in standings {
            let key = standing.vehicle_id.get().unwrap_or(-1);
            if self.last_standings.get(&key).is_some_and(|last| {
                !standing.changed(last) && !sectors_changed(&standing.sectors, &last.sectors, count)
            }) {
                continue;
            }

            self.insert_standing.raw_bind_parameter(1, epoch)?;
            standing.bind(&mut self.insert_standing, 2)?;
            self.insert_standing.raw_execute()?;
            let id = self.conn.last_insert_rowid();
            write_sectors(
                &mut self.insert_standing_sector,
                id,
                &standing.sectors,
                count,
            )?;
            self.last_standings.insert(key, standing);
        }

        Ok(())
    }
}

/// Writes the whole forecast whenever it changes.
pub struct ForecastWriter<'conn> {
    conn: &'conn Connection,
    insert: Statement<'conn>,
    last: Vec<bono_forecast_point>,
}

impl<'conn> ForecastWriter<'conn> {
    pub fn new(conn: &'conn Connection) -> rusqlite::Result<Self> {
        Ok(Self {
            conn,
            insert: prepare_insert::<bono_forecast_point>(conn, "forecast", &["generation"])?,
            last: Vec::new(),
        })
    }

    pub fn reset(&mut self) {
        self.last.clear();
    }

    /// Returns whether the forecast changed, and so was written.
    pub fn write(&mut self, points: Vec<bono_forecast_point>) -> rusqlite::Result<bool> {
        if points.is_empty() || same_forecast(&points, &self.last) {
            return Ok(false);
        }

        let tx = self.conn.unchecked_transaction()?;
        // The first row's id identifies this set even while the session clock is stopped.
        let generation: i64 =
            tx.query_row("SELECT coalesce(max(id), 0) + 1 FROM forecast", [], |row| {
                row.get(0)
            })?;
        for point in &points {
            self.insert.raw_bind_parameter(1, generation)?;
            point.bind(&mut self.insert, 2)?;
            self.insert.raw_execute()?;
        }
        tx.commit()?;

        self.last = points;
        Ok(true)
    }
}

/// The event's sessions and rules, written together as one set.
#[derive(Clone, Debug, Default)]
pub struct Event {
    pub sessions: Vec<bono_event_session>,
    pub rules: Vec<bono_event_rule>,
}

/// Writes the whole event whenever it changes: both tables' rows, with one generation.
pub struct EventWriter<'conn> {
    conn: &'conn Connection,
    insert_session: Statement<'conn>,
    insert_rule: Statement<'conn>,
    last: Event,
}

impl<'conn> EventWriter<'conn> {
    pub fn new(conn: &'conn Connection) -> rusqlite::Result<Self> {
        Ok(Self {
            conn,
            insert_session: prepare_insert::<bono_event_session>(
                conn,
                "event_session",
                &["generation"],
            )?,
            insert_rule: prepare_insert::<bono_event_rule>(conn, "event_rule", &["generation"])?,
            last: Event::default(),
        })
    }

    pub fn reset(&mut self) {
        self.last = Event::default();
    }

    /// Returns whether the event changed, and so was written.
    pub fn write(&mut self, event: Event) -> rusqlite::Result<bool> {
        if (event.sessions.is_empty() && event.rules.is_empty()) || same_event(&event, &self.last) {
            return Ok(false);
        }

        let tx = self.conn.unchecked_transaction()?;
        // Counted over both tables, so a set with no rows in one of them still has its own
        let generation = newest_event_generation(&tx)?.unwrap_or(0) + 1;
        for session in &event.sessions {
            self.insert_session.raw_bind_parameter(1, generation)?;
            session.bind(&mut self.insert_session, 2)?;
            self.insert_session.raw_execute()?;
        }
        for rule in &event.rules {
            self.insert_rule.raw_bind_parameter(1, generation)?;
            rule.bind(&mut self.insert_rule, 2)?;
            self.insert_rule.raw_execute()?;
        }
        tx.commit()?;

        self.last = event;
        Ok(true)
    }
}

/// The newest event set's generation, over both tables; `None` if none was written.
pub fn newest_event_generation(conn: &Connection) -> rusqlite::Result<Option<i64>> {
    // The aggregate max, which skips NULL (an empty table); the two-argument one would return it
    conn.prepare_cached(
        "SELECT max(generation) FROM (SELECT max(generation) AS generation FROM event_session \
         UNION ALL SELECT max(generation) FROM event_rule)",
    )?
    .query_row([], |row| row.get(0))
}

/// When a set is written: the latest row of any car's and of the player's, and the session as the
/// last scoring update gave it.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct SetDate {
    pub telemetry_id: Option<i64>,
    pub player_telemetry_id: Option<i64>,
    pub session_time: Option<f64>,
    pub session_type: Option<String>,
    pub session_number: Option<i64>,
}

/// Writes the player's setup when it changes: a `setup_set` row, and a `setup` row for each
/// setting that differs from the last set written for the car. Called inside the recorder's
/// transaction, so a set is written whole or not at all.
pub struct SetupWriter<'conn> {
    conn: &'conn Connection,
    /// The car the last set was for, and its file name and modified flag
    last_set: Option<(Option<i64>, Option<String>, Option<bool>)>,
    /// The last set's settings, by name
    last: HashMap<String, Setting>,
}

impl<'conn> SetupWriter<'conn> {
    pub fn new(conn: &'conn Connection) -> rusqlite::Result<Self> {
        Ok(Self {
            conn,
            last_set: None,
            last: HashMap::new(),
        })
    }

    /// The next set is written whole.
    pub fn reset(&mut self) {
        self.last_set = None;
        self.last.clear();
    }

    /// Returns whether anything changed, and so was written.
    pub fn write(
        &mut self,
        vehicle: Option<i64>,
        date: SetDate,
        setup: Setup,
    ) -> rusqlite::Result<bool> {
        if setup.settings.is_empty() {
            return Ok(false);
        }
        // Another car's setup is all new
        if self
            .last_set
            .as_ref()
            .is_some_and(|(car, ..)| *car != vehicle)
        {
            self.reset();
        }

        let changed = setup
            .settings
            .iter()
            .filter(|s| self.last.get(&s.name) != Some(*s))
            .collect::<Vec<_>>();
        let gone = self
            .last
            .keys()
            .filter(|name| !setup.settings.iter().any(|s| &s.name == *name))
            .cloned()
            .collect::<Vec<_>>();
        let set = (vehicle, setup.file_name.clone(), setup.modified);
        if changed.is_empty() && gone.is_empty() && self.last_set.as_ref() == Some(&set) {
            return Ok(false);
        }

        // In the recorder's transaction, with the telemetry it goes with
        self.conn.execute(
            "INSERT INTO setup_set (telemetry_id, player_telemetry_id, vehicle_id, session_time, \
             file_name, modified, source, session_type, session_number) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
            rusqlite::params![
                date.telemetry_id,
                date.player_telemetry_id,
                vehicle,
                date.session_time,
                setup.file_name,
                setup.modified,
                setup.source.map(|s| s.as_str()),
                date.session_type,
                date.session_number,
            ],
        )?;
        let id = self.conn.last_insert_rowid();
        let mut insert = self.conn.prepare_cached(
            "INSERT INTO setup (setup_set_id, name, value, step, min_step, max_step, saved_value) \
             VALUES (?, ?, ?, ?, ?, ?, ?)",
        )?;
        for s in &changed {
            insert.execute(rusqlite::params![
                id,
                s.name,
                s.value,
                s.step,
                s.min_step,
                s.max_step,
                s.saved_value
            ])?;
        }
        for name in &gone {
            insert.execute(rusqlite::params![
                id,
                name,
                None::<String>,
                None::<i64>,
                None::<i64>,
                None::<i64>,
                None::<String>
            ])?;
        }
        drop(insert);

        self.last_set = Some(set);
        self.last = setup
            .settings
            .into_iter()
            .map(|s| (s.name.clone(), s))
            .collect();
        Ok(true)
    }
}

/// The same forecast, whenever it was read.
fn same_forecast(a: &[bono_forecast_point], b: &[bono_forecast_point]) -> bool {
    a.len() == b.len()
        && a.iter().zip(b).all(|(a, b)| {
            let mut a = a.clone();
            a.session_time = b.session_time;
            !a.changed(b)
        })
}

/// The same event, whenever it was read.
fn same_event(a: &Event, b: &Event) -> bool {
    fn same<T: Columns + Clone>(a: &[T], b: &[T], read_at: impl Fn(&mut T, &T)) -> bool {
        a.len() == b.len()
            && a.iter().zip(b).all(|(a, b)| {
                let mut a = a.clone();
                read_at(&mut a, b);
                !a.changed(b)
            })
    }
    same(&a.sessions, &b.sessions, |a, b| a.session_time = b.session_time)
        && same(&a.rules, &b.rules, |a, b| a.session_time = b.session_time)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    /// A future adapter can provide partial service/penalty data without RaceRoom codes.
    #[test]
    fn portable_service_fields_need_no_source_layout() {
        let conn = Connection::open_in_memory().unwrap();
        create_tables(&conn).unwrap();
        let mut writer = ScoringWriter::new(&conn).unwrap();
        let mut car = bono_standing::default();
        car.vehicle_id = Some(42).into();
        car.penalty_drive_through_active = Some(false).into();
        car.penalty_stop_and_go_seconds = Some(15.0).into();
        car.pit_service_refuel_selected = Some(true).into();
        car.pit_service_refuel_active = Some(false).into();
        writer
            .write(1, bono_session_info::default(), vec![car.clone()])
            .unwrap();
        car.pit_service_refuel_active = Some(true).into();
        writer
            .write(1, bono_session_info::default(), vec![car])
            .unwrap();
        let result: (i64, bool, f64, bool, bool, Option<bool>, Option<String>) = conn.query_row(
            "SELECT (SELECT COUNT(*) FROM standings), penalty_drive_through_active, penalty_stop_and_go_seconds, pit_service_refuel_selected, pit_service_refuel_active, pit_service_front_tires_active, simulator_metadata FROM standings ORDER BY id DESC LIMIT 1", [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?))).unwrap();
        assert_eq!(result, (2, false, 15.0, true, true, None, None));
    }

    #[test]
    fn writes_as_many_sectors_as_the_session_has() {
        let conn = Connection::open_in_memory().unwrap();
        create_tables(&conn).unwrap();
        let mut writer = ScoringWriter::new(&conn).unwrap();
        let count = |table: &str| {
            conn.query_row(&format!("SELECT count(*) FROM {table}"), [], |r| {
                r.get::<_, i64>(0)
            })
            .unwrap()
        };

        // More than there's room for: the count is kept, and the sectors there's room for
        let mut session = bono_session_info::default();
        session.sector_count = Some(20).into();
        session.sectors[15].local_yellow = Some(true).into();
        let mut car = bono_standing::default();
        car.vehicle_id = Some(1).into();
        car.sectors[0].last_lap = Some(30.0).into();
        writer.write(1, session.clone(), vec![car.clone()]).unwrap();
        assert_eq!(count("session_sector"), 16);
        assert_eq!(count("standing_sector"), 16);
        let yellow: i64 = conn
            .query_row(
                "SELECT sector FROM session_sector WHERE local_yellow",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(yellow, 15);

        // Nothing changed: nothing written
        writer.write(1, session.clone(), vec![car.clone()]).unwrap();
        assert_eq!((count("session"), count("standings")), (1, 1));

        // A sector's time alone is a change
        car.sectors[1].last_lap = Some(35.0).into();
        writer.write(1, session.clone(), vec![car.clone()]).unwrap();
        assert_eq!((count("session"), count("standings")), (1, 2));
        let (last, standing): (f64, i64) = conn
            .query_row(
                "SELECT last_lap, standing_id FROM standing_sector WHERE sector = 1 \
                 ORDER BY standing_id DESC LIMIT 1",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!((last, standing), (35.0, 2));

        // Fewer sectors: only theirs are written, and only theirs compared
        session.sector_count = Some(2).into();
        session.sectors[15].local_yellow = Some(false).into();
        writer.write(1, session.clone(), vec![car.clone()]).unwrap();
        assert_eq!(count("session"), 2);
        assert_eq!(count("session_sector"), 18);
        // The new count's sectors for every car, changed or not
        assert_eq!(count("standings"), 3);
        assert_eq!(count("standing_sector"), 34);
        car.sectors[5].last_lap = Some(40.0).into();
        writer.write(1, session.clone(), vec![car.clone()]).unwrap();
        assert_eq!(count("standings"), 3);

        // A session without a count has none
        session.sector_count = None.into();
        writer.write(1, session, vec![car]).unwrap();
        assert_eq!(count("session"), 3);
        assert_eq!(count("session_sector"), 18);
        assert_eq!((count("standings"), count("standing_sector")), (4, 34));
    }

    #[test]
    fn finds_the_newest_epoch_wherever_the_recording_is() {
        let conn = Connection::open_in_memory().unwrap();
        // Recordings made before epochs are all one
        conn.execute_batch("CREATE TABLE telemetry (id INTEGER PRIMARY KEY)")
            .unwrap();
        assert_eq!(active_epoch(&conn).unwrap(), Epoch::ALL);

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("recording.db");
        let recording = Connection::open(&path).unwrap();
        create_tables(&recording).unwrap();
        // Before its first epoch's row
        assert_eq!(active_epoch(&recording).unwrap(), Epoch::ALL);
        TelemetryWriter::new(&recording)
            .unwrap()
            .write(1, &bono_telemetry::default())
            .unwrap();
        begin_epoch(&recording, 1, EpochReason::Start).unwrap();
        begin_epoch(&recording, 2, EpochReason::Session).unwrap();
        // Already there: it keeps where it started
        TelemetryWriter::new(&recording)
            .unwrap()
            .write(2, &bono_telemetry::default())
            .unwrap();
        begin_epoch(&recording, 2, EpochReason::Resumed).unwrap();
        let newest = Epoch {
            id: Some(2),
            telemetry_from: 2,
            session_from: 1,
            standings_from: 1,
        };
        assert_eq!(active_epoch(&recording).unwrap(), newest);

        // Attached, as the engineer reads it
        let attached = Connection::open_in_memory().unwrap();
        attached
            .execute("ATTACH DATABASE ?1 AS recording", [path.to_str().unwrap()])
            .unwrap();
        assert_eq!(active_epoch(&attached).unwrap(), newest);
    }

    #[test]
    fn recreates_an_older_schema() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE telemetry (id INTEGER PRIMARY KEY, obsolete REAL);
             CREATE TABLE telemetry_wheel (telemetry_id INTEGER REFERENCES telemetry(id));
             CREATE TABLE car_state (telemetry_id INTEGER REFERENCES telemetry(id));
             CREATE TABLE session (id INTEGER PRIMARY KEY);
             CREATE TABLE standings (id INTEGER PRIMARY KEY);
             CREATE TABLE forecast (id INTEGER PRIMARY KEY);
             INSERT INTO telemetry VALUES (1, 42);
             INSERT INTO telemetry_wheel VALUES (1);
             INSERT INTO car_state VALUES (1);",
        )
        .unwrap();
        recreate_tables(&conn).unwrap();
        assert!(!conn.column_exists(None, "telemetry", "obsolete").unwrap());
        assert!(conn.column_exists(None, "forecast", "generation").unwrap());
        let mut telemetry = TelemetryWriter::new(&conn).unwrap();
        telemetry.write(1, &bono_telemetry::default()).unwrap();
        ScoringWriter::new(&conn)
            .unwrap()
            .write(
                1,
                bono_session_info::default(),
                vec![bono_standing::default()],
            )
            .unwrap();
        ForecastWriter::new(&conn)
            .unwrap()
            .write(vec![bono_forecast_point::default()])
            .unwrap();
        assert_eq!(
            conn.query_row("SELECT count(*) FROM telemetry", [], |row| row
                .get::<_, i64>(0))
                .unwrap(),
            1
        );
    }

    /// A weekend's worth of rows, cut down: with `telemetry` emptied first, every row of it
    /// scanned `car_state`, taking seconds here and minutes for a real weekend.
    #[test]
    fn clears_a_big_recording_quickly() {
        let conn = Connection::open_in_memory().unwrap();
        create_tables(&conn).unwrap();
        conn.execute_batch(
            "WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM n WHERE i < 40000)
             INSERT INTO telemetry (id) SELECT i FROM n;
             INSERT INTO telemetry_wheel (telemetry_id, wheel)
                 SELECT id, wheel FROM telemetry, (SELECT 0 AS wheel UNION ALL SELECT 1
                     UNION ALL SELECT 2 UNION ALL SELECT 3);
             INSERT INTO car_state (telemetry_id, vehicle_id) SELECT id, id % 20 FROM telemetry
                 WHERE id % 10 = 0;",
        )
        .unwrap();

        let start = Instant::now();
        clear_tables(&conn, None).unwrap();
        assert!(
            start.elapsed() < Duration::from_secs(2),
            "took {:?}",
            start.elapsed()
        );

        for table in ["telemetry", "telemetry_wheel", "car_state"] {
            let count = conn
                .query_row(&format!("SELECT count(*) FROM {table}"), [], |row| {
                    row.get::<_, i64>(0)
                })
                .unwrap();
            assert_eq!(count, 0, "{table}");
        }
    }

    /// An event of a practice and a race, and a rule if `rule`, read at `session_time`.
    fn event(session_time: f64, rule: bool) -> Event {
        let session = |order: i64, kind: &str| {
            let mut session = bono_event_session::default();
            session.session_time = Some(session_time).into();
            session.event_order = Some(order).into();
            session.session_type = Some(kind.to_owned()).into();
            session.session_number = Some(1).into();
            session.length_time = Some(1200.0).into();
            session.finish = Some("time".to_owned()).into();
            session
        };
        let mut rules = Vec::new();
        if rule {
            let mut rule = bono_event_rule::default();
            rule.session_time = Some(session_time).into();
            rule.name = Some("fuel_use_multiplier".to_owned()).into();
            rule.value = Some("x2".to_owned()).into();
            rules.push(rule);
        }
        Event {
            sessions: vec![session(0, "practice"), session(1, "race")],
            rules,
        }
    }

    fn count(conn: &Connection, sql: &str) -> i64 {
        conn.query_row(sql, [], |r| r.get(0)).unwrap()
    }

    #[test]
    fn event_writer_writes_a_set_with_one_generation() {
        let conn = setup_writer();
        let mut writer = EventWriter::new(&conn).unwrap();
        assert_eq!(newest_event_generation(&conn).unwrap(), None);
        assert!(writer.write(event(10.0, true)).unwrap());

        assert_eq!(
            count(&conn, "SELECT count(*) FROM event_session WHERE generation = 1"),
            2
        );
        assert_eq!(
            count(&conn, "SELECT count(*) FROM event_rule WHERE generation = 1"),
            1
        );
        assert_eq!(newest_event_generation(&conn).unwrap(), Some(1));
    }

    #[test]
    fn event_writer_skips_an_unchanged_set() {
        let conn = setup_writer();
        let mut writer = EventWriter::new(&conn).unwrap();
        assert!(writer.write(event(10.0, true)).unwrap());
        assert!(!writer.write(event(20.0, true)).unwrap());

        assert_eq!(count(&conn, "SELECT count(*) FROM event_session"), 2);
        assert_eq!(count(&conn, "SELECT count(*) FROM event_rule"), 1);
    }

    #[test]
    fn event_generation_is_shared_when_a_set_has_no_rules() {
        let conn = setup_writer();
        let mut writer = EventWriter::new(&conn).unwrap();
        assert!(writer.write(event(10.0, true)).unwrap());
        assert!(writer.write(event(10.0, false)).unwrap());

        assert_eq!(newest_event_generation(&conn).unwrap(), Some(2));
        assert_eq!(
            count(&conn, "SELECT count(*) FROM event_session WHERE generation = 2"),
            2
        );
        assert_eq!(
            count(&conn, "SELECT count(*) FROM event_rule WHERE generation = 2"),
            0
        );
    }

    #[test]
    fn event_writer_writes_again_after_reset() {
        let conn = setup_writer();
        let mut writer = EventWriter::new(&conn).unwrap();
        assert!(writer.write(event(10.0, true)).unwrap());
        writer.reset();
        assert!(writer.write(event(10.0, true)).unwrap());

        assert_eq!(newest_event_generation(&conn).unwrap(), Some(2));
        assert_eq!(
            count(&conn, "SELECT count(*) FROM event_session WHERE generation = 2"),
            2
        );
        assert_eq!(
            count(&conn, "SELECT count(*) FROM event_rule WHERE generation = 2"),
            1
        );
    }

    fn setting(name: &str, value: &str) -> Setting {
        Setting {
            name: name.to_owned(),
            value: value.to_owned(),
            ..Default::default()
        }
    }

    fn setup(settings: &[(&str, &str)]) -> Setup {
        Setup {
            settings: settings.iter().map(|(n, v)| setting(n, v)).collect(),
            ..Default::default()
        }
    }

    fn setup_writer() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        create_tables(&conn).unwrap();
        conn
    }

    /// Each set's rows, as (set, name, value), in order.
    fn setup_rows(conn: &Connection) -> Vec<(i64, String, Option<String>)> {
        conn.prepare("SELECT setup_set_id, name, value FROM setup ORDER BY setup_set_id, name")
            .unwrap()
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap()
    }

    fn sets(conn: &Connection) -> i64 {
        conn.query_row("SELECT count(*) FROM setup_set", [], |r| r.get(0))
            .unwrap()
    }

    fn row(set: i64, name: &str, value: Option<&str>) -> (i64, String, Option<String>) {
        (set, name.to_owned(), value.map(str::to_owned))
    }

    #[test]
    fn a_setup_is_written_whole_then_only_its_changes() {
        let conn = setup_writer();
        let mut writer = SetupWriter::new(&conn).unwrap();
        let first = setup(&[("A", "1"), ("B", "2")]);
        assert!(
            writer
                .write(
                    Some(7),
                    SetDate {
                        session_time: Some(1.0),
                        ..SetDate::default()
                    },
                    first
                )
                .unwrap()
        );
        let second = setup(&[("A", "1"), ("B", "3")]);
        assert!(
            writer
                .write(
                    Some(7),
                    SetDate {
                        session_time: Some(2.0),
                        ..SetDate::default()
                    },
                    second
                )
                .unwrap()
        );

        assert_eq!(sets(&conn), 2);
        assert_eq!(
            setup_rows(&conn),
            vec![
                row(1, "A", Some("1")),
                row(1, "B", Some("2")),
                row(2, "B", Some("3"))
            ]
        );
    }

    #[test]
    fn the_same_setup_again_writes_nothing() {
        let conn = setup_writer();
        let mut writer = SetupWriter::new(&conn).unwrap();
        let same = setup(&[("A", "1")]);
        assert!(
            writer
                .write(Some(7), SetDate::default(), same.clone())
                .unwrap()
        );
        assert!(!writer.write(Some(7), SetDate::default(), same).unwrap());
        assert_eq!(sets(&conn), 1);
    }

    #[test]
    fn a_setting_that_disappears_is_written_as_null() {
        let conn = setup_writer();
        let mut writer = SetupWriter::new(&conn).unwrap();
        writer
            .write(
                Some(7),
                SetDate::default(),
                setup(&[("A", "1"), ("B", "2")]),
            )
            .unwrap();
        writer
            .write(Some(7), SetDate::default(), setup(&[("A", "1")]))
            .unwrap();
        assert_eq!(setup_rows(&conn)[2..], [row(2, "B", None)]);
    }

    #[test]
    fn a_new_vehicle_or_a_reset_writes_the_whole_setup_again() {
        let conn = setup_writer();
        let mut writer = SetupWriter::new(&conn).unwrap();
        writer
            .write(Some(7), SetDate::default(), setup(&[("A", "1")]))
            .unwrap();
        writer
            .write(Some(8), SetDate::default(), setup(&[("A", "1")]))
            .unwrap();
        writer.reset();
        writer
            .write(Some(8), SetDate::default(), setup(&[("A", "1")]))
            .unwrap();
        assert_eq!(
            setup_rows(&conn),
            vec![
                row(1, "A", Some("1")),
                row(2, "A", Some("1")),
                row(3, "A", Some("1"))
            ]
        );
    }

    #[test]
    fn file_name_and_modified_changes_write_a_set() {
        let conn = setup_writer();
        let mut writer = SetupWriter::new(&conn).unwrap();
        let mut loaded = setup(&[("A", "1")]);
        loaded.file_name = Some("race.sto".to_owned());
        loaded.modified = Some(false);
        writer
            .write(Some(7), SetDate::default(), loaded.clone())
            .unwrap();
        loaded.modified = Some(true);
        assert!(writer.write(Some(7), SetDate::default(), loaded).unwrap());

        assert_eq!(sets(&conn), 2);
        assert_eq!(setup_rows(&conn).len(), 1);
        let (file, modified): (String, bool) = conn
            .query_row(
                "SELECT file_name, modified FROM setup_set ORDER BY id DESC LIMIT 1",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!((file.as_str(), modified), ("race.sto", true));
    }

    #[test]
    fn an_empty_setup_writes_nothing() {
        let conn = setup_writer();
        let mut writer = SetupWriter::new(&conn).unwrap();
        assert!(
            !writer
                .write(Some(7), SetDate::default(), Setup::default())
                .unwrap()
        );
        assert_eq!(sets(&conn), 0);
    }
}
