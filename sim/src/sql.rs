//! The database the simulator plugins record into, with one table per struct in `include/bono`
//! and a column per field: `telemetry`, `telemetry_wheel`, `car_state`, `session`,
//! `session_sector`, `standings`, `standing_sector` and `forecast`. The columns come from the
//! headers' column lists (see `build.rs`).

use crate::sys::{
    bono_car_state, bono_forecast_point, bono_session_info, bono_session_sector, bono_standing,
    bono_standing_sector, bono_telemetry, bono_wheel_telemetry,
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
sim_session_columns!(columns!(bono_session_info));
sim_standing_columns!(columns!(bono_standing));
sim_session_sector_columns!(columns!(bono_session_sector));
sim_standing_sector_columns!(columns!(bono_standing_sector));
sim_telemetry_columns!(columns!(bono_telemetry));
sim_car_state_columns!(columns!(bono_car_state));
sim_wheel_telemetry_columns!(columns!(bono_wheel_telemetry));

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

        -- sector: zero-based, a row per sector of the session's sector_count; standing_sector's
        -- are telemetry's, session_sector's in the order the simulator reports them
        CREATE TABLE IF NOT EXISTS session (id INTEGER PRIMARY KEY{session});
        CREATE TABLE IF NOT EXISTS session_sector (
            session_id INTEGER NOT NULL REFERENCES session(id), sector INTEGER NOT NULL{session_sector},
            PRIMARY KEY (session_id, sector)
        ) WITHOUT ROWID;
        CREATE TABLE IF NOT EXISTS standings (id INTEGER PRIMARY KEY{standing});
        CREATE TABLE IF NOT EXISTS standing_sector (
            standing_id INTEGER NOT NULL REFERENCES standings(id), sector INTEGER NOT NULL{standing_sector},
            PRIMARY KEY (standing_id, sector)
        ) WITHOUT ROWID;
        CREATE TABLE IF NOT EXISTS forecast (id INTEGER PRIMARY KEY, generation INTEGER{forecast});",
        telemetry = bono_telemetry::DEFINITIONS,
        wheel = bono_wheel_telemetry::DEFINITIONS,
        state = bono_car_state::DEFINITIONS,
        session = bono_session_info::DEFINITIONS,
        session_sector = bono_session_sector::DEFINITIONS,
        standing = bono_standing::DEFINITIONS,
        standing_sector = bono_standing_sector::DEFINITIONS,
        forecast = bono_forecast_point::DEFINITIONS,
    ))?;
    Ok(())
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
pub fn clear_tables(conn: &Connection) -> rusqlite::Result<()> {
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
}

impl<'conn> TelemetryWriter<'conn> {
    pub fn new(conn: &'conn Connection) -> rusqlite::Result<Self> {
        Ok(Self {
            conn,
            insert_car: prepare_insert::<bono_telemetry>(conn, "telemetry", &[])?,
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
        })
    }

    pub fn reset(&mut self) {
        self.last_states.clear();
    }

    pub fn write(&mut self, telemetry: &bono_telemetry) -> rusqlite::Result<()> {
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
            insert_session: prepare_insert::<bono_session_info>(conn, "session", &[])?,
            insert_session_sector: prepare_insert::<bono_session_sector>(
                conn,
                "session_sector",
                &["session_id", "sector"],
            )?,
            insert_standing: prepare_insert::<bono_standing>(conn, "standings", &[])?,
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

    pub fn write(
        &mut self,
        session: bono_session_info,
        standings: Vec<bono_standing>,
    ) -> rusqlite::Result<()> {
        // `sector_count` is compared, so a row it changes on is written whatever its sectors say,
        // and so is every standing: the last ones have the old count's sectors
        let count = sector_count(&session);
        let recounted = self
            .last_session
            .as_ref()
            .is_some_and(|last| sector_count(last) != count);
        if self.last_session.as_ref().is_none_or(|last| {
            session.changed(last) || sectors_changed(&session.sectors, &last.sectors, count)
        }) {
            session.bind(&mut self.insert_session, 1)?;
            self.insert_session.raw_execute()?;
            let id = self.conn.last_insert_rowid();
            write_sectors(&mut self.insert_session_sector, id, &session.sectors, count)?;
            self.last_session = Some(session);
        }

        for standing in standings {
            let key = standing.vehicle_id.get().unwrap_or(-1);
            if !recounted
                && self.last_standings.get(&key).is_some_and(|last| {
                    !standing.changed(last)
                        && !sectors_changed(&standing.sectors, &last.sectors, count)
                })
            {
                continue;
            }

            standing.bind(&mut self.insert_standing, 1)?;
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

/// The same forecast, whenever it was read.
fn same_forecast(a: &[bono_forecast_point], b: &[bono_forecast_point]) -> bool {
    a.len() == b.len()
        && a.iter().zip(b).all(|(a, b)| {
            let mut a = a.clone();
            a.session_time = b.session_time;
            !a.changed(b)
        })
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
            .write(bono_session_info::default(), vec![car.clone()])
            .unwrap();
        car.pit_service_refuel_active = Some(true).into();
        writer
            .write(bono_session_info::default(), vec![car])
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
        writer.write(session.clone(), vec![car.clone()]).unwrap();
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
        writer.write(session.clone(), vec![car.clone()]).unwrap();
        assert_eq!((count("session"), count("standings")), (1, 1));

        // A sector's time alone is a change
        car.sectors[1].last_lap = Some(35.0).into();
        writer.write(session.clone(), vec![car.clone()]).unwrap();
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
        writer.write(session.clone(), vec![car.clone()]).unwrap();
        assert_eq!(count("session"), 2);
        assert_eq!(count("session_sector"), 18);
        // The new count's sectors for every car, changed or not
        assert_eq!(count("standings"), 3);
        assert_eq!(count("standing_sector"), 34);
        car.sectors[5].last_lap = Some(40.0).into();
        writer.write(session.clone(), vec![car.clone()]).unwrap();
        assert_eq!(count("standings"), 3);

        // A session without a count has none
        session.sector_count = None.into();
        writer.write(session, vec![car]).unwrap();
        assert_eq!(count("session"), 3);
        assert_eq!(count("session_sector"), 18);
        assert_eq!((count("standings"), count("standing_sector")), (4, 34));
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
        telemetry.write(&bono_telemetry::default()).unwrap();
        ScoringWriter::new(&conn)
            .unwrap()
            .write(bono_session_info::default(), vec![bono_standing::default()])
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
        clear_tables(&conn).unwrap();
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
}
