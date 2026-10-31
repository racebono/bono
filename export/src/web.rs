//! A read-only SQLite VFS that reads a database over HTTP(S), with range requests: only the parts
//! queries touch are downloaded. sqlite_zstd_vfs reads an export through it when it's opened as
//! `file:/__web__?vfs=zstd&mode=ro&immutable=1&web_url=...` (see `crate::open_url`): upstream's
//! README documents that URI, for its own `web` VFS (sqlite_web_vfs, which needs libcurl and
//! doesn't build on Windows), and the vendored `SQLiteNestedVFS.h` opens the outer database through
//! the VFS named `web`, with `web_url` passed on. A plain database can be read through it directly,
//! as `file:/__web__?vfs=web&mode=ro&immutable=1&web_url=...`.
//!
//! The file is read in `CHUNK`s, kept in memory (up to `CACHE`), reading further ahead as reads go
//! on in order. It must not change while it's open: each response's ETag must be the first's.
//!
//! Every read blocks for as long as its request takes: use connections reading through it from a
//! worker thread.

use rusqlite::ffi;
use std::{
    cell::RefCell,
    collections::HashMap,
    ffi::{CStr, c_char, c_int, c_void},
    sync::{Mutex, OnceLock},
    time::Duration,
};

/// The VFS's name, for `vfs=` in a URI. sqlite_zstd_vfs reads `/__web__` through the VFS of this
/// name.
pub const VFS: &str = "web";
/// The filename that URIs read through the VFS name, as sqlite_zstd_vfs expects.
pub const FILENAME: &str = "/__web__";

/// The file is fetched and cached in pieces this size, at offsets they're multiples of: exports'
/// outer pages, so a page is one piece.
const CHUNK: u64 = 64 * 1024;
/// How many chunks one request reads ahead at most, when reads go on in order (4 MiB).
const MAX_AHEAD: u64 = 64;
/// Each open file keeps up to this many bytes of chunks (64 MiB).
const CACHE: u64 = 1024 * CHUNK;
/// Attempts at a request that fails to connect, or with a server error.
const ATTEMPTS: u32 = 3;

/// Registers the VFS with SQLite, for every connection opened afterwards. Only the first call
/// registers it; the rest return its result.
pub fn register() -> rusqlite::Result<()> {
    static REGISTERED: OnceLock<c_int> = OnceLock::new();
    let rc = *REGISTERED.get_or_init(|| {
        // Safety: SQLite is initialized by `sqlite3_vfs_find`, and the VFS is leaked, so it lives
        // as long as SQLite might use it
        unsafe {
            let real = ffi::sqlite3_vfs_find(std::ptr::null());
            if real.is_null() {
                return ffi::SQLITE_ERROR;
            }
            let vfs = Box::leak(Box::new(ffi::sqlite3_vfs {
                iVersion: 2,
                szOsFile: (*real).szOsFile.max(size_of::<File>() as c_int),
                mxPathname: (*real).mxPathname,
                pNext: std::ptr::null_mut(),
                zName: c"web".as_ptr(),
                pAppData: real.cast(),
                xOpen: Some(open),
                xDelete: Some(delete),
                xAccess: Some(access),
                xFullPathname: Some(full_pathname),
                xDlOpen: Some(dl_open),
                xDlError: Some(dl_error),
                xDlSym: Some(dl_sym),
                xDlClose: Some(dl_close),
                xRandomness: Some(randomness),
                xSleep: Some(sleep),
                xCurrentTime: Some(current_time),
                xGetLastError: Some(get_last_error),
                xCurrentTimeInt64: Some(current_time_int64),
                xSetSystemCall: None,
                xGetSystemCall: None,
                xNextSystemCall: None,
            }));
            ffi::sqlite3_vfs_register(vfs, 0)
        }
    });
    if rc == ffi::SQLITE_OK {
        Ok(())
    } else {
        Err(rusqlite::Error::SqliteFailure(
            ffi::Error::new(rc),
            Some("couldn't register the web VFS".to_owned()),
        ))
    }
}

thread_local! {
    /// Why the last file this thread opened couldn't be, for `take_error`.
    static ERROR: RefCell<Option<String>> = const { RefCell::new(None) };
}

/// Why the last database this thread tried to open through the VFS couldn't be, which SQLite only
/// reports as "unable to open database file".
pub(crate) fn take_error() -> Option<String> {
    ERROR.with_borrow_mut(Option::take)
}

/// `url` without what can hold credentials, for messages: its query (a presigned URL's signature)
/// and fragment, and its user and password.
pub(crate) fn shown(url: &str) -> String {
    let url = url.split(['?', '#']).next().unwrap_or(url);
    userinfo(url).map_or_else(|| url.to_owned(), |info| url.replacen(info, "", 1))
}

/// `url`'s `user:password@`, if it has one.
fn userinfo(url: &str) -> Option<&str> {
    let start = url.find("://")? + 3;
    let authority = url[start..].split(['/', '?', '#']).next()?;
    let end = authority.rfind('@')? + 1;
    Some(&authority[..end])
}

/// What the VFS gives SQLite for each open file. Files that aren't a database read over HTTP are
/// the default VFS's, in the same memory (`szOsFile` fits either).
#[repr(C)]
struct File {
    base: ffi::sqlite3_file,
    remote: *mut Remote,
}

/// A file on a server, as far as it's been read.
struct Remote {
    url: String,
    agent: ureq::Agent,
    size: u64,
    etag: Option<String>,
    state: Mutex<State>,
}

/// A response: bytes of the file (fewer at its end), its size and its ETag.
struct Got {
    data: Vec<u8>,
    size: u64,
    etag: Option<String>,
}

#[derive(Default)]
struct State {
    /// Chunks by index, with when each was last used.
    chunks: HashMap<u64, (Box<[u8]>, u64)>,
    clock: u64,
    /// Where the last read ended, to tell reads in order.
    next: u64,
    /// How many chunks past a read in order to fetch with it.
    ahead: u64,
}

impl Remote {
    fn open(url: String) -> Result<Self, String> {
        let agent = ureq::Agent::config_builder()
            .http_status_as_error(false)
            .timeout_connect(Some(Duration::from_secs(10)))
            .timeout_global(Some(Duration::from_secs(60)))
            .build()
            .into();
        // A fragment isn't sent, and the client leaves it out of its errors, which `redact` matches
        let url = match url.split_once('#') {
            Some((url, _)) => url.to_owned(),
            None => url,
        };
        let mut remote = Self {
            url,
            agent,
            size: 0,
            etag: None,
            state: Mutex::default(),
        };
        // The first chunk tells the file's size, and SQLite reads it first anyway
        let Got { data, size, etag } = remote.get(0, CHUNK - 1)?;
        remote.size = size;
        remote.etag = etag;
        let state = remote.state.get_mut().unwrap();
        state.chunks.insert(0, (data.into_boxed_slice(), 0));
        Ok(remote)
    }

    /// `text` (an HTTP client's error) without what of the URL can hold credentials (see `shown`).
    fn redact(&self, text: &str) -> String {
        let mut text = text.replace(&self.url, &shown(&self.url));
        if let Some((_, query)) = self.url.split_once('?')
            && !query.is_empty()
        {
            text = text.replace(query, "...");
        }
        if let Some(info) = userinfo(&self.url) {
            text = text.replace(info, "");
        }
        text
    }

    /// Bytes `first..=last` of the file.
    fn get(&self, first: u64, last: u64) -> Result<Got, String> {
        let mut attempt = 1;
        loop {
            match self.try_get(first, last) {
                Ok(got) => return Ok(got),
                Err((error, retry)) if !retry || attempt == ATTEMPTS => {
                    return Err(format!("{}: {error}", shown(&self.url)));
                }
                Err(_) => {
                    std::thread::sleep(Duration::from_millis(250) * attempt);
                    attempt += 1;
                }
            }
        }
    }

    /// `get`, once. Errors say whether trying again might help.
    fn try_get(&self, first: u64, last: u64) -> Result<Got, (String, bool)> {
        let mut response = self
            .agent
            .get(&self.url)
            .header("Range", format!("bytes={first}-{last}"))
            .call()
            .map_err(|e| (self.redact(&e.to_string()), true))?;
        let status = response.status();
        if status == 200 {
            return Err(("the server doesn't answer range requests".into(), false));
        }
        if status != 206 {
            let retry = status.is_server_error() || status == 429;
            return Err((format!("the server answered {status}"), retry));
        }
        let header = |name| {
            response
                .headers()
                .get(name)
                .and_then(|v| v.to_str().ok())
                .map(str::to_owned)
        };
        let etag = header("etag");
        if self.size != 0 && etag != self.etag {
            return Err(("the file changed while it was open".into(), false));
        }
        // `bytes FIRST-LAST/SIZE`
        let range = header("content-range").unwrap_or_default();
        let parsed = range
            .strip_prefix("bytes ")
            .and_then(|r| r.split_once('/'))
            .and_then(|(span, size)| {
                let (from, to) = span.split_once('-')?;
                Some((
                    from.parse::<u64>().ok()?,
                    to.parse::<u64>().ok()?,
                    size.parse::<u64>().ok()?,
                ))
            });
        let Some((from, to, size)) = parsed else {
            return Err((format!("unexpected Content-Range {range:?}"), false));
        };
        // Exactly what was asked for, as far as the file goes: chunks are whole but for the last
        if from != first
            || size == 0
            || to != last.min(size - 1)
            || (self.size != 0 && size != self.size)
        {
            return Err((format!("unexpected Content-Range {range:?}"), false));
        }
        let data = response
            .body_mut()
            .with_config()
            // One more, as reaching the limit is an error
            .limit(to - from + 2)
            .read_to_vec()
            .map_err(|e| (self.redact(&e.to_string()), true))?;
        if data.len() as u64 != to - from + 1 {
            return Err(("the response was cut short".into(), true));
        }
        Ok(Got { data, size, etag })
    }

    /// Fills `buf` from `offset`, as far as the file goes; returns how far that is.
    fn read(&self, buf: &mut [u8], offset: u64) -> Result<usize, String> {
        let end = (offset + buf.len() as u64).min(self.size);
        if offset >= end {
            return Ok(0);
        }
        let (first, last) = (offset / CHUNK, (end - 1) / CHUNK);
        let mut state = self.state.lock().unwrap();
        state.ahead = if offset == state.next {
            (state.ahead * 2).clamp(1, MAX_AHEAD)
        } else {
            0
        };
        state.next = end;

        // Chunks fetched now count as used now, so the ones read ahead aren't evicted first
        state.clock += 1;
        let clock = state.clock;
        let missing: Vec<u64> = (first..=last)
            .filter(|i| !state.chunks.contains_key(i))
            .collect();
        if let (Some(&from), Some(&to)) = (missing.first(), missing.last()) {
            // Past the read, as far as `ahead` goes or until a chunk that's already here
            let chunks = self.size.div_ceil(CHUNK);
            let mut to = to;
            while to + 1 < chunks
                && to < last + state.ahead
                && !state.chunks.contains_key(&(to + 1))
            {
                to += 1;
            }
            let Got { data, .. } = self.get(from * CHUNK, ((to + 1) * CHUNK).min(self.size) - 1)?;
            for (i, chunk) in (from..).zip(data.chunks(CHUNK as usize)) {
                state.chunks.insert(i, (chunk.into(), clock));
            }
        }

        for i in first..=last {
            let (chunk, used) = state.chunks.get_mut(&i).expect("fetched above");
            *used = clock;
            let start = offset.max(i * CHUNK);
            let stop = end.min((i + 1) * CHUNK);
            buf[(start - offset) as usize..(stop - offset) as usize]
                .copy_from_slice(&chunk[(start - i * CHUNK) as usize..(stop - i * CHUNK) as usize]);
        }
        // Least recently used first, never the ones just read or fetched
        let limit = (CACHE / CHUNK) as usize;
        if state.chunks.len() > limit {
            let mut used: Vec<(u64, u64)> =
                state.chunks.iter().map(|(&i, &(_, u))| (u, i)).collect();
            used.sort_unstable();
            for &(when, i) in &used[..state.chunks.len() - limit] {
                if when != clock {
                    state.chunks.remove(&i);
                }
            }
        }
        Ok((end - offset) as usize)
    }
}

/// The default VFS, which the rest is passed to.
///
/// # Safety
/// `vfs` must be the VFS `register` made.
unsafe fn real(vfs: *mut ffi::sqlite3_vfs) -> *mut ffi::sqlite3_vfs {
    unsafe { (*vfs).pAppData.cast() }
}

/// Whether `name` (a filename SQLite passes) is one this VFS reads over HTTP, or one beside it (a
/// journal).
///
/// # Safety
/// `name` must be null or a nul-terminated string.
unsafe fn is_web(name: *const c_char) -> bool {
    !name.is_null()
        && unsafe { CStr::from_ptr(name) }
            .to_bytes()
            .starts_with(FILENAME.as_bytes())
}

unsafe extern "C" fn open(
    vfs: *mut ffi::sqlite3_vfs,
    name: ffi::sqlite3_filename,
    file: *mut ffi::sqlite3_file,
    flags: c_int,
    out_flags: *mut c_int,
) -> c_int {
    unsafe {
        if !is_web(name) {
            let real = real(vfs);
            return (*real).xOpen.unwrap()(real, name, file, flags, out_flags);
        }
        let file = file.cast::<File>();
        (*file).base.pMethods = std::ptr::null();
        if flags & ffi::SQLITE_OPEN_MAIN_DB == 0 || flags & ffi::SQLITE_OPEN_READWRITE != 0 {
            return ffi::SQLITE_CANTOPEN;
        }
        let url = ffi::sqlite3_uri_parameter(name, c"web_url".as_ptr());
        let result = if url.is_null() {
            Err("no web_url".to_owned())
        } else {
            match CStr::from_ptr(url).to_str() {
                Ok(url) => Remote::open(url.to_owned()),
                Err(_) => Err("web_url isn't UTF-8".to_owned()),
            }
        };
        match result {
            Ok(remote) => {
                (*file).remote = Box::into_raw(Box::new(remote));
                (*file).base.pMethods = &METHODS;
                if !out_flags.is_null() {
                    *out_flags = flags;
                }
                ffi::SQLITE_OK
            }
            Err(error) => {
                log::warn!("{error}");
                ERROR.set(Some(error));
                ffi::SQLITE_CANTOPEN
            }
        }
    }
}

unsafe extern "C" fn delete(vfs: *mut ffi::sqlite3_vfs, name: *const c_char, sync: c_int) -> c_int {
    unsafe {
        if is_web(name) {
            return ffi::SQLITE_READONLY;
        }
        let real = real(vfs);
        (*real).xDelete.unwrap()(real, name, sync)
    }
}

unsafe extern "C" fn access(
    vfs: *mut ffi::sqlite3_vfs,
    name: *const c_char,
    flags: c_int,
    out: *mut c_int,
) -> c_int {
    unsafe {
        if is_web(name) {
            // No journal or WAL beside it: it's read as is
            *out = 0;
            return ffi::SQLITE_OK;
        }
        let real = real(vfs);
        (*real).xAccess.unwrap()(real, name, flags, out)
    }
}

unsafe extern "C" fn full_pathname(
    vfs: *mut ffi::sqlite3_vfs,
    name: *const c_char,
    size: c_int,
    out: *mut c_char,
) -> c_int {
    unsafe {
        if is_web(name) {
            // As is: it isn't a path
            let name = CStr::from_ptr(name).to_bytes_with_nul();
            if name.len() > size as usize {
                return ffi::SQLITE_CANTOPEN;
            }
            std::ptr::copy_nonoverlapping(name.as_ptr().cast(), out, name.len());
            return ffi::SQLITE_OK;
        }
        let real = real(vfs);
        (*real).xFullPathname.unwrap()(real, name, size, out)
    }
}

unsafe extern "C" fn dl_open(vfs: *mut ffi::sqlite3_vfs, name: *const c_char) -> *mut c_void {
    unsafe {
        let real = real(vfs);
        (*real).xDlOpen.unwrap()(real, name)
    }
}

unsafe extern "C" fn dl_error(vfs: *mut ffi::sqlite3_vfs, size: c_int, out: *mut c_char) {
    unsafe {
        let real = real(vfs);
        (*real).xDlError.unwrap()(real, size, out)
    }
}

type Symbol = unsafe extern "C" fn(*mut ffi::sqlite3_vfs, *mut c_void, *const c_char);

unsafe extern "C" fn dl_sym(
    vfs: *mut ffi::sqlite3_vfs,
    handle: *mut c_void,
    symbol: *const c_char,
) -> Option<Symbol> {
    unsafe {
        let real = real(vfs);
        (*real).xDlSym.unwrap()(real, handle, symbol)
    }
}

unsafe extern "C" fn dl_close(vfs: *mut ffi::sqlite3_vfs, handle: *mut c_void) {
    unsafe {
        let real = real(vfs);
        (*real).xDlClose.unwrap()(real, handle)
    }
}

unsafe extern "C" fn randomness(
    vfs: *mut ffi::sqlite3_vfs,
    size: c_int,
    out: *mut c_char,
) -> c_int {
    unsafe {
        let real = real(vfs);
        (*real).xRandomness.unwrap()(real, size, out)
    }
}

unsafe extern "C" fn sleep(vfs: *mut ffi::sqlite3_vfs, microseconds: c_int) -> c_int {
    unsafe {
        let real = real(vfs);
        (*real).xSleep.unwrap()(real, microseconds)
    }
}

unsafe extern "C" fn current_time(vfs: *mut ffi::sqlite3_vfs, out: *mut f64) -> c_int {
    unsafe {
        let real = real(vfs);
        (*real).xCurrentTime.unwrap()(real, out)
    }
}

unsafe extern "C" fn get_last_error(
    vfs: *mut ffi::sqlite3_vfs,
    size: c_int,
    out: *mut c_char,
) -> c_int {
    unsafe {
        let real = real(vfs);
        (*real).xGetLastError.unwrap()(real, size, out)
    }
}

unsafe extern "C" fn current_time_int64(
    vfs: *mut ffi::sqlite3_vfs,
    out: *mut ffi::sqlite3_int64,
) -> c_int {
    unsafe {
        let real = real(vfs);
        match (*real).xCurrentTimeInt64 {
            Some(f) if (*real).iVersion >= 2 => f(real, out),
            _ => {
                let mut days = 0.0;
                let rc = (*real).xCurrentTime.unwrap()(real, &mut days);
                *out = (days * 86_400_000.0) as ffi::sqlite3_int64;
                rc
            }
        }
    }
}

static METHODS: ffi::sqlite3_io_methods = ffi::sqlite3_io_methods {
    iVersion: 1,
    xClose: Some(close),
    xRead: Some(read),
    xWrite: Some(write),
    xTruncate: Some(truncate),
    xSync: Some(sync),
    xFileSize: Some(file_size),
    xLock: Some(lock),
    xUnlock: Some(lock),
    xCheckReservedLock: Some(check_reserved_lock),
    xFileControl: Some(file_control),
    xSectorSize: Some(sector_size),
    xDeviceCharacteristics: Some(device_characteristics),
    xShmMap: None,
    xShmLock: None,
    xShmBarrier: None,
    xShmUnmap: None,
    xFetch: None,
    xUnfetch: None,
};

/// # Safety
/// `file` must be one `open` opened over HTTP, and not closed yet.
unsafe fn remote<'a>(file: *mut ffi::sqlite3_file) -> &'a Remote {
    unsafe { &*(*file.cast::<File>()).remote }
}

unsafe extern "C" fn close(file: *mut ffi::sqlite3_file) -> c_int {
    unsafe {
        let file = file.cast::<File>();
        drop(Box::from_raw((*file).remote));
        (*file).remote = std::ptr::null_mut();
    }
    ffi::SQLITE_OK
}

unsafe extern "C" fn read(
    file: *mut ffi::sqlite3_file,
    buf: *mut c_void,
    amount: c_int,
    offset: ffi::sqlite3_int64,
) -> c_int {
    unsafe {
        let remote = remote(file);
        let buf = std::slice::from_raw_parts_mut(buf.cast::<u8>(), amount as usize);
        match remote.read(buf, offset as u64) {
            Ok(read) if read == buf.len() => ffi::SQLITE_OK,
            Ok(read) => {
                // As SQLite requires of a short read
                buf[read..].fill(0);
                ffi::SQLITE_IOERR_SHORT_READ
            }
            Err(error) => {
                log::warn!("{error}");
                ffi::SQLITE_IOERR_READ
            }
        }
    }
}

unsafe extern "C" fn write(
    _: *mut ffi::sqlite3_file,
    _: *const c_void,
    _: c_int,
    _: ffi::sqlite3_int64,
) -> c_int {
    ffi::SQLITE_READONLY
}

unsafe extern "C" fn truncate(_: *mut ffi::sqlite3_file, _: ffi::sqlite3_int64) -> c_int {
    ffi::SQLITE_READONLY
}

unsafe extern "C" fn sync(_: *mut ffi::sqlite3_file, _: c_int) -> c_int {
    ffi::SQLITE_OK
}

unsafe extern "C" fn file_size(
    file: *mut ffi::sqlite3_file,
    out: *mut ffi::sqlite3_int64,
) -> c_int {
    unsafe { *out = remote(file).size as ffi::sqlite3_int64 };
    ffi::SQLITE_OK
}

/// Nothing to lock: nobody writes it.
unsafe extern "C" fn lock(_: *mut ffi::sqlite3_file, _: c_int) -> c_int {
    ffi::SQLITE_OK
}

unsafe extern "C" fn check_reserved_lock(_: *mut ffi::sqlite3_file, out: *mut c_int) -> c_int {
    unsafe { *out = 0 };
    ffi::SQLITE_OK
}

unsafe extern "C" fn file_control(_: *mut ffi::sqlite3_file, _: c_int, _: *mut c_void) -> c_int {
    ffi::SQLITE_NOTFOUND
}

unsafe extern "C" fn sector_size(_: *mut ffi::sqlite3_file) -> c_int {
    CHUNK as c_int
}

unsafe extern "C" fn device_characteristics(_: *mut ffi::sqlite3_file) -> c_int {
    ffi::SQLITE_IOCAP_IMMUTABLE
}
