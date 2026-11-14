//! iRacing. It has no plugins: while it runs, it publishes its telemetry in shared memory (see
//! `sdk`) with the session's description as a YAML document (see `yaml` and `session`), which a
//! thread of ours reads and records like LMU's plugin does (see `memory`).
//!
//! iRacing is installed by its own launcher (or Steam), anywhere: it's followed by its process
//! alone, and its folder isn't looked for (nothing needs it).

mod convert;
mod sdk;
mod session;
mod yaml;

use crate::{
    sim::{
        Simulator, SimulatorInfo, SimulatorSettings,
        memory::{Read, Recording, Source},
    },
    sqlite::pool::Pool,
};
use convert::Converter;
use sdk::{Frame, HEADER_LEN, Header, MEMORY_NAME, VERSION, Vars};
use session::SessionInfo;
use std::{path::PathBuf, time::Instant};

/// How often the session info is read again while its update count stays the same.
const INFO_RECHECK: std::time::Duration = std::time::Duration::from_secs(2);

pub struct IRacing(Recording);

impl IRacing {
    pub fn new() -> Self {
        Self(Recording::new(Self::NAME, "iracing", || {
            Box::new(Game::default())
        }))
    }
}

impl SimulatorInfo for IRacing {
    const NAME: &'static str = "iRacing";
    /// Lovely Sim Racing's track data's id for it
    const TRACK_DATA_ID: &'static str = "iracing";
    /// Mostly installed with its own launcher; Steam reporting it running isn't needed to follow
    /// it, since its process is
    const APP_ID: Option<u32> = None;
    /// The current DirectX 11 build, and the older 64-bit and 32-bit ones
    const EXECUTABLES: &'static [&'static str] =
        &["iRacingSim64DX11.exe", "iRacingSim64.exe", "iRacingSim.exe"];
    const INSTALL_DIR: Option<fn() -> Option<PathBuf>> = None;
}

impl Simulator for IRacing {
    fn database(&mut self, running: bool) -> Option<&Pool> {
        self.0.database(running)
    }

    fn apply(&mut self, settings: &SimulatorSettings) {
        self.0.apply(settings);
    }

    fn clear(&mut self) {
        self.0.clear();
    }
}

/// Reading the game's shared memory, on the reader thread.
#[derive(Default)]
struct Game {
    memory: Option<SharedMemory>,
    /// The variables, and the header values they were read for (their count, where they're
    /// described, an update's length, and the session info's update count)
    vars: Option<((i32, i32, i32, i32), Vars)>,
    /// The session info: its update count, its text as copied, and what it says
    info: Option<(i32, Vec<u8>, SessionInfo)>,
    /// The session info as last copied, and its update count, until a later copy confirms it
    pending_info: Option<(i32, Vec<u8>)>,
    /// When the session info was last found to be what was read
    info_checked: Option<Instant>,
    /// The tick count of the last update read
    last_tick: Option<i32>,
    /// The last update read
    buffer: Vec<u8>,
    converter: Converter,
    warned_version: bool,
    warned_range: bool,
}

impl Source for Game {
    fn read(&mut self, now: Instant) -> Read {
        if self.memory.is_none() {
            self.memory = SharedMemory::open();
        }
        let Some(memory) = &self.memory else {
            return Read::Unavailable;
        };
        let Some(header) = memory.header() else {
            return Read::Unavailable;
        };
        // In the menus, or between sessions (or not filled in yet). What was read for the last
        // session may not be the next one's
        if !header.connected() {
            self.last_tick = None;
            self.vars = None;
            self.info = None;
            self.pending_info = None;
            self.info_checked = None;
            return Read::Unavailable;
        }
        if header.version != VERSION {
            if !self.warned_version {
                log::warn!(
                    "iRacing's shared memory is version {}, not {VERSION}: not recording it",
                    header.version
                );
                self.warned_version = true;
            }
            return Read::Unavailable;
        }

        // The variables and the session info, read again when the session info changes. Each is
        // kept only if the game didn't change it while it was copied (the header is the same
        // after, and for the session info, a copy on a later read is too)
        let update = header.session_info_update;
        let layout = |h: &Header| {
            (
                h.version,
                h.var_count,
                h.var_header_offset,
                h.buf_count,
                h.buf_len,
                h.session_info_offset,
                h.session_info_len,
                h.session_info_update,
            )
        };
        let key = (
            header.var_count,
            header.var_header_offset,
            header.buf_len,
            update,
        );
        let same = |again: &Header| again.connected() && layout(again) == layout(&header);
        // What the header says is where it can't be: nothing's read until it says otherwise
        let fits =
            |range: Option<std::ops::Range<usize>>| range.is_some_and(|r| r.end <= memory.len);
        // The latest update, once there's one, with a length that fits too
        let update_range = header.latest().map(|(_, offset)| {
            let end = header.buf_len().and_then(|len| offset.checked_add(len));
            end.map(|end| offset..end)
        });
        if !(fits(header.var_headers())
            && fits(header.session_info())
            && update_range.is_none_or(fits))
        {
            if !self.warned_range {
                log::warn!(
                    "iRacing's shared memory header points past the memory: not recording it"
                );
                self.warned_range = true;
            }
            return Read::Idle;
        }

        if self.vars.as_ref().is_none_or(|(last, _)| *last != key) {
            self.vars = None;
            let (Some(headers), Some(buf_len)) = (
                header.var_headers().and_then(|range| memory.copy(range)),
                header.buf_len(),
            ) else {
                return Read::Idle;
            };
            if !memory.header().is_some_and(|again| same(&again)) {
                return Read::Idle;
            }
            self.vars = Some((key, Vars::from_headers(&headers, buf_len)));
        }
        // A new update's waited for; the one read is looked at again now and then, in case it was
        // read before the game wrote it (it may count the update before it does)
        let stale = self.info.as_ref().is_none_or(|(last, ..)| *last != update);
        if stale {
            self.info = None;
        }
        let due = stale
            || self
                .info_checked
                .is_none_or(|at| now.duration_since(at) >= INFO_RECHECK);
        if due {
            let copied = header
                .session_info()
                .and_then(|range| memory.copy(range))
                .filter(|_| memory.header().is_some_and(|again| same(&again)));
            match copied {
                None => self.pending_info = None,
                Some(bytes)
                    if self
                        .info
                        .as_ref()
                        .is_some_and(|(_, read, _)| *read == bytes) =>
                {
                    self.pending_info = None;
                    self.info_checked = Some(now);
                }
                // Kept once a later read finds the same: the game isn't writing it
                Some(bytes) => match self.pending_info.take() {
                    Some((pending, earlier)) if pending == update && earlier == bytes => {
                        let info = SessionInfo::new(&yaml::read(&bytes));
                        self.info = Some((update, bytes, info));
                        self.info_checked = Some(now);
                    }
                    _ => self.pending_info = Some((update, bytes)),
                },
            }
        }
        if self.info.is_none() {
            return Read::Idle;
        }

        let Some((tick, offset)) = header.latest() else {
            return Read::Idle;
        };
        if self.last_tick == Some(tick) {
            return Read::Idle;
        }
        // Like the SDK's own reader, the update there on connecting is taken as where to start
        // from: it may be the last session's
        if self.last_tick.is_none() {
            self.last_tick = Some(tick);
            return Read::Idle;
        }
        let Some(len) = header.buf_len() else {
            return Read::Idle;
        };
        // The SDK's way: copy the latest buffer, and keep it if the game didn't write over it
        // meanwhile (it still holds the same update, and no other one was started in it)
        if !header.holds(tick, offset)
            || !memory.copy_into(offset..offset + len, &mut self.buffer)
            || !memory
                .header()
                .is_some_and(|again| again.holds(tick, offset) && same(&again))
        {
            return Read::Idle;
        }
        self.last_tick = Some(tick);

        let (Some((_, vars)), Some((_, _, info))) = (&self.vars, &self.info) else {
            return Read::Idle;
        };
        match self
            .converter
            .convert(&Frame::new(vars, &self.buffer), info, now)
        {
            Some(update) => Read::Update(update),
            None => Read::Idle,
        }
    }
}

/// The game's shared memory, mapped read-only.
struct SharedMemory {
    handle: windows_sys::Win32::Foundation::HANDLE,
    view: windows_sys::Win32::System::Memory::MEMORY_MAPPED_VIEW_ADDRESS,
    /// How much of it is mapped
    len: usize,
}

// Only read, by copying
unsafe impl Send for SharedMemory {}

impl SharedMemory {
    /// `None` while the game hasn't made it (it isn't running yet, or hasn't loaded).
    fn open() -> Option<Self> {
        use windows_sys::Win32::{
            Foundation::CloseHandle,
            System::Memory::{
                FILE_MAP_READ, MEM_COMMIT, MEMORY_BASIC_INFORMATION, MapViewOfFile,
                OpenFileMappingW, PAGE_GUARD, PAGE_NOACCESS, UnmapViewOfFile, VirtualQuery,
            },
        };

        let name = MEMORY_NAME
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect::<Vec<_>>();
        let handle = unsafe { OpenFileMappingW(FILE_MAP_READ, 0, name.as_ptr()) };
        if handle.is_null() {
            return None;
        }
        // All of it: its size is the game's to choose
        let view = unsafe { MapViewOfFile(handle, FILE_MAP_READ, 0, 0, 0) };
        if view.Value.is_null() {
            unsafe { CloseHandle(handle) };
            return None;
        }
        // Only as much as can be read from the start: committed, and neither guarded nor
        // inaccessible
        let mut info = unsafe { std::mem::zeroed::<MEMORY_BASIC_INFORMATION>() };
        let queried =
            unsafe { VirtualQuery(view.Value, &mut info, size_of::<MEMORY_BASIC_INFORMATION>()) };
        let readable = info.State == MEM_COMMIT && info.Protect & (PAGE_GUARD | PAGE_NOACCESS) == 0;
        if queried == 0 || !readable || info.RegionSize < HEADER_LEN {
            unsafe {
                UnmapViewOfFile(view);
                CloseHandle(handle);
            }
            return None;
        }
        Some(Self {
            handle,
            view,
            len: info.RegionSize,
        })
    }

    fn header(&self) -> Option<Header> {
        Header::parse(&self.copy(0..HEADER_LEN)?)
    }

    /// A copy of `range`, if it's in the memory.
    fn copy(&self, range: std::ops::Range<usize>) -> Option<Vec<u8>> {
        let mut out = Vec::new();
        self.copy_into(range, &mut out).then_some(out)
    }

    /// Copies `range` into `out`, if it's in the memory. The game writes it as it pleases, so a
    /// copy can mix two of its writes: callers check it didn't. Read volatile, so every copy
    /// (and every check after one) really reads the memory, in order.
    fn copy_into(&self, range: std::ops::Range<usize>, out: &mut Vec<u8>) -> bool {
        if range.start > range.end || range.end > self.len {
            return false;
        }
        out.clear();
        out.reserve(range.len());
        let base = self.view.Value.cast::<u8>().cast_const();
        // In the mapping (checked above), which stays mapped while `self` lives
        out.extend(range.map(|at| unsafe { base.add(at).read_volatile() }));
        true
    }
}

impl Drop for SharedMemory {
    fn drop(&mut self) {
        use windows_sys::Win32::{Foundation::CloseHandle, System::Memory::UnmapViewOfFile};
        unsafe {
            UnmapViewOfFile(self.view);
            CloseHandle(self.handle);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sim::memory::record;
    use bono_sim::recorder::{DATABASE, Recorder};
    use sdk::Ibt;
    use std::{path::Path, time::Duration};

    /// Records a saved telemetry file as if the game were sharing it live, into `dir`. Returns
    /// how many updates were recorded.
    fn record_file(path: &Path, dir: &Path) -> usize {
        let ibt = Ibt::open(path).unwrap();
        let info = SessionInfo::new(&yaml::read(ibt.session_info()));
        let recorder = Recorder::new(dir, None).unwrap();
        let mut converter = Converter::default();
        let start = Instant::now();
        let mut recorded = 0;
        for i in 0..ibt.records() {
            let frame = ibt.frame(i);
            let now = start + Duration::from_secs_f64(i as f64 / 60.0);
            if let Some(update) = converter.convert(&frame, &info, now) {
                // Faster than the game: the writer's queue would fill, and drop scoring updates
                while update.scoring.is_some() && recorder.queued_scoring() >= 50 {
                    assert!(!recorder.failed(), "the recorder stopped writing");
                    std::thread::sleep(Duration::from_millis(1));
                }
                record(&recorder, update);
                recorded += 1;
            }
        }
        // A writer that stopped would leave the recording short
        assert!(
            !recorder.failed(),
            "{}: the recorder stopped writing",
            path.display()
        );
        drop(recorder);
        recorded
    }

    /// What iRacing saved on this computer: every `.ibt` file in `BONO_IRACING_TELEMETRY`.
    fn saved_files() -> Vec<PathBuf> {
        let Some(dir) = std::env::var_os("BONO_IRACING_TELEMETRY") else {
            return Vec::new();
        };
        let mut files: Vec<_> = std::fs::read_dir(dir)
            .unwrap()
            .filter_map(|entry| Some(entry.ok()?.path()))
            .filter(|path| path.extension().is_some_and(|ext| ext == "ibt"))
            .collect();
        files.sort();
        files
    }

    /// Every column of the recording's tables, and how many of its rows have it set, for the
    /// player's car where the table has cars.
    fn filled(conn: &rusqlite::Connection) -> Vec<(String, String, i64, i64)> {
        let mut out = Vec::new();
        for (table, filter) in [
            ("telemetry", "WHERE is_player = 1"),
            (
                "telemetry_wheel",
                "WHERE telemetry_id IN (SELECT id FROM telemetry WHERE is_player = 1)",
            ),
            (
                "car_state",
                "WHERE vehicle_id IN (SELECT vehicle_id FROM telemetry WHERE is_player = 1)",
            ),
            ("session", ""),
            ("session_sector", ""),
            ("standings", "WHERE is_player = 1"),
            (
                "standing_sector",
                "WHERE standing_id IN (SELECT id FROM standings WHERE is_player = 1)",
            ),
        ] {
            let columns: Vec<String> = conn
                .prepare(&format!("SELECT name FROM pragma_table_info('{table}')"))
                .unwrap()
                .query_map([], |row| row.get(0))
                .unwrap()
                .collect::<Result<_, _>>()
                .unwrap();
            for column in columns {
                let (set, rows): (i64, i64) = conn
                    .query_row(
                        &format!("SELECT count({column}), count(*) FROM {table} {filter}"),
                        [],
                        |row| Ok((row.get(0)?, row.get(1)?)),
                    )
                    .unwrap();
                out.push((table.to_owned(), column, set, rows));
            }
        }
        out
    }

    /// Records the telemetry iRacing saved, as if it were live, and checks what the engineer and
    /// spotter need is there. Prints which columns each file filled (`--nocapture`).
    #[test]
    #[ignore = "needs iRacing's saved telemetry (.ibt files) in BONO_IRACING_TELEMETRY"]
    fn records_saved_telemetry() {
        let files = saved_files();
        assert!(!files.is_empty(), "no .ibt files in BONO_IRACING_TELEMETRY");
        for path in files {
            let dir = tempfile::tempdir().unwrap();
            let recorded = record_file(&path, dir.path());
            let conn = rusqlite::Connection::open(dir.path().join(DATABASE)).unwrap();
            let count = |sql: &str| conn.query_row(sql, [], |row| row.get::<_, i64>(0)).unwrap();
            let player_rows = count("SELECT count(*) FROM telemetry WHERE is_player = 1");
            println!(
                "{}: {recorded} updates, {player_rows} player rows",
                path.display()
            );
            assert!(player_rows > 0, "{}", path.display());
            assert_eq!(count("SELECT count(*) FROM session WHERE track IS NULL"), 0);
            for (table, column, set, rows) in filled(&conn) {
                println!("  {table}.{column}: {set}/{rows}");
            }

            // Whole laps were driven: they're timed, sector by sector
            let laps = count(
                "SELECT count(DISTINCT lap) FROM telemetry WHERE is_player = 1 \
                 AND lap_start_time IS NOT NULL",
            );
            if laps >= 3 {
                assert!(
                    count(
                        "SELECT count(*) FROM standings WHERE is_player = 1 AND last_lap_time > 0"
                    ) > 0
                );
                assert!(
                    count(
                        "SELECT count(*) FROM standing_sector JOIN standings ON standings.id = standing_id \
                         WHERE is_player = 1 AND last_lap > 0"
                    ) > 0
                );
            }
        }
    }

    /// The sectors timed here add up to the lap times the game gives, to within what timing the
    /// line between updates allows. Not a session's first lap: the game may time it from the start
    /// rather than the line.
    #[test]
    #[ignore = "needs iRacing's saved telemetry (.ibt files) in BONO_IRACING_TELEMETRY"]
    fn sector_times_add_up_to_the_games_lap_times() {
        let mut compared = 0;
        for path in saved_files() {
            let dir = tempfile::tempdir().unwrap();
            record_file(&path, dir.path());
            let conn = rusqlite::Connection::open(dir.path().join(DATABASE)).unwrap();
            // Each lap's sectors, against the game's time for that lap (the next lap's rows)
            let mut statement = conn
                .prepare(
                    "SELECT s.laps_completed, s.last_lap_time, \
                     (SELECT sum(last_lap) FROM standing_sector WHERE standing_id = s.id), \
                     (SELECT count(*) FROM standing_sector WHERE standing_id = s.id AND last_lap IS NULL) \
                     FROM standings s WHERE is_player = 1 AND last_lap_time > 0 \
                     GROUP BY s.laps_completed HAVING s.id = max(s.id)",
                )
                .unwrap();
            let rows: Vec<(i64, f64, Option<f64>, i64)> = statement
                .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))
                .unwrap()
                .collect::<Result<_, _>>()
                .unwrap();
            for (lap, official, sum, missing) in rows {
                println!("{}: lap {lap} {official} vs {sum:?}", path.display());
                if let (Some(sum), 0, 2..) = (sum, missing, lap) {
                    compared += 1;
                    assert!(
                        (sum - official).abs() < 0.1,
                        "{}: lap {lap}: {sum} vs {official}",
                        path.display()
                    );
                }
            }
        }
        assert!(compared > 0, "no saved lap had every sector timed");
    }

    /// Pearson's correlation of the pairs.
    fn correlation(pairs: &[(f64, f64)]) -> f64 {
        let n = pairs.len() as f64;
        let (ma, mb) = pairs
            .iter()
            .fold((0.0, 0.0), |(a, b), (x, y)| (a + x / n, b + y / n));
        let (mut ab, mut aa, mut bb) = (0.0, 0.0, 0.0);
        for (x, y) in pairs {
            ab += (x - ma) * (y - mb);
            aa += (x - ma).powi(2);
            bb += (y - mb).powi(2);
        }
        ab / (aa * bb).sqrt()
    }

    /// The directions the converter takes iRacing's axes and rotations to have (see `convert`),
    /// against what's physical on the laps iRacing saved: the GPS track (`Lat`, `Lon`, `Alt`, in
    /// saved telemetry only), speed and gravity. Should iRacing turn an axis round, this fails.
    #[test]
    #[ignore = "needs iRacing's saved telemetry (.ibt files) in BONO_IRACING_TELEMETRY"]
    fn axis_conventions_hold_on_saved_laps() {
        use std::f64::consts::PI;
        let wrap = |a: f64| (a + PI).rem_euclid(2.0 * PI) - PI;
        let (mut checked, mut calmed, mut climbed, mut winded, mut turned) = (0, 0, 0, 0, 0);
        for path in saved_files() {
            let ibt = Ibt::open(&path).unwrap();
            let n = ibt.records();
            let name = path.display();
            let channels = [
                "Speed",
                "Yaw",
                "YawNorth",
                "YawRate",
                "Pitch",
                "PitchRate",
                "SteeringWheelAngle",
                "SteeringWheelTorque",
                "LatAccel",
                "LongAccel",
                "VertAccel",
                "VelocityX",
                "VelocityY",
                "VelocityZ",
                "Lat",
                "Lon",
                "Alt",
                "ThrottleRaw",
                "BrakeRaw",
                "WindVel",
                "WindDir",
            ];
            if n == 0 || ibt.tick_rate() <= 0 {
                continue;
            }
            if let Some(missing) = channels.iter().find(|c| ibt.frame(0).count(c) == 0) {
                println!("{name}: no {missing}, not checked");
                continue;
            }
            let at = |i: usize, name: &str| ibt.frame(i).f64(name).unwrap_or(f64::NAN);
            // How many updates up to each have a channel that isn't a number
            let mut broken = vec![0usize; n + 1];
            for i in 0..n {
                let finite = channels.iter().all(|c| at(i, c).is_finite());
                broken[i + 1] = broken[i] + usize::from(!finite);
            }
            // Driving, with room either side for the rates, every channel a number throughout
            let k = 30;
            let samples: Vec<usize> = (k..n.saturating_sub(k))
                .filter(|&i| ibt.frame(i).bool("IsOnTrack") == Some(true) && at(i, "Speed") > 10.0)
                .filter(|&i| broken[i + k + 1] == broken[i - k])
                .collect();
            if samples.len() < 600 {
                continue;
            }
            checked += 1;
            let dt = 1.0 / f64::from(ibt.tick_rate());
            let rate = |i: usize, name: &str| (at(i + 1, name) - at(i - 1, name)) / (2.0 * dt);
            let lat0 = at(samples[0], "Lat").to_radians();
            // Clockwise from north, as the GPS track goes
            let bearing = |i: usize| {
                let dx = (at(i + 3, "Lon") - at(i - 3, "Lon")) * lat0.cos();
                dx.atan2(at(i + 3, "Lat") - at(i - 3, "Lat"))
            };
            let pairs = |f: &dyn Fn(usize) -> (f64, f64)| -> Vec<(f64, f64)> {
                samples.iter().map(|&i| f(i)).collect()
            };

            // How much the GPS track turned (clockwise) around each update
            let turn = pairs(&|i| {
                let turned = wrap(bearing(i + k - 3) - bearing(i - k + 3));
                (at(i, "YawRate"), turned)
            });
            assert!(correlation(&turn) < -0.8, "{name}: YawRate isn't + left");
            let yaw = pairs(&|i| (at(i, "YawRate"), wrap(at(i + 1, "Yaw") - at(i - 1, "Yaw"))));
            assert!(correlation(&yaw) > 0.9, "{name}: YawRate isn't Yaw's rate");
            let north = samples
                .iter()
                .map(|&i| (at(i, "YawNorth") - bearing(i)).cos())
                .sum::<f64>()
                / samples.len() as f64;
            assert!(north > 0.9, "{name}: YawNorth isn't a compass heading");
            let steer = pairs(&|i| (at(i, "SteeringWheelAngle"), at(i, "YawRate")));
            assert!(correlation(&steer) > 0.7, "{name}: steering isn't + left");
            let torque = pairs(&|i| (at(i, "SteeringWheelTorque"), at(i, "SteeringWheelAngle")));
            assert!(
                correlation(&torque) < -0.5,
                "{name}: steering torque isn't against the angle"
            );
            let lateral = pairs(&|i| (at(i, "LatAccel"), at(i, "Speed") * at(i, "YawRate")));
            assert!(correlation(&lateral) > 0.8, "{name}: LatAccel isn't + left");
            let slip = pairs(&|i| {
                let slip = wrap(bearing(i) - at(i, "YawNorth"));
                (at(i, "VelocityY").atan2(at(i, "VelocityX")), slip)
            });
            assert!(correlation(&slip) < -0.7, "{name}: VelocityY isn't + left");
            let forward = samples
                .iter()
                .map(|&i| at(i, "VelocityX") / at(i, "Speed"))
                .sum::<f64>()
                / samples.len() as f64;
            assert!(forward > 0.95, "{name}: VelocityX isn't + forward");
            let longitudinal = pairs(&|i| (at(i, "LongAccel"), rate(i, "Speed")));
            assert!(
                correlation(&longitudinal) > 0.9,
                "{name}: LongAccel isn't + forward"
            );
            // Gravity alone: driving straight on, neither cornering nor braking hard (banking
            // adds the cornering to the vertical)
            let calm: Vec<f64> = samples
                .iter()
                .filter(|&&i| at(i, "LatAccel").abs() < 2.0 && at(i, "LongAccel").abs() < 2.0)
                .map(|&i| at(i, "VertAccel"))
                .collect();
            if calm.len() >= 60 {
                calmed += 1;
                let up = calm.iter().sum::<f64>() / calm.len() as f64;
                assert!(
                    (8.0..11.5).contains(&up),
                    "{name}: VertAccel isn't + up with gravity"
                );
            }
            let pitch_rate = pairs(&|i| (at(i, "PitchRate"), rate(i, "Pitch")));
            assert!(
                correlation(&pitch_rate) > 0.9,
                "{name}: PitchRate isn't Pitch's rate"
            );
            // Climbing says which way the nose points only where the track climbs
            let (low, high) = samples.iter().fold((f64::MAX, f64::MIN), |(l, h), &i| {
                (l.min(at(i, "Alt")), h.max(at(i, "Alt")))
            });
            if high - low >= 5.0 {
                let climb = |i: usize| (at(i + 3, "Alt") - at(i - 3, "Alt")) / (6.0 * dt);
                // Without braking or accelerating hard: the nose dives and squats more than slopes
                let steady: Vec<_> = samples
                    .iter()
                    .copied()
                    .filter(|&i| rate(i, "Speed").abs() < 1.0)
                    .collect();
                let pitch: Vec<_> = steady
                    .iter()
                    .map(|&i| (at(i, "Pitch"), climb(i) / at(i, "Speed")))
                    .collect();
                // Enough of them, over slopes that differ (half a percent), to tell
                let slopes = pitch.len().max(1) as f64;
                let mean = pitch.iter().map(|(_, s)| s).sum::<f64>() / slopes;
                let spread =
                    (pitch.iter().map(|(_, s)| (s - mean).powi(2)).sum::<f64>() / slopes).sqrt();
                if pitch.len() >= 600 && spread >= 0.005 {
                    climbed += 1;
                    assert!(correlation(&pitch) < 0.0, "{name}: Pitch isn't + nose down");
                    // Nose up, gravity pulls back on the car: the accelerometer reads it forward
                    let gravity: Vec<_> = steady
                        .iter()
                        .map(|&i| (at(i, "Pitch"), at(i, "LongAccel") - rate(i, "Speed")))
                        .collect();
                    assert!(
                        correlation(&gravity) < 0.0,
                        "{name}: Pitch isn't + nose down by gravity"
                    );
                }
                let vertical = pairs(&|i| {
                    let nose = at(i, "VelocityX") * (-at(i, "Pitch")).sin();
                    (at(i, "VelocityZ"), climb(i) - nose)
                });
                assert!(correlation(&vertical) > 0.0, "{name}: VelocityZ isn't + up");
            }
            // The wind, by what it does flat out on the straights: a tailwind is less drag
            let wind =
                samples.iter().map(|&i| at(i, "WindVel")).sum::<f64>() / samples.len() as f64;
            let gear = |i: usize| ibt.frame(i).i32("Gear");
            let flat_out = |i: usize| {
                [i - 6, i, i + 6].iter().all(|&j| {
                    at(j, "ThrottleRaw") >= 0.99
                        && at(j, "BrakeRaw") <= 0.01
                        && gear(j).is_some()
                        && gear(j) == gear(i)
                }) && at(i, "LatAccel").abs() < 2.0
                    && at(i, "Speed") > 20.0
            };
            // How much faster than the car usually goes at that gear and speed, uphill or not
            let gains: Vec<_> = samples
                .iter()
                .filter(|&&i| flat_out(i))
                .map(|&i| {
                    let v = at(i, "Speed");
                    let gain = (at(i + 6, "Speed") - at(i - 6, "Speed")) / (12.0 * dt)
                        + 9.81 * (at(i + 6, "Alt") - at(i - 6, "Alt")) / (12.0 * dt) / v;
                    ((gear(i), v.round() as i64), gain, i)
                })
                .collect();
            // Within each gear and speed, the faster against the more tailwind: both less what's
            // usual there, so what's left is the wind, where it varies (other straights, headings).
            // As the wind blows from WindDir clockwise from north, or else counter-clockwise
            let residuals = |clockwise: f64| {
                let tailwind = |i: usize| {
                    let from = clockwise * at(i, "WindDir") - at(i, "YawNorth");
                    -at(i, "WindVel") * from.cos()
                };
                let mut usual = std::collections::HashMap::<_, (f64, f64, usize)>::new();
                for (key, gain, i) in &gains {
                    let entry = usual.entry(*key).or_default();
                    *entry = (entry.0 + gain, entry.1 + tailwind(*i), entry.2 + 1);
                }
                gains
                    .iter()
                    .filter(|(key, _, _)| usual[key].2 >= 20)
                    .map(|(key, gain, i)| {
                        let (gains, tails, count) = usual[key];
                        let count = count as f64;
                        (tailwind(*i) - tails / count, gain - gains / count)
                    })
                    .collect::<Vec<_>>()
            };
            let rms = |values: &mut dyn Iterator<Item = f64>| {
                let (sum, count) = values.fold((0.0, 0usize), |(s, n), v| (s + v * v, n + 1));
                (sum / count.max(1) as f64).sqrt()
            };
            let (tail, other) = (residuals(1.0), residuals(-1.0));
            if wind >= 3.0 && tail.len() >= 2000 && rms(&mut tail.iter().map(|(t, _)| *t)) >= 0.5 {
                winded += 1;
                assert!(
                    correlation(&tail) > 0.0,
                    "{name}: WindDir isn't where the wind blows from"
                );
                // Wind from the north or south reads the same either way round, and so can a
                // track whose straights point two ways: only where the readings go apart
                let alike: Vec<_> = tail
                    .iter()
                    .zip(&other)
                    .map(|((a, _), (b, _))| (*a, *b))
                    .collect();
                if correlation(&alike) < 0.9 {
                    turned += 1;
                    assert!(
                        correlation(&tail) > correlation(&other),
                        "{name}: WindDir doesn't turn clockwise"
                    );
                }
            }
        }
        assert!(checked > 0, "no saved laps long enough to check");
        assert!(calmed > 0, "no saved laps calm enough to check gravity");
        assert!(climbed > 0, "no saved laps climb enough to check pitch");
        assert!(winded > 0, "no saved laps windy enough to check the wind");
        assert!(turned > 0, "no saved laps tell which way WindDir turns");
    }

    #[test]
    fn the_start_counts_count_only_when_they_are_counts() {
        use sdk::Buf;
        let header = |bufs: [(i32, i32, i32); 3]| {
            let mut header = Header::parse(&[0u8; HEADER_LEN]).unwrap();
            header.buf_count = 3;
            for (buf, (tick, offset, tick_begin)) in header.bufs.iter_mut().zip(bufs) {
                *buf = Buf {
                    tick,
                    offset,
                    tick_begin,
                };
            }
            header
        };
        // Padding (zero, or anything that isn't a count): the tick count alone
        assert!(header([(5, 100, 0), (6, 200, 0), (7, 300, 0)]).holds(7, 300));
        assert!(header([(5, 100, 9999), (6, 200, 9999), (7, 300, 9999)]).holds(7, 300));
        // Counts: a buffer being written isn't held whole, the others are
        let writing = header([(8, 100, 8), (6, 200, 9), (7, 300, 7)]);
        assert!(!writing.holds(6, 200));
        assert!(writing.holds(7, 300));
        assert!(!writing.holds(7, 100));
    }

    #[test]
    fn reads_the_layout_of_a_header() {
        let mut bytes = vec![0u8; HEADER_LEN + 144];
        let ints = [2, 1, 60, 7, 10, 300, 1, HEADER_LEN as i32, 3, 16];
        for (i, v) in ints.iter().enumerate() {
            bytes[i * 4..i * 4 + 4].copy_from_slice(&v.to_le_bytes());
        }
        for (i, (tick, offset)) in [(5, 1000), (7, 2000), (6, 3000)].iter().enumerate() {
            let at = 48 + i * 16;
            bytes[at..at + 4].copy_from_slice(&i32::to_le_bytes(*tick));
            bytes[at + 4..at + 8].copy_from_slice(&i32::to_le_bytes(*offset));
        }
        // One double at offset 8 of each update
        let var = HEADER_LEN;
        bytes[var..var + 4].copy_from_slice(&5i32.to_le_bytes());
        bytes[var + 4..var + 8].copy_from_slice(&8i32.to_le_bytes());
        bytes[var + 8..var + 12].copy_from_slice(&1i32.to_le_bytes());
        bytes[var + 16..var + 27].copy_from_slice(b"SessionTime");

        let header = Header::parse(&bytes).unwrap();
        assert!(header.connected());
        assert_eq!(header.latest(), Some((7, 2000)));
        assert_eq!(header.session_info(), Some(300..310));
        let vars = Vars::parse(&bytes, &header).unwrap();
        let mut update = vec![0u8; 16];
        update[8..].copy_from_slice(&123.5f64.to_le_bytes());
        let frame = Frame::new(&vars, &update);
        assert_eq!(frame.f64("SessionTime"), Some(123.5));
        assert_eq!(frame.i32("SessionTime"), None);
        assert_eq!(frame.f64("Speed"), None);
        assert_eq!(frame.f64_at("SessionTime", 1), None);
    }
}
