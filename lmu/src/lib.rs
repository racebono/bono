//! The Le Mans Ultimate plugin's recorder: the functions `lmu.cpp` calls, declared in `recorder.h`.
//! The plugin itself (the game's interface, and converting its data) is in `lmu.cpp`, compiled into
//! this DLL by `build.rs`.
#![cfg(windows)]

use lambiase_sim::{
    recorder::{self, Recorder},
    sys::{lambiase_session_info, lambiase_standing, lambiase_telemetry},
};
use std::{
    ffi::OsString,
    os::windows::ffi::OsStringExt,
    panic::{AssertUnwindSafe, catch_unwind},
    path::PathBuf,
};

mod forecast;

/// What `lambiase_lmu_recorder*` points to.
pub struct LmuRecorder(Recorder);

/// # Safety
/// `dir` must be valid for reads of `len` UTF-16 units.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lambiase_lmu_open(dir: *const u16, len: usize) -> *mut LmuRecorder {
    let dir = unsafe { path(dir, len) };
    guard(std::ptr::null_mut(), || {
        match Recorder::new(dir, Some(Box::new(forecast::fetch))) {
            Ok(recorder) => Box::into_raw(Box::new(LmuRecorder(recorder))),
            Err(_) => std::ptr::null_mut(),
        }
    })
}

/// # Safety
/// `recorder` must come from `lambiase_lmu_open`, and isn't used afterwards.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lambiase_lmu_close(recorder: *mut LmuRecorder) {
    let recorder = unsafe { Box::from_raw(recorder) };
    guard((), move || {
        let dir = recorder.0.dir().to_owned();
        // Waits for the writer, so the database is closed before it's deleted
        drop(recorder);
        recorder::remove_database(&dir);
    })
}

/// # Safety
/// `dir` must be valid for reads of `len` UTF-16 units.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lambiase_lmu_remove_database(dir: *const u16, len: usize) {
    let dir = unsafe { path(dir, len) };
    guard((), || recorder::remove_database(&dir))
}

/// # Safety
/// `recorder` must come from `lambiase_lmu_open`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lambiase_lmu_wants_telemetry(
    recorder: *const LmuRecorder,
    vehicle_id: i64,
    session_time: f64,
    speed: f64,
) -> bool {
    let recorder = unsafe { &(*recorder).0 };
    guard(false, || {
        recorder.wants_telemetry(vehicle_id, session_time, speed)
    })
}

/// # Safety
/// `recorder` must come from `lambiase_lmu_open`, and `telemetry` be valid for reads.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lambiase_lmu_push_telemetry(
    recorder: *const LmuRecorder,
    telemetry: *const lambiase_telemetry,
) {
    let recorder = unsafe { &(*recorder).0 };
    let telemetry = unsafe { &*telemetry };
    guard((), || recorder.push_telemetry(telemetry))
}

/// # Safety
/// `recorder` must come from `lambiase_lmu_open`, `session` be valid for reads and writes and
/// `standings` for reads and writes of `count` standings. They're moved out, leaving them empty
/// (every field NULL), so C++'s destructors free nothing.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lambiase_lmu_push_scoring(
    recorder: *const LmuRecorder,
    session: *mut lambiase_session_info,
    standings: *mut lambiase_standing,
    count: usize,
) {
    let recorder = unsafe { &(*recorder).0 };
    let session = std::mem::take(unsafe { &mut *session });
    let standings = match count {
        0 => Vec::new(),
        _ => unsafe { std::slice::from_raw_parts_mut(standings, count) }
            .iter_mut()
            .map(std::mem::take)
            .collect::<Vec<_>>(),
    };
    guard((), move || recorder.push_scoring(session, standings))
}

/// # Safety
/// `recorder` must come from `lambiase_lmu_open`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lambiase_lmu_clear(recorder: *const LmuRecorder) {
    let recorder = unsafe { &(*recorder).0 };
    guard((), || recorder.clear())
}

/// # Safety
/// `recorder` must come from `lambiase_lmu_open`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lambiase_lmu_failed(recorder: *const LmuRecorder) -> bool {
    let recorder = unsafe { &(*recorder).0 };
    recorder.failed()
}

unsafe fn path(dir: *const u16, len: usize) -> PathBuf {
    let dir = match len {
        0 => &[][..],
        _ => unsafe { std::slice::from_raw_parts(dir, len) },
    };
    PathBuf::from(OsString::from_wide(dir))
}

/// A panic can't unwind into the game (it would abort it): it's caught, and `fallback` returned.
fn guard<T>(fallback: T, f: impl FnOnce() -> T) -> T {
    catch_unwind(AssertUnwindSafe(f)).unwrap_or(fallback)
}
