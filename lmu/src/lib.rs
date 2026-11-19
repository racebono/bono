//! The Le Mans Ultimate plugin's recorder: the functions `lmu.cpp` calls, declared in `recorder.h`.
//! The plugin itself (the game's interface, and converting its data) is in `lmu.cpp`, compiled into
//! this DLL by `build.rs`.
#![cfg(windows)]

use bono_sim::{
    recorder::{self, ForecastSource, Recorder},
    sql::EpochReason,
    sys::{bono_session_info, bono_standing, bono_telemetry},
};
use std::{
    ffi::OsString,
    os::windows::ffi::OsStringExt,
    panic::{AssertUnwindSafe, catch_unwind},
    path::PathBuf,
    sync::Arc,
    time::Duration,
};

mod car_numbers;
mod class_positions;
mod condition;
mod forecast;
mod garage;
mod track_limits;

use car_numbers::CarNumbers;
use condition::VehicleCondition;
use track_limits::TrackLimits;

/// What `bono_lmu_recorder*` points to.
pub struct LmuRecorder(Recorder, Arc<CarNumbers>, TrackLimits, VehicleCondition);

/// What the recorder's writer thread reads from the web API now and then. Closing the recorder waits
/// for the writer on the game's thread, so a hung web API can hold the game's exit for both requests'
/// timeouts, 6 s at most. They need the writer's own thread as nothing else should wait on them, and
/// the wait only happens if the API stops answering just as the game closes.
fn web_api(numbers: Arc<CarNumbers>) -> ForecastSource {
    Box::new(move |session_time| {
        numbers.refresh();
        forecast::fetch(session_time)
    })
}

/// A client for LMU's local web API. It's on this PC: anything slow means it isn't answering, so
/// connecting, sending, waiting for the answer and reading it each get `step`. Finding the host gets
/// no timeout, and neither does the request as a whole: ureq documents that a timeout covering the
/// lookup may make it look the host up on a thread of its own, which nothing waits for and could
/// outlive the DLL. Windows answers for localhost itself. No proxy from the environment (finding
/// one is part of connecting) and no redirects, so nothing else is looked up or asked.
fn local_agent(step: Duration) -> ureq::Agent {
    ureq::Agent::config_builder()
        .proxy(None)
        .max_redirects(0)
        .timeout_connect(Some(step))
        .timeout_send_request(Some(step))
        .timeout_recv_response(Some(step))
        .timeout_recv_body(Some(step))
        .build()
        .into()
}

/// # Safety
/// `dir` must be valid for reads of `len` UTF-16 units.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn bono_lmu_open(dir: *const u16, len: usize) -> *mut LmuRecorder {
    let dir = unsafe { path(dir, len) };
    guard(std::ptr::null_mut(), || {
        let numbers = Arc::new(CarNumbers::default());
        match Recorder::new(dir, Some(web_api(numbers.clone()))) {
            Ok(recorder) => Box::into_raw(Box::new(LmuRecorder(
                recorder,
                numbers,
                TrackLimits::default(),
                VehicleCondition::start(),
            ))),
            Err(_) => std::ptr::null_mut(),
        }
    })
}

/// # Safety
/// `recorder` must come from `bono_lmu_open`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn bono_lmu_restart(recorder: *mut LmuRecorder) {
    let LmuRecorder(recorder, numbers, ..) = unsafe { &mut *recorder };
    guard((), || {
        let _ = recorder.restart(Some(web_api(numbers.clone())));
    })
}

/// # Safety
/// `recorder` must come from `bono_lmu_open`, and isn't used afterwards.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn bono_lmu_close(recorder: *mut LmuRecorder) {
    let recorder = unsafe { Box::from_raw(recorder) };
    guard((), move || {
        let dir = recorder.0.dir().to_owned();
        // The damage's thread stops first, so its request and the writer's time out together: the
        // game waits 6 s at most, not one wait after the other. Then this waits for the writer, so
        // the database is closed before it's deleted, and for the damage's thread (see
        // `VehicleCondition`'s `drop`)
        recorder.3.stop();
        drop(recorder);
        recorder::remove_database(&dir);
    })
}

/// # Safety
/// `dir` must be valid for reads of `len` UTF-16 units.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn bono_lmu_remove_database(dir: *const u16, len: usize) {
    let dir = unsafe { path(dir, len) };
    guard((), || recorder::remove_database(&dir))
}

/// # Safety
/// `recorder` must come from `bono_lmu_open`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn bono_lmu_wants_telemetry(
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
/// `recorder` must come from `bono_lmu_open`, and `telemetry` be valid for reads.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn bono_lmu_push_telemetry(
    recorder: *const LmuRecorder,
    telemetry: *const bono_telemetry,
) {
    let LmuRecorder(recorder, _, track_limits, condition) = unsafe { &*recorder };
    let mut telemetry = unsafe { *telemetry };
    guard((), || {
        track_limits.fill_telemetry(&mut telemetry);
        let is_player = telemetry
            .vehicle_id
            .get()
            .is_some_and(|vehicle| recorder.is_player(vehicle));
        condition.fill_telemetry(&mut telemetry, is_player);
        recorder.push_telemetry(&telemetry);
        // After the telemetry, so the setup is recorded with this update's row
        if is_player && let Some(setup) = condition.take_setup() {
            recorder.push_setup(setup);
        }
    })
}

/// # Safety
/// `recorder` must come from `bono_lmu_open`, `session` be valid for reads and writes and
/// `standings` for reads and writes of `count` standings. They're moved out, leaving them empty
/// (every field NULL), so C++'s destructors free nothing.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn bono_lmu_push_scoring(
    recorder: *const LmuRecorder,
    session: *mut bono_session_info,
    standings: *mut bono_standing,
    count: usize,
) {
    let LmuRecorder(recorder, numbers, track_limits, condition) = unsafe { &*recorder };
    let session = std::mem::take(unsafe { &mut *session });
    let standings = match count {
        0 => Vec::new(),
        _ => unsafe { std::slice::from_raw_parts_mut(standings, count) }
            .iter_mut()
            .map(std::mem::take)
            .collect::<Vec<_>>(),
    };
    guard((), move || {
        let mut session = session;
        track_limits.fill_session(&mut session);
        let mut standings = standings;
        numbers.fill(&mut standings);
        class_positions::fill(&mut standings);
        recorder.push_scoring(session, standings);
        condition.set_player(recorder.player());
    })
}

/// # Safety
/// `recorder` must come from `bono_lmu_open`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn bono_lmu_clear(recorder: *const LmuRecorder) {
    let LmuRecorder(recorder, numbers, track_limits, condition) = unsafe { &*recorder };
    guard((), || {
        numbers.clear();
        track_limits.clear();
        condition.clear();
        recorder.clear()
    })
}

/// # Safety
/// `recorder` must come from `bono_lmu_open`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn bono_lmu_begin_session(recorder: *const LmuRecorder) {
    let LmuRecorder(recorder, _, track_limits, condition) = unsafe { &*recorder };
    guard((), || {
        // The session's rule, its scoring says again: until then, points are unknown
        track_limits.clear();
        // The last session's damage isn't this one's: it's asked again once the player drives
        condition.clear();
        recorder.begin_epoch(EpochReason::Session)
    })
}

/// # Safety
/// `recorder` must come from `bono_lmu_open`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn bono_lmu_set_realtime(recorder: *const LmuRecorder, realtime: bool) {
    let condition = unsafe { &(*recorder).3 };
    guard((), || condition.set_realtime(realtime))
}

/// # Safety
/// `recorder` must come from `bono_lmu_open`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn bono_lmu_failed(recorder: *const LmuRecorder) -> bool {
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
