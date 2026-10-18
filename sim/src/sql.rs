//! The database the simulator plugins record into, with one table per struct in `include/lambiase`
//! and a column per field: `telemetry`, `telemetry_wheel`, `car_state`, `session`, `standings` and
//! `forecast`. The columns come from the headers' column lists (see `build.rs`).

use crate::sys::{
    lambiase_car_state, lambiase_forecast_point, lambiase_session_info, lambiase_standing,
    lambiase_telemetry, lambiase_wheel_telemetry,
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

sim_forecast_columns!(columns!(lambiase_forecast_point));
sim_session_columns!(columns!(lambiase_session_info));
sim_standing_columns!(columns!(lambiase_standing));
sim_telemetry_columns!(columns!(lambiase_telemetry));
sim_car_state_columns!(columns!(lambiase_car_state));
sim_wheel_telemetry_columns!(columns!(lambiase_wheel_telemetry));

pub fn create_tables(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(&format!(
        "CREATE TABLE IF NOT EXISTS telemetry (id INTEGER PRIMARY KEY{telemetry});

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

        CREATE TABLE IF NOT EXISTS session (id INTEGER PRIMARY KEY{session});
        CREATE TABLE IF NOT EXISTS standings (id INTEGER PRIMARY KEY{standing});
        CREATE TABLE IF NOT EXISTS forecast (id INTEGER PRIMARY KEY{forecast});",
        telemetry = lambiase_telemetry::DEFINITIONS,
        wheel = lambiase_wheel_telemetry::DEFINITIONS,
        state = lambiase_car_state::DEFINITIONS,
        session = lambiase_session_info::DEFINITIONS,
        standing = lambiase_standing::DEFINITIONS,
        forecast = lambiase_forecast_point::DEFINITIONS,
    ))
}

/// Deletes every row of every table.
pub fn clear_tables(conn: &Connection) -> rusqlite::Result<()> {
    let tables = conn
        .prepare("SELECT name FROM sqlite_master WHERE type = 'table' AND name NOT LIKE 'sqlite_%'")?
        .query_map([], |row| row.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;

    let tx = conn.unchecked_transaction()?;
    // Foreign keys are on (rusqlite's bundled SQLite), and `telemetry` comes before the tables
    // referencing it: checked at the commit instead, once they're all empty. Ends with it
    tx.pragma_update(None, "defer_foreign_keys", true)?;
    for table in tables {
        tx.execute(&format!("DELETE FROM \"{}\"", table.replace('"', "\"\"")), [])?;
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
    last_states: HashMap<i64, (lambiase_car_state, f64)>,
}

impl<'conn> TelemetryWriter<'conn> {
    pub fn new(conn: &'conn Connection) -> rusqlite::Result<Self> {
        Ok(Self {
            conn,
            insert_car: prepare_insert::<lambiase_telemetry>(conn, "telemetry", &[])?,
            insert_wheel: prepare_insert::<lambiase_wheel_telemetry>(
                conn,
                "telemetry_wheel",
                &["telemetry_id", "wheel"],
            )?,
            insert_state: prepare_insert::<lambiase_car_state>(
                conn,
                "car_state",
                &["telemetry_id", "vehicle_id", "session_time"],
            )?,
            last_states: HashMap::new(),
        })
    }

    pub fn reset(&mut self) {
        self.last_states.clear();
    }

    pub fn write(&mut self, telemetry: &lambiase_telemetry) -> rusqlite::Result<()> {
        telemetry.bind(&mut self.insert_car, 1)?;
        self.insert_car.raw_execute()?;
        let telemetry_id = self.conn.last_insert_rowid();

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
        self.insert_state.raw_bind_parameter(2, &telemetry.vehicle_id)?;
        self.insert_state.raw_bind_parameter(3, &telemetry.session_time)?;
        telemetry.state.bind(&mut self.insert_state, 4)?;
        self.insert_state.raw_execute()?;
        self.last_states.insert(vehicle, (telemetry.state, time));
        Ok(())
    }
}

/// Writes scoring only when it changes: see scoring.h.
pub struct ScoringWriter<'conn> {
    insert_session: Statement<'conn>,
    insert_standing: Statement<'conn>,
    last_session: Option<lambiase_session_info>,
    last_standings: HashMap<i64, lambiase_standing>,
}

impl<'conn> ScoringWriter<'conn> {
    pub fn new(conn: &'conn Connection) -> rusqlite::Result<Self> {
        Ok(Self {
            insert_session: prepare_insert::<lambiase_session_info>(conn, "session", &[])?,
            insert_standing: prepare_insert::<lambiase_standing>(conn, "standings", &[])?,
            last_session: None,
            last_standings: HashMap::new(),
        })
    }

    pub fn reset(&mut self) {
        self.last_session = None;
        self.last_standings.clear();
    }

    pub fn write(
        &mut self,
        session: lambiase_session_info,
        standings: Vec<lambiase_standing>,
    ) -> rusqlite::Result<()> {
        if self
            .last_session
            .as_ref()
            .is_none_or(|last| session.changed(last))
        {
            session.bind(&mut self.insert_session, 1)?;
            self.insert_session.raw_execute()?;
            self.last_session = Some(session);
        }

        for standing in standings {
            let key = standing.vehicle_id.get().unwrap_or(-1);
            if self
                .last_standings
                .get(&key)
                .is_some_and(|last| !standing.changed(last))
            {
                continue;
            }

            standing.bind(&mut self.insert_standing, 1)?;
            self.insert_standing.raw_execute()?;
            self.last_standings.insert(key, standing);
        }

        Ok(())
    }
}

/// Writes the whole forecast whenever it changes.
pub struct ForecastWriter<'conn> {
    conn: &'conn Connection,
    insert: Statement<'conn>,
    last: Vec<lambiase_forecast_point>,
}

impl<'conn> ForecastWriter<'conn> {
    pub fn new(conn: &'conn Connection) -> rusqlite::Result<Self> {
        Ok(Self {
            conn,
            insert: prepare_insert::<lambiase_forecast_point>(conn, "forecast", &[])?,
            last: Vec::new(),
        })
    }

    pub fn reset(&mut self) {
        self.last.clear();
    }

    /// Returns whether the forecast changed, and so was written.
    pub fn write(&mut self, points: Vec<lambiase_forecast_point>) -> rusqlite::Result<bool> {
        if points.is_empty() || same_forecast(&points, &self.last) {
            return Ok(false);
        }

        let tx = self.conn.unchecked_transaction()?;
        for point in &points {
            point.bind(&mut self.insert, 1)?;
            self.insert.raw_execute()?;
        }
        tx.commit()?;

        self.last = points;
        Ok(true)
    }
}

/// The same forecast, whenever it was read.
fn same_forecast(a: &[lambiase_forecast_point], b: &[lambiase_forecast_point]) -> bool {
    a.len() == b.len()
        && a.iter().zip(b).all(|(a, b)| {
            let mut a = a.clone();
            a.session_time = b.session_time;
            !a.changed(b)
        })
}
