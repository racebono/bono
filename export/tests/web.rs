//! Reading exports over HTTP, from a local server.

mod serve;

use rusqlite::{Connection, OpenFlags};
use serve::Server;
use std::{path::Path, sync::atomic::Ordering, time::Duration};

/// A database of `rows` rows of noise, which compresses about as well as telemetry, exported into
/// `dir/export.db`.
fn exported(dir: &Path, rows: u64) {
    let source = dir.join("source.db");
    let conn = Connection::open(&source).unwrap();
    conn.execute_batch(&format!(
        "CREATE TABLE t (id INTEGER PRIMARY KEY, x REAL, s TEXT);
         WITH RECURSIVE n(i, r) AS (
             SELECT 1, 12345 UNION ALL
             SELECT i + 1, (r * 1103515245 + 12345) % 2147483648 FROM n WHERE i < {rows}
         )
         INSERT INTO t SELECT i, r / 1e6, 'car ' || (r % 40) FROM n;"
    ))
    .unwrap();
    drop(conn);
    bono_export::export(
        &source,
        &dir.join("export.db"),
        "CREATE INDEX t_s ON t (s);",
    )
    .unwrap();
}

fn summary(conn: &Connection) -> (i64, f64, i64, f64) {
    conn.query_row(
        "SELECT count(*), sum(x), (SELECT count(*) FROM t WHERE s = 'car 7'),
                (SELECT x FROM t WHERE id = 123456) FROM t",
        [],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
    )
    .unwrap()
}

#[test]
fn reads_an_export_over_http() {
    let dir = tempfile::tempdir().unwrap();
    exported(dir.path(), 300_000);
    let size = dir.path().join("export.db").metadata().unwrap().len();
    let server = Server::start(dir.path().to_owned(), Duration::ZERO);

    let local = summary(&bono_export::open(&dir.path().join("export.db")).unwrap());
    let remote = bono_export::open_url(&server.url("export.db")).unwrap();
    assert_eq!(summary(&remote), local);
    assert!(
        remote.execute("CREATE TABLE u (x)", []).is_err(),
        "it's read-only"
    );

    // Looking one row up reads a little of the file, not all of it
    let (requests, bytes) = server.counts();
    let conn = bono_export::open_url(&server.url("export.db")).unwrap();
    let x: f64 = conn
        .query_row("SELECT x FROM t WHERE id = 7", [], |row| row.get(0))
        .unwrap();
    assert!(x > 0.0);
    let (requests, bytes) = (server.counts().0 - requests, server.counts().1 - bytes);
    assert!(requests < 20, "{requests} requests");
    assert!(bytes < size / 4, "{bytes} of {size} bytes");
}

#[test]
fn reads_a_plain_database_over_http() {
    let dir = tempfile::tempdir().unwrap();
    let conn = Connection::open(dir.path().join("plain.db")).unwrap();
    conn.execute_batch(
        "CREATE TABLE t (x); WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM n WHERE i < 50000)
         INSERT INTO t SELECT i FROM n;",
    )
    .unwrap();
    drop(conn);
    let server = Server::start(dir.path().to_owned(), Duration::ZERO);

    bono_export::web::register().unwrap();
    let conn = Connection::open_with_flags(
        format!(
            "file:{}?vfs={}&mode=ro&immutable=1&web_url={}",
            bono_export::web::FILENAME,
            bono_export::web::VFS,
            server
                .url("plain.db")
                .replace(':', "%3A")
                .replace('/', "%2F")
        ),
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_URI,
    )
    .unwrap();
    let sum: i64 = conn
        .query_row("SELECT sum(x) FROM t", [], |row| row.get(0))
        .unwrap();
    assert_eq!(sum, 50000 * 50001 / 2);
}

#[test]
fn says_why_it_cant_open_one() {
    let dir = tempfile::tempdir().unwrap();
    exported(dir.path(), 1000);
    let server = Server::start(dir.path().to_owned(), Duration::ZERO);

    let error = bono_export::open_url(&server.url("missing.db?signature=secret"))
        .err()
        .unwrap();
    let message = format!("{error:#}");
    assert!(message.contains("404"), "{message}");
    assert!(!message.contains("secret"), "{message}");
    // The HTTP client's own errors too
    for url in [
        "/missing.db?signature=secret",
        "/missing.db?signature=secret#fragment",
    ] {
        let message = format!("{:#}", bono_export::open_url(url).err().unwrap());
        assert!(!message.contains("secret"), "{message}");
    }

    // An empty file isn't a database
    std::fs::write(dir.path().join("empty.db"), b"").unwrap();
    assert!(bono_export::open_url(&server.url("empty.db")).is_err());

    server.ranges.store(false, Ordering::Relaxed);
    let message = format!(
        "{:#}",
        bono_export::open_url(&server.url("export.db"))
            .err()
            .unwrap()
    );
    assert!(message.contains("range requests"), "{message}");
}

#[test]
fn notices_the_file_changing() {
    let dir = tempfile::tempdir().unwrap();
    exported(dir.path(), 300_000);
    let server = Server::start(dir.path().to_owned(), Duration::ZERO);
    let conn = bono_export::open_url(&server.url("export.db")).unwrap();
    conn.query_row("SELECT x FROM t WHERE id = 1", [], |_| Ok(()))
        .unwrap();

    // Another export in its place
    let other = tempfile::tempdir().unwrap();
    exported(other.path(), 200_000);
    std::fs::rename(other.path().join("export.db"), dir.path().join("export.db")).unwrap();
    assert!(
        conn.query_row("SELECT sum(x) FROM t", [], |_| Ok(()))
            .is_err()
    );
}
