//! RaceRoom Racing Experience, through Steam. It has no plugins: it publishes its state in a
//! shared memory block (`$R3E`, see `r3e.h`), which a thread of ours reads while the game runs and
//! records like LMU's plugin does (`bono_sim::recorder`), into a folder of the app's own. The
//! recording is deleted once the game closes.

use crate::{
    sim::{Simulator, SimulatorInfo, SimulatorSettings},
    sqlite::{Database, pool::Pool},
};
use bono_sim::{
    recorder::{DATABASE, Recorder, SETTINGS, try_remove_database},
    sys::{bono_session_info, bono_standing, bono_telemetry},
};
use serde_json::{Map, Value};
use std::{
    collections::HashMap,
    num::NonZeroUsize,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread::JoinHandle,
    time::{Duration, Instant},
};
use sys::*;

const STEAM_APP_ID: u32 = 211500;

const MAX_IDLE: NonZeroUsize = NonZeroUsize::new(2).unwrap();
const MAX_OPEN: NonZeroUsize = NonZeroUsize::new(4).unwrap();

/// How often the shared memory is read: the game updates it every frame.
const POLL: Duration = Duration::from_millis(16);
/// How often the session and the standings are recorded (LMU's scoring rate).
const SCORING_EVERY: Duration = Duration::from_millis(200);
/// How long before looking for the shared memory again, while the game hasn't made it yet.
const RETRY: Duration = Duration::from_secs(1);

/// Once the game closes, how long the database is left for the connections still reading it.
const REMOVE_FOR: Duration = Duration::from_secs(60);
const REMOVE_EVERY: Duration = Duration::from_secs(1);

pub struct RaceRoom {
    /// Where the recording goes
    dir: PathBuf,
    /// Reading the game while it runs
    reader: Option<Reader>,
    /// The database being recorded, read-only
    live: Option<Pool>,
}

impl RaceRoom {
    pub fn new() -> Self {
        Self {
            dir: std::env::temp_dir().join("bono").join("raceroom"),
            reader: None,
            live: None,
        }
    }

    /// Changes settings.json, keeping what else is in it: the recorder acts on any value that
    /// changes.
    fn update_settings(&self, change: impl FnOnce(&mut Map<String, Value>)) -> anyhow::Result<()> {
        let path = self.dir.join(SETTINGS);
        let mut settings = std::fs::read(&path)
            .ok()
            .and_then(|bytes| serde_json::from_slice::<Map<String, Value>>(&bytes).ok())
            .unwrap_or_default();
        change(&mut settings);

        std::fs::create_dir_all(&self.dir)?;
        // Written whole, so the recorder never reads half a file
        let temp = path.with_extension("json.tmp");
        std::fs::write(&temp, serde_json::to_vec_pretty(&settings)?)?;
        std::fs::rename(&temp, &path)?;
        Ok(())
    }
}

impl SimulatorInfo for RaceRoom {
    const NAME: &'static str = "RaceRoom";
    const TRACK_DATA_ID: &'static str = "rrre";
    const APP_ID: u32 = STEAM_APP_ID;
}

impl Simulator for RaceRoom {
    /// The database being recorded while the game runs (once it's on track), started on the first
    /// call it does. Not running, the reader stops and the recording is deleted.
    fn database(&mut self, running: bool) -> Option<&Pool> {
        if !running {
            // Stopped first, so the recorder writes what it has and lets go of the file
            self.reader = None;
            if let Some(pool) = self.live.take() {
                tokio::spawn(remove_when_closed(self.dir.clone(), pool));
            }
            return None;
        }

        if self.reader.is_none() {
            // Nothing is recording yet: don't serve a database left by an earlier app run.
            if let Err(e) = try_remove_database(&self.dir) {
                log::warn!("Couldn't remove RaceRoom's previous recording: {e}");
                return None;
            }
            self.reader = Some(Reader::start(self.dir.clone()));
        }

        let path = self.dir.join(DATABASE);
        if !path.is_file() {
            return None;
        }
        Some(self.live.get_or_insert_with(|| {
            Pool::new(Database::read_only(Arc::new(path)), MAX_IDLE, MAX_OPEN)
        }))
    }

    /// The recorder reads settings.json when it starts and whenever it changes.
    fn apply(&mut self, settings: &SimulatorSettings) {
        let frequency = settings.opponent_frequency.get();
        if let Err(e) = self.update_settings(|json| {
            json.insert("opponent_frequency".into(), frequency.into());
        }) {
            log::error!("Error passing the recording settings to RaceRoom's recorder: {e}");
        }
    }

    /// The recorder clears its data when the "clear" count in settings.json changes.
    fn clear(&mut self) {
        if let Err(e) = self.update_settings(|json| {
            let clear = json.get("clear").and_then(Value::as_i64).unwrap_or(0);
            json.insert("clear".into(), (clear + 1).into());
        }) {
            log::error!("Error asking RaceRoom's recorder to clear its data: {e}");
        }
    }
}

/// Deletes the database in `dir` once nothing has it open.
async fn remove_when_closed(dir: PathBuf, pool: Pool) {
    drop(pool);

    let mut waited = Duration::ZERO;
    loop {
        tokio::time::sleep(REMOVE_EVERY).await;
        waited += REMOVE_EVERY;
        match try_remove_database(&dir) {
            Ok(()) => return,
            Err(e) if waited >= REMOVE_FOR => {
                log::warn!(
                    "Couldn't remove RaceRoom's recording in {} (is another program reading \
                     it?): {e}. It's replaced the next time the game records",
                    dir.display()
                );
                return;
            }
            Err(_) => {}
        }
    }
}

/// The thread reading the game into the recorder, stopped when dropped.
struct Reader {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Reader {
    fn start(dir: PathBuf) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let thread = std::thread::Builder::new()
            .name("bono raceroom".into())
            .spawn({
                let stop = stop.clone();
                move || read(&dir, &stop)
            })
            .inspect_err(|e| log::error!("Error starting RaceRoom's reader: {e}"))
            .ok();
        Self { stop, thread }
    }
}

impl Drop for Reader {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// Reads the shared memory until `stop`, recording what's new.
fn read(dir: &Path, stop: &AtomicBool) {
    let mut memory = None::<SharedMemory>;
    let mut recorder = None::<Recorder>;
    let mut converter = Converter::default();
    let mut warned_version = false;
    let mut retry_at = Instant::now();
    let mut retry_delay = RETRY;
    let mut warned_recorder = false;
    let mut healthy_since = None::<Instant>;

    while !stop.load(Ordering::Relaxed) {
        if memory.is_none() {
            memory = SharedMemory::open();
        }
        let Some(shared) = &memory else {
            std::thread::sleep(RETRY);
            continue;
        };

        let snapshot = shared.read();
        if snapshot.version_major != R3E_VERSION_MAJOR {
            if !warned_version {
                log::warn!(
                    "RaceRoom's shared memory is version {}.{}, not {R3E_VERSION_MAJOR}: not \
                     recording it",
                    { snapshot.version_major },
                    { snapshot.version_minor }
                );
                warned_version = true;
            }
            std::thread::sleep(RETRY);
            continue;
        }

        let now = Instant::now();
        if recorder.as_ref().is_some_and(Recorder::failed) && healthy_since.take().is_some() {
            retry_at = now + retry_delay;
            retry_delay = (retry_delay * 2).min(Duration::from_secs(60));
            if !warned_recorder {
                log::error!("Recording RaceRoom stopped on an error, retrying with backoff");
                warned_recorder = true;
            }
        }
        if now < retry_at {
            std::thread::sleep(POLL);
            continue;
        }

        let Some(update) = converter.convert(&snapshot, now) else {
            std::thread::sleep(POLL);
            continue;
        };

        // Only once there's something to record, so menus leave no empty database.
        // A failed writer resumes the same recording, preserving earlier sessions.
        if recorder.as_ref().is_none_or(Recorder::failed) {
            let result = match &mut recorder {
                Some(recorder) => recorder.restart(None),
                None => Recorder::new(dir, None).map(|new| recorder = Some(new)),
            };
            if let Err(e) = result {
                if !warned_recorder {
                    log::error!("Error starting to record RaceRoom: {e}");
                    warned_recorder = true;
                }
                retry_at = now + retry_delay;
                retry_delay = (retry_delay * 2).min(Duration::from_secs(60));
                std::thread::sleep(POLL);
                continue;
            }
            healthy_since = Some(now);
        }
        // Opening a writer thread isn't success yet: only reset after it stays alive.
        if healthy_since.is_some_and(|since| now.duration_since(since) >= Duration::from_secs(60)) {
            retry_delay = RETRY;
            warned_recorder = false;
        }
        if let Some(recorder) = &recorder {
            record(recorder, update);
        }
        std::thread::sleep(POLL);
    }
}

/// Hands an update to the recorder.
fn record(recorder: &Recorder, update: Update) {
    // Another event (track, layout): what was recorded is about something else
    if update.new_event {
        recorder.clear();
    }
    // Scoring first: it says which car is the player's, which the telemetry needs
    if let Some((session, standings)) = update.scoring {
        recorder.push_scoring(session, standings);
    }
    for telemetry in &update.telemetry {
        let vehicle = telemetry.vehicle_id.get().unwrap_or(-1);
        let time = telemetry.session_time.get().unwrap_or(0.0);
        let speed = telemetry.speed.get().unwrap_or(0.0);
        if recorder.wants_telemetry(vehicle, time, speed) {
            recorder.push_telemetry(telemetry);
        }
    }
}

/// The game's shared memory, mapped read-only.
struct SharedMemory {
    handle: windows_sys::Win32::Foundation::HANDLE,
    view: windows_sys::Win32::System::Memory::MEMORY_MAPPED_VIEW_ADDRESS,
}

// The view is only read, by copying it whole
unsafe impl Send for SharedMemory {}

impl SharedMemory {
    /// `None` while the game hasn't made it (it isn't running yet, or hasn't loaded).
    fn open() -> Option<Self> {
        use windows_sys::Win32::{
            Foundation::CloseHandle,
            System::Memory::{FILE_MAP_READ, MapViewOfFile, OpenFileMappingW},
        };

        let name = R3E_SHARED_MEMORY_NAME
            .iter()
            .map(|&b| b as u16)
            .collect::<Vec<_>>();
        let handle = unsafe { OpenFileMappingW(FILE_MAP_READ, 0, name.as_ptr()) };
        if handle.is_null() {
            return None;
        }

        let view = unsafe { MapViewOfFile(handle, FILE_MAP_READ, 0, 0, size_of::<r3e_shared>()) };
        if view.Value.is_null() {
            unsafe { CloseHandle(handle) };
            return None;
        }
        Some(Self { handle, view })
    }

    /// A copy of what's in it now. The game writes it as it pleases, so a copy can mix two
    /// frames: fine for what's recorded.
    fn read(&self) -> Box<r3e_shared> {
        let mut copy = Box::<r3e_shared>::new_uninit();
        unsafe {
            std::ptr::copy_nonoverlapping(
                self.view.Value.cast::<u8>(),
                copy.as_mut_ptr().cast::<u8>(),
                size_of::<r3e_shared>(),
            );
            copy.assume_init()
        }
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

/// One read of the game, for the recorder.
struct Update {
    telemetry: Vec<bono_telemetry>,
    /// Every `SCORING_EVERY`
    scoring: Option<(bono_session_info, Vec<bono_standing>)>,
    /// The track or layout changed since the last update
    new_event: bool,
}

/// Turns reads of the game into the recorder's updates (LMU's units and conventions).
#[derive(Default)]
struct Converter {
    /// The physics time of the last update: the game paused, or in menus, doesn't move it
    last_time: Option<f64>,
    last_scoring: Option<Instant>,
    /// The event being recorded: track, layout and event index
    event: Option<(i32, i32, i32)>,
    /// Each car's last position and heading, by slot: heading comes from how it moved
    cars: HashMap<i32, Motion>,
}

#[derive(Clone, Copy)]
struct Motion {
    x: f64,
    z: f64,
    heading: Option<f64>,
}

/// How far a car must have moved for a new heading (less is noise, or standing still).
const HEADING_MIN_MOVE: f64 = 0.3;

impl Converter {
    /// The update for `shared` read at `now`: `None` when there's nothing new to record (menus, a
    /// pause, no session, the same physics frame).
    fn convert(&mut self, shared: &r3e_shared, now: Instant) -> Option<Update> {
        let player = shared.player;
        let time = player.game_simulation_time;
        let in_session = shared.game_in_menus != 1
            && shared.session_type != R3E_SESSION_UNAVAILABLE
            && shared.layout_length > 0.0;
        if !in_session || self.last_time == Some(time) {
            return None;
        }
        let dt = self
            .last_time
            .map(|last| time - last)
            .filter(|dt| *dt > 0.0);
        self.last_time = Some(time);

        let event = (shared.track_id, shared.layout_id, shared.event_index);
        let new_event = self.event.is_some_and(|last| last != event);
        self.event = Some(event);
        if new_event {
            self.cars.clear();
        }

        let count = (shared.num_cars.max(0) as usize).min(R3E_NUM_DRIVERS_MAX as usize);
        let drivers = shared.all_drivers_data_1;
        let drivers = &drivers[..count];
        let vehicle_info = shared.vehicle_info;
        let player_slot = vehicle_info.slot_id;

        let mut telemetry = Vec::with_capacity(drivers.len());
        for driver in drivers {
            let info = driver.driver_info;
            let slot = info.slot_id;
            let mut out = bono_telemetry::default();
            out.vehicle_id = Some(slot as i64).into();
            out.session_time = Some(time).into();
            out.time_since_last_update = dt.into();
            out.lap = Some(driver.completed_laps.max(0) as i64).into();
            out.lap_start_time = seconds(driver.lap_time_current_self)
                .map(|into_lap| time - into_lap)
                .into();
            out.lap_distance = non_negative(driver.lap_distance).into();
            out.sector = sector(driver.track_sector).into();
            out.in_pit_lane = flag(driver.in_pitlane).into();
            out.lap_invalidated = flag(driver.current_lap_valid).map(|valid| !valid).into();
            out.gap_to_place_ahead = seconds(driver.time_delta_front).into();
            out.gap_to_place_behind = seconds(driver.time_delta_behind).into();

            let position = driver.position;
            let (x, y, z) = (position.x as f64, position.y as f64, position.z as f64);
            out.position_x = Some(x).into();
            out.position_y = Some(y).into();
            out.position_z = Some(z).into();
            out.speed = non_negative(driver.car_speed).into();
            out.virtual_energy = non_negative(driver.virtual_energy).into();

            // The direction it moved in since it last moved enough (forward is -z at heading 0)
            let last = self.cars.get(&slot).copied();
            let heading = match last {
                Some(last) if (x - last.x).hypot(z - last.z) >= HEADING_MIN_MOVE => {
                    Some(heading(x - last.x, z - last.z))
                }
                Some(last) => last.heading,
                None => None,
            };
            let moved = last.is_none_or(|last| heading != last.heading);
            if moved {
                self.cars.insert(slot, Motion { x, z, heading });
            }
            out.heading = heading.into();

            if slot == player_slot {
                player_telemetry(shared, &mut out);
            }
            telemetry.push(out);
        }

        let scoring = match self.last_scoring {
            Some(last) if now.duration_since(last) < SCORING_EVERY => None,
            _ => {
                self.last_scoring = Some(now);
                Some(scoring(shared, drivers, time))
            }
        };

        Some(Update {
            telemetry,
            scoring,
            new_event,
        })
    }
}

/// What only the player's car has: its physics, inputs, engine, tires and settings.
fn player_telemetry(shared: &r3e_shared, out: &mut bono_telemetry) {
    let player = shared.player;

    // The player's own position and heading are exact
    let (position, velocity) = (player.position, player.velocity);
    out.position_x = Some(position.x).into();
    out.position_y = Some(position.y).into();
    out.position_z = Some(position.z).into();
    if velocity.x.hypot(velocity.z) > 1.0 {
        out.heading = Some(heading(velocity.x, velocity.z)).into();
    }
    let orientation = shared.car_orientation;
    out.pitch = Some(orientation.pitch as f64).into();
    out.roll = Some(orientation.roll as f64).into();

    out.speed = non_negative(shared.car_speed).into();
    out.delta_best = Some(shared.time_delta_best_self as f64)
        .filter(|delta| *delta > -1000.0)
        .into();
    // Local axes: +x left, +y up, +z back, like LMU's, so lateral (+ right) is -x
    let (velocity, acceleration, rotation) = (
        player.local_velocity,
        player.local_acceleration,
        player.local_angular_velocity,
    );
    out.velocity_longitudinal = Some(-velocity.z).into();
    out.velocity_lateral = Some(-velocity.x).into();
    out.velocity_vertical = Some(velocity.y).into();
    out.acceleration_longitudinal = Some(-acceleration.z).into();
    out.acceleration_lateral = Some(-acceleration.x).into();
    out.acceleration_vertical = Some(acceleration.y).into();
    out.pitch_rate = Some(rotation.x).into();
    out.yaw_rate = Some(rotation.y).into();
    out.roll_rate = Some(rotation.z).into();

    out.throttle = non_negative(shared.throttle_raw).into();
    out.brake = non_negative(shared.brake_raw).into();
    out.clutch = non_negative(shared.clutch_raw).into();
    out.steering = Some(shared.steer_input_raw as f64).into();
    out.throttle_filtered = non_negative(shared.throttle).into();
    out.brake_filtered = non_negative(shared.brake).into();
    out.clutch_filtered = non_negative(shared.clutch).into();
    out.steering_shaft_torque = Some(player.steering_force).into();

    out.gear = Some(shared.gear as i64).filter(|gear| *gear >= -1).into();
    out.engine_rpm = non_negative(shared.engine_rps)
        .map(|rps| rps * 60.0 / std::f64::consts::TAU)
        .into();
    out.engine_torque = Some(player.engine_torque).into();
    out.engine_water_temperature = non_negative(shared.engine_temp).into();
    out.engine_oil_temperature = non_negative(shared.engine_oil_temp).into();
    out.turbo_boost_pressure = non_negative(shared.turbo_pressure).into();

    out.fuel = non_negative(shared.fuel_left).into();
    out.virtual_energy =
        fraction(shared.virtual_energy_left, shared.virtual_energy_capacity).into();
    out.battery_charge = non_negative(shared.battery_soc)
        .map(|percent| percent / 100.0)
        .into();
    out.speed_limiter_active = flag(shared.pit_limiter).into();

    out.front_third_spring_deflection =
        Some(player.third_spring_suspension_deflection_front).into();
    out.rear_third_spring_deflection = Some(player.third_spring_suspension_deflection_rear).into();
    out.front_wing_height = Some(player.front_wing_height).into();
    let ride_height = player.ride_height;
    out.front_ride_height = Some((ride_height[0] + ride_height[1]) / 2.0).into();
    out.rear_ride_height = Some((ride_height[2] + ride_height[3]) / 2.0).into();
    out.front_downforce = Some(player.current_downforce).into();

    let state = &mut out.state;
    state.traction_control = setting(shared.traction_control_setting).into();
    state.abs = setting(shared.abs_setting).into();
    state.engine_map = setting(shared.engine_map_setting).into();
    state.rear_brake_bias = non_negative(shared.brake_bias).into();
    state.speed_limiter_on = flag(shared.pit_limiter).into();
    state.headlights = Some(shared.headlights)
        .filter(|lights| *lights >= 0)
        .map(|lights| lights > 0)
        .into();
    state.front_tire_compound = setting(shared.tire_subtype_front).into();
    state.rear_tire_compound = setting(shared.tire_subtype_rear).into();
    state.fuel_capacity = non_negative(shared.fuel_capacity).into();
    state.fuel_per_lap = non_negative(shared.fuel_per_lap).into();
    state.virtual_energy_per_lap = fraction(
        shared.virtual_energy_per_lap,
        shared.virtual_energy_capacity,
    )
    .into();
    // The game's are how whole each part is
    let damage = shared.car_damage;
    let damaged = |health: f32| non_negative(health).map(|health| (1.0 - health).clamp(0.0, 1.0));
    state.engine_damage = damaged(damage.engine).into();
    state.transmission_damage = damaged(damage.transmission).into();
    state.aerodynamics_damage = damaged(damage.aerodynamics).into();
    state.suspension_damage = damaged(damage.suspension).into();

    let (deflection, camber) = (player.suspension_deflection, player.camber);
    let (tire_temp, brake_temp) = (shared.tire_temp, shared.brake_temp);
    let (pressure, wear, load) = (shared.tire_pressure, shared.tire_wear, shared.tire_load);
    let (rps, material) = (shared.tire_rps, shared.tire_on_mtrl);
    for (i, wheel) in out.wheels.iter_mut().enumerate() {
        wheel.suspension_deflection = Some(deflection[i]).into();
        wheel.ride_height = Some(ride_height[i]).into();
        wheel.camber = Some(camber[i]).into();
        wheel.brake_temperature = non_negative(brake_temp[i].current_temp).into();
        wheel.rotation_speed = Some(rps[i] as f64).into();
        wheel.tire_load = non_negative(load[i]).into();
        wheel.surface_type = surface(material[i]).into();
        wheel.tire_pressure = non_negative(pressure[i]).into();
        let temps = tire_temp[i].current_temp;
        wheel.tire_surface_temperature_left = non_negative(temps[0]).into();
        wheel.tire_surface_temperature_center = non_negative(temps[1]).into();
        wheel.tire_surface_temperature_right = non_negative(temps[2]).into();
        wheel.tire_optimal_temperature = non_negative(tire_temp[i].optimal_temp).into();
        wheel.tire_wear = non_negative(wear[i]).into();
    }
}

/// The session, and every car's standing.
fn scoring(
    shared: &r3e_shared,
    drivers: &[r3e_driver_data],
    time: f64,
) -> (bono_session_info, Vec<bono_standing>) {
    let mut session = bono_session_info::default();
    session.session_time = Some(time).into();

    let (track, layout) = (text(&shared.track_name), text(&shared.layout_name));
    let track = match layout.is_empty() || layout == track {
        true => track,
        false => format!("{track} - {layout}"),
    };
    session.track = Some(track).into();
    session.track_length = non_negative(shared.layout_length).into();
    session.session_type = match shared.session_type {
        R3E_SESSION_PRACTICE => Some("practice"),
        R3E_SESSION_QUALIFY => Some("qualifying"),
        R3E_SESSION_RACE => Some("race"),
        R3E_SESSION_WARMUP => Some("warmup"),
        _ => None,
    }
    .map(str::to_owned)
    .into();
    session.session_number = setting(shared.session_iteration).into();
    session.phase = match shared.session_phase {
        R3E_SESSION_PHASE_GARAGE => Some("before_start"),
        R3E_SESSION_PHASE_GRIDWALK => Some("grid_walk"),
        R3E_SESSION_PHASE_FORMATION => Some("formation_lap"),
        R3E_SESSION_PHASE_COUNTDOWN => Some("countdown"),
        R3E_SESSION_PHASE_GREEN => Some("green"),
        R3E_SESSION_PHASE_CHECKERED => Some("over"),
        _ => None,
    }
    .map(str::to_owned)
    .into();

    // Timed sessions: on the same clock as the telemetry
    let remaining = non_negative(shared.session_time_remaining);
    let elapsed = non_negative(shared.session_time_duration)
        .zip(remaining)
        .map(|(duration, remaining)| duration - remaining);
    session.time_remaining = remaining.into();
    session.session_end_time = remaining
        .map(|remaining| scoring_time(time + remaining))
        .into();
    if shared.session_length_format != R3E_SESSION_LENGTH_TIME_BASED {
        session.max_laps = setting(shared.number_of_laps).into();
    }
    session.vehicle_count = Some(drivers.len() as i64).into();
    session.start_light = setting(shared.start_lights).into();
    session.in_realtime =
        Some(shared.game_in_menus != 1 && shared.game_in_replay != 1 && shared.game_paused != 1)
            .into();
    session.player_name = Some(text(&shared.player_name)).into();
    if shared.game_mode == R3E_GAMEMODE_MULTIPLAYER
        || shared.game_mode == R3E_GAMEMODE_MULTIPLAYERRANKED
    {
        session.online_role = Some("client".to_owned()).into();
    }

    let flags = shared.flags;
    let sector_yellow = flags.sector_yellow;
    session.local_yellow_1 = flag(sector_yellow[0]).into();
    session.local_yellow_2 = flag(sector_yellow[1]).into();
    session.local_yellow_3 = flag(sector_yellow[2]).into();

    session.pit_window_status = match shared.pit_window_status {
        R3E_PIT_WINDOW_DISABLED => Some("disabled"),
        R3E_PIT_WINDOW_CLOSED => Some("closed"),
        R3E_PIT_WINDOW_OPEN => Some("open"),
        R3E_PIT_WINDOW_STOPPED => Some("stopped"),
        R3E_PIT_WINDOW_COMPLETED => Some("completed"),
        _ => None,
    }
    .map(str::to_owned)
    .into();
    let (start, end) = (
        setting(shared.pit_window_start),
        setting(shared.pit_window_end),
    );
    match shared.session_length_format {
        // Minutes into the session
        R3E_SESSION_LENGTH_TIME_BASED => {
            let at = |minutes: i64| {
                elapsed.map(|elapsed| scoring_time(time - elapsed + minutes as f64 * 60.0))
            };
            session.pit_window_start_time = start.and_then(at).into();
            session.pit_window_end_time = end.and_then(at).into();
        }
        _ => {
            session.pit_window_start_lap = start.into();
            session.pit_window_end_lap = end.into();
        }
    }

    let player_slot = shared.vehicle_info.slot_id;
    let standings = drivers
        .iter()
        .map(|driver| {
            standing(
                shared,
                driver,
                time,
                driver.driver_info.slot_id == player_slot,
            )
        })
        .collect();
    (session, standings)
}

/// Derived scoring timestamps mix the f64 clock with f32 durations. Discard their
/// sub-millisecond noise so exact change detection can deduplicate scoring rows.
fn scoring_time(time: f64) -> f64 {
    (time * 1000.0).round() / 1000.0
}

fn standing(
    shared: &r3e_shared,
    driver: &r3e_driver_data,
    time: f64,
    is_player: bool,
) -> bono_standing {
    let info = driver.driver_info;
    let mut out = bono_standing::default();
    out.session_time = Some(time).into();
    out.vehicle_id = Some(info.slot_id as i64).into();
    out.is_player = Some(is_player).into();
    out.driver = Some(text(&info.name)).into();
    out.vehicle_class = Some(info.class_id)
        .filter(|class| *class >= 0)
        .map(|class| format!("class {class}"))
        .into();
    out.position = setting(driver.place).into();
    out.laps_completed = setting(driver.completed_laps).into();
    out.sector = sector(driver.track_sector).into();
    out.finish_status = match driver.finish_status {
        R3E_FINISH_STATUS_NONE => Some("none"),
        R3E_FINISH_STATUS_FINISHED => Some("finished"),
        R3E_FINISH_STATUS_DNF => Some("dnf"),
        R3E_FINISH_STATUS_DNQ => Some("dnq"),
        R3E_FINISH_STATUS_DNS => Some("dns"),
        R3E_FINISH_STATUS_DQ => Some("disqualified"),
        _ => None,
    }
    .map(str::to_owned)
    .into();
    out.lap_start_time = seconds(driver.lap_time_current_self)
        .map(|into_lap| scoring_time(time - into_lap))
        .into();

    // The game's sector times add up along the lap: the third is the lap's
    let (previous, best, current) = (
        driver.sector_time_previous_self,
        driver.sector_time_best_self,
        driver.sector_time_current_self,
    );
    let split = |times: [f32; 3], i: usize| match i {
        0 => seconds(times[0]),
        _ => seconds(times[i])
            .zip(seconds(times[i - 1]))
            .map(|(to, from)| to - from),
    };
    out.last_lap_time = seconds(previous[2]).into();
    out.last_sector_1 = split(previous, 0).into();
    out.last_sector_2 = split(previous, 1).into();
    out.last_sector_3 = split(previous, 2).into();
    out.best_lap_time = seconds(best[2]).into();
    out.best_lap_sector_1 = split(best, 0).into();
    out.current_sector_1 = split(current, 0).into();
    out.current_sector_2 = split(current, 1).into();

    out.time_behind_next = seconds(driver.time_delta_front).into();
    out.pit_stops = setting(driver.num_pitstops).into();
    out.mandatory_pit_stop = match driver.pitstop_status {
        R3E_PITSTOP_STATUS_TWO_TYRES_UNSERVED => Some("unserved_two_tires"),
        R3E_PITSTOP_STATUS_FOUR_TYRES_UNSERVED => Some("unserved_four_tires"),
        R3E_PITSTOP_STATUS_SERVED => Some("served"),
        _ => None,
    }
    .map(str::to_owned)
    .into();
    let penalties = driver.penalties;
    out.penalties = Some(
        [
            penalties.drive_through,
            penalties.stop_and_go,
            penalties.pit_stop,
            penalties.time_deduction,
            penalties.slow_down,
        ]
        .iter()
        .filter(|penalty| **penalty >= 0.0)
        .count() as i64,
    )
    .into();
    out.penalty_drive_through_active = non_negative(penalties.drive_through).map(|_| true).into();
    out.penalty_stop_and_go_seconds = non_negative(penalties.stop_and_go).into();
    out.penalty_pit_stop_active = non_negative(penalties.pit_stop).map(|_| true).into();
    out.penalty_time_seconds = non_negative(penalties.time_deduction).into();
    out.penalty_slow_down_seconds = non_negative(penalties.slow_down).into();
    out.penalty_type = penalty_type(driver.penaltyType).map(str::to_owned).into();
    let mut source = serde_json::json!({
        "penalty_type": setting(driver.penaltyType),
        "penalty_reason": setting(driver.penaltyReason),
        "penalties": {
            "drive_through": non_negative(penalties.drive_through),
            "stop_and_go": non_negative(penalties.stop_and_go),
            "pit_stop": non_negative(penalties.pit_stop),
            "time_deduction": non_negative(penalties.time_deduction),
            "slow_down": non_negative(penalties.slow_down),
        }
    });
    out.penalty_reason = penalty_reason(driver.penaltyType, driver.penaltyReason)
        .map(str::to_owned)
        .into();
    out.in_pits = flag(driver.in_pitlane).into();
    out.lap_distance = non_negative(driver.lap_distance).into();
    out.fuel_fraction = non_negative(driver.virtual_energy).into();

    if is_player {
        out.pit_state = match shared.pit_state {
            0 => Some("none"),
            1 => Some("requested"),
            2 => Some("entering"),
            3 => Some("stopped"),
            4 => Some("exiting"),
            _ => None,
        }
        .map(str::to_owned)
        .into();
        let menu = shared.pit_menu_state;
        let action = setting(shared.pit_action);
        source["pit_menu_state"] = serde_json::json!(menu.map(setting));
        source["pit_action"] = serde_json::json!(action);
        out.pit_service_preparing = action.map(|bits| bits & 1 != 0).into();
        out.pit_service_penalty_selected = flag(menu[1]).into();
        out.pit_service_penalty_active = action.map(|bits| bits & 2 != 0).into();
        out.pit_service_driver_change_selected = flag(menu[2]).into();
        out.pit_service_driver_change_active = action.map(|bits| bits & 4 != 0).into();
        out.pit_service_refuel_selected = flag(menu[3]).into();
        out.pit_service_refuel_active = action.map(|bits| bits & 8 != 0).into();
        out.pit_service_front_tires_selected = flag(menu[4]).into();
        out.pit_service_front_tires_active = action.map(|bits| bits & 16 != 0).into();
        out.pit_service_rear_tires_selected = flag(menu[5]).into();
        out.pit_service_rear_tires_active = action.map(|bits| bits & 32 != 0).into();
        out.pit_service_body_repair_selected = flag(menu[6]).into();
        out.pit_service_body_repair_active = action.map(|bits| bits & 64 != 0).into();
        out.pit_service_front_wing_selected = flag(menu[7]).into();
        out.pit_service_front_wing_active = action.map(|bits| bits & 128 != 0).into();
        out.pit_service_rear_wing_selected = flag(menu[8]).into();
        out.pit_service_rear_wing_active = action.map(|bits| bits & 256 != 0).into();
        out.pit_service_suspension_selected = flag(menu[9]).into();
        out.pit_service_suspension_active = action.map(|bits| bits & 512 != 0).into();
        out.pit_total_duration = non_negative(shared.pit_total_duration).into();
        out.pit_elapsed_time = non_negative(shared.pit_elapsed_time).into();
        out.pit_min_duration_total = non_negative(shared.pit_min_duration_total).into();
        out.pit_min_duration_left = non_negative(shared.pit_min_duration_left).into();
        out.control = match shared.control_type {
            R3E_CONTROL_PLAYER => Some("player"),
            R3E_CONTROL_AI => Some("ai"),
            R3E_CONTROL_REMOTE => Some("remote"),
            R3E_CONTROL_REPLAY => Some("replay"),
            _ => None,
        }
        .map(str::to_owned)
        .into();

        let flags = shared.flags;
        out.flag = if flags.checkered == 1 {
            Some("checkered")
        } else if flags.black == 1 {
            Some("black")
        } else if flags.blue == 1 {
            Some("blue")
        } else if flags.white == 1 {
            Some("white")
        } else if flags.green == 1 {
            Some("green")
        } else {
            None
        }
        .map(str::to_owned)
        .into();
        out.under_yellow = flag(flags.yellow).into();
        out.in_garage = flag(shared.game_player_in_garage).into();
    }
    out.simulator_metadata = Some(serde_json::json!({ "raceroom": source }).to_string()).into();
    out
}

/// Reasons are scoped to the reported type, not to all outstanding penalties.
fn penalty_type(kind: i32) -> Option<&'static str> {
    [
        "drive_through",
        "stop_and_go",
        "pit_stop",
        "time_deduction",
        "slow_down",
        "disqualified",
    ]
    .get(usize::try_from(kind).ok()?)
    .copied()
}

fn penalty_reason(kind: i32, reason: i32) -> Option<&'static str> {
    let reasons: &[&str] = match kind {
        0 => &[
            "invalid",
            "cut_track",
            "pit_speeding",
            "false_start",
            "ignored_blue",
            "driving_too_slow",
            "passed_before_green",
            "passed_before_finish",
            "passed_before_pit_entrance",
            "ignored_slow_down",
        ],
        1 => &[
            "invalid",
            "cut_track_first",
            "cut_track_multiple",
            "yellow_flag_overtake",
            "virtual_energy",
        ],
        2 => &["invalid", "ignored_pit_window"],
        3 => &[
            "invalid",
            "mandatory_pit_stop_late",
            "ignored_minimum_pit_duration",
        ],
        4 => &["invalid", "cut_track_first", "cut_track_multiple"],
        5 => &[
            "false_start",
            "pit_speeding",
            "wrong_way",
            "entering_pits_under_red",
            "exiting_pits_under_red",
            "failed_driver_change",
            "three_drive_throughs_in_lap",
            "lapped_field_multiple_times",
            "ignored_drive_through",
            "ignored_stop_and_go",
            "ignored_pit_stop",
            "ignored_time_penalty",
            "excessive_cutting",
            "ignored_blue",
        ],
        _ => return None,
    };
    reasons
        .get(usize::try_from(reason).ok()?)
        .copied()
        .filter(|reason| *reason != "invalid")
}

/// The heading of a car moving by (dx, dz): 0 along -z, as LMU's plugin has it.
fn heading(dx: f64, dz: f64) -> f64 {
    (-dx).atan2(-dz)
}

/// A value the game gives -1 (or any negative) for when it doesn't know.
fn non_negative(value: f32) -> Option<f64> {
    (value.is_finite() && value >= 0.0).then_some(value as f64)
}

/// A time in seconds, -1 when not known.
fn seconds(value: f32) -> Option<f64> {
    (value > 0.0).then_some(value as f64)
}

/// An integer setting, -1 when not known.
fn setting(value: i32) -> Option<i64> {
    (value >= 0).then_some(value as i64)
}

/// -1 not known, 0 no, anything else yes.
fn flag(value: i32) -> Option<bool> {
    (value >= 0).then_some(value != 0)
}

/// `part` of `whole`, both known.
fn fraction(part: f32, whole: f32) -> Option<f64> {
    non_negative(part)
        .zip(non_negative(whole).filter(|whole| *whole > 0.0))
        .map(|(part, whole)| part / whole)
}

/// The game's sectors are 1 to 3; the recording's from 0.
fn sector(value: i32) -> Option<i64> {
    (1..=3).contains(&value).then_some(value as i64 - 1)
}

/// The surface under a tire, as LMU's plugin numbers them.
fn surface(material: i32) -> Option<i64> {
    match material {
        R3E_MTRL_TYPE_TARMAC | R3E_MTRL_TYPE_CONCRETE => Some(0),
        R3E_MTRL_TYPE_GRASS => Some(2),
        R3E_MTRL_TYPE_DIRT => Some(3),
        R3E_MTRL_TYPE_GRAVEL => Some(4),
        R3E_MTRL_TYPE_RUMBLE => Some(5),
        _ => None,
    }
}

/// A NUL-terminated UTF-8 string of the game's.
fn text(bytes: &[u8]) -> String {
    let end = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());
    String::from_utf8_lossy(&bytes[..end]).trim().to_owned()
}

mod sys {
    #![allow(non_upper_case_globals)]
    #![allow(non_camel_case_types)]
    #![allow(non_snake_case)]
    #![allow(dead_code)]

    include!(concat!(env!("OUT_DIR"), "/rrre.rs"));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn removes_previous_recording_before_starting_reader() {
        let dir = tempfile::tempdir().unwrap();
        for suffix in ["", "-wal", "-shm"] {
            std::fs::write(dir.path().join(format!("{DATABASE}{suffix}")), "old run").unwrap();
        }
        let mut sim = RaceRoom::new();
        sim.dir = dir.path().to_path_buf();

        assert!(sim.database(true).is_none());
        assert!(sim.reader.is_some());
        assert!(sim.live.is_none());
        for suffix in ["", "-wal", "-shm"] {
            assert!(!dir.path().join(format!("{DATABASE}{suffix}")).exists());
        }
    }

    #[test]
    fn does_not_start_reader_when_previous_recording_cannot_be_removed() {
        let dir = tempfile::tempdir().unwrap();
        // A directory at the database path makes removal fail without relying on file locks.
        std::fs::create_dir(dir.path().join(DATABASE)).unwrap();
        let mut sim = RaceRoom::new();
        sim.dir = dir.path().to_path_buf();

        assert!(sim.database(true).is_none());
        assert!(sim.reader.is_none());
        assert!(sim.live.is_none());
    }

    fn name(s: &str) -> [u8; 64] {
        let mut out = [0u8; 64];
        out[..s.len()].copy_from_slice(s.as_bytes());
        out
    }

    fn driver(slot: i32, driver: &str, place: i32, z: f32) -> r3e_driver_data {
        let mut data = unsafe { std::mem::zeroed::<r3e_driver_data>() };
        let mut info = data.driver_info;
        info.name = name(driver);
        info.slot_id = slot;
        info.class_id = 1703;
        data.driver_info = info;
        data.place = place;
        data.completed_laps = 3;
        data.track_sector = 2;
        data.current_lap_valid = 1;
        data.lap_distance = 1200.0;
        data.lap_time_current_self = 40.0;
        data.sector_time_previous_self = [30.0, 65.0, 100.0];
        data.sector_time_best_self = [29.0, 64.0, 98.5];
        data.sector_time_current_self = [31.0, -1.0, -1.0];
        data.time_delta_front = 1.5;
        data.time_delta_behind = -1.0;
        data.car_speed = 50.0;
        data.position = r3e_vec3_f32 { x: 0.0, y: 0.0, z };
        data.penalties = r3e_cut_track_penalties {
            drive_through: 0.0,
            stop_and_go: -1.0,
            pit_stop: -1.0,
            time_deduction: -1.0,
            slow_down: -1.0,
        };
        data.pitstop_status = R3E_PITSTOP_STATUS_UNAVAILABLE;
        data.finish_status = R3E_FINISH_STATUS_NONE;
        data
    }

    /// The player (slot 7, second) and a car ahead (slot 3), at Spa, 500 s into the session.
    fn shared(time: f64, z: f32) -> Box<r3e_shared> {
        let mut shared = unsafe { Box::<r3e_shared>::new_zeroed().assume_init() };
        shared.version_major = R3E_VERSION_MAJOR;
        shared.track_name = name("Spa-Francorchamps");
        shared.layout_name = name("Grand Prix");
        shared.track_id = 1;
        shared.layout_id = 2;
        shared.layout_length = 7004.0;
        shared.session_type = R3E_SESSION_RACE;
        shared.session_iteration = 1;
        shared.session_phase = R3E_SESSION_PHASE_GREEN;
        shared.session_length_format = R3E_SESSION_LENGTH_TIME_BASED;
        shared.session_time_duration = 3600.0;
        shared.session_time_remaining = 3100.0;
        shared.player_name = name("Ana Lopes");
        shared.control_type = R3E_CONTROL_PLAYER;
        shared.game_mode = R3E_GAMEMODE_SINGLERACE;

        let mut player = shared.player;
        player.game_simulation_time = time;
        player.position = r3e_vec3_f64 {
            x: 0.0,
            y: 1.0,
            z: z as f64,
        };
        // Along -z: heading 0
        player.velocity = r3e_vec3_f64 {
            x: 0.0,
            y: 0.0,
            z: -50.0,
        };
        // Sliding a little to its left (+x), and turning right (pulled to the right, -x)
        player.local_velocity = r3e_vec3_f64 {
            x: 2.0,
            y: 0.0,
            z: -50.0,
        };
        player.local_acceleration = r3e_vec3_f64 {
            x: -9.0,
            y: 0.0,
            z: 0.0,
        };
        shared.player = player;

        let mut info = shared.vehicle_info;
        info.slot_id = 7;
        shared.vehicle_info = info;
        shared.car_speed = 50.0;
        shared.gear = 4;
        shared.engine_rps = 600.0;
        shared.throttle_raw = 1.0;
        shared.brake_raw = 0.0;
        shared.fuel_left = 42.0;
        shared.fuel_capacity = 100.0;
        shared.virtual_energy_left = 30.0;
        shared.virtual_energy_capacity = 120.0;
        shared.brake_bias = 0.42;
        shared.pit_limiter = 0;
        shared.car_damage = r3e_car_damage {
            engine: 1.0,
            transmission: 0.75,
            aerodynamics: -1.0,
            suspension: 1.0,
            unused1: 0.0,
            unused2: 0.0,
        };
        shared.tire_on_mtrl = [R3E_MTRL_TYPE_TARMAC, 1, R3E_MTRL_TYPE_GRASS, 1];
        shared.tire_pressure = [180.0; 4];
        let mut temp = shared.tire_temp[0];
        temp.current_temp = [80.0, 85.0, 90.0];
        shared.tire_temp = [temp; 4];
        shared.flags = r3e_flags {
            yellow: 0,
            yellowCausedIt: -1,
            yellowOvertake: -1,
            yellowPositionsGained: -1,
            sector_yellow: [0, 1, 0],
            closest_yellow_distance_into_track: -1.0,
            blue: 1,
            black: 0,
            green: 0,
            checkered: 0,
            white: 0,
            black_and_white: 0,
        };

        shared.num_cars = 2;
        shared.all_drivers_data_1[0] = driver(3, "Lena Fischer", 1, z - 20.0);
        shared.all_drivers_data_1[1] = driver(7, "Ana Lopes", 2, z);
        shared
    }

    #[test]
    fn detailed_penalties_preserve_values_and_type_scoped_reasons() {
        let shared = shared(500.0, 0.0);
        let mut driver = driver(7, "Ana", 2, 0.0);
        driver.penalties = r3e_cut_track_penalties {
            drive_through: 0.0,
            stop_and_go: 10.0,
            pit_stop: -1.0,
            time_deduction: 5.0,
            slow_down: 2.5,
        };
        driver.penaltyType = 0;
        driver.penaltyReason = 2;
        let out = standing(&shared, &driver, 500.0, true);
        assert_eq!(out.penalties.get(), Some(4));
        assert_eq!(out.penalty_drive_through_active.get(), Some(true));
        assert_eq!(out.penalty_stop_and_go_seconds.get(), Some(10.0));
        assert_eq!(out.penalty_pit_stop_active.get(), None);
        assert_eq!(out.penalty_time_seconds.get(), Some(5.0));
        assert_eq!(out.penalty_slow_down_seconds.get(), Some(2.5));
        assert_eq!(out.penalty_reason.as_deref(), Some("pit_speeding"));
        for (kind, reason) in [
            (1, "cut_track_multiple"),
            (2, "ignored_pit_window"),
            (3, "ignored_minimum_pit_duration"),
            (4, "cut_track_multiple"),
            (5, "wrong_way"),
        ] {
            assert_eq!(
                penalty_reason(kind, if kind == 2 { 1 } else { 2 }),
                Some(reason)
            );
        }
        driver.penaltyType = 99;
        driver.penaltyReason = 100;
        let out = standing(&shared, &driver, 500.0, false);
        assert_eq!(out.penalty_type.as_deref(), None);
        assert_eq!(out.penalty_reason.as_deref(), None);
        assert!(
            out.simulator_metadata
                .as_deref()
                .unwrap()
                .contains("\"penalty_reason\":100")
        );
        driver.penaltyType = -1;
        driver.penaltyReason = -1;
        driver.penalties = r3e_cut_track_penalties {
            drive_through: -1.0,
            stop_and_go: -1.0,
            pit_stop: -1.0,
            time_deduction: -1.0,
            slow_down: -1.0,
        };
        let out = standing(&shared, &driver, 500.0, false);
        assert_eq!(out.penalties.get(), Some(0));
        assert!(
            out.simulator_metadata
                .as_deref()
                .unwrap()
                .contains("\"penalty_reason\":null")
        );
        assert_eq!(out.penalty_slow_down_seconds.get(), None);
        assert_eq!(non_negative(f32::NAN), None);
        assert_eq!(non_negative(f32::INFINITY), None);
        assert_eq!(penalty_reason(0, 0), None);
        assert_eq!(penalty_reason(5, 0), Some("false_start"));
    }

    #[test]
    fn records_pit_transitions_menu_intent_and_serving_without_count_changes() {
        use bono_sim::sql::{ScoringWriter, create_tables};
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        create_tables(&conn).unwrap();
        let mut writer = ScoringWriter::new(&conn).unwrap();
        let mut shared = shared(500.0, 0.0);
        let driver = driver(7, "Ana", 2, 0.0);
        shared.pit_menu_state = [-1; 12];
        shared.pit_total_duration = -1.0;
        shared.pit_elapsed_time = -1.0;
        shared.pit_min_duration_total = -1.0;
        shared.pit_min_duration_left = -1.0;
        shared.pit_action = -1;
        for (step, state) in ["none", "requested", "entering", "stopped", "exiting"]
            .into_iter()
            .enumerate()
        {
            shared.pit_state = step as i32;
            let out = standing(&shared, &driver, 500.0 + step as f64, true);
            assert_eq!(out.pit_state.as_deref(), Some(state));
            assert_eq!(out.pit_elapsed_time.get(), None);
            assert_eq!(out.pit_service_front_tires_selected.get(), None);
            assert_eq!(out.pit_service_penalty_active.get(), None);
            writer
                .write(bono_session_info::default(), vec![out])
                .unwrap();
        }
        shared.pit_state = 3;
        shared.pit_action = 2 | 8 | 16;
        shared.pit_menu_state[4] = 1;
        shared.pit_elapsed_time = 3.0;
        shared.pit_total_duration = 10.0;
        shared.pit_min_duration_total = 20.0;
        shared.pit_min_duration_left = 0.0;
        let out = standing(&shared, &driver, 506.0, true);
        assert_eq!(out.pit_service_penalty_active.get(), Some(true));
        assert_eq!(out.pit_min_duration_left.get(), Some(0.0));
        assert_eq!(out.pit_service_front_tires_selected.get(), Some(true));
        assert_eq!(out.pit_service_refuel_active.get(), Some(true));
        assert_eq!(out.pit_service_front_tires_active.get(), Some(true));
        assert_eq!(out.pit_service_rear_tires_active.get(), Some(false));
        writer
            .write(bono_session_info::default(), vec![out])
            .unwrap();
        // Menu-only change, then progress-only change, both persist with count still one.
        shared.pit_menu_state[4] = 0;
        writer
            .write(
                bono_session_info::default(),
                vec![standing(&shared, &driver, 507.0, true)],
            )
            .unwrap();
        shared.pit_elapsed_time = 4.0;
        writer
            .write(
                bono_session_info::default(),
                vec![standing(&shared, &driver, 508.0, true)],
            )
            .unwrap();
        shared.pit_action = 0;
        writer
            .write(
                bono_session_info::default(),
                vec![standing(&shared, &driver, 509.0, true)],
            )
            .unwrap();
        let (rows, count, elapsed, action, left): (i64, i64, f64, i64, f64) = conn.query_row(
            "SELECT (SELECT COUNT(*) FROM standings), penalties, pit_elapsed_time, pit_service_penalty_active, pit_min_duration_left FROM standings ORDER BY id DESC LIMIT 1", [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))).unwrap();
        assert_eq!((rows, count, elapsed, action, left), (9, 1, 4.0, 0, 0.0));
        let opponent = standing(&shared, &driver, 509.0, false);
        assert_eq!(opponent.pit_state.as_deref(), None);
        assert_eq!(opponent.pit_service_penalty_active.get(), None);
        assert_eq!(opponent.pit_service_front_tires_selected.get(), None);
        shared.pit_state = -1;
        assert_eq!(
            standing(&shared, &driver, 510.0, true).pit_state.as_deref(),
            None
        );
    }

    #[test]
    fn records_what_the_game_shares() {
        let mut converter = Converter::default();
        let start = Instant::now();
        let update = converter.convert(&shared(500.0, 0.0), start).unwrap();
        assert!(!update.new_event);

        let (session, standings) = update.scoring.expect("the first update scores");
        assert_eq!(
            session.track.as_deref(),
            Some("Spa-Francorchamps - Grand Prix")
        );
        assert_eq!(session.track_length.get(), Some(7004.0));
        assert_eq!(session.session_type.as_deref(), Some("race"));
        assert_eq!(session.phase.as_deref(), Some("green"));
        assert_eq!(session.time_remaining.get(), Some(3100.0));
        assert_eq!(session.session_end_time.get(), Some(3600.0));
        assert_eq!(session.local_yellow_2.get(), Some(true));
        assert_eq!(session.local_yellow_1.get(), Some(false));

        let me = standings
            .iter()
            .find(|s| s.is_player.get() == Some(true))
            .unwrap();
        assert_eq!(me.vehicle_id.get(), Some(7));
        assert_eq!(me.driver.as_deref(), Some("Ana Lopes"));
        assert_eq!(me.position.get(), Some(2));
        assert_eq!(me.sector.get(), Some(1));
        assert_eq!(me.flag.as_deref(), Some("blue"));
        assert_eq!(me.control.as_deref(), Some("player"));
        assert_eq!(me.lap_start_time.get(), Some(460.0));
        // The game's sectors add up: these don't
        assert_eq!(me.last_lap_time.get(), Some(100.0));
        assert_eq!(me.last_sector_1.get(), Some(30.0));
        assert_eq!(me.last_sector_2.get(), Some(35.0));
        assert_eq!(me.last_sector_3.get(), Some(35.0));
        assert_eq!(me.best_lap_time.get(), Some(98.5));
        assert_eq!(me.current_sector_1.get(), Some(31.0));
        assert_eq!(me.current_sector_2.get(), None);
        assert_eq!(me.penalties.get(), Some(1));
        let other = standings
            .iter()
            .find(|s| s.vehicle_id.get() == Some(3))
            .unwrap();
        assert_eq!(other.is_player.get(), Some(false));
        assert_eq!(other.vehicle_class.as_deref(), Some("class 1703"));
        assert_eq!(other.flag.as_deref(), None);

        let mine = update
            .telemetry
            .iter()
            .find(|t| t.vehicle_id.get() == Some(7))
            .unwrap();
        assert_eq!(mine.session_time.get(), Some(500.0));
        assert_eq!(mine.lap.get(), Some(3));
        assert_eq!(mine.sector.get(), Some(1));
        assert_eq!(mine.lap_invalidated.get(), Some(false));
        assert_eq!(mine.heading.get(), Some(0.0));
        assert_eq!(mine.speed.get(), Some(50.0));
        // Lateral is + right, as LMU's plugin has it
        assert_eq!(mine.velocity_longitudinal.get(), Some(50.0));
        assert_eq!(mine.velocity_lateral.get(), Some(-2.0));
        assert_eq!(mine.acceleration_lateral.get(), Some(9.0));
        assert_eq!(mine.gear.get(), Some(4));
        assert!((mine.engine_rpm.get().unwrap() - 5729.58).abs() < 0.01);
        assert_eq!(mine.fuel.get(), Some(42.0));
        assert_eq!(mine.virtual_energy.get(), Some(0.25));
        assert_eq!(mine.state.rear_brake_bias.get(), Some(0.42f32 as f64));
        assert_eq!(mine.state.engine_damage.get(), Some(0.0));
        assert_eq!(mine.state.transmission_damage.get(), Some(0.25));
        assert_eq!(mine.state.aerodynamics_damage.get(), None);
        assert_eq!(mine.wheels[0].surface_type.get(), Some(0));
        assert_eq!(mine.wheels[2].surface_type.get(), Some(2));
        assert_eq!(
            mine.wheels[3].tire_surface_temperature_right.get(),
            Some(90.0)
        );
        assert_eq!(mine.wheels[1].tire_pressure.get(), Some(180.0));

        // The other car: no heading until it's moved
        let theirs = update
            .telemetry
            .iter()
            .find(|t| t.vehicle_id.get() == Some(3))
            .unwrap();
        assert_eq!(theirs.heading.get(), None);
        assert_eq!(theirs.position_z.get(), Some(-20.0));
        assert_eq!(theirs.fuel.get(), None);
    }

    /// Recorded like LMU's plugin records: the session, and the player's car flagged as such.
    #[test]
    fn records_into_the_database() {
        let dir = tempfile::tempdir().unwrap();
        let recorder = Recorder::new(dir.path(), None).unwrap();
        let mut converter = Converter::default();
        let start = Instant::now();
        for step in 0..10 {
            let time = 500.0 + step as f64 * 0.02;
            let update = converter
                .convert(
                    &shared(time, -(step as f32)),
                    start + Duration::from_millis(20 * step),
                )
                .unwrap();
            record(&recorder, update);
        }
        drop(recorder);

        let conn = rusqlite::Connection::open(dir.path().join(DATABASE)).unwrap();
        let count = |sql: &str| conn.query_row(sql, [], |row| row.get::<_, i64>(0)).unwrap();
        assert_eq!(
            conn.query_row("SELECT track FROM session LIMIT 1", [], |row| row
                .get::<_, String>(0))
                .unwrap(),
            "Spa-Francorchamps - Grand Prix"
        );
        assert_eq!(
            count("SELECT COUNT(*) FROM telemetry WHERE is_player = 1"),
            10
        );
        assert_eq!(
            count("SELECT COUNT(DISTINCT vehicle_id) FROM telemetry WHERE is_player = 1"),
            1
        );
        // The other car, at every 5th update
        assert_eq!(
            count("SELECT COUNT(*) FROM telemetry WHERE vehicle_id = 3"),
            2
        );
        assert_eq!(
            count("SELECT COUNT(*) FROM standings WHERE driver = 'Lena Fischer'"),
            1
        );
    }

    #[test]
    fn scoring_deduplicates_float_derived_timestamps() {
        use bono_sim::sql::{ScoringWriter, create_tables};

        let conn = rusqlite::Connection::open_in_memory().unwrap();
        create_tables(&conn).unwrap();
        let mut writer = ScoringWriter::new(&conn).unwrap();
        let mut shared = shared(500.0, 0.0);
        shared.pit_window_start = 10;
        shared.pit_window_end = 20;

        // Five scoring updates per second, with durations rounded to the game's
        // f32 precision. The session deadline, pit window and lap starts stay fixed.
        for step in 0..300 {
            let time = 500.0 + step as f64 * 0.2;
            shared.session_time_remaining = (3600.0 - time) as f32;
            let mut drivers = shared.all_drivers_data_1;
            for driver in &mut drivers[..2] {
                driver.lap_time_current_self = (time - 460.0) as f32;
            }
            let (session, standings) = scoring(&shared, &drivers[..2], time);
            assert_eq!(session.session_end_time.get(), Some(3600.0));
            assert_eq!(session.pit_window_start_time.get(), Some(600.0));
            assert_eq!(session.pit_window_end_time.get(), Some(1200.0));
            assert_eq!(standings[0].lap_start_time.get(), Some(460.0));
            writer.write(session, standings).unwrap();
        }
        let count = |table: &str| {
            conn.query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
                row.get::<_, i64>(0)
            })
            .unwrap()
        };
        assert_eq!(count("session"), 1);
        assert_eq!(count("standings"), 2);

        // A changed deadline/window and a new lap must still be recorded.
        shared.session_time_remaining = 3100.0;
        shared.pit_window_start = 11;
        shared.pit_window_end = 21;
        let mut drivers = shared.all_drivers_data_1;
        drivers[0].completed_laps += 1;
        drivers[0].lap_time_current_self = 0.2;
        drivers[1].lap_time_current_self = 100.0;
        let (session, standings) = scoring(&shared, &drivers[..2], 560.0);
        assert_eq!(session.session_end_time.get(), Some(3660.0));
        assert_eq!(session.pit_window_start_time.get(), Some(720.0));
        assert_eq!(session.pit_window_end_time.get(), Some(1320.0));
        assert_eq!(standings[0].lap_start_time.get(), Some(559.8));
        writer.write(session, standings).unwrap();
        assert_eq!(count("session"), 2);
        assert_eq!(count("standings"), 3);
    }

    #[test]
    fn follows_the_game_as_it_moves() {
        let mut converter = Converter::default();
        let start = Instant::now();
        converter.convert(&shared(500.0, 0.0), start).unwrap();

        // The same physics frame (paused): nothing new
        assert!(converter.convert(&shared(500.0, 0.0), start).is_none());

        // Moved 1 m along -z, in 20 ms: the other car heads that way too, and scoring waits
        let update = converter
            .convert(&shared(500.02, -1.0), start + Duration::from_millis(20))
            .unwrap();
        assert!(update.scoring.is_none());
        let theirs = update
            .telemetry
            .iter()
            .find(|t| t.vehicle_id.get() == Some(3))
            .unwrap();
        assert_eq!(theirs.heading.get(), Some(0.0));
        assert!((theirs.time_since_last_update.get().unwrap() - 0.02).abs() < 1e-9);

        // Heading right, -x: a quarter turn
        assert!((heading(-1.0, 0.0) - std::f64::consts::FRAC_PI_2).abs() < 1e-12);

        // Scoring again after a while; another layout is another event
        let mut other = shared(501.0, -2.0);
        other.layout_id = 3;
        let update = converter
            .convert(&other, start + Duration::from_secs(1))
            .unwrap();
        assert!(update.scoring.is_some());
        assert!(update.new_event);

        // Menus: nothing
        let mut menus = shared(502.0, -3.0);
        menus.game_in_menus = 1;
        assert!(
            converter
                .convert(&menus, start + Duration::from_secs(2))
                .is_none()
        );
    }
}
