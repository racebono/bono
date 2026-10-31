//! How big each step of an export leaves a recording, how long it takes, and how fast the app's
//! queries read it afterwards: `cargo bench -p bono-export`.
//!
//! - `EXPORT_BENCH_DB`: a recording (`telemetry.db`) to export. Without it, a race of
//!   `EXPORT_BENCH_MINUTES` (default 10) is made up, in the recorder's schema. Its values are
//!   smooth curves, which compress differently from a real car's.
//! - `EXPORT_BENCH_DIR`: where the copies go, in a folder of their own deleted afterwards (default:
//!   Cargo's target folder).
//! - `EXPORT_BENCH_LATENCY_MS`: how long the local HTTP server waits before each answer (default
//!   30, a nearby region's round trip).
//!
//! Each query is timed on a new connection (first run: nothing in SQLite's cache yet, though the
//! files likely are in the OS's), then as the best of 5 more. Then the final database and the
//! export are read over HTTP, from a local server answering range requests: each query once, on a
//! new connection, with the requests it took and the bytes they carried. The server has no
//! bandwidth limit: at 100 Mbit/s, each MiB adds 0.08 s.

// Only part of it is used here
#[allow(dead_code)]
#[path = "../tests/serve/mod.rs"]
mod serve;

use bono_export::Step;
use bono_sim::sql;
use rusqlite::{Connection, OpenFlags, params, types::Value};
use serve::Server;
use std::{
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

fn main() -> anyhow::Result<()> {
    let base = match std::env::var_os("EXPORT_BENCH_DIR") {
        Some(dir) => PathBuf::from(dir),
        None => PathBuf::from(env!("CARGO_TARGET_TMPDIR")),
    };
    std::fs::create_dir_all(&base)?;
    // A folder of its own, deleted when done: nothing else in `base` is touched
    let temp = tempfile::Builder::new()
        .prefix("export-bench-")
        .tempdir_in(&base)?;
    let dir = temp.path();

    let source = match std::env::var_os("EXPORT_BENCH_DB") {
        Some(path) => PathBuf::from(path),
        None => {
            let minutes = std::env::var("EXPORT_BENCH_MINUTES")
                .ok()
                .map(|m| m.parse())
                .transpose()?
                .unwrap_or(10.0);
            let path = dir.join("telemetry.db");
            let start = Instant::now();
            make_up_race(&path, minutes)?;
            eprintln!(
                "made up {minutes} minutes of racing in {:?}",
                start.elapsed()
            );
            path
        }
    };

    // Each step's database, copied as it's left
    let mut stages = vec![(
        "original".to_owned(),
        source.clone(),
        size_with_wal(&source)?,
        None,
    )];
    let mut last = Instant::now();
    bono_export::export_with(
        &source,
        &dir.join("export.db"),
        sql::EXPORT_SCHEMA,
        |step, path| {
            let took = last.elapsed();
            let copy = dir.join(format!("{step:?}.db"));
            std::fs::copy(path, &copy)?;
            stages.push((
                format!("{step:?}"),
                copy,
                path.metadata()?.len(),
                Some(took),
            ));
            last = Instant::now();
            Ok(())
        },
    )?;

    let original = stages[0].2 as f64;
    println!("| Step | Size | vs original | Step took |");
    println!("| --- | ---: | ---: | ---: |");
    for (name, _, size, took) in &stages {
        println!(
            "| {name} | {:.1} MiB | {:.1}% | {} |",
            *size as f64 / (1024.0 * 1024.0),
            *size as f64 / original * 100.0,
            took.map_or("".into(), |t| format!("{:.2} s", t.as_secs_f64()))
        );
    }

    let open = |name: &str, path: &Path| -> anyhow::Result<Connection> {
        if name == format!("{:?}", Step::Compress) {
            bono_export::open(path)
        } else {
            Ok(Connection::open_with_flags(
                path,
                OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
            )?)
        }
    };
    let local = || (0, 0);
    let mut first = Vec::new();
    for (name, path, _, _) in &stages {
        first.push(workload(&open(name, path)?, &local)?);
    }
    // Stages take turns, so none is timed only while the OS is busy with the files just written
    let conns = stages
        .iter()
        .map(|(name, path, _, _)| open(name, path))
        .collect::<anyhow::Result<Vec<_>>>()?;
    let mut best = conns
        .iter()
        .map(|conn| workload(conn, &local))
        .collect::<rusqlite::Result<Vec<_>>>()?;
    for _ in 0..4 {
        for (best, conn) in best.iter_mut().zip(&conns) {
            for (b, (_, t, _)) in best.iter_mut().zip(workload(conn, &local)?) {
                b.1 = b.1.min(t);
            }
        }
    }

    println!();
    print!("| Step |");
    for (query, ..) in &first[0] {
        print!(" {query} |");
    }
    println!();
    println!("|---{}|", "|---:".repeat(first[0].len()));
    for (((name, ..), first), best) in stages.iter().zip(&first).zip(&best) {
        print!("| {name} |");
        for ((_, first, _), (_, best, _)) in first.iter().zip(best) {
            print!(" {} / {} |", ms(*first), ms(*best));
        }
        println!();
    }
    println!("\n(milliseconds: first run on a new connection / best of 5 more)");

    let latency = std::env::var("EXPORT_BENCH_LATENCY_MS")
        .ok()
        .map(|ms| ms.parse())
        .transpose()?
        .unwrap_or(30);
    let server = Server::start(dir.to_owned(), Duration::from_millis(latency));
    let counts = || server.counts();
    println!();
    print!("| Over HTTP, {latency} ms away | open |");
    for (query, ..) in &first[0] {
        print!(" {query} |");
    }
    println!(" all |");
    println!("|---{}|", "|---:".repeat(first[0].len() + 2));
    for step in [Step::Vacuum, Step::Compress] {
        let name = format!("{step:?}.db");
        let (start, before) = (Instant::now(), counts());
        let conn = if step == Step::Compress {
            bono_export::open_url(&server.url(&name))?
        } else {
            plain_over_http(&server.url(&name))?
        };
        // Opening reads the schema
        conn.query_row("SELECT count(*) FROM sqlite_schema", [], |_| Ok(()))?;
        let opened = counts();
        print!(
            "| {step:?} | {} |",
            fetched(start.elapsed(), (opened.0 - before.0, opened.1 - before.1))
        );
        for (_, took, counted) in workload(&conn, &counts)? {
            print!(" {} |", fetched(took, counted));
        }
        let after = counts();
        println!(
            " {} req, {:.1} of {:.1} MiB |",
            after.0 - before.0,
            mib(after.1 - before.1),
            mib(dir.join(&name).metadata()?.len())
        );
    }
    println!("\n(each query once, in turn on one new connection: milliseconds, requests, MiB)");
    Ok(())
}

fn ms(d: Duration) -> String {
    format!("{:.2}", d.as_secs_f64() * 1000.0)
}

fn mib(bytes: u64) -> f64 {
    bytes as f64 / (1024.0 * 1024.0)
}

fn fetched(took: Duration, (requests, bytes): (u64, u64)) -> String {
    format!(
        "{:.0} ms, {requests}, {:.1}",
        took.as_secs_f64() * 1000.0,
        mib(bytes)
    )
}

/// A plain database at `url`, read through the web VFS.
fn plain_over_http(url: &str) -> anyhow::Result<Connection> {
    bono_export::web::register()?;
    let mut uri = format!(
        "file:{}?vfs={}&mode=ro&immutable=1&web_url=",
        bono_export::web::FILENAME,
        bono_export::web::VFS
    );
    for c in url.chars() {
        match c {
            '%' | '?' | '#' | '&' | '=' => uri.push_str(&format!("%{:02X}", c as u8)),
            _ => uri.push(c),
        }
    }
    Ok(Connection::open_with_flags(
        uri,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_URI,
    )?)
}

fn size_with_wal(path: &Path) -> std::io::Result<u64> {
    let mut wal = path.as_os_str().to_owned();
    wal.push("-wal");
    Ok(path.metadata()?.len() + std::fs::metadata(wal).map_or(0, |m| m.len()))
}

/// A query's name, how long it took, and the requests and bytes it took over HTTP.
type Timed = (&'static str, Duration, (u64, u64));

/// The app's queries on a recording: finding the player, listing laps, and reading laps, cars'
/// states and single rows (see `src/engineer/tool.rs` and `src/spotter.rs`).
///
/// Each query's time, and how much `counts` (requests and bytes, over HTTP) went up meanwhile.
fn workload(conn: &Connection, counts: &dyn Fn() -> (u64, u64)) -> rusqlite::Result<Vec<Timed>> {
    let mut times = Vec::new();
    let mut time = |name, f: &mut dyn FnMut() -> rusqlite::Result<()>| {
        let (start, before) = (Instant::now(), counts());
        f()?;
        let after = counts();
        times.push((
            name,
            start.elapsed(),
            (after.0 - before.0, after.1 - before.1),
        ));
        Ok::<_, rusqlite::Error>(())
    };

    let mut player = (0i64, 0i64);
    time("player's last row", &mut || {
        player = conn.query_row(
            "SELECT id, vehicle_id FROM telemetry WHERE is_player = 1 ORDER BY id DESC LIMIT 1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        Ok(())
    })?;

    let mut laps = Vec::<(i64, i64, i64)>::new();
    time("every car's laps", &mut || {
        laps = conn
            .prepare(
                "SELECT vehicle_id, lap, MIN(id) AS first_id, MAX(id) AS last_id, \
                        MAX(lap_start_time), MAX(lap_invalidated), MAX(in_pit_lane), MAX(speed) \
                 FROM telemetry WHERE id BETWEEN ? AND ? GROUP BY vehicle_id, lap",
            )?
            .query_map(params![0, player.0], |row| {
                Ok((row.get::<_, Option<i64>>(0)?, row.get(2)?, row.get(3)?))
            })?
            .filter_map(|row| match row {
                Ok((Some(vehicle), first, last)) if vehicle == player.1 => {
                    Some(Ok((vehicle, first, last)))
                }
                Ok(_) => None,
                Err(e) => Some(Err(e)),
            })
            .collect::<rusqlite::Result<_>>()?;
        Ok(())
    })?;

    time("player's laps traced", &mut || {
        let mut stmt = conn.prepare(
            "SELECT session_time, lap_distance, speed, brake, throttle, gear FROM telemetry \
             WHERE vehicle_id = ? AND id BETWEEN ? AND ? ORDER BY id",
        )?;
        for lap in &laps {
            stmt.query_map(params![lap.0, lap.1, lap.2], |_| Ok(()))?
                .collect::<rusqlite::Result<Vec<_>>>()?;
        }
        Ok(())
    })?;

    time("player's laps' tyre heat", &mut || {
        let mut stmt = conn.prepare(
            "SELECT w.wheel, AVG(w.tire_surface_temperature_center), \
                    MAX(w.tire_surface_temperature_left), AVG(w.brake_temperature) \
             FROM telemetry t JOIN telemetry_wheel w ON w.telemetry_id = t.id \
             WHERE t.vehicle_id = ? AND t.id BETWEEN ? AND ? GROUP BY w.wheel",
        )?;
        for lap in &laps {
            stmt.query_map(params![lap.0, lap.1, lap.2], |_| Ok(()))?
                .collect::<rusqlite::Result<Vec<_>>>()?;
        }
        Ok(())
    })?;

    let mut random = Lcg(42);
    // The recording's own cars: simulators number them as they like
    let vehicles = conn
        .prepare("SELECT DISTINCT vehicle_id FROM car_state WHERE vehicle_id IS NOT NULL")?
        .query_map([], |row| row.get::<_, i64>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    time("500 cars' states", &mut || {
        let mut stmt = conn.prepare(
            "SELECT * FROM car_state WHERE vehicle_id = ? AND telemetry_id <= ? \
             ORDER BY telemetry_id DESC LIMIT 1",
        )?;
        for _ in 0..if vehicles.is_empty() { 0 } else { 500 } {
            let (vehicle, id) = (
                vehicles[random.below(vehicles.len() as u64) as usize],
                random.below(player.0 as u64) as i64,
            );
            stmt.query_map(params![vehicle, id], |_| Ok(()))?
                .collect::<rusqlite::Result<Vec<_>>>()?;
        }
        Ok(())
    })?;

    time("500 rows with wheels", &mut || {
        let mut row = conn.prepare("SELECT * FROM telemetry WHERE id = ?")?;
        let mut wheels =
            conn.prepare("SELECT * FROM telemetry_wheel WHERE telemetry_id = ? ORDER BY wheel")?;
        for _ in 0..500 {
            let id = 1 + random.below(player.0 as u64) as i64;
            row.query_map([id], |_| Ok(()))?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            wheels
                .query_map([id], |_| Ok(()))?
                .collect::<rusqlite::Result<Vec<_>>>()?;
        }
        Ok(())
    })?;

    time("standings now", &mut || {
        conn.prepare(
            "SELECT * FROM standings WHERE id IN (SELECT MAX(id) FROM standings GROUP BY vehicle_id)",
        )?
        .query_map([], |_| Ok(()))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(())
    })?;

    Ok(times)
}

struct Lcg(u64);

impl Lcg {
    fn below(&mut self, n: u64) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        (self.0 >> 33) % n.max(1)
    }
}

const CARS: i64 = 20;
const HZ: i64 = 50;
/// Opponents are recorded at every 5th update, as by default
const OPPONENT_EVERY: i64 = 5;
const LAP_SECONDS: f64 = 95.0;
const LAP_METRES: f64 = 5000.0;

/// A race of `minutes` in the recorder's schema, with rows as often as the recorder writes them,
/// and every column filled with a smooth curve (some left NULL, as no simulator gives every field).
fn make_up_race(path: &Path, minutes: f64) -> anyhow::Result<()> {
    let conn = Connection::open(path)?;
    conn.execute_batch("PRAGMA journal_mode = WAL; PRAGMA synchronous = NORMAL;")?;
    sql::create_tables(&conn)?;

    let mut telemetry = Insert::new(&conn, "telemetry")?;
    let mut wheels = Insert::new(&conn, "telemetry_wheel")?;
    let mut states = Insert::new(&conn, "car_state")?;
    let mut standings = Insert::new(&conn, "standings")?;
    let mut session = Insert::new(&conn, "session")?;
    let mut forecast = Insert::new(&conn, "forecast")?;
    for point in 0..20 {
        forecast.row(point, 0.0, &[("generation", 1.into())])?;
    }

    let ticks = (minutes * 60.0) as i64 * HZ;
    let mut tx = conn.unchecked_transaction()?;
    for tick in 0..ticks {
        let t = tick as f64 / HZ as f64;
        for car in 0..CARS {
            if car != 0 && tick % OPPONENT_EVERY != car % OPPONENT_EVERY {
                continue;
            }
            let along = t + car as f64 * 3.0;
            let lap = (along / LAP_SECONDS) as i64;
            let id = telemetry.row(
                car,
                t,
                &[
                    ("vehicle_id", car.into()),
                    ("is_player", (car == 0).into()),
                    ("session_time", t.into()),
                    ("lap", lap.into()),
                    (
                        "lap_start_time",
                        (lap as f64 * LAP_SECONDS - car as f64 * 3.0).into(),
                    ),
                    (
                        "lap_distance",
                        (along % LAP_SECONDS / LAP_SECONDS * LAP_METRES).into(),
                    ),
                    ("in_pit_lane", 0.into()),
                ],
            )?;
            for wheel in 0..4 {
                wheels.row(
                    car * 4 + wheel,
                    t,
                    &[("telemetry_id", id.into()), ("wheel", wheel.into())],
                )?;
            }
            // Driver aids, damage and the like change now and then
            if tick % (60 * HZ) < OPPONENT_EVERY {
                states.row(
                    car,
                    t,
                    &[
                        ("telemetry_id", id.into()),
                        ("vehicle_id", car.into()),
                        ("session_time", t.into()),
                    ],
                )?;
            }
        }
        // Each car's standing changes with every sector, the session's every few seconds
        if tick % HZ == 0 {
            for car in 0..CARS {
                if (tick / HZ + car) % 30 == 0 {
                    standings.row(
                        car,
                        t,
                        &[("vehicle_id", car.into()), ("is_player", (car == 0).into())],
                    )?;
                }
            }
            if tick % (3 * HZ) == 0 {
                session.row(0, t, &[("session_time", t.into())])?;
            }
        }
        // The recorder commits about once a second
        if tick % HZ == HZ - 1 {
            tx.commit()?;
            tx = conn.unchecked_transaction()?;
        }
    }
    tx.commit()?;
    Ok(())
}

/// Inserts rows into a table, made up but for the columns given.
struct Insert<'conn> {
    conn: &'conn Connection,
    stmt: rusqlite::Statement<'conn>,
    columns: Vec<(String, String)>,
}

impl<'conn> Insert<'conn> {
    fn new(conn: &'conn Connection, table: &str) -> rusqlite::Result<Self> {
        let columns = conn
            .prepare(
                "SELECT name, type FROM pragma_table_info(?1) WHERE name != 'id' ORDER BY cid",
            )?
            .query_map([table], |row| Ok((row.get(0)?, row.get(1)?)))?
            .collect::<rusqlite::Result<Vec<(String, String)>>>()?;
        let names = columns
            .iter()
            .map(|(name, _)| name.as_str())
            .collect::<Vec<_>>();
        let stmt = conn.prepare(&format!(
            "INSERT INTO {table} ({}) VALUES ({})",
            names.join(", "),
            vec!["?"; names.len()].join(", ")
        ))?;
        Ok(Self {
            conn,
            stmt,
            columns,
        })
    }

    /// Returns the row's rowid.
    fn row(&mut self, key: i64, t: f64, given: &[(&str, Value)]) -> rusqlite::Result<i64> {
        let values = self
            .columns
            .iter()
            .enumerate()
            .map(|(k, (name, kind))| {
                if let Some((_, value)) = given.iter().find(|(given, _)| given == name) {
                    return value.clone();
                }
                if k % 6 == 5 {
                    return Value::Null;
                }
                match kind.as_str() {
                    "REAL" => curve(k, key, t).into(),
                    "INTEGER" => {
                        (((t * 0.01 * (k % 5 + 1) as f64) as i64 + key) % (2 + k as i64 % 9)).into()
                    }
                    _ if k % 3 == 0 => format!("{name} {key}").into(),
                    _ => Value::Null,
                }
            })
            .collect::<Vec<_>>();
        self.stmt.execute(rusqlite::params_from_iter(values))?;
        Ok(self.conn.last_insert_rowid())
    }
}

/// A curve for column `k` of car (or wheel) `key` at `t` seconds, in single precision as the
/// games give it.
fn curve(k: usize, key: i64, t: f64) -> f64 {
    let rate = 0.02 + 0.013 * (k % 11) as f64;
    let scale = 10f64.powi((k % 4) as i32);
    (((t * rate + key as f64 * 0.7 + k as f64).sin() + 1.5) * scale) as f32 as f64
}
