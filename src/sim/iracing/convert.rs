//! Turns iRacing's updates (see `sdk`) and session info (see `session`) into the recording's rows,
//! in its units and conventions (see `include/bono`).
//!
//! iRacing shares the player's car in detail, but every other car only as arrays indexed by car:
//! where it is around the lap, its laps and lap times, its place, gear and revs. What it doesn't
//! share is worked out here from those, update by update: when each lap started, sector times,
//! speed, pit stops, and the gaps between the cars. It gives no positions in the world, only
//! distances around the lap.
//!
//! The enum values and bits are `irsdk_defines.h`'s. The SDK doesn't document which way its car
//! axes and rotations point (forward, left or right, up or down), so the values whose meaning
//! depends on that (orientation, rates, accelerations, the steering angle and torque) aren't
//! recorded.

use super::{
    sdk::{Frame, UNLIMITED_LAPS, UNLIMITED_TIME},
    session::SessionInfo,
};
use crate::sim::memory::{SCORING_EVERY, Update};
use bono_sim::sys::{bono_session_info, bono_standing, bono_telemetry};
use serde_json::{Map, Value, json};
use std::{collections::HashMap, time::Instant};

/// `irsdk_TrkLoc`
const NOT_IN_WORLD: i32 = -1;
const IN_PIT_STALL: i32 = 1;

/// `irsdk_SessionState`
const STATE_GET_IN_CAR: i32 = 1;
const STATE_WARMUP: i32 = 2;
const STATE_PARADE_LAPS: i32 = 3;
const STATE_RACING: i32 = 4;
const STATE_CHECKERED: i32 = 5;
const STATE_COOL_DOWN: i32 = 6;

/// `irsdk_Flags`
const FLAG_CHECKERED: u32 = 0x0001;
const FLAG_WHITE: u32 = 0x0002;
const FLAG_GREEN: u32 = 0x0004;
const FLAG_YELLOW: u32 = 0x0008;
const FLAG_RED: u32 = 0x0010;
const FLAG_BLUE: u32 = 0x0020;
const FLAG_YELLOW_WAVING: u32 = 0x0100;
const FLAG_ONE_LAP_TO_GREEN: u32 = 0x0200;
const FLAG_GREEN_HELD: u32 = 0x0400;
const FLAG_CAUTION: u32 = 0x4000;
const FLAG_CAUTION_WAVING: u32 = 0x8000;
const FLAG_BLACK: u32 = 0x010000;
const FLAG_DISQUALIFY: u32 = 0x020000;

/// `irsdk_EngineWarnings`
const WARNING_WATER_TEMP: u32 = 0x01;
const WARNING_PIT_SPEED_LIMITER: u32 = 0x10;
const WARNING_OIL_TEMP: u32 = 0x40;

/// `irsdk_PitSvFlags`
const SERVICE_LF_TIRE: u32 = 0x01;
const SERVICE_RF_TIRE: u32 = 0x02;
const SERVICE_LR_TIRE: u32 = 0x04;
const SERVICE_RR_TIRE: u32 = 0x08;
const SERVICE_FUEL: u32 = 0x10;

/// The recording's wheels (front left, front right, rear left, rear right) as iRacing names them.
const WHEELS: [&str; 4] = ["LF", "RF", "LR", "RR"];

/// Faster than this (m/s) between two updates, a car was moved (towed, reset), not driven.
const MAX_SPEED: f64 = 150.0;
/// Room for the lap distance's noise, metres.
const MAX_JUMP: f64 = 20.0;
/// How far back a car may go before it counts as moved (metres): standing still isn't exact.
const MAX_BACK: f64 = 2.0;
/// Speed is how far a car went over at least this long (s), to even out the lap distance's steps.
const SPEED_WINDOW: f64 = 0.25;
/// How close (s) a lap timed here must be to the game's best lap to be taken for it: the line
/// crossing is worked out between updates, from the lap distance, which the game's timing isn't.
const BEST_LAP_MATCH: f64 = 0.1;
/// Slower than this (m/s), a car in its pit box has stopped there.
const STOPPED: f64 = 0.5;
/// How far apart (s) the lap count going up and the car crossing the line may be for one lap.
const LINE_MATCH: f64 = 1.0;
/// How many of each car's last whole laps are kept, to find its best among.
const RECENT_LAPS: usize = 4;

/// What tells events apart: the session info's track, session and sub-session ids, and the track.
type Event = ((Option<i64>, Option<i64>, Option<i64>), Option<String>);

/// Turns updates into the recorder's.
#[derive(Default)]
pub struct Converter {
    last_time: Option<f64>,
    last_scoring: Option<Instant>,
    /// The event being recorded: its ids and track
    event: Option<Event>,
    /// The session being recorded: its number and id
    session: Option<(Option<i32>, Option<i32>)>,
    /// What's been worked out about each car, by car index
    cars: HashMap<i64, CarTrack>,
    /// When the session's clock runs out, as last recorded
    session_end: Option<f64>,
}

/// One car in one update, as read and as worked out.
struct Car<'a> {
    idx: i64,
    driver: &'a super::session::Driver,
    /// Around the lap, 0-1; `None` while it isn't in the world
    pct: Option<f64>,
    on_pit_road: Option<bool>,
    laps_completed: Option<i64>,
    /// What to add to `laps_completed` for the lap it's on by its distance, while the lap count
    /// and the distance go round the line an update or so apart
    lap_offset: i64,
    position: Option<i64>,
    /// The game's estimate of how long into the lap it is where it is
    est: Option<f64>,
    /// Race time behind the leader
    f2: Option<f64>,
}

impl Car<'_> {
    /// How far it's gone in laps, for laps behind.
    fn progress(&self) -> Option<f64> {
        Some((self.laps_completed? + self.lap_offset) as f64 + self.pct.unwrap_or(0.0))
    }
}

impl Converter {
    /// The update for `frame`, read at `now` with the session info it goes with: `None` when
    /// there's nothing new to record (the same update, a pause, no track yet).
    pub fn convert(
        &mut self,
        frame: &Frame<'_>,
        info: &SessionInfo,
        now: Instant,
    ) -> Option<Update> {
        let time = frame.f64("SessionTime")?;
        let length = info.track_length?;
        if self.last_time == Some(time) {
            return None;
        }
        let dt = self
            .last_time
            .map(|last| time - last)
            .filter(|dt| *dt > 0.0);
        self.last_time = Some(time);

        let event = (info.event, info.track.clone());
        let new_event = self.event.as_ref().is_some_and(|last| *last != event);
        self.event = Some(event);
        // Another session, or its clock went back (a restart, a replay rewound)
        let session = (frame.i32("SessionNum"), frame.i32("SessionUniqueID"));
        if new_event || self.session != Some(session) || dt.is_none() {
            self.cars.clear();
            self.session_end = None;
        }
        self.session = Some(session);

        let player = frame.i32("PlayerCarIdx").map(i64::from).or(info.player);
        let mut cars: Vec<_> = info.cars().collect();
        cars.sort_by_key(|(idx, _)| *idx);
        let cars: Vec<Car<'_>> = cars
            .into_iter()
            .map(|(idx, driver)| {
                let car = PerCar::new(frame, idx, player);
                let surface = car.i32("CarIdxTrackSurface", "PlayerTrackSurface");
                let pct = car
                    .f64("CarIdxLapDistPct", "LapDistPct")
                    .filter(|pct| (0.0..=1.0).contains(pct))
                    .filter(|_| surface != Some(NOT_IN_WORLD));
                let on_pit_road = car.bool("CarIdxOnPitRoad", "OnPitRoad");
                let official_best = car
                    .f64("CarIdxBestLapTime", "LapBestLapTime")
                    .filter(|t| *t > 0.0);
                // The laps the recording counts by, which its lap starts go with too
                let completed = car
                    .i32("CarIdxLapCompleted", "LapCompleted")
                    .filter(|laps| *laps >= -1);
                let track = self.cars.entry(idx).or_default();
                track.update(
                    time,
                    pct,
                    completed,
                    on_pit_road,
                    surface,
                    official_best,
                    length,
                    &info.sectors,
                );
                let lap_offset = track.lap_offset(time);
                Car {
                    idx,
                    driver,
                    pct,
                    on_pit_road,
                    // -1 before the first lap starts (out of the pits), and out of the world
                    // (disconnected), where it's no count at all
                    laps_completed: completed
                        .filter(|laps| *laps >= 0 || pct.is_some())
                        .map(|laps| i64::from(laps.max(0))),
                    lap_offset,
                    position: car
                        .i32("CarIdxPosition", "PlayerCarPosition")
                        .filter(|p| *p > 0)
                        .map(i64::from),
                    est: frame
                        .f64_at("CarIdxEstTime", idx as usize)
                        .filter(|t| *t >= 0.0),
                    f2: frame
                        .f64_at("CarIdxF2Time", idx as usize)
                        .filter(|t| *t >= 0.0),
                }
            })
            .collect();

        let current = frame
            .i32("SessionNum")
            .and_then(|num| info.session(num.into()));
        let race = current.and_then(|session| session.kind) == Some("race");
        let gaps = track_gaps(&cars);
        let places = place_gaps(&cars, race);

        let mut telemetry = Vec::with_capacity(cars.len());
        for car in cars.iter().filter(|car| car.pct.is_some()) {
            let track = &self.cars[&car.idx];
            let i = car.idx as usize;
            let mut out = bono_telemetry::default();
            out.vehicle_id = Some(car.idx).into();
            out.session_time = Some(time).into();
            out.time_since_last_update = dt.into();
            out.lap = car.laps_completed.into();
            out.lap_start_time = track.lap_start.into();
            out.lap_distance = car.pct.map(|pct| pct * length).into();
            out.sector = track.sector.map(|s| s as i64).into();
            out.in_pit_lane = car.on_pit_road.into();
            let (ahead, behind) = gaps.get(&car.idx).copied().unwrap_or_default();
            out.gap_to_car_ahead = ahead.into();
            out.gap_to_car_behind = behind.into();
            let (ahead, behind) = places.get(&car.idx).copied().unwrap_or_default();
            out.gap_to_place_ahead = ahead.into();
            out.gap_to_place_behind = behind.into();
            out.speed = track.speed.into();
            out.gear = frame
                .i32_at("CarIdxGear", i)
                .filter(|gear| *gear >= -1)
                .map(i64::from)
                .into();
            out.engine_rpm = frame
                .f64_at("CarIdxRPM", i)
                .filter(|rpm| *rpm >= 0.0)
                .into();
            out.state.incident_points = car.driver.team_incidents.map(|n| n as f64).into();
            if Some(car.idx) == player {
                player_telemetry(frame, info, &mut out);
            }
            telemetry.push(out);
        }

        let scoring = match self.last_scoring {
            Some(last) if now.duration_since(last) < SCORING_EVERY => None,
            _ => {
                self.last_scoring = Some(now);
                let remaining = frame
                    .f64("SessionTimeRemain")
                    .filter(|t| *t >= 0.0 && *t < UNLIMITED_TIME);
                self.session_end = session_end(self.session_end, time, remaining);
                Some(self.scoring(frame, info, &cars, player, race, time, &places))
            }
        };

        Some(Update {
            telemetry,
            scoring,
            new_event,
        })
    }

    #[allow(clippy::too_many_arguments)]
    fn scoring(
        &self,
        frame: &Frame<'_>,
        info: &SessionInfo,
        cars: &[Car<'_>],
        player: Option<i64>,
        race: bool,
        time: f64,
        places: &HashMap<i64, (Option<f64>, Option<f64>)>,
    ) -> (bono_session_info, Vec<bono_standing>) {
        let num = frame.i32("SessionNum");
        let current = num.and_then(|num| info.session(num.into()));
        let flags = frame.bits("SessionFlags");
        let state = frame.i32("SessionState");

        let mut s = bono_session_info::default();
        s.session_time = Some(time).into();
        let remaining = frame
            .f64("SessionTimeRemain")
            .filter(|t| *t >= 0.0 && *t < UNLIMITED_TIME);
        s.time_remaining = remaining.into();
        s.session_end_time = self.session_end.into();
        s.time_of_day = frame.f64("SessionTimeOfDay").into();
        s.track = info.track.clone().into();
        s.track_length = info.track_length.into();
        s.session_type = current.and_then(|c| c.kind).map(str::to_owned).into();
        s.session_number = current
            .filter(|c| c.kind.is_some())
            .map(|c| c.number)
            .into();
        s.phase = phase(state, flags).map(str::to_owned).into();
        s.full_course_yellow = flags
            .map(full_course_yellow(frame))
            .map(str::to_owned)
            .into();
        s.max_laps = frame
            .i32("SessionLapsTotal")
            .filter(|laps| *laps > 0 && *laps < UNLIMITED_LAPS)
            .map(i64::from)
            .into();
        s.event_start_time_of_day = info.start_time_of_day.into();
        s.vehicle_count = Some(cars.len() as i64).into();
        s.max_vehicles = info.starters.into();
        s.sector_count = Some(info.sectors.len() as i64).filter(|n| *n > 0).into();
        s.in_realtime = frame.bool("IsOnTrack").into();
        s.player_name = player
            .and_then(|p| info.drivers.get(&p)?.name.clone())
            .into();
        s.fixed_setup = info.fixed_setup.into();
        // iRacing's "%" values are fractions (`Throttle`: 0 off to 1 full). The weather drifts
        // a little with every update: rounded, so a session row is written when it changes
        s.rain = frame
            .f64("Precipitation")
            .map(|r| (r.clamp(0.0, 1.0) * 1000.0).round() / 1000.0)
            .into();
        let skies = frame.i32("Skies").filter(|s| (0..=3).contains(s));
        s.cloud_coverage = skies.map(i64::from).into();
        s.cloudiness = skies.map(|s| f64::from(s) / 3.0).into();
        s.ambient_temperature = frame.f64("AirTemp").map(round_tenth).into();
        s.track_temperature = frame
            .f64("TrackTempCrew")
            .or_else(|| frame.f64("TrackTemp"))
            .map(round_tenth)
            .into();
        s.track_wetness_average = frame.i32("TrackWetness").and_then(wetness).into();
        s.pit_speed_limit = info.pit_speed_limit.into();
        s.max_incident_points = info.incident_limit.into();

        let phase = s.phase.as_deref().map(str::to_owned);
        let standings = cars
            .iter()
            .map(|car| {
                let is_player = Some(car.idx) == player;
                self.standing(
                    frame,
                    info,
                    car,
                    cars,
                    is_player,
                    race,
                    time,
                    places,
                    &phase,
                    current.and_then(|c| c.results.get(&car.idx)),
                )
            })
            .collect();
        (s, standings)
    }

    #[allow(clippy::too_many_arguments)]
    fn standing(
        &self,
        frame: &Frame<'_>,
        info: &SessionInfo,
        car: &Car<'_>,
        cars: &[Car<'_>],
        is_player: bool,
        race: bool,
        time: f64,
        places: &HashMap<i64, (Option<f64>, Option<f64>)>,
        phase: &Option<String>,
        result: Option<&super::session::Result>,
    ) -> bono_standing {
        let i = car.idx as usize;
        let d = car.driver;
        let track = self.cars.get(&car.idx);
        let mut out = bono_standing::default();
        out.session_time = Some(time).into();
        out.vehicle_id = Some(car.idx).into();
        out.is_player = Some(is_player).into();
        out.driver = d.name.clone().into();
        out.vehicle = d.car.clone().into();
        out.vehicle_class = d
            .class
            .clone()
            .or_else(|| d.class_id.map(|id| format!("class {id}")))
            .into();
        out.team = d.team.clone().into();
        out.car_number = d.car_number.clone().into();
        out.position = car.position.into();
        let per_car = PerCar::new(frame, car.idx, is_player.then_some(car.idx));
        out.class_position = per_car
            .i32("CarIdxClassPosition", "PlayerCarClassPosition")
            .filter(|p| *p > 0)
            .map(i64::from)
            .into();
        out.qualifying_position = info.qualifying.get(&car.idx).copied().into();
        out.laps_completed = car.laps_completed.into();
        out.lap_start_time = track.and_then(|t| t.lap_start).map(round_ms).into();
        out.last_lap_time = per_car
            .f64("CarIdxLastLapTime", "LapLastLapTime")
            .filter(|t| *t > 0.0)
            .into();
        out.best_lap_time = per_car
            .f64("CarIdxBestLapTime", "LapBestLapTime")
            .filter(|t| *t > 0.0)
            .into();
        out.lap_distance = car
            .pct
            .zip(info.track_length)
            .map(|(pct, length)| pct * length)
            .into();
        out.time_into_lap = car.est.into();
        out.estimated_lap_time = d.class_lap_time.into();
        out.in_pits = car.on_pit_road.into();
        let control = match () {
            _ if info.replay || frame.bool("IsReplayPlaying") == Some(true) => "replay",
            _ if is_player => "player",
            _ if d.ai => "ai",
            _ => "remote",
        };
        out.control = Some(control.to_owned()).into();
        out.phase = phase.clone().into();
        out.push_to_pass_active = frame.bool_at("CarIdxP2P_Status", i).into();

        if let Some(track) = track {
            out.sector = track.sector.map(|s| s as i64).into();
            out.pit_stops = Some(track.pit_stops).into();
            out.pit_state = track.pit_state.map(str::to_owned).into();
            for (k, sector) in out.sectors.iter_mut().enumerate() {
                sector.last_lap = track.last_lap.get(k).copied().flatten().into();
                sector.best_lap = track.best_lap.get(k).copied().flatten().into();
                sector.best = track.best.get(k).copied().flatten().into();
                sector.current_lap = track.current.get(k).copied().flatten().into();
            }
        }

        // In races, the gaps in the standings and laps down
        if race {
            out.time_behind_leader = car.f2.into();
            out.time_behind_next = places.get(&car.idx).and_then(|(ahead, _)| *ahead).into();
            let leader = cars.iter().find(|c| c.position == Some(1));
            let next = car
                .position
                .and_then(|p| cars.iter().find(|c| c.position == Some(p - 1)));
            let behind = |other: Option<&Car<'_>>| {
                let laps = other?.progress()? - car.progress()?;
                Some((laps + 1e-9).floor().max(0.0) as i64)
            };
            out.laps_behind_leader = match car.position {
                Some(1) => Some(0),
                _ => behind(leader),
            }
            .into();
            out.laps_behind_next = behind(next).into();
        }

        // The flags shown to the car: the player's own, or each car's where the game has them
        let flags = match is_player {
            true => frame.bits("SessionFlags"),
            false => frame.bits_at("CarIdxSessionFlags", i),
        };
        if let Some(bits) = flags {
            out.flag = car_flag(bits).map(str::to_owned).into();
            out.under_yellow = Some(
                bits & (FLAG_YELLOW | FLAG_YELLOW_WAVING | FLAG_CAUTION | FLAG_CAUTION_WAVING) != 0,
            )
            .into();
            out.penalties = Some(i64::from(bits & FLAG_BLACK != 0)).into();
            if bits & FLAG_DISQUALIFY != 0 {
                out.finish_status = Some("disqualified".to_owned()).into();
            }
        }

        // The compounds, by the player's car's list: only cars of the same model share it
        let compound = |index: Option<i32>| {
            let same_car = is_player || (d.car_id.is_some() && d.car_id == player_car_id(info));
            info.car
                .tires
                .get(&i64::from(index?))
                .filter(|_| same_car)
                .cloned()
        };
        let index = match is_player {
            true => frame
                .i32("PlayerTireCompound")
                .or_else(|| frame.i32_at("CarIdxTireCompound", i)),
            false => frame.i32_at("CarIdxTireCompound", i),
        };
        let name = compound(index);
        out.front_tire_compound_name = name.clone().into();
        out.rear_tire_compound_name = name.into();

        let mut source = Map::new();
        let mut put = |key: &str, value: Value| {
            if !value.is_null() {
                source.insert(key.to_owned(), value);
            }
        };
        put("car_idx", json!(car.idx));
        put("car_id", json!(d.car_id));
        put("car_class_id", json!(d.class_id));
        put(
            "track_surface",
            json!(per_car.i32("CarIdxTrackSurface", "PlayerTrackSurface")),
        );
        put("tire_compound", json!(index));
        put(
            "qualifying_tire_compound",
            json!(
                frame
                    .i32_at("CarIdxQualTireCompound", i)
                    .filter(|c| *c >= 0)
            ),
        );
        put(
            "fast_repairs_used",
            json!(frame.i32_at("CarIdxFastRepairsUsed", i)),
        );
        put(
            "push_to_pass_count",
            json!(frame.i32_at("CarIdxP2P_Count", i)),
        );
        put(
            "pace_line",
            json!(frame.i32_at("CarIdxPaceLine", i).filter(|l| *l >= 0)),
        );
        put(
            "pace_row",
            json!(frame.i32_at("CarIdxPaceRow", i).filter(|r| *r >= 0)),
        );
        put("pace_flags", json!(frame.bits_at("CarIdxPaceFlags", i)));
        put("session_flags", json!(flags));
        put("incidents", json!(result.and_then(|r| r.incidents)));
        put(
            "reason_out",
            json!(result.and_then(|r| r.reason_out.clone())),
        );
        put("team_incidents", json!(d.team_incidents));

        if is_player {
            out.in_garage = frame.bool("IsInGarage").into();
            let services = frame.bits("PitSvFlags");
            let selected = |bits: u32| services.map(|s| s & bits != 0);
            out.pit_service_refuel_selected = selected(SERVICE_FUEL).into();
            out.pit_service_front_tires_selected =
                selected(SERVICE_LF_TIRE | SERVICE_RF_TIRE).into();
            out.pit_service_rear_tires_selected =
                selected(SERVICE_LR_TIRE | SERVICE_RR_TIRE).into();
            out.fuel_fraction = frame.f64("FuelLevelPct").map(|f| f.clamp(0.0, 1.0)).into();

            put("pit_service_flags", json!(services));
            put(
                "pit_service_status",
                json!(frame.i32("PlayerCarPitSvStatus")),
            );
            put("pit_service_active", json!(frame.bool("PitstopActive")));
            put(
                "pit_repair_left_s",
                json!(frame.f64("PitRepairLeft").map(round_tenth)),
            );
            put(
                "pit_optional_repair_left_s",
                json!(frame.f64("PitOptRepairLeft").map(round_tenth)),
            );
            put(
                "pit_service_fuel",
                json!(frame.f64("PitSvFuel").map(round_tenth)),
            );
            put(
                "fast_repairs_available",
                json!(frame.i32("FastRepairAvailable")),
            );
            put("pits_open", json!(frame.bool("PitsOpen")));
            put(
                "weather_declared_wet",
                json!(frame.bool("WeatherDeclaredWet")),
            );
            put("in_pit_stall", json!(frame.bool("PlayerCarInPitStall")));
            put(
                "tow_time_s",
                json!(
                    frame
                        .f64("PlayerCarTowTime")
                        .filter(|t| *t > 0.0)
                        .map(round_tenth)
                ),
            );
        }
        out.simulator_metadata = Some(json!({ "iracing": source }).to_string()).into();
        out
    }
}

/// One car's values in an update: its entries in the game's arrays, or for the player's car the
/// game's own variables for it (saved telemetry has no arrays).
struct PerCar<'a, 'f> {
    frame: &'a Frame<'f>,
    idx: usize,
    is_player: bool,
}

impl<'a, 'f> PerCar<'a, 'f> {
    fn new(frame: &'a Frame<'f>, idx: i64, player: Option<i64>) -> Self {
        Self {
            frame,
            idx: idx as usize,
            is_player: player == Some(idx),
        }
    }

    /// Whether to read the player's own variable: the array isn't there.
    fn own(&self, array: &str) -> bool {
        self.is_player && self.frame.count(array) == 0
    }

    fn f64(&self, array: &str, own: &str) -> Option<f64> {
        match self.own(array) {
            true => self.frame.f64(own),
            false => self.frame.f64_at(array, self.idx),
        }
    }

    fn i32(&self, array: &str, own: &str) -> Option<i32> {
        match self.own(array) {
            true => self.frame.i32(own),
            false => self.frame.i32_at(array, self.idx),
        }
    }

    fn bool(&self, array: &str, own: &str) -> Option<bool> {
        match self.own(array) {
            true => self.frame.bool(own),
            false => self.frame.bool_at(array, self.idx),
        }
    }
}

/// The player's car's model, as the session info has it.
fn player_car_id(info: &SessionInfo) -> Option<i64> {
    info.drivers.get(&info.player?)?.car_id
}

/// What only the player's car has: its inputs, engine, fuel, settings and tires.
fn player_telemetry(frame: &Frame<'_>, info: &SessionInfo, out: &mut bono_telemetry) {
    let f = |name: &str| frame.f64(name);
    let pedal = |name: &str| f(name).map(|v| v.clamp(0.0, 1.0));
    // 0 disengaged (pedal down) to 1 engaged: the recording's is how far the pedal is down
    let clutch = |name: &str| pedal(name).map(|engaged| 1.0 - engaged);

    out.speed = f("Speed").map(f64::abs).into();
    out.delta_best = f("LapDeltaToBestLap")
        .filter(|_| frame.bool("LapDeltaToBestLap_OK") == Some(true))
        .into();
    // The spotter's own count of the cars alongside (`irsdk_CarLeftRight`); Off is not reported
    let (left, right) = match frame.i32("CarLeftRight") {
        Some(1) => (Some(0), Some(0)),
        Some(2) => (Some(1), Some(0)),
        Some(3) => (Some(0), Some(1)),
        Some(4) => (Some(1), Some(1)),
        Some(5) => (Some(2), Some(0)),
        Some(6) => (Some(0), Some(2)),
        _ => (None, None),
    };
    out.cars_left = left.into();
    out.cars_right = right.into();

    out.throttle = pedal("ThrottleRaw").into();
    out.brake = pedal("BrakeRaw").into();
    out.clutch = clutch("ClutchRaw").into();
    out.throttle_filtered = pedal("Throttle").into();
    out.brake_filtered = pedal("Brake").into();
    out.clutch_filtered = clutch("Clutch").into();

    out.gear = frame
        .i32("Gear")
        .filter(|gear| *gear >= -1)
        .map(i64::from)
        .into();
    out.engine_rpm = f("RPM").filter(|rpm| *rpm >= 0.0).into();
    out.engine_water_temperature = f("WaterTemp").into();
    out.engine_oil_temperature = f("OilTemp").into();
    let warnings = frame.bits("EngineWarnings");
    out.engine_overheating = warnings
        .map(|w| w & (WARNING_WATER_TEMP | WARNING_OIL_TEMP) != 0)
        .into();
    out.speed_limiter_active = warnings.map(|w| w & WARNING_PIT_SPEED_LIMITER != 0).into();
    out.abs_active = frame.bool("BrakeABSactive").into();

    out.fuel = f("FuelLevel").filter(|l| *l >= 0.0).into();
    out.battery_charge = f("EnergyERSBatteryPct").map(|c| c.clamp(0.0, 1.0)).into();
    // Mechanical torque and power: what the motor gives the drivetrain, negative while it takes
    let torque = f("TorqueMGU_K");
    out.electric_motor_torque = torque.into();
    out.electric_motor_state = torque
        .map(|t| match t {
            t if t > 0.0 => 2,
            t if t < 0.0 => 3,
            _ => 1,
        })
        .into();
    out.regeneration_power = f("PowerMGU_K").map(|w| (-w).max(0.0) / 1000.0).into();

    out.front_third_spring_deflection = f("HFshockDefl").into();
    out.rear_third_spring_deflection = f("HRshockDefl").into();
    let mean = |a: Option<f64>, b: Option<f64>| a.zip(b).map(|(a, b)| (a + b) / 2.0);
    out.front_ride_height = mean(f("LFrideHeight"), f("RFrideHeight")).into();
    out.rear_ride_height = mean(f("LRrideHeight"), f("RRrideHeight")).into();

    let state = &mut out.state;
    let setting = |name: &str| f(name).map(|v| v.round() as i64);
    state.traction_control = setting("dcTractionControl").into();
    state.abs = setting("dcABS").into();
    state.engine_map = setting("dcFuelMixture").into();
    state.front_anti_roll_bar = setting("dcAntiRollFront").into();
    state.rear_anti_roll_bar = setting("dcAntiRollRear").into();
    state.speed_limiter_on = frame.bool("dcPitSpeedLimiterToggle").into();
    state.steering_wheel_range = f("SteeringWheelAngleMax")
        .filter(|r| *r > 0.0)
        .map(f64::to_degrees)
        .into();
    state.engine_max_rpm = info.car.red_line.into();
    state.max_gears = info.car.forward_gears.into();
    state.fuel_capacity = info.car.fuel_capacity.into();
    state.incident_points = frame
        .i32("PlayerCarTeamIncidentCount")
        .or_else(|| frame.i32("PlayerCarMyIncidentCount"))
        .filter(|n| *n >= 0)
        .map(f64::from)
        .into();
    state.push_to_pass_engaged = frame.bool("P2P_Status").into();

    let compound = frame.i32("PlayerTireCompound").filter(|c| *c >= 0);
    for (wheel, prefix) in out.wheels.iter_mut().zip(WHEELS) {
        let w = |name: &str| f(&format!("{prefix}{name}"));
        wheel.suspension_deflection = w("shockDefl").into();
        wheel.ride_height = w("rideHeight").into();
        // bar
        wheel.brake_pressure = w("brakeLinePress").map(|bar| bar * 100.0).into();
        wheel.tire_pressure = w("pressure").into();
        wheel.tire_surface_temperature_left = w("tempL").into();
        wheel.tire_surface_temperature_center = w("tempM").into();
        wheel.tire_surface_temperature_right = w("tempR").into();
        let carcass = [w("tempCL"), w("tempCM"), w("tempCR")];
        wheel.tire_inner_temperature_left = carcass[0].into();
        wheel.tire_inner_temperature_center = carcass[1].into();
        wheel.tire_inner_temperature_right = carcass[2].into();
        wheel.tire_carcass_temperature = average(&carcass).into();
        // Tread left, across the tyre: what's left, like LMU's (see the header)
        wheel.tire_wear = average(&[w("wearL"), w("wearM"), w("wearR")])
            .map(|left| left.clamp(0.0, 1.0))
            .into();
        wheel.tire_compound = compound.map(i64::from).into();
    }
}

/// The mean of the values, all known.
fn average(values: &[Option<f64>]) -> Option<f64> {
    let sum = values.iter().copied().sum::<Option<f64>>()?;
    Some(sum / values.len() as f64)
}

/// The session's phase. A red flag stops it whatever its state.
fn phase(state: Option<i32>, flags: Option<u32>) -> Option<&'static str> {
    let flags = flags.unwrap_or(0);
    if flags & FLAG_RED != 0 {
        return Some("stopped");
    }
    Some(match state? {
        STATE_GET_IN_CAR => "before_start",
        STATE_WARMUP => "countdown",
        STATE_PARADE_LAPS => "formation_lap",
        STATE_RACING if flags & (FLAG_CAUTION | FLAG_CAUTION_WAVING) != 0 => "full_course_yellow",
        STATE_RACING => "green",
        STATE_CHECKERED | STATE_COOL_DOWN => "over",
        _ => return None,
    })
}

/// The full course caution's state, from the session's flags.
fn full_course_yellow(frame: &Frame<'_>) -> impl Fn(u32) -> &'static str {
    let pits_open = frame.bool("PitsOpen");
    move |flags| {
        if flags & (FLAG_CAUTION | FLAG_CAUTION_WAVING) == 0 {
            "none"
        } else if flags & FLAG_ONE_LAP_TO_GREEN != 0 {
            "last_lap"
        } else if flags & FLAG_CAUTION_WAVING != 0 {
            "pending"
        } else {
            match pits_open {
                Some(false) => "pits_closed",
                Some(true) => "pits_open",
                None => "pending",
            }
        }
    }
}

/// The flag shown to a car, the most pressing first.
fn car_flag(bits: u32) -> Option<&'static str> {
    Some(match () {
        _ if bits & FLAG_CHECKERED != 0 => "checkered",
        _ if bits & (FLAG_BLACK | FLAG_DISQUALIFY) != 0 => "black",
        _ if bits & FLAG_BLUE != 0 => "blue",
        _ if bits & FLAG_WHITE != 0 => "white",
        _ if bits & (FLAG_GREEN | FLAG_GREEN_HELD) != 0 => "green",
        _ => return None,
    })
}

/// `irsdk_TrackWetness` (1 dry to 7 extremely wet) as the recording's 0-1.
fn wetness(level: i32) -> Option<f64> {
    (1..=7).contains(&level).then(|| f64::from(level - 1) / 6.0)
}

/// How far the clock and the time left can disagree about when the session ends (s): they're
/// read a frame apart.
const SESSION_END_JITTER: f64 = 0.1;

/// When the session's clock runs out, held steady: `last`, unless the time left at `time` moves
/// it. Run out, it stays when it ran out.
fn session_end(last: Option<f64>, time: f64, remaining: Option<f64>) -> Option<f64> {
    let Some(remaining) = remaining else {
        return last.filter(|end| *end <= time);
    };
    let end = round_ms(time + remaining);
    match last {
        Some(last) if remaining <= 0.0 && last <= time => Some(last),
        Some(last) if (end - last).abs() <= SESSION_END_JITTER => Some(last),
        _ => Some(end),
    }
}

/// Derived times mix the clock with the game's f32 values: their sub-millisecond noise would make
/// every scoring row differ.
fn round_ms(time: f64) -> f64 {
    (time * 1000.0).round() / 1000.0
}

fn round_tenth(value: f64) -> f64 {
    (value * 10.0).round() / 10.0
}

/// Each car's gaps to the cars ahead of and behind it on track, in seconds: how long the car behind
/// takes to get where the one ahead is (see `follow_gap`). Cars in the pit lane aren't on track.
fn track_gaps(cars: &[Car<'_>]) -> HashMap<i64, (Option<f64>, Option<f64>)> {
    let mut on_track: Vec<_> = cars
        .iter()
        .filter(|car| car.on_pit_road != Some(true))
        .filter_map(|car| Some((car.pct?, car)))
        .collect();
    on_track.sort_by(|a, b| a.0.total_cmp(&b.0));
    let mut gaps = HashMap::new();
    let n = on_track.len();
    if n < 2 {
        return gaps;
    }
    for (k, (_, car)) in on_track.iter().enumerate() {
        let (_, ahead) = on_track[(k + 1) % n];
        let (_, behind) = on_track[(k + n - 1) % n];
        gaps.insert(car.idx, (follow_gap(car, ahead), follow_gap(behind, car)));
    }
    gaps
}

/// How long `behind` takes to get where `ahead` is on track. Between cars of a class, by the game's
/// estimate of how long into the lap each is (the same estimate for both); between classes, by how
/// far apart they are at the follower's class's lap time.
fn follow_gap(behind: &Car<'_>, ahead: &Car<'_>) -> Option<f64> {
    let (from, to) = (behind.pct?, ahead.pct?);
    let lap = behind.driver.class_lap_time;
    let same_class =
        behind.driver.class_id.is_some() && behind.driver.class_id == ahead.driver.class_id;
    if same_class && let (Some(from_est), Some(to_est)) = (behind.est, ahead.est) {
        // The one ahead is past the line: the rest of the lap closes the gap
        let across = match to < from {
            true => lap?,
            false => 0.0,
        };
        return Some(to_est - from_est + across).filter(|gap| *gap >= 0.0);
    }
    Some((to - from).rem_euclid(1.0) * lap?)
}

/// In races, each car's gaps to the places ahead and behind, in seconds, from the game's time
/// behind the leader.
fn place_gaps(cars: &[Car<'_>], race: bool) -> HashMap<i64, (Option<f64>, Option<f64>)> {
    let mut gaps = HashMap::new();
    if !race {
        return gaps;
    }
    let by_place: HashMap<i64, &Car<'_>> = cars
        .iter()
        .filter_map(|car| Some((car.position?, car)))
        .collect();
    for car in cars {
        let Some(place) = car.position else {
            continue;
        };
        let gap = |ahead: Option<&&Car<'_>>, behind: &Car<'_>| {
            Some(behind.f2? - ahead?.f2?).filter(|gap| *gap >= 0.0)
        };
        let ahead = gap(by_place.get(&(place - 1)), car);
        let behind = by_place
            .get(&(place + 1))
            .and_then(|behind| gap(Some(&car), behind));
        gaps.insert(car.idx, (ahead, behind));
    }
    gaps
}

/// What's worked out about one car from update to update.
#[derive(Default)]
struct CarTrack {
    /// Its last update: session time and where it was around the lap
    last: Option<(f64, f64)>,
    /// The game's count of laps completed
    lap: Option<i32>,
    /// When the current lap started: when the car crossed the line, or else when the count went up
    lap_start: Option<f64>,
    /// When the car last crossed the line, while no lap count has gone up for it yet
    line: Option<f64>,
    /// When the lap count last went up, while the car hasn't been seen crossing the line for it
    counted: Option<f64>,
    sector: Option<usize>,
    /// When the car entered its sector, if it was seen to
    sector_start: Option<f64>,
    /// This lap's sector times so far
    current: Vec<Option<f64>>,
    last_lap: Vec<Option<f64>>,
    /// The sectors of the lap the game has for the car's best
    best_lap: Vec<Option<f64>>,
    best_lap_total: Option<f64>,
    /// The best time of each sector, of any lap
    best: Vec<Option<f64>>,
    /// The last whole laps timed here: their total and sectors
    recent: Vec<(f64, Vec<Option<f64>>)>,
    /// Laps gone, counted from when it was first seen, for its speed
    distance: f64,
    /// When and at what distance its speed was last worked out
    anchor: Option<(f64, f64)>,
    speed: Option<f64>,
    on_pit_road: Option<bool>,
    /// Its time in the pit lane: whether it was seen entering it, and if it has stopped in its
    /// pit box yet
    visit: Option<(bool, bool)>,
    pit_stops: i64,
    pit_state: Option<&'static str>,
}

impl CarTrack {
    #[allow(clippy::too_many_arguments)]
    fn update(
        &mut self,
        time: f64,
        pct: Option<f64>,
        lap: Option<i32>,
        on_pit_road: Option<bool>,
        surface: Option<i32>,
        official_best: Option<f64>,
        length: f64,
        sectors: &[f64],
    ) {
        let n = sectors.len();
        if self.current.len() != n {
            self.current = vec![None; n];
            self.last_lap = vec![None; n];
            self.best_lap = vec![None; n];
            self.best = vec![None; n];
            self.best_lap_total = None;
            self.recent.clear();
            self.sector = None;
            self.sector_start = None;
        }

        self.best_lap_matching(official_best);

        let Some(pct) = pct else {
            // Out of the world (in the garage, disconnected): where it comes back says nothing
            // about how it got there, and this lap's times so far can't be completed
            self.last = None;
            self.sector = None;
            self.sector_start = None;
            self.current = vec![None; n];
            self.line = None;
            self.counted = None;
            self.lap_start = None;
            self.lap = lap.or(self.lap);
            self.anchor = None;
            self.speed = None;
            self.on_pit_road = None;
            self.visit = None;
            self.pit_state = None;
            return;
        };

        // How far it went since the last update, across the line if it crossed it
        let step = self.last.map(|(t0, p0)| {
            let mut dp = pct - p0;
            if dp < -0.5 {
                dp += 1.0;
            } else if dp > 0.5 {
                dp -= 1.0;
            }
            (t0, p0, time - t0, dp)
        });
        let continuous = step.is_some_and(|(_, _, dt, dp)| {
            dt > 0.0 && dp * length >= -MAX_BACK && dp * length <= MAX_SPEED * dt + MAX_JUMP
        });
        // How far along the update a boundary at `boundary` was crossed, and when: driven over
        // it, from just past where the car was (where it was is the update before's crossing)
        let passed = |boundary: f64| {
            let (t0, p0, dt, dp) = step?;
            let along = (boundary - p0).rem_euclid(1.0);
            // Landing on the line is crossing it, whatever the rounding
            (continuous && dp > 0.0 && along > 0.0 && along <= dp * (1.0 + 1e-9) + 1e-12)
                .then(|| (along, t0 + (along / dp).min(1.0) * dt))
        };

        // Pit stops: stopping in its box after it was seen driving into the pit lane (starting in
        // the pits, coming out of the garage or being towed there isn't one)
        if let Some(on) = on_pit_road {
            match (self.on_pit_road, on) {
                (Some(false), true) => self.visit = Some((continuous, false)),
                (None, true) => self.visit = Some((false, false)),
                (_, false) => self.visit = None,
                _ => {}
            }
            self.on_pit_road = Some(on);
        }
        if !continuous && let Some((counted, _)) = &mut self.visit {
            *counted = false;
        }
        // In its box is where it is: stopped there is a stop
        let stopped_in_box =
            surface == Some(IN_PIT_STALL) && self.speed.is_some_and(|speed| speed < STOPPED);
        if let Some((counted, stopped)) = &mut self.visit
            && stopped_in_box
            && !*stopped
        {
            *stopped = true;
            if *counted {
                self.pit_stops += 1;
            }
        }
        self.pit_state = match (self.visit, on_pit_road) {
            (_, None) => None,
            (Some(_), _) if stopped_in_box => Some("stopped"),
            (Some((_, true)), _) => Some("exiting"),
            (Some(_), _) | (None, Some(true)) => Some("entering"),
            (None, Some(false)) => Some("none"),
        };

        // The lap starts at the line. The count and the distance can go round an update or so
        // apart: the crossing waits a moment for the count, and the count for the next crossing
        // (on tracks whose laps finish elsewhere than they start, that's a while)
        let line = passed(0.0).map(|(_, at)| at);
        let counted_back = matches!((self.lap, lap), (Some(previous), Some(lap)) if lap < previous);
        let rose = matches!((self.lap, lap), (Some(previous), Some(lap)) if lap > previous);
        if counted_back {
            // Another session
            self.lap_start = None;
            self.line = None;
            self.counted = None;
            self.sector_start = None;
            self.current = vec![None; n];
        } else if !continuous {
            // Moved, not driven: a crossing or count before it says nothing about after it
            self.line = None;
            self.counted = None;
            if rose {
                self.lap_start = None;
            }
        } else if rose {
            let crossed = line.or(self.line.filter(|at| time - at <= LINE_MATCH));
            self.lap_start = Some(crossed.unwrap_or(time));
            self.counted = crossed.is_none().then_some(time);
            self.line = None;
        } else if let Some(at) = line {
            self.line = Some(at);
            if let Some(counted) = self.counted.take() {
                self.lap_start = Some(at);
                // Long after the count, it's the next lap's start too, if the count follows
                if at - counted <= LINE_MATCH {
                    self.line = None;
                }
            }
        }
        if lap.is_some() {
            self.lap = lap;
        }

        // Sector times, from boundary to boundary: every boundary crossed, in order
        let sector = sector_of(pct, sectors);
        if !continuous {
            // Moved, not driven: this lap's times so far can't be completed
            self.sector_start = None;
            self.current = vec![None; n];
        } else if let Some(mut previous) = self.sector {
            let mut from = 0.0;
            for _ in 0..n {
                let next = (previous + 1) % n;
                let Some((along, at)) = passed(sectors[next]).filter(|(along, _)| *along > from)
                else {
                    break;
                };
                from = along;
                if let Some(start) = self.sector_start {
                    let split = at - start;
                    self.current[previous] = Some(split);
                    let best = &mut self.best[previous];
                    if best.is_none_or(|best| split < best) {
                        *best = Some(split);
                    }
                }
                if next == 0 {
                    self.finish_lap();
                }
                self.sector_start = Some(at);
                previous = next;
            }
            // Not where its crossings put it (it went back over a boundary): its time isn't known
            if Some(previous) != sector {
                self.sector_start = None;
            }
        }
        self.sector = sector;

        // Speed, over at least SPEED_WINDOW
        match step {
            Some((_, _, _, dp)) if continuous => self.distance += dp,
            _ => {
                self.anchor = None;
                self.speed = None;
            }
        }
        match self.anchor {
            Some((t, d)) if time - t >= SPEED_WINDOW => {
                self.speed = Some(((self.distance - d) * length / (time - t)).max(0.0));
                self.anchor = Some((time, self.distance));
            }
            None => self.anchor = Some((time, self.distance)),
            _ => {}
        }
        self.last = Some((time, pct));
    }

    /// What to add to the lap count for the lap the car is on by its distance at `time`: while
    /// the count went up before the car crossed the line (-1), or the car crossed it before the
    /// count did (1).
    fn lap_offset(&self, time: f64) -> i64 {
        let recent = |at: Option<f64>| at.is_some_and(|at| time - at <= LINE_MATCH);
        match () {
            _ if recent(self.counted) => -1,
            _ if recent(self.line) => 1,
            _ => 0,
        }
    }

    /// The lap just completed: its sectors are the last lap's.
    fn finish_lap(&mut self) {
        let n = self.current.len();
        self.last_lap = std::mem::replace(&mut self.current, vec![None; n]);
        if let Some(total) = self.last_lap.iter().copied().sum::<Option<f64>>() {
            if self.recent.len() >= RECENT_LAPS {
                self.recent.remove(0);
            }
            self.recent.push((total, self.last_lap.clone()));
        }
    }

    /// The best lap's sectors: of the lap timed here that's the game's best (its best lap
    /// may not count every lap, so the fastest here isn't always it). Not known when it's a lap
    /// that wasn't seen whole.
    fn best_lap_matching(&mut self, official: Option<f64>) {
        let Some(official) = official else {
            return;
        };
        if self
            .best_lap_total
            .is_some_and(|total| (total - official).abs() <= BEST_LAP_MATCH)
        {
            return;
        }
        let off = |total: f64| (total - official).abs();
        match self
            .recent
            .iter()
            .filter(|(total, _)| off(*total) <= BEST_LAP_MATCH)
            .min_by(|a, b| off(a.0).total_cmp(&off(b.0)))
        {
            Some((total, sectors)) => {
                self.best_lap_total = Some(*total);
                self.best_lap = sectors.clone();
            }
            // A best lap this didn't time whole (yet: it's looked for again on every update)
            None => {
                self.best_lap_total = None;
                self.best_lap = vec![None; self.current.len()];
            }
        }
    }
}

/// The timing sector a car is in, from where each starts.
fn sector_of(pct: f64, sectors: &[f64]) -> Option<usize> {
    let n = sectors.len();
    if n == 0 {
        return None;
    }
    // Before the first sector's start (if it isn't at the line), it's still the last one
    Some(match sectors.partition_point(|start| *start <= pct) {
        0 => n - 1,
        k => k - 1,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const SECTORS: [f64; 3] = [0.0, 0.3, 0.7];
    const LENGTH: f64 = 1000.0;

    /// A car driven at 50 m/s from `pct` for `seconds`, updated 60 times a second.
    fn drive(track: &mut CarTrack, time: &mut f64, pct: &mut f64, lap: &mut i32, seconds: f64) {
        let dt = 1.0 / 60.0;
        for _ in 0..(seconds * 60.0).round() as usize {
            *time += dt;
            *pct += 50.0 * dt / LENGTH;
            if *pct >= 1.0 {
                *pct -= 1.0;
                *lap += 1;
            }
            track.update(
                *time,
                Some(*pct),
                Some(*lap),
                Some(false),
                Some(3),
                None,
                LENGTH,
                &SECTORS,
            );
        }
    }

    #[test]
    fn times_laps_and_sectors_at_the_lines() {
        let mut track = CarTrack::default();
        let (mut time, mut pct, mut lap) = (100.0, 0.9, 1);
        drive(&mut track, &mut time, &mut pct, &mut lap, 1.0);
        assert_eq!(track.lap_start, None);
        // Over the line at 102 s (100 m to go at 50 m/s)
        drive(&mut track, &mut time, &mut pct, &mut lap, 1.5);
        assert!((track.lap_start.unwrap() - 102.0).abs() < 1e-6);
        assert_eq!(track.sector, Some(0));
        assert!((track.speed.unwrap() - 50.0).abs() < 0.01);
        // On through a whole lap of 20 s
        drive(&mut track, &mut time, &mut pct, &mut lap, 20.0);
        // The first lap seen whole: 6, 8 and 6 s
        assert!((track.lap_start.unwrap() - 122.0).abs() < 1e-6);
        let times: Vec<_> = track.last_lap.iter().map(|t| t.unwrap()).collect();
        for (time, expected) in times.iter().zip([6.0, 8.0, 6.0]) {
            assert!((time - expected).abs() < 1e-6, "{times:?}");
        }
        assert!((track.current[0].is_none()));
        assert_eq!(track.recent.len(), 1);

        // The game's best lap is the one timed here
        track.best_lap_matching(Some(20.01));
        assert!((track.best_lap_total.unwrap() - 20.0).abs() < 1e-6);
        assert!((track.best[1].unwrap() - 8.0).abs() < 1e-6);
    }

    #[test]
    fn a_car_moved_has_no_times_for_the_sectors_it_skipped() {
        let mut track = CarTrack::default();
        let (mut time, mut pct, mut lap) = (100.0, 0.1, 1);
        drive(&mut track, &mut time, &mut pct, &mut lap, 2.0);
        // Towed into the next sector: no time for it, nor for the one it lands in
        time += 1.0 / 60.0;
        track.update(
            time,
            Some(0.5),
            Some(1),
            Some(false),
            Some(3),
            None,
            LENGTH,
            &SECTORS,
        );
        assert_eq!(track.sector, Some(1));
        assert_eq!(track.sector_start, None);
        assert_eq!(track.speed, None);
        let (mut pct, mut lap) = (0.5, 1);
        drive(&mut track, &mut time, &mut pct, &mut lap, 7.0);
        assert_eq!(track.sector, Some(2));
        assert_eq!(track.current[1], None);
    }

    fn update(track: &mut CarTrack, time: f64, pct: Option<f64>, lap: i32, sectors: &[f64]) {
        track.update(
            time,
            pct,
            Some(lap),
            Some(false),
            Some(3),
            None,
            LENGTH,
            sectors,
        );
    }

    #[test]
    fn a_lap_starts_at_the_line_when_the_count_goes_up_an_update_apart() {
        // Counted an update after the line
        let mut track = CarTrack::default();
        update(&mut track, 10.0, Some(0.98), 1, &SECTORS);
        update(&mut track, 10.5, Some(0.02), 1, &SECTORS);
        assert_eq!(track.lap_start, None);
        update(&mut track, 11.0, Some(0.06), 2, &SECTORS);
        assert!((track.lap_start.unwrap() - 10.25).abs() < 1e-9);

        // Counted an update before it
        let mut track = CarTrack::default();
        update(&mut track, 10.0, Some(0.94), 1, &SECTORS);
        update(&mut track, 10.5, Some(0.98), 2, &SECTORS);
        assert_eq!(track.lap_start, Some(10.5));
        update(&mut track, 11.0, Some(0.02), 2, &SECTORS);
        assert!((track.lap_start.unwrap() - 10.75).abs() < 1e-9);
        // Later crossings without a count aren't this lap's
        update(&mut track, 40.0, Some(0.02), 2, &SECTORS);
        assert!((track.lap_start.unwrap() - 10.75).abs() < 1e-9);
    }

    #[test]
    fn a_track_with_one_sector_times_it_line_to_line() {
        let mut track = CarTrack::default();
        let one = [0.0];
        // Over the line at 1 s and 11 s
        update(&mut track, 0.0, Some(0.9), 1, &one);
        update(&mut track, 2.0, Some(0.1), 2, &one);
        update(&mut track, 6.0, Some(0.5), 2, &one);
        update(&mut track, 10.0, Some(0.9), 2, &one);
        update(&mut track, 12.0, Some(0.1), 3, &one);
        assert!((track.last_lap[0].unwrap() - 10.0).abs() < 1e-9);
        assert_eq!(track.recent.len(), 1);
    }

    #[test]
    fn leaving_the_world_drops_the_laps_times_so_far() {
        let mut track = CarTrack::default();
        let (mut time, mut pct, mut lap) = (100.0, 0.1, 1);
        // Through the second sector's start, timing the first from where it was first seen
        drive(&mut track, &mut time, &mut pct, &mut lap, 26.0);
        assert!(track.current[0].is_some());
        update(&mut track, time + 1.0, None, lap, &SECTORS);
        assert!(track.current.iter().all(Option::is_none));
    }

    #[test]
    fn a_best_lap_not_timed_whole_has_no_sectors() {
        let mut track = CarTrack::default();
        track.current = vec![None; 3];
        track.best_lap = vec![Some(30.0), Some(30.0), Some(30.0)];
        track.best_lap_total = Some(90.0);
        track.recent = vec![(88.0, vec![Some(29.0), Some(30.0), Some(29.0)])];
        track.best_lap_matching(Some(89.0));
        assert_eq!(track.best_lap_total, None);
        assert!(track.best_lap.iter().all(Option::is_none));
        track.best_lap_matching(Some(88.05));
        assert_eq!(track.best_lap_total, Some(88.0));
    }

    #[test]
    fn counts_the_stops_it_sees_made() {
        let mut track = CarTrack::default();
        let update = |track: &mut CarTrack, t: f64, pit: bool, surface: i32| {
            track.update(
                t,
                Some(0.05),
                Some(1),
                Some(pit),
                Some(surface),
                None,
                LENGTH,
                &SECTORS,
            );
            track.pit_state
        };
        // Starting in its box isn't a stop. It's stopped there once it's seen not moving
        assert_eq!(
            update(&mut track, 0.5, true, IN_PIT_STALL),
            Some("entering")
        );
        update(&mut track, 1.0, true, IN_PIT_STALL);
        assert_eq!(update(&mut track, 1.5, true, IN_PIT_STALL), Some("stopped"));
        assert_eq!(update(&mut track, 2.0, true, 2), Some("exiting"));
        assert_eq!(update(&mut track, 3.0, false, 3), Some("none"));
        assert_eq!(track.pit_stops, 0);
        assert_eq!(update(&mut track, 4.0, true, 2), Some("entering"));
        assert_eq!(update(&mut track, 5.0, true, IN_PIT_STALL), Some("stopped"));
        assert_eq!(update(&mut track, 6.0, true, IN_PIT_STALL), Some("stopped"));
        assert_eq!(update(&mut track, 7.0, true, 2), Some("exiting"));
        assert_eq!(track.pit_stops, 1);
        // A drive through isn't a stop
        update(&mut track, 8.0, false, 3);
        update(&mut track, 9.0, true, 2);
        update(&mut track, 10.0, false, 3);
        assert_eq!(track.pit_stops, 1);

        // Nor is driving through its box
        let mut track = CarTrack::default();
        let (mut time, mut pct) = (0.0, 0.05);
        for surface in [3, 2, 2, IN_PIT_STALL, IN_PIT_STALL, 2] {
            time += 0.5;
            pct += 0.01;
            let pit = surface != 3;
            track.update(
                time,
                Some(pct),
                Some(1),
                Some(pit),
                Some(surface),
                None,
                LENGTH,
                &SECTORS,
            );
        }
        assert_eq!(track.pit_stops, 0);
        assert_eq!(track.pit_state, Some("entering"));
    }

    #[test]
    fn times_every_sector_an_update_goes_through() {
        let mut track = CarTrack::default();
        update(&mut track, 0.0, Some(0.25), 1, &SECTORS);
        // Into the second sector at 0.5 s
        update(&mut track, 1.0, Some(0.35), 1, &SECTORS);
        update(&mut track, 5.0, Some(0.65), 1, &SECTORS);
        // Through the third sector's start (5.5 s) and the line (8.5 s) in one update
        update(&mut track, 9.0, Some(0.05), 2, &SECTORS);
        assert_eq!(track.last_lap[0], None);
        assert!((track.last_lap[1].unwrap() - 5.0).abs() < 1e-9);
        assert!((track.last_lap[2].unwrap() - 3.0).abs() < 1e-9);
        assert!((track.lap_start.unwrap() - 8.5).abs() < 1e-9);
        assert_eq!(track.sector, Some(0));
    }

    #[test]
    fn landing_on_a_line_crosses_it_once() {
        let mut track = CarTrack::default();
        let one = [0.0];
        update(&mut track, 0.0, Some(0.9), 1, &one);
        update(&mut track, 1.0, Some(0.95), 1, &one);
        update(&mut track, 2.0, Some(0.0), 2, &one);
        assert_eq!(track.sector_start, Some(2.0));
        assert_eq!(track.lap_start, Some(2.0));
        // Leaving it isn't crossing it again
        update(&mut track, 3.0, Some(0.05), 2, &one);
        assert_eq!(track.sector_start, Some(2.0));
        assert!(track.recent.is_empty());
        assert_eq!(track.lap_offset(3.0), 0);
    }

    #[test]
    fn a_lap_counted_away_from_the_line_starts_at_it() {
        // A track whose laps finish before the line they start at
        let mut track = CarTrack::default();
        update(&mut track, 0.0, Some(0.80), 1, &SECTORS);
        update(&mut track, 1.0, Some(0.85), 2, &SECTORS);
        assert_eq!(track.lap_start, Some(1.0));
        for (time, pct) in [(2.0, 0.9), (3.0, 0.95), (4.0, 0.99)] {
            update(&mut track, time, Some(pct), 2, &SECTORS);
        }
        update(&mut track, 5.0, Some(0.03), 2, &SECTORS);
        assert!((track.lap_start.unwrap() - 4.25).abs() < 1e-9);
    }

    #[test]
    fn counting_back_drops_the_sector_being_timed() {
        let mut track = CarTrack::default();
        update(&mut track, 0.0, Some(0.25), 3, &SECTORS);
        update(&mut track, 1.0, Some(0.35), 3, &SECTORS);
        assert!(track.sector_start.is_some());
        update(&mut track, 2.0, Some(0.45), 0, &SECTORS);
        assert_eq!(track.sector_start, None);
        assert_eq!(track.lap_start, None);
    }

    #[test]
    fn reads_the_flags_and_states() {
        assert_eq!(phase(Some(STATE_RACING), Some(FLAG_GREEN)), Some("green"));
        assert_eq!(
            phase(Some(STATE_RACING), Some(FLAG_CAUTION)),
            Some("full_course_yellow")
        );
        assert_eq!(phase(Some(STATE_RACING), Some(FLAG_RED)), Some("stopped"));
        assert_eq!(phase(Some(STATE_PARADE_LAPS), None), Some("formation_lap"));
        assert_eq!(phase(Some(STATE_CHECKERED), None), Some("over"));
        assert_eq!(car_flag(FLAG_BLUE | FLAG_GREEN), Some("blue"));
        assert_eq!(car_flag(FLAG_CHECKERED | FLAG_BLACK), Some("checkered"));
        assert_eq!(car_flag(0), None);
        assert_eq!(wetness(1), Some(0.0));
        assert_eq!(wetness(7), Some(1.0));
        assert_eq!(wetness(0), None);
        assert_eq!(sector_of(0.5, &SECTORS), Some(1));
        assert_eq!(sector_of(0.05, &[0.1, 0.5]), Some(1));
        assert_eq!(sector_of(0.5, &[]), None);

        // Held steady across a frame's disagreement, and once the clock's run out
        let end = session_end(None, 100.0, Some(900.0));
        assert_eq!(end, Some(1000.0));
        assert_eq!(session_end(end, 200.0, Some(800.033)), end);
        assert_eq!(session_end(end, 1001.0, Some(0.0)), end);
        assert_eq!(session_end(end, 1001.0, None), end);
        // Moved by more: the session was lengthened
        assert_eq!(session_end(end, 200.0, Some(1000.0)), Some(1200.0));
        // No time left given before it ends: none
        assert_eq!(session_end(end, 500.0, None), None);
    }
}
