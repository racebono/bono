//! Exports a SQLite database into one small copy that's fast to read, for keeping: a snapshot of it,
//! moved out of WAL mode onto 16 KiB pages, given extra indexes and the query planner's statistics,
//! vacuumed, then compressed page by page with zstd (see `vfs`). Read it with `open`: it's no
//! longer a plain SQLite file.
//!
//! The export is checked against the snapshot before it's kept: every table's rows, every index,
//! view and trigger, and the database's version numbers.
//!
//! Everything here blocks, for as long as reading and writing the whole database takes (seconds
//! for a race, more for a weekend): call it from a worker thread.

pub mod vfs;

use anyhow::{Context, bail, ensure};
use rusqlite::{Connection, OpenFlags, config::DbConfig, types::ValueRef};
use sha2::{Digest, Sha256};
use std::{collections::HashSet, fs::OpenOptions, path::Path, time::Duration};

/// The export's page size. Bigger pages compress better, but every page read is decompressed whole.
pub const PAGE_SIZE: i64 = 16384;
/// The zstd level pages are compressed at. Decompressing is as fast at any level; compressing is
/// slower the higher it is, and no smaller past 19 for pages this size.
pub const LEVEL: i32 = 19;
/// Pages of the outer database, which holds each compressed page as a row. Big ones fit several
/// compressed pages each: smaller and faster to read than with SQLite's 4 KiB (see the benchmark).
const OUTER_PAGE_SIZE: i64 = 65536;
/// Page caches of each connection `open` makes, in KiB, for the pages (decompressed) and the outer
/// database's: sqlite_zstd_vfs recommends more than SQLite's 2 MiB default, as each page missing
/// from them is read and decompressed again. Only filled as pages are read.
const CACHE_KIB: i64 = 32 * 1024;

/// The steps of an export, in order.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Step {
    /// A consistent copy of the source, as it was when the export started.
    Snapshot,
    /// Out of WAL mode, onto `PAGE_SIZE` pages.
    PageSize,
    /// The caller's schema changes and indexes.
    Schema,
    /// The query planner's statistics (`ANALYZE`).
    Statistics,
    /// Vacuumed once more, after the steps that grew it.
    Vacuum,
    /// Compressed, and checked against the snapshot: the export.
    Compress,
}

impl Step {
    pub const ALL: [Step; 6] = [
        Step::Snapshot,
        Step::PageSize,
        Step::Schema,
        Step::Statistics,
        Step::Vacuum,
        Step::Compress,
    ];
}

/// Exports the database at `source` into `dest`, replacing it if it exists. `schema` is SQL run on
/// the copy after it's moved onto bigger pages, for indexes that help reading it (empty for none).
/// It may add tables, columns, indexes and the like, but the export fails if it removes or changes
/// anything of the source's.
///
/// `source` is only read, so another process may keep writing it: the export has what it held
/// when the export started. Temporary files go next to `dest`, and are gone when this returns.
pub fn export(source: &Path, dest: &Path, schema: &str) -> anyhow::Result<()> {
    export_with(source, dest, schema, |_, _| Ok(()))
}

/// `export`, calling `after` with the database as each step leaves it (for `Step::Compress`, the
/// export, just before it's moved to `dest`). It must not change it.
pub fn export_with(
    source: &Path,
    dest: &Path,
    schema: &str,
    mut after: impl FnMut(Step, &Path) -> anyhow::Result<()>,
) -> anyhow::Result<()> {
    vfs::register()?;
    let source = std::path::absolute(source)?;
    let dest = std::path::absolute(dest)?;
    if let (Ok(a), Ok(b)) = (source.canonicalize(), dest.canonicalize()) {
        ensure!(a != b, "can't export {} into itself", source.display());
    }

    let dir = dest
        .parent()
        .with_context(|| format!("{} has no folder", dest.display()))?;
    let temp = tempfile::Builder::new()
        .prefix(".export-")
        .tempdir_in(dir)
        .with_context(|| format!("couldn't make a temporary folder in {}", dir.display()))?;
    let work = temp.path().join("work.db");
    let packed = temp.path().join("packed.db");

    // Read-only: whatever happens here, the source is never written. VACUUM INTO only reads it
    {
        let conn = Connection::open_with_flags(
            &source,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
        .with_context(|| format!("couldn't open {}", source.display()))?;
        // Its writer can briefly hold it (a checkpoint)
        conn.busy_timeout(Duration::from_secs(1))?;
        conn.execute("VACUUM INTO ?1", [path_str(&work)?])
            .context("couldn't snapshot the database")?;
    }
    let snapshot = Contents::read(&read_only(&work)?)?;
    after(Step::Snapshot, &work)?;

    let mut conn = Connection::open_with_flags(
        &work,
        OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    // The page size can't change in WAL mode, and the VFS doesn't support it. No journal and no
    // syncing either: a crash leaves a broken copy, but it's a temporary one, and VACUUM takes a
    // fraction of the time without (seconds instead of a minute, for a race)
    let mode: String =
        conn.pragma_update_and_check(None, "journal_mode", "OFF", |row| row.get(0))?;
    ensure!(
        mode.eq_ignore_ascii_case("off"),
        "the copy stayed in {mode} mode"
    );
    conn.pragma_update(None, "synchronous", "OFF")?;
    conn.pragma_update(None, "page_size", PAGE_SIZE)?;
    conn.execute_batch("VACUUM")?;
    check_format(&conn)?;
    after(Step::PageSize, &work)?;

    let tx = conn.transaction()?;
    tx.execute_batch(schema)
        .context("couldn't apply the export's schema")?;
    tx.commit()?;
    after(Step::Schema, &work)?;

    conn.execute_batch("ANALYZE")?;
    after(Step::Statistics, &work)?;

    conn.execute_batch("VACUUM")?;
    after(Step::Vacuum, &work)?;

    // `outer_unsafe` skips the outer database's journal: a crash leaves a broken file, but it's a
    // temporary one. `threads=-1` compresses on every core
    conn.execute(
        "VACUUM INTO ?1",
        [format!(
            "{}?vfs={}&level={LEVEL}&outer_page_size={OUTER_PAGE_SIZE}&outer_unsafe=true&threads=-1",
            vfs::uri(&packed)?,
            vfs::VFS
        )],
    )
    .context("couldn't compress the database")?;
    drop(conn);

    {
        let conn = open(&packed)?;
        check_format(&conn)?;
        snapshot
            .check(&conn)
            .context("the export doesn't match the database")?;
    }
    // Written without syncing (`outer_unsafe`), so it's synced before it replaces `dest`. Opened
    // for writing, which Windows needs to flush it
    OpenOptions::new().write(true).open(&packed)?.sync_all()?;
    after(Step::Compress, &packed)?;
    std::fs::rename(&packed, &dest)
        .with_context(|| format!("couldn't move the export to {}", dest.display()))?;
    Ok(())
}

/// Opens a database `export` made, read-only.
pub fn open(path: &Path) -> anyhow::Result<Connection> {
    vfs::register()?;
    let conn = Connection::open_with_flags(
        format!(
            "{}?vfs={}&mode=ro&outer_cache_size=-{CACHE_KIB}",
            vfs::uri(path)?,
            vfs::VFS
        ),
        OpenFlags::SQLITE_OPEN_READ_ONLY
            | OpenFlags::SQLITE_OPEN_URI
            | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .with_context(|| format!("couldn't open {}", path.display()))?;
    conn.pragma_update(None, "cache_size", -CACHE_KIB)?;
    Ok(conn)
}

fn read_only(path: &Path) -> anyhow::Result<Connection> {
    Ok(Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?)
}

fn path_str(path: &Path) -> anyhow::Result<&str> {
    path.to_str()
        .with_context(|| format!("{} isn't UTF-8", path.display()))
}

/// Checks the copy is on `PAGE_SIZE` pages and out of WAL mode.
fn check_format(conn: &Connection) -> anyhow::Result<()> {
    let page_size: i64 = conn.pragma_query_value(None, "page_size", |row| row.get(0))?;
    ensure!(
        page_size == PAGE_SIZE,
        "the copy has {page_size} byte pages, not {PAGE_SIZE}"
    );
    let mode: String = conn.pragma_query_value(None, "journal_mode", |row| row.get(0))?;
    ensure!(!mode.eq_ignore_ascii_case("wal"), "the copy is in WAL mode");
    Ok(())
}

/// What a database holds, to check a copy has all of it.
struct Contents {
    tables: Vec<Table>,
    /// Indexes, views and triggers: type, name, table and SQL.
    objects: Vec<[Option<String>; 4]>,
    user_version: i64,
    application_id: i64,
}

struct Table {
    name: String,
    columns: Vec<String>,
    rows: u64,
    digest: [u64; 4],
}

impl Contents {
    fn read(conn: &Connection) -> anyhow::Result<Self> {
        // Shadow tables hold virtual tables' data; the planner's statistics aren't data
        let names = conn
            .prepare(
                "SELECT name FROM pragma_table_list WHERE schema = 'main' \
                 AND type IN ('table', 'shadow') \
                 AND name NOT IN ('sqlite_schema', 'sqlite_master') \
                 AND name NOT LIKE 'sqlite\\_stat%' ESCAPE '\\' ORDER BY name",
            )?
            .query_map([], |row| row.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;

        let mut tables = Vec::with_capacity(names.len());
        for name in names {
            let columns = conn
                .prepare("SELECT name FROM pragma_table_info(?1) ORDER BY cid")?
                .query_map([&name], |row| row.get::<_, String>(0))?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            let (rows, digest) = digest(conn, &name, &columns)?;
            tables.push(Table {
                name,
                columns,
                rows,
                digest,
            });
        }

        let objects = conn
            .prepare(
                "SELECT type, name, tbl_name, sql FROM sqlite_schema WHERE type != 'table' \
                 ORDER BY type, name",
            )?
            .query_map([], |row| {
                Ok([row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?])
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;

        Ok(Self {
            tables,
            objects,
            user_version: conn.pragma_query_value(None, "user_version", |row| row.get(0))?,
            application_id: conn.pragma_query_value(None, "application_id", |row| row.get(0))?,
        })
    }

    /// Checks `conn` has all of this: the same rows in each table (in the same columns, though it
    /// can have more), the same indexes, views and triggers, and the same version numbers.
    fn check(&self, conn: &Connection) -> anyhow::Result<()> {
        for table in &self.tables {
            let (rows, digest) = digest(conn, &table.name, &table.columns)
                .with_context(|| format!("couldn't read table {}", table.name))?;
            ensure!(
                rows == table.rows,
                "table {} has {rows} rows instead of {}",
                table.name,
                table.rows
            );
            ensure!(
                digest == table.digest,
                "table {}'s rows changed",
                table.name
            );
        }

        let objects = conn
            .prepare("SELECT type, name, tbl_name, sql FROM sqlite_schema WHERE type != 'table'")?
            .query_map([], |row| {
                Ok([row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?])
            })?
            .collect::<rusqlite::Result<HashSet<[Option<String>; 4]>>>()?;
        for object in &self.objects {
            if !objects.contains(object) {
                bail!(
                    "{} {} is missing or changed",
                    object[0].as_deref().unwrap_or_default(),
                    object[1].as_deref().unwrap_or_default()
                );
            }
        }

        let user_version: i64 = conn.pragma_query_value(None, "user_version", |row| row.get(0))?;
        let application_id: i64 =
            conn.pragma_query_value(None, "application_id", |row| row.get(0))?;
        ensure!(
            (user_version, application_id) == (self.user_version, self.application_id),
            "the database's user_version or application_id changed"
        );
        Ok(())
    }
}

/// The number of rows in `table`, and a digest of their values in `columns` that doesn't depend
/// on the order they're read in: the sum of each row's SHA-256, in four 64-bit lanes. Rowids are
/// left out unless they're a column (`INTEGER PRIMARY KEY`), as VACUUM may change the others.
fn digest(conn: &Connection, table: &str, columns: &[String]) -> anyhow::Result<(u64, [u64; 4])> {
    ensure!(!columns.is_empty(), "table {table} has no columns");
    // Or a column that's gone would read as a string of its name
    conn.set_db_config(DbConfig::SQLITE_DBCONFIG_DQS_DML, false)?;
    let mut stmt = conn.prepare(&format!(
        "SELECT {} FROM {}",
        columns
            .iter()
            .map(|c| quote(c))
            .collect::<Vec<_>>()
            .join(", "),
        quote(table)
    ))?;
    let mut rows = stmt.query([])?;
    let (mut count, mut sum) = (0u64, [0u64; 4]);
    while let Some(row) = rows.next()? {
        let mut hash = Sha256::new();
        for i in 0..columns.len() {
            match row.get_ref(i)? {
                ValueRef::Null => hash.update([0]),
                ValueRef::Integer(v) => {
                    hash.update([1]);
                    hash.update(v.to_le_bytes());
                }
                ValueRef::Real(v) => {
                    hash.update([2]);
                    hash.update(v.to_bits().to_le_bytes());
                }
                ValueRef::Text(v) => {
                    hash.update([3]);
                    hash.update((v.len() as u64).to_le_bytes());
                    hash.update(v);
                }
                ValueRef::Blob(v) => {
                    hash.update([4]);
                    hash.update((v.len() as u64).to_le_bytes());
                    hash.update(v);
                }
            }
        }
        let hash = hash.finalize();
        for (lane, bytes) in sum.iter_mut().zip(hash.as_chunks::<8>().0) {
            *lane = lane.wrapping_add(u64::from_le_bytes(*bytes));
        }
        count += 1;
    }
    Ok((count, sum))
}

/// `name` as an SQL identifier.
fn quote(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    /// A database like a recording: in WAL mode, still open by its writer, with rows only in the WAL.
    fn recording(dir: &Path) -> (PathBuf, Connection) {
        let path = dir.join("telemetry.db");
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch(
            "PRAGMA journal_mode = WAL;
             PRAGMA wal_autocheckpoint = 0;
             PRAGMA user_version = 7;
             CREATE TABLE telemetry (id INTEGER PRIMARY KEY, vehicle_id INTEGER, speed REAL, name TEXT);
             CREATE TABLE wheel (telemetry_id INTEGER, wheel INTEGER, temp REAL,
                 PRIMARY KEY (telemetry_id, wheel)) WITHOUT ROWID;
             CREATE TABLE notes (body BLOB, n);
             CREATE INDEX notes_by_n ON notes (n);
             CREATE VIEW fast AS SELECT * FROM telemetry WHERE speed > 50;
             WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM n WHERE i < 5000)
             INSERT INTO telemetry SELECT i, i % 20, i * 0.25, 'car ' || (i % 20) FROM n;
             INSERT INTO wheel SELECT id, w, speed + w FROM telemetry,
                 (SELECT 0 AS w UNION ALL SELECT 1 UNION ALL SELECT 2 UNION ALL SELECT 3);
             INSERT INTO notes VALUES (x'00ff', 1), (x'00ff', 1), (NULL, 2.5), ('text', -0.0);",
        )
        .unwrap();
        (path, conn)
    }

    fn file_digest(path: &Path) -> Vec<u8> {
        Sha256::digest(std::fs::read(path).unwrap()).to_vec()
    }

    #[test]
    fn exports_everything_without_touching_the_source() {
        let dir = tempfile::tempdir().unwrap();
        let (source, writer) = recording(dir.path());
        let wal = dir.path().join("telemetry.db-wal");
        let before = (file_digest(&source), file_digest(&wal));

        let dest = dir.path().join("export.db");
        let mut steps = Vec::new();
        export_with(
            &source,
            &dest,
            "CREATE INDEX telemetry_by_vehicle ON telemetry (vehicle_id);",
            |step, _| {
                steps.push(step);
                Ok(())
            },
        )
        .unwrap();
        assert_eq!(steps, Step::ALL);
        assert_eq!((file_digest(&source), file_digest(&wal)), before);
        // The temporary folder is gone
        let left: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        assert_eq!(left.len(), 4, "{left:?}");

        let conn = open(&dest).unwrap();
        check_format(&conn).unwrap();
        Contents::read(&read_only(&source).unwrap())
            .unwrap()
            .check(&conn)
            .unwrap();
        let (rows, indexed, analyzed, user_version): (i64, bool, bool, i64) = conn
            .query_row(
                "SELECT (SELECT count(*) FROM wheel),
                    EXISTS (SELECT 1 FROM sqlite_schema WHERE name = 'telemetry_by_vehicle'),
                    EXISTS (SELECT 1 FROM sqlite_schema WHERE name = 'sqlite_stat1'),
                    (SELECT user_version FROM pragma_user_version)",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .unwrap();
        assert_eq!(
            (rows, indexed, analyzed, user_version),
            (20000, true, true, 7)
        );
        drop(writer);
    }

    #[test]
    fn refuses_to_lose_anything() {
        let dir = tempfile::tempdir().unwrap();
        let (source, _writer) = recording(dir.path());
        let dest = dir.path().join("export.db");

        for (schema, why) in [
            ("DROP INDEX notes_by_n", "index notes_by_n is missing"),
            (
                "DELETE FROM notes WHERE rowid = (SELECT min(rowid) FROM notes)",
                "table notes has 3 rows instead of 4",
            ),
            (
                "UPDATE wheel SET temp = temp + 1e-9 WHERE telemetry_id = 4000",
                "table wheel's rows changed",
            ),
            (
                "UPDATE notes SET n = 0.0 WHERE n = -0.0",
                "table notes's rows changed",
            ),
            (
                "ALTER TABLE telemetry DROP COLUMN name",
                "couldn't read table telemetry",
            ),
            (
                "ALTER TABLE notes RENAME COLUMN n TO m",
                "couldn't read table notes",
            ),
            ("PRAGMA user_version = 8", "user_version"),
        ] {
            let error = format!("{:#}", export(&source, &dest, schema).unwrap_err());
            assert!(error.contains(why), "{schema}: {error}");
            assert!(!dest.exists(), "{schema}");
        }
        // Adding is fine
        export(
            &source,
            &dest,
            "ALTER TABLE notes ADD COLUMN extra; CREATE TABLE more (x)",
        )
        .unwrap();
    }

    #[test]
    fn refuses_to_export_into_itself() {
        let dir = tempfile::tempdir().unwrap();
        let (source, _writer) = recording(dir.path());
        let before = file_digest(&source);
        assert!(export(&source, &source, "").is_err());
        assert_eq!(file_digest(&source), before);
    }
}
