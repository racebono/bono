//! [sqlite_zstd_vfs](https://github.com/mlin/sqlite_zstd_vfs): a SQLite VFS that stores each page of
//! a database zstd-compressed, in an outer SQLite database. Open a database through it with a
//! `file:` URI naming it, `?vfs=zstd`, once `register` has run. Compressing a database is a
//! `VACUUM INTO 'file:...?vfs=zstd'`; see the upstream README for the URI parameters (`level`,
//! `threads`, `outer_page_size`, `outer_unsafe`, ...).

// Linked for the C++ code, which calls zstd directly
extern crate zstd_sys;

use rusqlite::ffi;
use std::{
    ffi::{CStr, c_char, c_int},
    path::{Component, Path, Prefix},
    sync::OnceLock,
};

/// The VFS's name, for `vfs=` in a URI.
pub const VFS: &str = "zstd";

unsafe extern "C" {
    fn bono_zstd_vfs_register(error: *mut *const c_char) -> c_int;
}

/// Registers the VFS with SQLite, for every connection opened afterwards. Only the first call
/// registers it; the rest return its result.
pub fn register() -> rusqlite::Result<()> {
    static REGISTERED: OnceLock<Result<(), (c_int, &'static str)>> = OnceLock::new();
    let result = REGISTERED.get_or_init(|| {
        let mut error: *const c_char = std::ptr::null();
        // Safety: `error` is written only with a static string, when it fails
        let rc = unsafe { bono_zstd_vfs_register(&mut error) };
        if rc == ffi::SQLITE_OK {
            return Ok(());
        }
        let message = if error.is_null() {
            "couldn't register the zstd VFS"
        } else {
            // Safety: a static, nul-terminated string
            unsafe { CStr::from_ptr(error) }
                .to_str()
                .unwrap_or("couldn't register the zstd VFS")
        };
        Err((rc, message))
    });
    result.map_err(|(rc, message)| {
        rusqlite::Error::SqliteFailure(ffi::Error::new(rc), Some(message.to_owned()))
    })
}

/// `path` as a `file:` URI, for appending `?` and query parameters to. Relative paths are made
/// absolute first, against the current directory.
pub fn uri(path: &Path) -> std::io::Result<String> {
    let invalid = |why: &str| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("{why}: {}", path.display()),
        )
    };

    let path = std::path::absolute(path)?;
    let mut uri = String::from("file://");
    for component in path.components() {
        match component {
            // `file:///C:/dir`, as SQLite's URI filenames spell Windows drives
            Component::Prefix(prefix) => match prefix.kind() {
                Prefix::Disk(drive) | Prefix::VerbatimDisk(drive) => {
                    uri.push('/');
                    uri.push(char::from(drive));
                    uri.push(':');
                }
                Prefix::UNC(server, share) | Prefix::VerbatimUNC(server, share) => {
                    uri.push_str("//");
                    escape(
                        &mut uri,
                        server.to_str().ok_or_else(|| invalid("not UTF-8"))?,
                    );
                    uri.push('/');
                    escape(
                        &mut uri,
                        share.to_str().ok_or_else(|| invalid("not UTF-8"))?,
                    );
                }
                _ => return Err(invalid("unsupported path prefix")),
            },
            Component::RootDir => uri.push('/'),
            Component::Normal(name) => {
                if !uri.ends_with('/') {
                    uri.push('/');
                }
                escape(&mut uri, name.to_str().ok_or_else(|| invalid("not UTF-8"))?);
            }
            // `absolute` keeps `..` (it doesn't resolve symlinks), which SQLite resolves too
            Component::ParentDir => {
                if !uri.ends_with('/') {
                    uri.push('/');
                }
                uri.push_str("..");
            }
            Component::CurDir => {}
        }
    }
    Ok(uri)
}

/// Percent-encodes what a URI filename's path can't hold as is.
fn escape(uri: &mut String, name: &str) {
    for byte in name.bytes() {
        match byte {
            b'%' | b'?' | b'#' | b'/' | b'\\' | b'&' | b'=' | 0..=b' ' | 0x7f.. => {
                uri.push_str(&format!("%{byte:02X}"));
            }
            _ => uri.push(char::from(byte)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::{Connection, OpenFlags};

    #[test]
    fn compresses_and_reads_back() {
        register().unwrap();
        register().unwrap();

        let dir = tempfile::tempdir().unwrap();
        let plain = dir.path().join("plain.db");
        let packed = dir.path().join("100% packed #1.db");

        let conn = Connection::open(&plain).unwrap();
        conn.execute_batch(
            "PRAGMA page_size = 16384;
             CREATE TABLE t (id INTEGER PRIMARY KEY, x REAL, s TEXT);
             WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM n WHERE i < 20000)
             INSERT INTO t SELECT i, i * 0.5, 'row ' || (i % 100) FROM n;",
        )
        .unwrap();
        conn.execute(
            "VACUUM INTO ?1",
            [format!("{}?vfs={VFS}&level=6", uri(&packed).unwrap())],
        )
        .unwrap();
        drop(conn);
        assert!(packed.metadata().unwrap().len() < plain.metadata().unwrap().len());

        let conn = Connection::open_with_flags(
            format!("{}?vfs={VFS}&mode=ro", uri(&packed).unwrap()),
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_URI,
        )
        .unwrap();
        let (count, sum, page_size): (i64, f64, i64) = conn
            .query_row(
                "SELECT count(*), sum(x), (SELECT page_size FROM pragma_page_size) FROM t",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        assert_eq!((count, sum, page_size), (20000, 100_005_000.0, 16384));
    }

    #[test]
    fn uris() {
        let uri = uri(Path::new("dir/a b?.db")).unwrap();
        assert!(uri.starts_with("file:///"), "{uri}");
        assert!(uri.ends_with("/dir/a%20b%3F.db"), "{uri}");
        assert!(!uri.contains('\\'), "{uri}");
    }
}
