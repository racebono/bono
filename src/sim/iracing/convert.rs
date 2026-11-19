//! Turns iRacing's updates (see `sdk`) and session info (see `session`) into the recording's rows,
//! in its units and conventions (see `include/bono`).
//!
//! iRacing shares the player's car in detail, but every other car only as arrays indexed by car:
//! where it is around the lap, its laps and lap times, its place, gear and revs. What it doesn't
//! share is worked out here from those, update by update: when each lap started, sector times,
//! speed, pit stops, and the gaps between the cars. It gives no positions in the world, only
//! distances around the lap.
//!
//! The enum values and bits are `irsdk_defines.h`'s.
//!
//! The SDK doesn't document which way its car axes and rotations point. They're taken from what
//! iRacing's own saved laps show (`.ibt` files, which also have the car's GPS position), each
//! checked against something physical, in every file checked, and turned into the recording's
//! conventions (lateral, yaw and steering positive to the right, pitch positive nose up):
//!
//! - `Yaw` and `YawRate` are positive turning left: over a lap, `YawRate` turns the opposite way
//!   to the GPS track, and matches how `Yaw` changes. `YawNorth` is a compass heading, clockwise
//!   from north, as the GPS bearing is.
//! - `SteeringWheelAngle` is positive to the left: it has the yaw rate's sign. The docs say right,
//!   which the laps contradict. `SteeringWheelTorque` is on the same axis, against the angle.
//!   `SteeringWheelAngleMax` is taken for the wheel's span lock to lock: no lap reaches it, and as
//!   one side's lock it would make the cars' steering several turns each way.
//! - `LatAccel` and `VelocityY` are positive to the left: the acceleration has the sign of speed
//!   times yaw rate, and the velocity's angle is opposite to the GPS track's slip from the heading.
//! - `VelocityX` and `LongAccel` are positive forward, `VelocityZ` and `VertAccel` up (the
//!   accelerations including gravity: `VertAccel` reads about 9.8 driving on the flat).
//! - `Pitch` and `PitchRate` are positive nose down: pitch goes against the GPS climb rate and
//!   against gravity's pull on `LongAccel`.
//! - `Roll` isn't recorded: the laps don't say which way it points (it mixes how the car leans
//!   with the track's camber, and the two anchors disagree between tracks).
//! - `WindDir` is the compass bearing the wind blows from, clockwise from north (as weather
//!   reports give it): flat out on the straights, a car gains with the tailwind that reading gives,
//!   and loses with the headwind. The laps tell it apart from blowing towards it, or turning the
//!   other way round, by less than they do the car's axes, but every lap with enough wind agrees.
//!
//! `sim::iracing::tests::axis_conventions_hold_on_saved_laps` checks these against saved laps.

use super::{
    sdk::{Frame, UNLIMITED_LAPS, UNLIMITED_TIME},
    session::{PlayerSetup, SessionInfo},
};
use crate::sim::memory::{SCORING_EVERY, Update};
use bono_sim::{
    setup::{Setting, Setup, SetupSource},
    sys::{bono_session_info, bono_standing, bono_telemetry},
};
use serde_json::{Map, Value, json};
use std::{
    collections::{HashMap, HashSet, VecDeque},
    time::Instant,
};

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
const SERVICE_FAST_REPAIR: u32 = 0x40;

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
/// The steps (m/s) the wind is recorded in.
const WIND_STEP: f64 = 0.5;
/// Slower than this (m/s), which way a car moves is noise.
const MIN_DIRECTION_SPEED: f64 = 1.0;
/// Slower than this (m/s), a car in its pit box has stopped there.
const STOPPED: f64 = 0.5;
/// How far apart (laps) the lap count going up and the car crossing the line may be, either way
/// round, for that crossing to start that lap: more than a finish and a start line apart, short of
/// another lap's.
const COUNT_FAR: f64 = 0.5;
/// Longer than this (s) between two updates, a car's progress isn't known to be driven.
const MAX_GAP: f64 = 5.0;
/// How far apart (s) a lap timed here must finish from the game's best changing to be the lap
/// that set it: the game gives the time a moment after the line.
const BEST_LAP_WINDOW: f64 = 5.0;
/// How many of each car's last whole laps are kept, to find its best among.
const RECENT_LAPS: usize = 4;
/// How long (s) after a car's crossing of the line its lap count may go up for the count to be timed
/// at the crossing: a moment, as the count and the distance go round a frame or so apart.
const COUNT_LAG: f64 = 1.0;
/// How long (s) after a car is over the line the game's places may take to catch up with it.
const PLACES_LAG: f64 = 3.0;
/// How far (s) after the update that counted a lap the game's lap time may put its crossing: the
/// game's timing and the updates' are a frame or so apart.
const LAP_TIME_JITTER: f64 = 0.05;
/// Under this (s), a car's time behind the leader on its first lap may be the game's placeholder.
const PLACEHOLDER_MAX: f64 = 0.1;
/// How long (s) before the last update without the flag the winner's lap count may have gone up
/// for that to be its finish, in a timed race: the count and the session's state can land a
/// moment apart. Only a
/// moment: iRacing has been seen to show the flag seconds before the leader is over the line, and
/// a longer window would take a count a lap before (a timed race's flag just after the leader
/// started its last lap) for the finish.
const FINISH_WINDOW: f64 = 2.0;
/// How far (laps) behind the car furthest on when the flag comes a car may be for its count, a
/// moment before, to be the winner's: cars over the line together, not one most of a lap behind on
/// the lead lap just over it (whose count reaches the leader's).
const ALONGSIDE: f64 = 0.1;
/// More fuel than this (litres) than at the update before, the car took fuel on: fuel is taken on
/// a little at a time, and driving never adds any.
const REFUELLED: f64 = 0.001;

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
    /// The wind as last recorded (see `steady_wind`)
    wind: Option<(f64, f64)>,
    /// The player's car and its tyre readings as last seen, by wheel (see `stamp_readings`)
    tyre_readings: (Option<i64>, [Option<TyreReading>; 4]),
    /// Since a race's flag (see `Flag`); `None` in it when the race wasn't seen before the flag
    flag: Option<Option<Flag>>,
    /// Whether this session's race was seen running before its flag
    raced: bool,
    /// The player's setup as last sent to the recorder (see `setup`); `None` in a new event
    last_setup: Option<PlayerSetup>,
    /// Whether the car has been in the garage since it was last on track: leaving it, the car is in
    /// its stall, on pit road, and a setup sent then is still the garage's
    from_garage: bool,
    /// Whether the car was out on track (in the world, off pit road) in the last frame
    on_track: bool,
}

/// A race's flag, for who has finished. The winner is the first car over the line with all the
/// race's laps done, in a race of so many laps; in a timed one, the first over with more laps than
/// any car had when the flag came (iRacing shows it a few seconds before the leader is over;
/// whoever is over first then, if the lead changes hands on the way). A car, the winner too, has
/// finished once its count has gone up since the winner's did: over the line after it (a car a lap
/// down too, its race done at the flag).
struct Flag {
    /// The last update before the flag (it came since), the first with it, the most laps any
    /// car had then, and the cars furthest on then (see `ALONGSIDE`)
    since: f64,
    seen: f64,
    most: Option<i64>,
    leading: Vec<i64>,
    /// Whether who won can't be known (see `convert`): none is taken, and nobody finishes but by
    /// the cool-down
    undecided: bool,
    /// The update a winner was first taken at: it can change for a moment after that, its
    /// crossing or the update that saw it over the line, whichever is latest (a count timed ahead of the line is timed at the line), as
    /// counts come an update or so after their crossings: until the update after the moment's
    /// last (the next one, however long after: the recording can hold up)
    decided: Option<f64>,
    /// When the winner's count went up for its finish, once it has, the laps it had then, and
    /// which car it is
    finish: Option<f64>,
    laps: Option<i64>,
    winner: Option<i64>,
}

/// One wheel's carcass temperatures and tread left, and when they took these values.
#[derive(Clone, Copy, PartialEq)]
struct TyreReading {
    values: [Option<f64>; 6],
    time: f64,
}

/// One car in one update, as read and as worked out.
#[derive(Clone)]
struct Car<'a> {
    idx: i64,
    driver: &'a super::session::Driver,
    /// Around the lap, 0-1; `None` while it isn't in the world
    pct: Option<f64>,
    on_pit_road: Option<bool>,
    /// -1 before its first lap starts (out of the pits), as the game gives it: the standings keep
    /// it, so that the out lap's end is a lap ending there too
    laps_completed: Option<i64>,
    /// What to add to `laps_completed` for the lap it's on by its distance, while the lap count
    /// and the distance go round the line an update or so apart
    lap_offset: i64,
    position: Option<i64>,
    class_position: Option<i64>,
    /// Whether the game places it first: the car its times behind the leader are from, while
    /// `position` can be the order on track (see `live_places`)
    leads: bool,
    /// The game's estimate of how long into the lap it is where it is
    est: Option<f64>,
    /// Race time behind the leader, but for the placeholder the game gives until a car's first
    /// lap is done (0, or a thousandth of a second or so a place), held a moment past the line
    f2: Option<f64>,
}

impl Car<'_> {
    /// Race time behind the leader, as of a timing line the car last passed: the game's leader's
    /// is 0, whatever the game still holds from before it led. Before a car has finished a lap the
    /// game's figure isn't a time yet (see `f2`).
    fn race_time(&self) -> Option<f64> {
        match self.leads {
            true => Some(0.0),
            false => self
                .f2
                .filter(|t| *t > 0.0 && self.laps_completed.is_some_and(|laps| laps >= 1)),
        }
    }

    /// How far it's gone in laps, for laps behind.
    fn progress(&self) -> Option<f64> {
        Some((self.laps_completed?.max(0) + self.lap_offset) as f64 + self.pct.unwrap_or(0.0))
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
        // A replay's frames aren't the live session's: they aren't recorded (the recorder skips
        // replays), nor do they move on what's been worked out about it (laps, pit stops, the
        // clock), which carries on once it's live again
        if info.replay || frame.bool("IsReplayPlaying") == Some(true) {
            return None;
        }
        if self.last_time == Some(time) {
            return None;
        }
        let dt = self
            .last_time
            .map(|last| time - last)
            .filter(|dt| *dt > 0.0);
        let went_back = self.last_time.is_some_and(|last| time < last);
        self.last_time = Some(time);

        let event = (info.event, info.track.clone());
        let new_event = self.event.as_ref().is_some_and(|last| *last != event);
        self.event = Some(event);
        // The recording starts again (see `memory::record`): the setup is sent again
        if new_event {
            self.last_setup = None;
        }
        let session = (frame.i32("SessionNum"), frame.i32("SessionUniqueID"));
        // Another session, or the clock going back, is scored right away, for the recorder to tell
        // from the scoring before this update's telemetry (and before the clock moves on again)
        let boundary = new_event || self.session != Some(session) || went_back;
        // Another session, or its clock went back (a restart, a replay rewound)
        if new_event || self.session != Some(session) || dt.is_none() {
            self.cars.clear();
            self.session_end = None;
            self.wind = None;
            self.tyre_readings = Default::default();
            self.flag = None;
            self.raced = false;
        }
        self.session = Some(session);

        let player = frame.i32("PlayerCarIdx").map(i64::from).or(info.player);
        // Every update, the car in the world or not: a reading can change in the garage. Another
        // car's readings are its own
        if self.tyre_readings.0 != player {
            self.tyre_readings = (player, Default::default());
        }
        let readings = stamp_readings(&mut self.tyre_readings.1, frame, time);
        let current = frame
            .i32("SessionNum")
            .and_then(|num| info.session(num.into()));
        let race = current.and_then(|session| session.kind) == Some("race");
        // Before a race's green the game gives no places: they're the grid's, qualifying's order,
        // when it's the event's only race (heats and features start from grids of their own).
        // All of them or none: the game's places aren't mixed with the grid's
        let races = info
            .sessions
            .iter()
            .filter(|s| s.kind == Some("race"))
            .count();
        let placed = info.cars().any(|(idx, _)| {
            PerCar::new(frame, idx, player)
                .i32("CarIdxPosition", "PlayerCarPosition")
                .is_some_and(|p| p > 0)
        });
        let gridded = race
            && races == 1
            && !placed
            && frame
                .i32("SessionState")
                .is_some_and(|state| (STATE_GET_IN_CAR..STATE_RACING).contains(&state));
        let grid = match gridded {
            true => info.grid(),
            false => HashMap::new(),
        };
        let mut cars: Vec<_> = info.cars().collect();
        cars.sort_by_key(|(idx, _)| *idx);
        let mut cars: Vec<Car<'_>> = cars
            .into_iter()
            .map(|(idx, driver)| {
                let car = PerCar::new(frame, idx, player);
                let surface = car.i32("CarIdxTrackSurface", "PlayerTrackSurface");
                let pct = car
                    .f64("CarIdxLapDistPct", "LapDistPct")
                    .filter(|pct| (0.0..=1.0).contains(pct))
                    // All the way round is at the line
                    .map(|pct| if pct >= 1.0 { 0.0 } else { pct })
                    .filter(|_| surface != Some(NOT_IN_WORLD));
                let on_pit_road = car.bool("CarIdxOnPitRoad", "OnPitRoad");
                let official_best = car
                    .f64("CarIdxBestLapTime", "LapBestLapTime")
                    .filter(|t| *t > 0.0);
                let last_lap_time = car
                    .f64("CarIdxLastLapTime", "LapLastLapTime")
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
                    last_lap_time,
                    length,
                    &info.sectors,
                );
                let lap_offset = track.lap_offset();
                // Before a car's first lap is done the game's time behind the leader is a
                // placeholder (a thousandth of a second or so a place, so it moves with the place),
                // and it can hold past the line (past the next, on a short lap): a time once it's
                // changed to one that can't be the placeholder
                let f2 = frame
                    .f64_at("CarIdxF2Time", idx as usize)
                    .filter(|t| *t >= 0.0);
                match completed {
                    Some(laps) if laps >= 1 => {
                        track.f2_timed |= f2
                            .is_some_and(|t| Some(t) != track.first_lap_f2 && t >= PLACEHOLDER_MAX);
                    }
                    // In the world before its first line (not out of it, disconnected, with
                    // its last time still held)
                    _ if pct.is_some() => {
                        track.first_lap_f2 = f2;
                        track.f2_timed = false;
                    }
                    _ => {}
                }
                let f2 = f2.filter(|_| track.f2_timed);
                // Push to pass shows the car has it: a car without it reads 0 and off
                track.push_to_pass_seen |= car.bool("CarIdxP2P_Status", "P2P_Status") == Some(true)
                    || car
                        .i32("CarIdxP2P_Count", "P2P_Count")
                        .is_some_and(|count| count > 0);
                let position = car
                    .i32("CarIdxPosition", "PlayerCarPosition")
                    .filter(|p| *p > 0)
                    .map(i64::from)
                    .or_else(|| grid.get(&idx).map(|(place, _)| *place));
                Car {
                    idx,
                    driver,
                    pct,
                    on_pit_road,
                    // -1 out of the world too (disconnected), where it's no count at all
                    laps_completed: completed
                        .filter(|laps| *laps >= 0 || pct.is_some())
                        .map(i64::from),
                    lap_offset,
                    position,
                    leads: position == Some(1),
                    class_position: car
                        .i32("CarIdxClassPosition", "PlayerCarClassPosition")
                        .filter(|p| *p > 0)
                        .map(i64::from)
                        .or_else(|| grid.get(&idx).map(|(_, place)| *place)),
                    est: frame
                        .f64_at("CarIdxEstTime", idx as usize)
                        .filter(|t| *t >= 0.0),
                    f2,
                }
            })
            .collect();

        let state = frame.i32("SessionState");
        let gaps = track_gaps(&cars);
        // Who's racing: everyone between the green and the flag; after the flag, the cars still
        // to finish (each over the line after the leader, see `Flag`), until the cool-down. Those
        // that have keep their finishing gaps and laps down, whatever the cool-down lap does
        let flagged = race && matches!(state, Some(STATE_CHECKERED | STATE_COOL_DOWN));
        self.raced |= race && state == Some(STATE_RACING);
        let counted = |idx: i64| self.cars.get(&idx).and_then(|t| t.counted_at);
        // When the car's count went up to what it is: a time kept from a count before isn't
        let counted_now = |car: &Car<'_>| {
            self.cars
                .get(&car.idx)
                .filter(|t| t.counted_lap.map(i64::from) == car.laps_completed)
                .and_then(|t| t.counted_at)
        };
        if !flagged {
            self.flag = None;
        } else if self.flag.is_none() && !self.raced {
            // Recording only from after the flag: who has finished, and with what, isn't known
            self.flag = Some(None);
        } else if self.flag.is_none() {
            let furthest = cars
                .iter()
                .filter_map(|c| c.progress())
                .max_by(f64::total_cmp);
            let leading = cars
                .iter()
                .filter(|c| {
                    c.progress()
                        .zip(furthest)
                        .is_some_and(|(gone, furthest)| furthest - gone <= ALONGSIDE)
                })
                .map(|c| c.idx)
                .collect();
            self.flag = Some(Some(Flag {
                since: time - dt.unwrap_or(0.0),
                seen: time,
                undecided: false,
                decided: None,
                most: cars.iter().filter_map(|c| c.laps_completed).max(),
                leading,
                finish: None,
                laps: None,
                winner: None,
            }));
        }
        let total = race_laps(frame);
        // The winner's finish timed at the line since (see `CarTrack::update`): it competes at
        // that, as does the moment it can change for
        if let Some(Some(flag)) = &mut self.flag
            && let Some(at) = flag.winner.and_then(|w| self.cars.get(&w)?.finish_at)
        {
            flag.finish = Some(at);
        }
        // The update that saw the winner over the line (its count's, or its line's when the count
        // came ahead of it): a car over first is seen by then, its count a moment after at most
        let finish_seen = match &self.flag {
            Some(Some(flag)) => flag.winner.and_then(|w| self.cars.get(&w)?.finish_seen),
            _ => None,
        };
        if let Some(Some(flag)) = &mut self.flag
            && !flag.undecided
            && flag.decided.is_none_or(|at| {
                let from = at
                    .max(flag.finish.unwrap_or(at))
                    .max(finish_seen.unwrap_or(at));
                time - dt.unwrap_or(0.0) - from <= COUNT_LAG
            })
        {
            // A race of so many laps: the first car over the line with them all done, whenever the
            // flag came (a moment before or after). Until a moment after, as below. Not known
            // with a car that has them done but wasn't timed over the line (done before the
            // recording began, or with a move), nor with one already past them (a flag a lap
            // late), before a winner is: one after it is over the line after it
            let done = cars
                .iter()
                .filter(|c| total.is_some() && c.laps_completed == total)
                .filter_map(|c| Some((counted_now(c)?, c.laps_completed, c.idx)));
            flag.undecided |= flag.winner.is_none()
                && total.is_some_and(|total| {
                    cars.iter().any(|c| {
                        c.laps_completed.is_some_and(|laps| laps > total)
                            || (c.laps_completed == Some(total) && counted_now(c).is_none())
                    })
                });
            // Else, the first car over the line since with more laps than anyone had at the flag.
            // Until a moment after: a car over the line just before can have its count go up an
            // update after the one that came first, and a count timed ahead of the line is timed
            // at it once the car is over (see `CarTrack::update`)
            let first = cars
                .iter()
                .filter(|c| c.laps_completed > flag.most)
                .filter_map(|c| Some((counted_now(c)?, c.laps_completed, c.idx)))
                .filter(|(at, _, _)| *at >= flag.since - FINISH_WINDOW);
            // Or over the line a moment before the flag came (iRacing's count and state can go
            // round a moment apart), its count up to those laps then or since: only a car among
            // the furthest on then, as with the flag early a car most of a lap behind on the lead
            // lap can have been over the line a moment before too
            let leading = cars
                .iter()
                .filter(|c| flag.leading.contains(&c.idx))
                .filter(|c| c.laps_completed.is_some() && c.laps_completed == flag.most)
                .filter_map(|c| Some((counted_now(c)?, c.laps_completed, c.idx)))
                .filter(|(at, _, _)| *at >= flag.since - FINISH_WINDOW);
            // The winner taken, at its finish (timed at its line once it's over, below), not its
            // count since (its cool-down lap's, with the moment from an update long before)
            let taken = flag
                .winner
                .zip(flag.finish)
                .map(|(idx, at)| (at, flag.laps, idx));
            let window = first.chain(leading).filter(|_| total.is_none());
            let fresh = done.chain(window).min_by(|a, b| a.0.total_cmp(&b.0));
            // A timed race's count from before the first update with the flag, by more than the
            // moment the two can be apart, only taken as the window runs from the last update
            // without it: the flag came somewhere in between (the recording held up), and the
            // count may have started the last lap rather than finished it. Whenever it comes (a
            // count can come an update or so after its crossing), a winner taken already too
            flag.undecided |=
                total.is_none() && fresh.is_some_and(|(at, _, _)| at < flag.seen - FINISH_WINDOW);
            let best = fresh
                .into_iter()
                .chain(taken)
                .min_by(|a, b| a.0.total_cmp(&b.0));
            if flag.undecided {
                flag.finish = None;
                flag.laps = None;
                flag.winner = None;
            } else if let Some((at, laps, idx)) = best {
                flag.finish = Some(at);
                flag.laps = laps;
                flag.winner = Some(idx);
                flag.decided.get_or_insert(time);
            }
        }
        // The winner's finish timed at the line once it's over (see `CarTrack::update`), however
        // long after the moment above
        if let Some(Some(flag)) = &mut self.flag
            && let Some(at) = flag.winner.and_then(|w| self.cars.get(&w)?.finish_at)
        {
            flag.finish = Some(at);
        }
        // Over the line since the winner was: finished, as are all in the cool-down
        let over_line: HashSet<i64> = match &self.flag {
            Some(Some(Flag {
                finish: Some(finish),
                ..
            })) => cars
                .iter()
                .filter(|c| counted(c.idx).is_some_and(|at| at >= *finish))
                .map(|c| c.idx)
                .collect(),
            _ => HashSet::new(),
        };
        let finished: HashSet<i64> = match &self.flag {
            None => HashSet::new(),
            // Not seen racing: taken for finished, all of them
            Some(None) => cars.iter().map(|c| c.idx).collect(),
            Some(Some(_)) => cars
                .iter()
                .filter(|c| state == Some(STATE_COOL_DOWN) || over_line.contains(&c.idx))
                .map(|c| c.idx)
                .collect(),
        };
        let racing = |car: &Car<'_>| match state {
            None | Some(STATE_RACING) => true,
            Some(STATE_CHECKERED) => !finished.contains(&car.idx),
            Some(_) => false,
        };
        // A car that has finished on the winner's lap: when it was over the line, for how long
        // after the winner, exactly (iRacing's time behind the leader can stay a lap old for half
        // a minute)
        if let Some(Some(Flag {
            finish: Some(_),
            laps: Some(laps),
            ..
        })) = self.flag
        {
            for car in &cars {
                // Decided as it finishes (its laps down worked out then too): a car a lap down
                // reaches the winner's laps on its cool-down lap, which isn't its finish. Only one
                // seen over the line: the cool-down finishes the rest with no crossing of theirs
                if let Some(track) = self.cars.get_mut(&car.idx)
                    && track.finish_progress.is_none()
                    && over_line.contains(&car.idx)
                    && car.laps_completed == Some(laps)
                {
                    track.finish_at = track.counted_at;
                    track.finish_seen = Some(time);
                }
            }
        }
        let finish = match &self.flag {
            Some(Some(flag)) => flag.finish,
            _ => None,
        };
        let finish_gaps: HashMap<i64, f64> = cars
            .iter()
            .filter(|c| finished.contains(&c.idx))
            .filter_map(|c| {
                let at = self.cars.get(&c.idx)?.finish_at?;
                Some((c.idx, (at - finish?).max(0.0)))
            })
            .collect();
        // The places as they are on track (see `live_places`): while the race runs, and after the
        // flag only while who has finished can be known
        let live = match state {
            Some(STATE_RACING) => true,
            Some(STATE_CHECKERED) => matches!(&self.flag, Some(Some(flag)) if !flag.undecided),
            _ => false,
        };
        let winners_laps = match &self.flag {
            Some(Some(flag)) => flag.laps,
            _ => None,
        };
        if race && live {
            live_places(&mut cars, |car| {
                if car.pct.is_none() || car.laps_completed.is_none_or(|laps| laps < 0) {
                    return None;
                }
                let track = self.cars.get(&car.idx)?;
                if racing(car) {
                    // Moved since its line (towed, reset, back in the world): where it is now
                    // isn't where it is in the race
                    // Nor with the winner's laps done but its finish not timed (counted while the
                    // recording held up): how far it's gone since says nothing of when it finished
                    let laps = car.laps_completed?;
                    if track.moved || winners_laps.is_some_and(|winners| laps >= winners) {
                        return None;
                    }
                    // After the flag, a car still racing isn't past the line it's to finish at
                    // (over it with the count still to come, or with the count at a finish line
                    // further on): behind the cars finished there
                    let gone = car.progress()?;
                    return Some(match state == Some(STATE_CHECKERED) {
                        true => (gone.min((laps + 1) as f64), f64::INFINITY),
                        false => (gone, f64::INFINITY),
                    });
                }
                // Finished: at the line, in the order over it, until the game's places catch up.
                // Only that line: not one round the cool-down lap after it (how far it had gone
                // when it first had, see `laps_behind`, is that line's lap give or take its count
                // coming a little either side of it)
                let at = track.counted_at?;
                let laps = car.laps_completed? as f64;
                let finishing = track
                    .finish_progress
                    .is_none_or(|gone| laps <= gone.round());
                (finished.contains(&car.idx) && finishing && time - at <= PLACES_LAG)
                    .then_some((laps, at))
            });
        }
        let places = place_gaps(&cars, race, &racing, &finish_gaps);
        let behind = behind_leader(&cars, race, &racing, &finish_gaps);
        let before_green =
            state.is_some_and(|state| (STATE_GET_IN_CAR..STATE_RACING).contains(&state));
        if race {
            self.laps_behind(&cars, before_green, &finished, &over_line, time);
        }
        // The player's own fuel: only while in the car (iRacing gives the fuel level and pit road
        // as the player's then; a replay isn't converted), and only the player's car keeps a window
        let in_car = frame.bool("IsOnTrack") == Some(true);
        for (idx, track) in &mut self.cars {
            if Some(*idx) != player {
                track.fuel_window.clear();
                track.last_fuel = None;
            }
        }
        let fuel_per_lap = player.and_then(|p| {
            let fuel = frame.f64("FuelLevel").filter(|l| *l >= 0.0 && in_car);
            self.cars
                .get_mut(&p)?
                .fuel_per_lap(fuel, frame.bool("OnPitRoad"))
        });

        let mut telemetry = Vec::with_capacity(cars.len());
        for car in cars.iter().filter(|car| car.pct.is_some()) {
            let track = &self.cars[&car.idx];
            let i = car.idx as usize;
            let mut out = bono_telemetry::default();
            out.vehicle_id = Some(car.idx).into();
            out.session_time = Some(time).into();
            out.time_since_last_update = dt.into();
            out.lap = car.laps_completed.map(|laps| laps.max(0)).into();
            out.lap_start_time = track.lap_start.into();
            out.lap_distance = car.pct.map(|pct| pct * length).into();
            out.sector = track.sector.map(|s| s as i64).into();
            out.in_pit_lane = car.on_pit_road.into();
            // The gaps on track, and which car each is to
            if let Some([(ahead, ahead_id), (behind, behind_id)]) = gaps.get(&car.idx).copied() {
                out.gap_to_car_ahead = ahead.into();
                out.gap_to_car_behind = behind.into();
                out.car_ahead_id = Some(ahead_id).into();
                out.car_behind_id = Some(behind_id).into();
            }
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
            // Push to pass: whether it's on, and in races what's left (its count is the uses
            // outside races, "or remaining in Race"), once the car's shown it has it
            let per_car = PerCar::new(frame, car.idx, player);
            out.state.push_to_pass_engaged = per_car.bool("CarIdxP2P_Status", "P2P_Status").into();
            out.state.push_to_pass_activations_left = per_car
                .i32("CarIdxP2P_Count", "P2P_Count")
                .filter(|count| race && *count >= 0 && track.push_to_pass_seen)
                .map(i64::from)
                .into();
            if Some(car.idx) == player {
                player_telemetry(frame, info, &mut out);
                for (wheel, reading) in out.wheels.iter_mut().zip(readings) {
                    wheel.tire_reading_time = reading.into();
                }
                out.state.fuel_per_lap = fuel_per_lap.into();
            }
            telemetry.push(out);
        }

        let scoring = match self.last_scoring {
            Some(last) if !boundary && now.duration_since(last) < SCORING_EVERY => None,
            _ => {
                self.last_scoring = Some(now);
                let remaining = frame
                    .f64("SessionTimeRemain")
                    .filter(|t| *t >= 0.0 && *t < UNLIMITED_TIME);
                self.session_end = session_end(self.session_end, time, remaining);
                self.wind = steady_wind(self.wind, wind(frame));
                Some(self.scoring(frame, info, &cars, player, race, time, &places, &behind))
            }
        };

        Some(Update {
            telemetry,
            scoring,
            new_event,
            setup: self.setup(frame, info),
            // The recorder tells another session, the clock going back and replays from the
            // scoring
            new_epoch: None,
        })
    }

    /// The player's setup for the recorder, when the session info's is new or has changed: the
    /// game sends it again on leaving the garage and loading a setup, and maybe after a stop.
    fn setup(&mut self, frame: &Frame<'_>, info: &SessionInfo) -> Option<Setup> {
        let in_garage = frame.bool("IsInGarage") == Some(true);
        let on_pit_road = frame.bool("OnPitRoad") == Some(true);
        // Out on track: in the world, off pit road. A car not in the world (the player watching,
        // on the way from the garage to the car) hasn't left the garage yet
        let in_world = frame.i32("PlayerTrackSurface") != Some(NOT_IN_WORLD);
        // Nor has a car on the grid, which leaving the garage for a race goes straight to
        let before_green = frame
            .i32("SessionState")
            .is_some_and(|state| state < STATE_RACING);
        let on_track = in_world && !on_pit_road && !in_garage;
        if in_garage {
            self.from_garage = true;
        } else if (on_track && !before_green) || (on_pit_road && self.on_track) {
            // Out racing, or into the pit lane from the track (a stop, before the green too)
            self.from_garage = false;
        }
        self.on_track = on_track;
        if info.setup == self.last_setup {
            return None;
        }
        let first = self.last_setup.is_none();
        self.last_setup = info.setup.clone();
        let setup = info.setup.as_ref()?;
        let source = if first || self.from_garage {
            Some(SetupSource::Garage)
        } else if on_pit_road {
            Some(SetupSource::PitStop)
        } else {
            None
        };
        Some(Setup {
            file_name: setup.file_name.clone(),
            modified: setup.modified,
            source,
            settings: setup
                .settings
                .iter()
                .map(|(name, value)| Setting {
                    name: name.clone(),
                    value: value.clone(),
                    ..Default::default()
                })
                .collect(),
        })
    }

    /// Each car's laps behind the leader and the car a place ahead, from how far each has gone:
    /// none before the green (whatever the laps counted on the way to it), and as they were when
    /// it finished for a car that has (none when recording began after the flag). A car that has
    /// finished has gone as far as it had when it did, not its cool-down lap; between two over
    /// the line for it (`over_line`), both just over then, it's the whole laps between them. A
    /// finished car's are final once the game's places have caught up with it and with the car
    /// ahead, finished too (a car ahead still racing can finish a lap on).
    fn laps_behind(
        &mut self,
        cars: &[Car<'_>],
        before_green: bool,
        finished: &HashSet<i64>,
        over_line: &HashSet<i64>,
        time: f64,
    ) {
        if matches!(self.flag, Some(None)) {
            return;
        }
        for car in cars {
            if let Some(track) = self.cars.get_mut(&car.idx) {
                track.finish_progress = match finished.contains(&car.idx) {
                    true => track.finish_progress.or(car.progress()),
                    false => None,
                };
            }
        }
        let gone: HashMap<i64, f64> = cars
            .iter()
            .filter_map(|car| {
                let finish = self.cars.get(&car.idx).and_then(|t| t.finish_progress);
                Some((car.idx, finish.or(car.progress())?))
            })
            .collect();
        let by_place: HashMap<i64, i64> = cars
            .iter()
            .filter_map(|car| Some((car.position?, car.idx)))
            .collect();
        // Finished, and the game's places caught up with it over the line
        let settled: HashSet<i64> = cars
            .iter()
            .map(|car| car.idx)
            .filter(|idx| {
                finished.contains(idx)
                    && self
                        .cars
                        .get(idx)
                        .is_some_and(|t| t.counted_at.is_none_or(|at| time - at > PLACES_LAG))
            })
            .collect();
        for car in cars {
            let Some(track) = self.cars.get_mut(&car.idx) else {
                continue;
            };
            if before_green {
                track.laps_behind = None;
                continue;
            }
            let done = finished.contains(&car.idx);
            if done && track.laps_final {
                continue;
            }
            let ahead = car.position.and_then(|p| by_place.get(&(p - 1)));
            track.laps_final =
                settled.contains(&car.idx) && ahead.is_none_or(|ahead| settled.contains(ahead));
            let behind = |other: Option<&i64>| {
                let other = other?;
                let laps = gone.get(other)? - gone.get(&car.idx)?;
                let laps = match over_line.contains(&car.idx) && over_line.contains(other) {
                    true => laps.round(),
                    false => (laps + 1e-9).floor(),
                };
                Some(laps.max(0.0) as i64)
            };
            let leader = match car.position {
                Some(1) => Some(0),
                _ => behind(by_place.get(&1)),
            };
            let next = behind(ahead);
            track.laps_behind = Some((leader, next));
        }
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
        behind: &HashMap<i64, f64>,
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
        s.max_laps = race_laps(frame).into();
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
        s.wind_x = self.wind.map(|(x, _)| x).into();
        s.wind_z = self.wind.map(|(_, z)| z).into();
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
                    is_player,
                    race,
                    time,
                    places,
                    behind,
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
        is_player: bool,
        race: bool,
        time: f64,
        places: &HashMap<i64, (Option<f64>, Option<f64>)>,
        behind: &HashMap<i64, f64>,
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
        out.class_position = car.class_position.into();
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
            out.time_behind_leader = behind.get(&car.idx).copied().into();
            out.time_behind_next = places.get(&car.idx).and_then(|(ahead, _)| *ahead).into();
            // See `laps_behind`
            if let Some((leader, next)) = track.and_then(|t| t.laps_behind) {
                out.laps_behind_leader = leader.into();
                out.laps_behind_next = next.into();
            }
        }

        // The flags shown to the car: the player's own, or each car's where the game has them
        let flags = match is_player {
            true => frame.bits("SessionFlags"),
            false => frame.bits_at("CarIdxSessionFlags", i),
        };
        if let Some(bits) = flags {
            out.flag = car_flag(bits).map(str::to_owned).into();
            // A local yellow: a full course one is the session's `full_course_yellow`, which the
            // spotter calls on its own
            out.under_yellow = Some(
                bits & (FLAG_YELLOW | FLAG_YELLOW_WAVING) != 0
                    && bits & (FLAG_CAUTION | FLAG_CAUTION_WAVING) == 0,
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
            // Fast repair mends all the damage: the nearest the recording has is body repair.
            // Only when it's ticked: unticked, a stop still repairs, just not at once
            out.pit_service_body_repair_selected =
                selected(SERVICE_FAST_REPAIR).filter(|on| *on).into();
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
    // Axes and rotations as the module's doc says iRacing's point: left, nose down and turning
    // left are positive there, the other way round here
    let negated = |name: &str| f(name).map(|v| -v);
    out.velocity_longitudinal = f("VelocityX").into();
    out.velocity_lateral = negated("VelocityY").into();
    out.velocity_vertical = f("VelocityZ").into();
    out.acceleration_longitudinal = f("LongAccel").into();
    out.acceleration_lateral = negated("LatAccel").into();
    out.acceleration_vertical = f("VertAccel").into();
    out.pitch = negated("Pitch").into();
    out.pitch_rate = negated("PitchRate").into();
    out.yaw_rate = negated("YawRate").into();
    // A compass heading: 0 north, turning right positive. Travel is off the nose by the slip,
    // taken in the car's own plane: on a slope or banking that's a little off the ground's (the
    // more the car slides), as roll's direction isn't known to tilt it back level
    let heading = f("YawNorth").map(|h| h.sin().atan2(h.cos()));
    out.heading = heading.into();
    out.course = heading
        .zip(f("VelocityX").zip(f("VelocityY")))
        .filter(|(_, (x, y))| x.hypot(*y) >= MIN_DIRECTION_SPEED)
        .map(|(h, (x, y))| {
            let course = h + (-y).atan2(x);
            course.sin().atan2(course.cos())
        })
        .into();
    // Half the span either way
    let half_lock = f("SteeringWheelAngleMax")
        .filter(|max| *max > 0.0)
        .map(|max| max / 2.0);
    out.steering = negated("SteeringWheelAngle")
        .zip(half_lock)
        .map(|(angle, half)| (angle / half).clamp(-1.0, 1.0))
        .into();
    out.steering_shaft_torque = negated("SteeringWheelTorque").into();
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
        // "as set in the garage"
        wheel.tire_cold_pressure = w("coldPressure").into();
        // The carcass temperatures and the tread left aren't live: they can hold still for a
        // stint on track, and when the game updates them isn't documented. They're recorded as
        // a reading, with when it last changed (see `stamp_readings`)
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

/// The values a wheel's reading is made of: its carcass temperatures and tread left, across it.
const READING: [&str; 6] = ["tempCL", "tempCM", "tempCR", "wearL", "wearM", "wearR"];

/// The session time each wheel's carcass temperatures and tread left last changed at, by wheel
/// (`last` holds each wheel's reading as last seen), the first time they're seen counting as a
/// change: the game doesn't say when it updates them. A wheel with none of them has no reading.
fn stamp_readings(
    last: &mut [Option<TyreReading>; 4],
    frame: &Frame<'_>,
    time: f64,
) -> [Option<f64>; 4] {
    let mut times = [None; 4];
    for ((last, out), prefix) in last.iter_mut().zip(&mut times).zip(WHEELS) {
        // NaN would be a change every update
        let values = READING.map(|name| {
            frame
                .f64(&format!("{prefix}{name}"))
                .filter(|v| v.is_finite())
        });
        if values.iter().all(Option::is_none) {
            *last = None;
            continue;
        }
        let reading = match *last {
            Some(reading) if reading.values == values => reading,
            _ => TyreReading { values, time },
        };
        *last = Some(reading);
        *out = Some(reading.time);
    }
    times
}

/// Which way the air moves, in m/s, in the world the player's heading points in (`heading` 0 is
/// north, along world -z, and turning right is towards -x): x west, z south. From where the wind
/// blows from (`WindDir`, see the module's doc), at its speed (`WindVel`).
fn wind(frame: &Frame<'_>) -> Option<(f64, f64)> {
    let (speed, from) = frame
        .f64("WindVel")
        .zip(frame.f64("WindDir"))
        .filter(|(speed, _)| *speed >= 0.0)?;
    // Towards the bearing opposite: east -sin, north -cos; x is west and z south
    Some((speed * from.sin(), speed * from.cos()))
}

/// The wind as recorded: in half metres a second, and only moved off what it was once it's a
/// whole step away from it. The weather's gusts, and a wind on the edge between two steps, would
/// otherwise write a session row with every update.
fn steady_wind(last: Option<(f64, f64)>, now: Option<(f64, f64)>) -> Option<(f64, f64)> {
    let (x, z) = now?;
    let half = |v: f64| (v * 2.0).round() / 2.0 + 0.0;
    match last {
        Some((lx, lz)) if (x - lx).abs() < WIND_STEP && (z - lz).abs() < WIND_STEP => last,
        _ => Some((half(x), half(z))),
    }
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

/// How many laps the session is, when it's a number of laps.
fn race_laps(frame: &Frame<'_>) -> Option<i64> {
    frame
        .i32("SessionLapsTotal")
        .filter(|laps| *laps > 0 && *laps < UNLIMITED_LAPS)
        .map(i64::from)
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

/// Each car's gaps to the cars ahead of and behind it on track, in seconds, and the car each is to:
/// how long the car behind takes to get where the one ahead is (see `follow_gap`). Cars in the pit
/// lane aren't on track.
fn track_gaps(cars: &[Car<'_>]) -> HashMap<i64, [(Option<f64>, i64); 2]> {
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
        gaps.insert(
            car.idx,
            [
                (follow_gap(car, ahead), ahead.idx),
                (follow_gap(behind, car), behind.idx),
            ],
        );
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
    let apart = lap.map(|lap| (to - from).rem_euclid(1.0) * lap);
    if same_class && let (Some(from_est), Some(to_est)) = (behind.est, ahead.est) {
        // The one ahead past the line (whether or not its estimate is yet): round the lap
        let gap = to_est - from_est;
        return match (lap, apart) {
            // Far from how far apart they are (a car beside it, the estimates' noise taking
            // the gap round the lap): how far apart they are
            (Some(lap), Some(apart)) => {
                let gap = gap.rem_euclid(lap);
                Some(match (gap - apart).abs() > lap / 4.0 {
                    true => apart,
                    false => gap,
                })
            }
            _ => Some(gap).filter(|gap| *gap >= 0.0),
        };
    }
    apart
}

/// The places as they are on track, overall and in each class: iRacing only changes its places as
/// cars cross the line, so a pass mid-lap would wait for it. `gone` gives how far a car has gone
/// in laps, and when it crossed the line for those level with it, or `None` to keep it where the
/// game has it: out of the world, before its first line, finished a while (where how far it has
/// gone no longer orders it), and before the green. Those cars keep the game's order, each just
/// behind the last car the game had ahead of it; between them all, the cars take the places the
/// game gave them, and each class's cars, in the same order, the class's.
fn live_places(cars: &mut [Car<'_>], gone: impl Fn(&Car<'_>) -> Option<(f64, f64)>) {
    let order = live_order(cars, gone);
    let mut places: Vec<i64> = order.iter().filter_map(|&i| cars[i].position).collect();
    places.sort_unstable();
    for (&i, place) in order.iter().zip(places) {
        cars[i].position = Some(place);
    }
    let classes: HashSet<Option<i64>> = cars.iter().map(|car| car.driver.class_id).collect();
    for class in classes {
        let in_class: Vec<usize> = order
            .iter()
            .copied()
            .filter(|&i| cars[i].driver.class_id == class && cars[i].class_position.is_some())
            .collect();
        let mut places: Vec<i64> = in_class
            .iter()
            .filter_map(|&i| cars[i].class_position)
            .collect();
        places.sort_unstable();
        for (&i, place) in in_class.iter().zip(places) {
            cars[i].class_position = Some(place);
        }
    }
}

/// The cars the game places, in their order on track (see `live_places`).
fn live_order(cars: &[Car<'_>], gone: impl Fn(&Car<'_>) -> Option<(f64, f64)>) -> Vec<usize> {
    let mut placed: Vec<(usize, i64)> = (0..cars.len())
        .filter_map(|i| Some((i, cars[i].position?)))
        .collect();
    placed.sort_by_key(|&(_, p)| p);
    let mut moving = Vec::new();
    let mut kept = Vec::new();
    for &(i, p) in &placed {
        match gone(&cars[i]) {
            Some(key) => moving.push((i, p, key)),
            None => kept.push((i, p)),
        }
    }
    // Furthest on first; level, the first over the line, then the furthest on past it, then in
    // the game's order
    let past = |i: usize| cars[i].progress().unwrap_or(0.0);
    moving.sort_by(|a, b| {
        (b.2.0.total_cmp(&a.2.0))
            .then(a.2.1.total_cmp(&b.2.1))
            .then(past(b.0).total_cmp(&past(a.0)))
            .then(a.1.cmp(&b.1))
    });
    // A car kept where the game has it goes once no car still to come was ahead of it there
    let mut order = Vec::with_capacity(placed.len());
    let mut kept = kept.into_iter().peekable();
    for k in 0..moving.len() {
        while let Some(&(i, p)) = kept.peek()
            && moving[k..].iter().all(|m| m.1 > p)
        {
            order.push(i);
            kept.next();
        }
        order.push(moving[k].0);
    }
    order.extend(kept.map(|(i, _)| i));
    order
}

/// In races, each car's gaps to the places ahead and behind, in seconds. Between cars `racing`
/// on track less than a lap apart, it's how long the one behind takes to get where the other is
/// (see `follow_gap`): the game's times behind the leader are only as of a timing line each car
/// last passed. Otherwise (before the green, a car that has finished, one in the pit lane or not
/// yet over the line, further apart) it's the difference in those times.
fn place_gaps(
    cars: &[Car<'_>],
    race: bool,
    racing: impl Fn(&Car<'_>) -> bool,
    finish_gaps: &HashMap<i64, f64>,
) -> HashMap<i64, (Option<f64>, Option<f64>)> {
    let mut gaps = HashMap::new();
    if !race {
        return gaps;
    }
    let by_place: HashMap<i64, &Car<'_>> = cars
        .iter()
        .filter_map(|car| Some((car.position?, car)))
        .collect();
    // Out on track and counting laps (not the -1 before its first line, whose progress isn't a
    // lap behind the cars over it)
    let racing_car = |car: &Car<'_>| {
        racing(car)
            && car.pct.is_some()
            && car.on_pit_road != Some(true)
            && car.laps_completed.is_some_and(|laps| laps >= 0)
    };
    let gap = |ahead: &Car<'_>, behind: &Car<'_>| {
        // Both finished on the winner's lap: how far apart they were over the line
        if let (Some(a), Some(b)) = (finish_gaps.get(&ahead.idx), finish_gaps.get(&behind.idx)) {
            return Some(b - a).filter(|gap| *gap >= 0.0);
        }
        let apart = ahead.progress().zip(behind.progress()).map(|(a, b)| a - b);
        if racing_car(ahead) && racing_car(behind) {
            match apart {
                Some(apart) if (0.0..1.0).contains(&apart) => {
                    if let Some(gap) = follow_gap(behind, ahead) {
                        return Some(gap);
                    }
                }
                // By it since the line (the game places cars over the line): no gap to it,
                // rather than one from before
                Some(apart) if apart < 0.0 => return None,
                _ => {}
            }
        }
        Some(behind.race_time()? - ahead.race_time()?).filter(|gap| *gap >= 0.0)
    };
    for car in cars {
        let Some(place) = car.position else {
            continue;
        };
        let ahead = by_place.get(&(place - 1)).and_then(|ahead| gap(ahead, car));
        let behind = by_place
            .get(&(place + 1))
            .and_then(|behind| gap(car, behind));
        gaps.insert(car.idx, (ahead, behind));
    }
    gaps
}

/// In races, each car's time behind the leader, live while it and the leader race on track: how
/// long it takes to get where the leader is (see `follow_gap` for the lap it's on, and its class's
/// lap time for each whole lap more), on its own, so a pass or a stop behind it doesn't move it.
/// Otherwise (before the green, finished, in the pit lane, without a class lap time a lap or more
/// back) the game's, as of a timing line it last passed: only while the car placed first is the
/// game's leader too, as the game's times are from its own (a car passed since the line), and
/// which car's each is from, as each car's is from its own last line, isn't known.
fn behind_leader(
    cars: &[Car<'_>],
    race: bool,
    racing: impl Fn(&Car<'_>) -> bool,
    finish_gaps: &HashMap<i64, f64>,
) -> HashMap<i64, f64> {
    let mut behind = HashMap::new();
    if !race {
        return behind;
    }
    let on_track = |car: &Car<'_>| {
        racing(car)
            && car.pct.is_some()
            && car.on_pit_road != Some(true)
            && car.laps_completed.is_some_and(|l| l >= 0)
    };
    let first = cars.iter().find(|c| c.position == Some(1));
    let leader = first.filter(|c| on_track(c));
    let games_leader = first.is_none_or(|c| c.leads);
    for car in cars {
        let live = || {
            let leader = leader.filter(|_| on_track(car))?;
            let laps = leader.progress()? - car.progress()?;
            // By the leader since the line (the game places cars over it): leading on the road
            if laps < 0.0 {
                return Some(0.0);
            }
            let lap = follow_gap(car, leader)?;
            let whole = laps.floor();
            match whole > 0.0 {
                false => Some(lap),
                true => Some(whole * car.driver.class_lap_time? + lap),
            }
        };
        // A finishing gap first: the game can still place first a car the winner passed on the
        // way to the line
        let time = match car.position {
            Some(place) => finish_gaps
                .get(&car.idx)
                .copied()
                .or_else(|| (place == 1).then_some(0.0))
                .or_else(live)
                .or_else(|| car.race_time().filter(|_| games_leader)),
            None => None,
        };
        if let Some(time) = time {
            behind.insert(car.idx, time);
        }
    }
    behind
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
    /// Whether `lap_start` is when the car crossed the line
    lap_start_at_line: bool,
    /// When the car last crossed the line, and its distance there, while no lap count has gone
    /// up for it yet
    line: Option<(f64, f64)>,
    /// The update that saw it cross
    line_seen: f64,
    /// When the lap count last went up, and the car's distance then, while it hasn't been seen
    /// crossing the line for it
    counted: Option<(f64, f64)>,
    sector: Option<usize>,
    /// The sector it's being timed in: the last boundary it was seen crossing (going back a
    /// little over one doesn't take it back)
    timing: Option<usize>,
    /// When the car entered the sector it's being timed in, if it was seen to
    sector_start: Option<f64>,
    /// This lap's sector times so far
    current: Vec<Option<f64>>,
    last_lap: Vec<Option<f64>>,
    /// The sectors of the lap the game has for the car's best
    best_lap: Vec<Option<f64>>,
    best_lap_total: Option<f64>,
    /// The game's best lap the sectors are looked for (or found) for, and when it was set
    best_official: Option<(f64, f64)>,
    /// The best time of each sector, of any lap
    best: Vec<Option<f64>>,
    /// The last whole laps timed here: their total, sectors and when they finished
    recent: Vec<(f64, Vec<Option<f64>>, f64)>,
    /// Laps gone, counted from when it was first seen, for its speed
    distance: f64,
    /// The furthest it's been: a boundary is crossed only going past it
    furthest: f64,
    /// When and at what distance its speed was last worked out
    anchor: Option<(f64, f64)>,
    speed: Option<f64>,
    on_pit_road: Option<bool>,
    /// Its time in the pit lane: whether it was seen entering it, and if it has stopped in its
    /// pit box yet
    visit: Option<(bool, bool)>,
    pit_stops: i64,
    pit_state: Option<&'static str>,
    /// Whether the car was seen to have push to pass (on, or some count) this session
    push_to_pass_seen: bool,
    /// Whether its last update was driven from the one before (not moved, nor out of the world)
    driven: bool,
    /// Whether it's been out of the world or moved (towed, reset) since its count last went up
    /// driven: where it is says nothing of where it is in the race (see `live_places`)
    moved: bool,
    /// The player's fuel (litres) at its `distance`, over the last lap's distance driven: since
    /// it last left the pit lane, was moved or took on fuel
    fuel_window: VecDeque<(f64, f64)>,
    /// The player's fuel at the update before, while the window's kept
    last_fuel: Option<f64>,
    /// The game's time behind the leader while the car's first lap wasn't done: a placeholder
    first_lap_f2: Option<f64>,
    /// Whether the game has given the car a time behind the leader since (see `convert`)
    f2_timed: bool,
    /// When its lap count last went up, driven (see `update`)
    counted_at: Option<f64>,
    /// The count `counted_at` is for: a finished car's is kept through counts since untimed
    counted_lap: Option<i32>,
    /// The game's last lap time at the update before
    official_last: Option<f64>,
    /// In races after the flag, how far it had gone when it finished, and whether its laps down
    /// have been worked out since (see `laps_behind`)
    finish_progress: Option<f64>,
    laps_final: bool,
    /// Finished on the winner's lap: when it was over the line
    finish_at: Option<f64>,
    /// The update `finish_at` was set or timed at its line in
    finish_seen: Option<f64>,
    /// In races, its laps behind the leader and the car a place ahead (see `laps_behind`)
    laps_behind: Option<(Option<i64>, Option<i64>)>,
}

impl CarTrack {
    /// The fuel (litres) the player's car used over the last lap's distance it drove, once it has
    /// driven a whole lap since it last left the pit lane, was moved or took on fuel. iRacing gives
    /// no figure of its own. Called after `update`, with the same update's fuel.
    fn fuel_per_lap(&mut self, fuel: Option<f64>, on_pit_road: Option<bool>) -> Option<f64> {
        let Some(fuel) = fuel.filter(|_| self.driven && on_pit_road == Some(false)) else {
            self.fuel_window.clear();
            self.last_fuel = None;
            return None;
        };
        // Fuel taken on (since the update before, standing still too): the distance before it
        // burned other fuel
        if self.last_fuel.is_some_and(|last| fuel > last + REFUELLED) {
            self.fuel_window.clear();
        }
        self.last_fuel = Some(fuel);
        let distance = self.distance;
        // Standing still adds no distance: the fuel there is the first seen at it
        if self.fuel_window.back().is_none_or(|(d, _)| distance > *d) {
            self.fuel_window.push_back((distance, fuel));
        }
        let from = distance - 1.0;
        while self.fuel_window.get(1).is_some_and(|(d, _)| *d <= from) {
            self.fuel_window.pop_front();
        }
        let &(d0, f0) = self.fuel_window.front()?;
        if d0 > from {
            return None;
        }
        // The fuel a lap back, between the samples either side of it
        let then = match self.fuel_window.get(1) {
            Some(&(d1, f1)) if d1 > d0 => f0 + (f1 - f0) * (from - d0) / (d1 - d0),
            _ => f0,
        };
        Some((then - fuel).max(0.0))
    }

    #[allow(clippy::too_many_arguments)]
    fn update(
        &mut self,
        time: f64,
        pct: Option<f64>,
        lap: Option<i32>,
        on_pit_road: Option<bool>,
        surface: Option<i32>,
        official_best: Option<f64>,
        last_lap_time: Option<f64>,
        length: f64,
        sectors: &[f64],
    ) {
        // The game's last lap time, when it's changed since the update before
        let new_lap_time = last_lap_time.filter(|t| Some(*t) != self.official_last);
        self.official_last = last_lap_time;
        let n = sectors.len();
        if self.current.len() != n {
            self.current = vec![None; n];
            self.last_lap = vec![None; n];
            self.best_lap = vec![None; n];
            self.best = vec![None; n];
            self.best_lap_total = None;
            self.best_official = None;
            self.recent.clear();
            self.sector = None;
            self.timing = None;
            self.sector_start = None;
        }

        self.best_lap_matching(official_best, time);

        let Some(pct) = pct else {
            // Out of the world (in the garage, disconnected): where it comes back says nothing
            // about how it got there, and this lap's times so far can't be completed
            self.last = None;
            self.sector = None;
            self.timing = None;
            self.sector_start = None;
            self.current = vec![None; n];
            self.line = None;
            self.counted = None;
            self.lap_start = None;
            self.lap_start_at_line = false;
            self.furthest = self.distance;
            // Counted out of the world: no crossing (see below)
            if matches!((self.lap, lap), (Some(previous), Some(lap)) if lap > previous)
                && self.finish_progress.is_none()
            {
                self.counted_at = None;
            }
            self.lap = lap.or(self.lap);
            self.anchor = None;
            self.speed = None;
            self.on_pit_road = None;
            self.visit = None;
            self.pit_state = None;
            self.driven = false;
            self.moved = true;
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
        // A count going up by more than a lap is a car moved too
        let skipped = matches!((self.lap, lap), (Some(previous), Some(lap)) if lap > previous + 1);
        let continuous = !skipped
            && step.is_some_and(|(_, _, dt, dp)| {
                dt > 0.0
                    && dt <= MAX_GAP
                    && dp * length >= -MAX_BACK
                    && dp * length <= MAX_SPEED * dt + MAX_JUMP
            });
        self.driven = continuous;
        // Moved, not driven: not a first update, nor one after the recording held up
        self.moved |=
            skipped || (!continuous && step.is_some_and(|(_, _, dt, _)| dt > 0.0 && dt <= MAX_GAP));
        let (before, furthest) = (self.distance, self.furthest);
        // How far along the update a boundary at `boundary` was crossed, and when: driven over
        // it, from just past where the car was (where it was is the update before's crossing),
        // and further than it had been (back over it and on again isn't crossing it again)
        let passed = |boundary: f64| {
            let (t0, p0, dt, dp) = step?;
            let along = (boundary - p0).rem_euclid(1.0);
            // Landing on the line is crossing it, whatever the rounding
            (continuous
                && dp > 0.0
                && along > 0.0
                && along <= dp * (1.0 + 1e-9) + 1e-12
                && before + along > furthest)
                .then(|| (along, t0 + (along / dp).min(1.0) * dt))
        };
        let after = match step {
            Some((_, _, _, dp)) if continuous => before + dp,
            _ => before,
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
        let still = continuous
            && step.is_some_and(|(_, _, dt, dp)| (dp * length).abs() <= STOPPED * dt)
            && self.speed.is_some_and(|speed| speed < STOPPED);
        let stopped_in_box = surface == Some(IN_PIT_STALL) && still;
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
        let line = passed(0.0).map(|(along, at)| (at, before + along));
        let near = |line: &(f64, f64)| after - line.1 <= COUNT_FAR;
        let counted_back = matches!((self.lap, lap), (Some(previous), Some(lap)) if lap < previous);
        let rose = matches!((self.lap, lap), (Some(previous), Some(lap)) if lap > previous);
        // Counted an update or so before the car got to the line: when it gets there, at the speed
        // it's going (cars over together go in their order)
        let mut due = None;
        // The crossing the count is for, when the count came with it or an update or so after the
        // update that saw it: the update before this one a moment after it at most (however long
        // before the car crossed, or after the update before: the recording can hold up)
        let mut timed_crossing = None;
        // Counted after a gap too long to tell driving from moving (the recording held up): the
        // crossing, from the game's time for the lap it finished, new since before the gap.
        // Only from a lap start at a crossing seen (as the game's are, but for its first lap,
        // timed from the start), and only a crossing in the gap the car could have driven to and
        // on from. The game documents only that it's the car's last lap time: that it runs between
        // the crossings seen here, and changes with the count, is checked by all this, not taken
        // as given (failing any of it, the count isn't timed)
        let recovered = match (step, self.lap_start, new_lap_time) {
            (Some((t0, p0, dt, _)), Some(start), Some(lap_time))
                if rose
                    && !skipped
                    && dt > MAX_GAP
                    && self.lap_start_at_line
                    && lap.is_some_and(|lap| lap >= 2) =>
            {
                let at = start + lap_time;
                let reach = |from: f64, to: f64, laps: f64| {
                    laps * length <= MAX_SPEED * (to - from).max(0.0) + MAX_JUMP
                };
                let at = at.min(time);
                (at > t0
                    && start + lap_time <= time + LAP_TIME_JITTER
                    && reach(t0, at, 1.0 - p0)
                    && reach(at, time, pct))
                .then_some(at)
            }
            _ => None,
        };
        if counted_back {
            // Another session
            self.lap_start = None;
            self.lap_start_at_line = false;
            self.line = None;
            self.counted = None;
            self.sector_start = None;
            self.current = vec![None; n];
        } else if !continuous {
            // Moved, not driven: a crossing or count before it says nothing about after it
            self.line = None;
            self.counted = None;
            if rose {
                self.lap_start = recovered;
                self.lap_start_at_line = recovered.is_some();
            }
        } else if rose {
            let crossed = line.or(self.line.filter(near)).map(|(at, _)| at);
            timed_crossing = line.map(|(at, _)| at).or(self
                .line
                .filter(|line| {
                    near(line) && step.is_some_and(|(t0, ..)| t0 - self.line_seen <= COUNT_LAG)
                })
                .map(|(at, _)| at));
            self.lap_start = Some(crossed.unwrap_or(time));
            self.lap_start_at_line = crossed.is_some();
            // Waiting for the line only while the car is still coming up to it: past it, its
            // crossing wasn't seen (dropped with a move), and the count's the lap it's on
            self.counted = (crossed.is_none() && pct > 0.5).then_some((time, after));
            if self.counted.is_some() {
                due = step
                    .filter(|(_, _, dt, dp)| *dt > 0.0 && *dp > 0.0)
                    .map(|(_, _, dt, dp)| time + (1.0 - pct) * dt / dp)
                    .filter(|at| at - time <= COUNT_LAG);
            }
            self.line = None;
        } else if let Some((at, at_distance)) = line {
            match self.counted.take() {
                // The lap counted before the car got to the line (an update before it, or a bit
                // before on tracks whose laps finish elsewhere than they start) starts here
                Some((count, counted)) if after - counted <= COUNT_FAR => {
                    self.lap_start = Some(at);
                    self.lap_start_at_line = true;
                    // Timed ahead of the line, a moment before it got there: timed at it now
                    if at - count <= COUNT_LAG
                        && let Some(was) = self.counted_at.filter(|was| *was >= count)
                    {
                        self.counted_at = Some(at);
                        if self.finish_at == Some(was) {
                            self.finish_at = Some(at);
                            self.finish_seen = Some(time);
                        }
                    }
                }
                _ => {
                    self.line = Some((at, at_distance));
                    self.line_seen = time;
                }
            }
        }
        // When the count went up, driven: the crossing it's for, worked out between updates (so
        // cars over the line together go in their order), when the count came with it or a moment
        // after (`timed_crossing`) or a moment before it (`due`); or else now (the count can go up at a finish line elsewhere than the start). A
        // count that came with a move (a reset, a reconnect) isn't a crossing, but for one timed
        // by the game's lap time after a long gap: none, not the lap before's (but for a car that
        // has finished, which stays over the line it was timed over)
        if rose && continuous {
            self.counted_at = Some(due.or(timed_crossing).unwrap_or(time));
            self.counted_lap = lap;
            self.moved = false;
        } else if rose {
            if recovered.is_some() {
                self.counted_lap = lap;
            }
            self.counted_at =
                recovered.or(self.counted_at.filter(|_| self.finish_progress.is_some()));
        }
        if lap.is_some() {
            self.lap = lap;
        }

        // Sector times, from boundary to boundary: every boundary crossed, in order
        let sector = sector_of(pct, sectors);
        if !continuous {
            // Moved, not driven: this lap's times so far can't be completed, and it's timed from
            // where it is
            self.sector_start = None;
            self.current = vec![None; n];
            self.timing = sector;
        } else if let Some(mut previous) = self.timing {
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
                    self.finish_lap(at);
                }
                self.sector_start = Some(at);
                previous = next;
            }
            self.timing = Some(previous);
        } else {
            self.timing = sector;
        }
        self.sector = sector;

        // Speed, over at least SPEED_WINDOW
        self.distance = after;
        if !continuous {
            self.anchor = None;
            self.speed = None;
            self.furthest = after;
        }
        self.furthest = self.furthest.max(after);
        match self.anchor {
            Some((t, d)) if time - t >= SPEED_WINDOW => {
                self.speed = Some(((self.distance - d) * length / (time - t)).max(0.0));
                self.anchor = Some((time, self.distance));
            }
            None => self.anchor = Some((time, self.distance)),
            _ => {}
        }
        self.last = Some((time, pct));
        // Again, with a lap finished just now
        self.best_lap_matching(official_best, time);
    }

    /// What to add to the lap count for the lap the car is on by its distance: while the count
    /// went up before the car crossed the line (-1), or the car crossed it before the count did
    /// (1), until the other follows, half a lap on at most.
    fn lap_offset(&self) -> i64 {
        let gone = |(_, at): (f64, f64)| self.distance - at;
        match () {
            _ if self
                .counted
                .is_some_and(|counted| gone(counted) <= COUNT_FAR) =>
            {
                -1
            }
            _ if self.line.is_some_and(|line| gone(line) <= COUNT_FAR) => 1,
            _ => 0,
        }
    }

    /// The lap just completed at `time`: its sectors are the last lap's.
    fn finish_lap(&mut self, time: f64) {
        let n = self.current.len();
        self.last_lap = std::mem::replace(&mut self.current, vec![None; n]);
        if let Some(total) = self.last_lap.iter().copied().sum::<Option<f64>>() {
            if self.recent.len() >= RECENT_LAPS {
                self.recent.remove(0);
            }
            self.recent.push((total, self.last_lap.clone(), time));
        }
    }

    /// The best lap's sectors: of the lap that set the game's best (its best lap may not count
    /// every lap, so the fastest here isn't always it), the lap timed here that finished around
    /// when the game's best changed and adds up to it. Not known when it's a lap that wasn't seen
    /// whole.
    fn best_lap_matching(&mut self, official: Option<f64>, time: f64) {
        let Some(official) = official else {
            return;
        };
        if self.best_official.is_none_or(|(best, _)| best != official) {
            self.best_official = Some((official, time));
            self.best_lap_total = None;
            self.best_lap = vec![None; self.current.len()];
        }
        let Some((_, set_at)) = self.best_official else {
            return;
        };
        // Found already. A lap finishing long after the best changed isn't it (see below)
        if self.best_lap_total.is_some() {
            return;
        }
        let off = |total: f64| (total - official).abs();
        if let Some((total, sectors, _)) = self
            .recent
            .iter()
            .filter(|(total, _, finished)| {
                off(*total) <= BEST_LAP_MATCH && (finished - set_at).abs() <= BEST_LAP_WINDOW
            })
            .min_by(|a, b| off(a.0).total_cmp(&off(b.0)))
        {
            self.best_lap_total = Some(*total);
            self.best_lap = sectors.clone();
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
        track.best_lap_matching(Some(20.01), time);
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
        // A later crossing without a count isn't this lap's
        let (mut time, mut pct) = (11.0, 0.02);
        while pct < 1.0 {
            time += 0.5;
            pct += 0.04;
            update(&mut track, time, Some(pct % 1.0), 2, &SECTORS);
        }
        assert!((track.lap_start.unwrap() - 10.75).abs() < 1e-9);
        assert!(track.line.is_some());

        // Counted past the line, its crossing not seen: the count's the lap it's on
        let mut track = CarTrack::default();
        update(&mut track, 10.0, Some(0.02), 1, &SECTORS);
        update(&mut track, 10.5, Some(0.04), 2, &SECTORS);
        assert_eq!(track.lap_start, Some(10.5));
        assert_eq!(track.lap_offset(), 0);
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
    fn gaps_go_round_the_lap_only_when_the_cars_are_apart() {
        let driver = super::super::session::Driver {
            class_id: Some(1),
            class_lap_time: Some(90.0),
            ..Default::default()
        };
        let car = |pct: f64, est: f64| Car {
            idx: 0,
            driver: &driver,
            pct: Some(pct),
            on_pit_road: Some(false),
            laps_completed: Some(1),
            lap_offset: 0,
            position: None,
            class_position: None,
            leads: false,
            est: Some(est),
            f2: None,
        };
        // Side by side, the estimates a hair the wrong way round
        let gap = follow_gap(&car(0.5, 45.0), &car(0.5001, 44.99)).unwrap();
        assert!(gap < 0.1, "{gap}");
        // Across the line, the estimate goes round the lap
        let gap = follow_gap(&car(0.99, 89.1), &car(0.01, 0.9)).unwrap();
        assert!((gap - 1.8).abs() < 1e-9, "{gap}");
        assert!((follow_gap(&car(0.2, 18.0), &car(0.3, 27.5)).unwrap() - 9.5).abs() < 1e-9);
    }

    /// Once racing, the gap to the place ahead is live between cars less than a lap apart, not the
    /// game's times behind the leader (as of each car's last time over the line); a lap or more
    /// apart, or before the green, it's those times.
    /// The time behind the leader is each car's own live time to get where the leader is: across
    /// classes at the follower's lap time, so a pass between classes doesn't move it; a lap or
    /// more back, whole laps at its class's lap time; in the pit lane, the game's.
    #[test]
    fn the_time_behind_the_leader_is_each_cars_own() {
        let fast = super::super::session::Driver {
            class_id: Some(1),
            class_lap_time: Some(100.0),
            ..Default::default()
        };
        let slow = super::super::session::Driver {
            class_id: Some(2),
            class_lap_time: Some(200.0),
            ..Default::default()
        };
        let car = |idx: i64, driver, position: i64, laps: i64, pct: f64, f2: f64| Car {
            idx,
            driver,
            pct: Some(pct),
            on_pit_road: Some(false),
            laps_completed: Some(laps),
            lap_offset: 0,
            position: Some(position),
            class_position: Some(position),
            leads: position == 1,
            est: Some(pct * 100.0),
            f2: Some(f2),
        };
        let close = |got: Option<&f64>, want: f64| {
            assert!(got.is_some_and(|got| (got - want).abs() < 1e-6), "{got:?}");
        };
        // The leader (fast) at 80%, a slow car at 60%, a fast car just behind it at 59%, and a
        // fast car a lap down at 70%
        let cars = [
            car(0, &fast, 1, 5, 0.8, 0.0),
            car(1, &slow, 2, 5, 0.6, 30.0),
            car(2, &fast, 3, 5, 0.59, 31.0),
            car(3, &fast, 4, 4, 0.7, 120.0),
        ];
        let behind = behind_leader(&cars, true, |_| true, &HashMap::new());
        close(behind.get(&0), 0.0);
        close(behind.get(&1), 40.0);
        close(behind.get(&2), 21.0);
        close(behind.get(&3), 110.0);
        // The fast car by the slow one: its time moves on as it does, not by the slow car's
        let mut passed = cars.clone();
        passed[1] = car(1, &slow, 3, 5, 0.6, 30.0);
        passed[2] = car(2, &fast, 2, 5, 0.61, 31.0);
        let behind = behind_leader(&passed, true, |_| true, &HashMap::new());
        close(behind.get(&2), 19.0);
        close(behind.get(&1), 40.0);
        // In the pit lane, the game's; not racing (finished, before the green), the game's
        let mut pitted = cars.clone();
        pitted[3].on_pit_road = Some(true);
        close(
            behind_leader(&pitted, true, |_| true, &HashMap::new()).get(&3),
            120.0,
        );
        close(
            behind_leader(&cars, true, |_| false, &HashMap::new()).get(&2),
            31.0,
        );
        assert!(behind_leader(&cars, false, |_| true, &HashMap::new()).is_empty());
    }

    #[test]
    fn place_gaps_are_live_within_a_lap() {
        let driver = super::super::session::Driver {
            class_id: Some(1),
            class_lap_time: Some(90.0),
            ..Default::default()
        };
        let car = |idx: i64, position: i64, laps: i64, pct: f64, est: f64, f2: f64| Car {
            idx,
            driver: &driver,
            pct: Some(pct),
            on_pit_road: Some(false),
            laps_completed: Some(laps),
            lap_offset: 0,
            position: Some(position),
            class_position: Some(position),
            leads: position == 1,
            est: Some(est),
            f2: Some(f2),
        };
        // The leader at 60% of lap 3, the next car at 50% of it (9 s back by the estimates, 2 s at
        // the line), the third over a lap down (91 s back at the line)
        let cars = [
            car(0, 1, 3, 0.6, 54.0, 0.0),
            car(1, 2, 3, 0.5, 45.0, 2.0),
            car(2, 3, 2, 0.45, 40.5, 91.0),
        ];
        let gaps = place_gaps(&cars, true, |_| true, &HashMap::new());
        let (ahead, behind) = gaps[&1];
        assert!(
            ahead.is_some_and(|gap| (gap - 9.0).abs() < 1e-9),
            "{ahead:?}"
        );
        assert!(
            behind.is_some_and(|gap| (gap - 89.0).abs() < 1e-9),
            "{behind:?}"
        );
        assert_eq!(gaps[&0].0, None);
        // Not racing (before the green, or finished): the game's times
        assert_eq!(
            place_gaps(&cars, true, |_| false, &HashMap::new())[&1].0,
            Some(2.0)
        );
        // Either car in the pit lane, or not yet counting laps: the game's times too
        let mut pitted = cars.clone();
        pitted[0].on_pit_road = Some(true);
        assert_eq!(
            place_gaps(&pitted, true, |_| true, &HashMap::new())[&1].0,
            Some(2.0)
        );
        let mut starting = cars.clone();
        starting[1].laps_completed = Some(-1);
        assert_eq!(
            place_gaps(&starting, true, |_| true, &HashMap::new())[&1].0,
            None
        );
        // Not a race: none
        assert!(place_gaps(&cars, false, |_| true, &HashMap::new()).is_empty());
    }

    /// A pass mid-lap changes the places at once, overall and in the class, though the game's
    /// wait for the line; a car out of the world stays behind those the game had ahead of it, and
    /// before the green the places are the game's.
    #[test]
    fn places_change_as_cars_pass() {
        use super::super::sdk::{Value, Vars};
        use super::super::session::{Driver, Session};
        let class = |id: i64| Driver {
            class_id: Some(id),
            class_lap_time: Some(90.0),
            ..Default::default()
        };
        // Cars 0 and 1 in one class, car 2 another, car 3 out of the world
        let info = SessionInfo {
            track_length: Some(1000.0),
            player: Some(0),
            drivers: [(0, class(1)), (1, class(1)), (2, class(2)), (3, class(1))]
                .into_iter()
                .collect(),
            sessions: vec![Session {
                num: 0,
                kind: Some("race"),
                number: 1,
                results: HashMap::new(),
            }],
            ..Default::default()
        };
        let start = Instant::now();
        let mut converter = Converter::default();
        // The game's places as of the line: car 2, car 0, car 3, car 1
        let mut frame = |t: f64, state: i32, pct: [f64; 4]| {
            let (vars, data) = Vars::of(&[
                ("SessionTime", Value::Double(t)),
                ("SessionNum", Value::Int(0)),
                ("SessionState", Value::Int(state)),
                ("PlayerCarIdx", Value::Int(0)),
                ("CarIdxLapDistPct", Value::Doubles(pct.to_vec())),
                ("CarIdxTrackSurface", Value::Ints(vec![3, 3, 3, -1])),
                ("CarIdxLapCompleted", Value::Ints(vec![3, 3, 3, 3])),
                ("CarIdxPosition", Value::Ints(vec![2, 4, 1, 3])),
                ("CarIdxClassPosition", Value::Ints(vec![1, 3, 1, 2])),
            ]);
            converter
                .convert(
                    &Frame::new(&vars, &data),
                    &info,
                    start + std::time::Duration::from_secs_f64(t),
                )
                .unwrap()
        };
        let places = |update: &Update| {
            let (_, standings) = update.scoring.as_ref().unwrap();
            (0..4)
                .map(|car| {
                    let row = standings
                        .iter()
                        .find(|s| s.vehicle_id.get() == Some(car))
                        .unwrap();
                    (row.position.get(), row.class_position.get())
                })
                .collect::<Vec<_>>()
        };
        // Car 1 by car 0, and car 2, since the line: by car 3 too, which car 0 stays ahead of
        let passed = frame(1.0, STATE_RACING, [0.5, 0.6, 0.55, 0.0]);
        assert_eq!(
            places(&passed),
            [
                (Some(3), Some(2)),
                (Some(1), Some(1)),
                (Some(2), Some(1)),
                (Some(4), Some(3)),
            ]
        );
        // Before the green: the game's
        let gridded = frame(2.0, STATE_PARADE_LAPS, [0.5, 0.6, 0.55, 0.0]);
        assert_eq!(
            places(&gridded),
            [
                (Some(2), Some(1)),
                (Some(4), Some(3)),
                (Some(1), Some(1)),
                (Some(3), Some(2)),
            ]
        );
    }

    /// The game's times behind the leader are from its own leader: none of them while the car
    /// placed first has passed it since the line, as which leader each is from isn't known.
    #[test]
    fn times_behind_the_leader_wait_for_the_games_leader() {
        let driver = super::super::session::Driver {
            class_id: Some(1),
            class_lap_time: Some(90.0),
            ..Default::default()
        };
        let car = |idx: i64, position: i64, leads: bool, pct: f64, pit: bool, f2: f64| Car {
            idx,
            driver: &driver,
            pct: Some(pct),
            on_pit_road: Some(pit),
            laps_completed: Some(5),
            lap_offset: 0,
            position: Some(position),
            class_position: Some(position),
            leads,
            est: Some(pct * 90.0),
            f2: Some(f2),
        };
        // The game's leader (8 s ahead of car 1 at the line) in the pit lane, passed by car 1
        // since; car 2 in the pit lane too, 12 s behind the game's leader
        let cars = [
            car(0, 2, true, 0.4, true, 0.0),
            car(1, 1, false, 0.5, false, 8.0),
            car(2, 3, false, 0.3, true, 12.0),
        ];
        let behind = behind_leader(&cars, true, |_| true, &HashMap::new());
        assert_eq!(behind.get(&1), Some(&0.0));
        assert_eq!(behind.get(&2), None);
        assert_eq!(behind.get(&0), None);
        // Car 1 over the line and the game's leader: the game's times again
        let mut over = cars.clone();
        over[0].leads = false;
        over[1].leads = true;
        let behind = behind_leader(&over, true, |_| true, &HashMap::new());
        assert_eq!(behind.get(&2), Some(&12.0));
    }

    /// Through a race's end: the first lap's placeholder isn't a time until the game changes it,
    /// gaps are live between cars still racing, and a car that has finished keeps its finishing
    /// gaps and laps down through the cool-down.
    #[test]
    fn a_race_ends_with_its_finishing_gaps() {
        use super::super::sdk::{Value, Vars};
        use super::super::session::{Driver, Session};
        let driver = Driver {
            class_id: Some(1),
            class_lap_time: Some(90.0),
            ..Default::default()
        };
        let info = SessionInfo {
            track_length: Some(1000.0),
            player: Some(0),
            drivers: (0..3).map(|car| (car, driver.clone())).collect(),
            sessions: vec![Session {
                num: 0,
                kind: Some("race"),
                number: 1,
                results: HashMap::new(),
            }],
            ..Default::default()
        };
        let start = Instant::now();
        let mut converter = Converter::default();
        // Car 2 leads car 0, then car 1; every frame a second on, with the scoring
        let mut frame = |t: f64, state: i32, pct: [f64; 3], laps: [i32; 3], f2: [f64; 3]| {
            let (vars, data) = Vars::of(&[
                ("SessionTime", Value::Double(t)),
                ("SessionNum", Value::Int(0)),
                ("SessionState", Value::Int(state)),
                ("PlayerCarIdx", Value::Int(0)),
                ("CarIdxLapDistPct", Value::Doubles(pct.to_vec())),
                ("CarIdxLapCompleted", Value::Ints(laps.to_vec())),
                ("CarIdxPosition", Value::Ints(vec![2, 3, 1])),
                ("CarIdxClassPosition", Value::Ints(vec![2, 3, 1])),
                (
                    "CarIdxEstTime",
                    Value::Doubles(pct.iter().map(|p| p * 90.0).collect()),
                ),
                ("CarIdxF2Time", Value::Doubles(f2.to_vec())),
            ]);
            converter
                .convert(
                    &Frame::new(&vars, &data),
                    &info,
                    start + std::time::Duration::from_secs_f64(t),
                )
                .unwrap()
        };
        let gaps = |update: &Update, car: i64| {
            let row = update
                .telemetry
                .iter()
                .find(|t| t.vehicle_id.get() == Some(car))
                .unwrap();
            (row.gap_to_place_ahead.get(), row.gap_to_place_behind.get())
        };
        let standing = |update: &Update, car: i64| {
            let (_, standings) = update.scoring.as_ref().unwrap();
            let row = standings
                .iter()
                .find(|s| s.vehicle_id.get() == Some(car))
                .unwrap();
            (row.time_behind_leader.get(), row.laps_behind_leader.get())
        };
        let close = |got: Option<f64>, want: f64| {
            assert!(got.is_some_and(|got| (got - want).abs() < 1e-6), "{got:?}");
        };

        // The first lap, live from the start: the time behind the leader adds up the gaps
        let first = frame(
            1.0,
            STATE_RACING,
            [0.95, 0.94, 0.96],
            [0, 0, 0],
            [0.001, 0.002, 0.0],
        );
        close(standing(&first, 0).0, 0.9);
        close(standing(&first, 1).0, 1.8);
        assert_eq!(standing(&first, 2).0, Some(0.0));
        // Racing: live, not the game's times as of the line
        let racing = frame(
            4.0,
            STATE_RACING,
            [0.8, 0.75, 0.9],
            [3, 3, 3],
            [2.0, 5.0, 0.0],
        );
        close(gaps(&racing, 0).0, 9.0);
        close(gaps(&racing, 0).1, 4.5);
        // On track too, with the car each gap is to
        let player = racing
            .telemetry
            .iter()
            .find(|t| t.vehicle_id.get() == Some(0))
            .unwrap();
        assert_eq!(
            (player.car_ahead_id.get(), player.car_behind_id.get()),
            (Some(2), Some(1))
        );
        close(player.gap_to_car_ahead.get(), 9.0);
        // The leader takes the flag: the gap to it is its finishing one, the others still race
        let flag = frame(
            5.0,
            STATE_CHECKERED,
            [0.95, 0.9, 0.01],
            [3, 3, 4],
            [2.0, 5.0, 0.0],
        );
        close(gaps(&flag, 0).0, 2.0);
        close(gaps(&flag, 0).1, 4.5);
        // Car 0 finishes: car 1's gap to it is its finishing one too; and the leader nearly round
        // its cool-down lap leaves car 1 on the lead lap
        let done = frame(
            6.0,
            STATE_CHECKERED,
            [0.01, 0.95, 0.97],
            [4, 3, 4],
            [2.0, 5.0, 0.0],
        );
        close(gaps(&done, 1).0, 3.0);
        assert_eq!(standing(&done, 1).1, Some(0));
        // The cool-down: the leader drives on round, and nobody's laps down change (the leader
        // over the line again, a moment after the winner was taken, is still the winner at its
        // finish)
        let cool = frame(
            7.0,
            STATE_COOL_DOWN,
            [0.05, 0.02, 0.1],
            [4, 4, 5],
            [2.0, 5.0, 0.0],
        );
        assert_eq!(standing(&cool, 1).1, Some(0));
        assert_eq!(standing(&cool, 0).1, Some(0));
        // Both finished on the winner's lap: how far apart they were over the line, exactly
        // (car 0 over it at 5.83 s, car 1 at 6.71 s), not the game's times
        close(gaps(&cool, 1).0, (6.0 + 0.05 / 0.07) - (5.0 + 0.05 / 0.06));
        // Behind the winner (over at 4.91 s), still car 2
        close(
            standing(&cool, 0).0,
            (5.0 + 0.05 / 0.06) - (4.0 + 0.1 / 0.11),
        );
    }

    /// A count after a gap too long to tell driving from moving is timed at its crossing from the
    /// game's lap time, when that's new, fits the gap and the car could have driven it, and the
    /// lap it times isn't the first; otherwise it isn't a crossing (nor the one before).
    #[test]
    fn a_count_after_a_long_gap_is_timed_by_the_games_lap() {
        // Driven at 50 m/s (a lap in 20 s), over the line at 10.4 s, then nothing from 28 s (at
        // 0.88) to 36 s (at 0.28, its count up), over the line at 30.4 s by its speed
        let run = |first: i32, before: f64, after: f64| {
            let mut track = CarTrack::default();
            let mut step = |time: f64, pct: f64, lap: i32, lap_time: f64| {
                track.update(
                    time,
                    Some(pct),
                    Some(lap),
                    Some(false),
                    Some(3),
                    None,
                    Some(lap_time),
                    LENGTH,
                    &SECTORS,
                );
            };
            step(10.0, 0.98, first, before);
            let mut t = 10.5;
            while t <= 28.0 {
                step(t, 0.005 + 0.05 * (t - 10.5), first + 1, before);
                t += 0.5;
            }
            step(36.0, 0.28, first + 2, after);
            track
        };
        let close = |got: Option<f64>, want: f64| {
            assert!(got.is_some_and(|got| (got - want).abs() < 1e-6), "{got:?}");
        };
        let track = run(1, 21.0, 20.0);
        close(track.counted_at, 30.4);
        close(track.lap_start, 30.4);
        // The game's time not new yet, its first lap (timed from the start), a crossing before
        // the gap, and one too soon for the car to have got to the line
        for (first, before, after) in [
            (1, 20.0, 20.0),
            (-1, 21.0, 20.0),
            (1, 21.0, 12.0),
            (1, 21.0, 17.7),
        ] {
            let track = run(first, before, after);
            assert_eq!(track.counted_at, None);
            assert_eq!(track.lap_start, None);
        }

        // Nor from a lap start that wasn't a crossing seen (the recording began past the line)
        let mut track = CarTrack::default();
        let mut step = |time: f64, pct: Option<f64>, lap: i32, lap_time: f64| {
            track.update(
                time,
                pct,
                Some(lap),
                Some(false),
                Some(3),
                None,
                Some(lap_time),
                LENGTH,
                &SECTORS,
            );
        };
        step(10.0, Some(0.01), 1, 21.0);
        let mut t = 10.5;
        while t <= 28.0 {
            step(t, Some(0.035 + 0.05 * (t - 10.5)), 2, 21.0);
            t += 0.5;
        }
        step(36.0, Some(0.31), 3, 20.0);
        assert_eq!(track.counted_at, None);
        assert_eq!(track.lap_start, None);
    }

    /// A count that goes up out of the world (its car not there to cross the line) isn't timed,
    /// nor left at the crossing before; a car that has finished keeps the one it finished over.
    #[test]
    fn a_count_out_of_the_world_isnt_a_crossing() {
        for finished in [false, true] {
            let mut track = CarTrack::default();
            if finished {
                track.finish_progress = Some(2.0);
            }
            update(&mut track, 10.0, Some(0.98), 1, &SECTORS);
            update(&mut track, 10.5, Some(0.005), 2, &SECTORS);
            let crossed = track.counted_at;
            assert!(
                crossed.is_some_and(|at| (at - 10.4).abs() < 1e-6),
                "{crossed:?}"
            );
            update(&mut track, 11.0, None, 3, &SECTORS);
            assert_eq!(track.counted_at, crossed.filter(|_| finished));
        }
    }

    /// Two cars over the line in one update are counted in the order they crossed: a car a lap
    /// down just ahead of the leader isn't over it after the leader's finish.
    #[test]
    fn counts_go_up_in_the_order_cars_crossed() {
        let (mut lapped, mut leader) = (CarTrack::default(), CarTrack::default());
        let sectors = [0.0, 0.5];
        let drive = |track: &mut CarTrack, t: f64, pct: f64, lap: i32| {
            track.update(
                t,
                Some(pct),
                Some(lap),
                Some(false),
                None,
                None,
                None,
                1000.0,
                &sectors,
            );
        };
        drive(&mut lapped, 1.0, 0.97, 2);
        drive(&mut leader, 1.0, 0.95, 3);
        drive(&mut lapped, 2.0, 0.06, 3);
        drive(&mut leader, 2.0, 0.02, 4);
        let (lapped, leader) = (lapped.counted_at.unwrap(), leader.counted_at.unwrap());
        assert!(lapped < leader, "{lapped} {leader}");
        assert!((lapped - (1.0 + 0.03 / 0.09)).abs() < 1e-6, "{lapped}");
    }

    /// Where a car's time behind the leader is iRacing's (here, in the pit lane, so not racing
    /// live), the first lap's placeholder isn't a time: held a moment past the line, nor when it
    /// moves with the place; once the car is timed, any time is one.
    #[test]
    fn the_first_laps_placeholder_isnt_a_time() {
        use super::super::sdk::{Value, Vars};
        use super::super::session::{Driver, Session};
        let driver = Driver {
            class_id: Some(1),
            class_lap_time: Some(90.0),
            ..Default::default()
        };
        let info = SessionInfo {
            track_length: Some(1000.0),
            player: Some(0),
            drivers: (0..3).map(|car| (car, driver.clone())).collect(),
            sessions: vec![Session {
                num: 0,
                kind: Some("race"),
                number: 1,
                results: HashMap::new(),
            }],
            ..Default::default()
        };
        let start = Instant::now();
        let mut converter = Converter::default();
        let mut behind = |t: f64, pct: [f64; 3], laps: i32, f2: [f64; 3]| {
            let (vars, data) = Vars::of(&[
                ("SessionTime", Value::Double(t)),
                ("SessionNum", Value::Int(0)),
                ("SessionState", Value::Int(STATE_RACING)),
                ("PlayerCarIdx", Value::Int(0)),
                ("CarIdxLapDistPct", Value::Doubles(pct.to_vec())),
                ("CarIdxLapCompleted", Value::Ints(vec![laps; 3])),
                ("CarIdxOnPitRoad", Value::Bools(vec![true, false, false])),
                ("CarIdxPosition", Value::Ints(vec![2, 3, 1])),
                ("CarIdxClassPosition", Value::Ints(vec![2, 3, 1])),
                (
                    "CarIdxEstTime",
                    Value::Doubles(pct.iter().map(|p| p * 90.0).collect()),
                ),
                ("CarIdxF2Time", Value::Doubles(f2.to_vec())),
            ]);
            let update = converter
                .convert(
                    &Frame::new(&vars, &data),
                    &info,
                    start + std::time::Duration::from_secs_f64(t),
                )
                .unwrap();
            let (_, standings) = update.scoring.as_ref().unwrap();
            standings
                .iter()
                .find(|s| s.vehicle_id.get() == Some(0))
                .unwrap()
                .time_behind_leader
                .get()
        };
        let placeholder = [0.001, 0.002, 0.0];
        assert_eq!(behind(1.0, [0.95, 0.94, 0.96], 0, placeholder), None);
        assert_eq!(behind(2.0, [0.02, 0.01, 0.03], 1, placeholder), None);
        assert_eq!(
            behind(2.5, [0.03, 0.02, 0.04], 1, [0.003, 0.002, 0.0]),
            None
        );
        assert_eq!(
            behind(3.0, [0.04, 0.03, 0.05], 1, [2.0, 5.0, 0.0]),
            Some(2.0)
        );
        assert_eq!(
            behind(3.5, [0.05, 0.04, 0.06], 1, [0.001, 5.0, 0.0]),
            Some(0.001)
        );
        // On a short lap, the placeholder past the next line isn't a time either
        let mut converter = Converter::default();
        let mut behind = |t: f64, laps: i32, f2: [f64; 3]| {
            // Short of half a lap, where a count going up is for the line just crossed
            let pct = [0.3, 0.2, 0.4];
            let (vars, data) = Vars::of(&[
                ("SessionTime", Value::Double(t)),
                ("SessionNum", Value::Int(0)),
                ("SessionState", Value::Int(STATE_RACING)),
                ("PlayerCarIdx", Value::Int(0)),
                ("CarIdxLapDistPct", Value::Doubles(pct.to_vec())),
                ("CarIdxLapCompleted", Value::Ints(vec![laps; 3])),
                ("CarIdxOnPitRoad", Value::Bools(vec![true, false, false])),
                ("CarIdxPosition", Value::Ints(vec![2, 3, 1])),
                ("CarIdxF2Time", Value::Doubles(f2.to_vec())),
            ]);
            let update = converter
                .convert(
                    &Frame::new(&vars, &data),
                    &info,
                    start + std::time::Duration::from_secs_f64(t),
                )
                .unwrap();
            let (_, standings) = update.scoring.as_ref().unwrap();
            standings
                .iter()
                .find(|s| s.vehicle_id.get() == Some(0))
                .unwrap()
                .time_behind_leader
                .get()
        };
        assert_eq!(behind(1.0, 0, [0.001, 0.002, 0.0]), None);
        assert_eq!(behind(2.0, 1, [0.001, 0.002, 0.0]), None);
        assert_eq!(behind(3.0, 2, [0.002, 0.003, 0.0]), None);
        assert_eq!(behind(4.0, 2, [1.5, 0.003, 0.0]), Some(1.5));
    }

    /// The winner is the first car over the line with more laps than any had at the flag: not the
    /// car first when the flag came (passed for the lead before the line), and with no car placed
    /// first then. A car finished over the line after it has a finishing gap, not a live one.
    #[test]
    fn the_winner_is_the_first_over_the_line_with_the_most_laps() {
        use super::super::sdk::{Value, Vars};
        use super::super::session::{Driver, Session};
        let driver = Driver {
            class_id: Some(1),
            class_lap_time: Some(90.0),
            ..Default::default()
        };
        let info = SessionInfo {
            track_length: Some(1000.0),
            player: Some(0),
            drivers: (0..3).map(|car| (car, driver.clone())).collect(),
            sessions: vec![Session {
                num: 0,
                kind: Some("race"),
                number: 1,
                results: HashMap::new(),
            }],
            ..Default::default()
        };
        // A race's last moments, one update every half second: the gap to the place ahead of `car`
        // in the last one
        let gap_ahead = |updates: &[(f64, i32, [f64; 3], [i32; 3], [i32; 3])], car: i64| {
            let start = Instant::now();
            let mut converter = Converter::default();
            let mut last = None;
            for (t, state, pct, laps, places) in updates {
                let (vars, data) = Vars::of(&[
                    ("SessionTime", Value::Double(*t)),
                    ("SessionNum", Value::Int(0)),
                    ("SessionState", Value::Int(*state)),
                    ("PlayerCarIdx", Value::Int(0)),
                    ("CarIdxLapDistPct", Value::Doubles(pct.to_vec())),
                    ("CarIdxLapCompleted", Value::Ints(laps.to_vec())),
                    ("CarIdxPosition", Value::Ints(places.to_vec())),
                    ("CarIdxClassPosition", Value::Ints(places.to_vec())),
                    (
                        "CarIdxEstTime",
                        Value::Doubles(pct.iter().map(|p| p * 90.0).collect()),
                    ),
                    ("CarIdxF2Time", Value::Doubles(vec![0.7, 0.7, 5.0])),
                ]);
                last = converter.convert(
                    &Frame::new(&vars, &data),
                    &info,
                    start + std::time::Duration::from_secs_f64(*t),
                );
            }
            last.unwrap()
                .telemetry
                .iter()
                .find(|t| t.vehicle_id.get() == Some(car))
                .unwrap()
                .gap_to_place_ahead
                .get()
        };
        // Car 1 passes car 0 for the lead after the flag and is over the line first: it has won,
        // so car 0, over the line after it (the game's places still as they were), is placed
        // behind it with how long after it it crossed
        let passed = gap_ahead(
            &[
                (1.0, STATE_RACING, [0.9, 0.89, 0.5], [2, 2, 2], [1, 2, 3]),
                (
                    1.5,
                    STATE_CHECKERED,
                    [0.94, 0.935, 0.54],
                    [2, 2, 2],
                    [1, 2, 3],
                ),
                (
                    2.0,
                    STATE_CHECKERED,
                    [0.97, 0.01, 0.58],
                    [2, 3, 2],
                    [1, 2, 3],
                ),
                (
                    2.5,
                    STATE_CHECKERED,
                    [0.01, 0.05, 0.62],
                    [3, 3, 2],
                    [1, 2, 3],
                ),
            ],
            0,
        );
        let finishing = (2.0 + 0.03 / 0.04 * 0.5) - (1.5 + 0.065 / 0.075 * 0.5);
        assert!(
            passed.is_some_and(|gap| (gap - finishing).abs() < 1e-6),
            "{passed:?}"
        );
        // No car placed first when the flag comes: car 0 over the line first has won, so car 1's
        // gap to it is a finishing one
        let unplaced = gap_ahead(
            &[
                (1.0, STATE_RACING, [0.9, 0.85, 0.5], [2, 2, 2], [1, 2, 3]),
                (
                    1.5,
                    STATE_CHECKERED,
                    [0.94, 0.89, 0.54],
                    [2, 2, 2],
                    [0, 0, 0],
                ),
                (
                    2.0,
                    STATE_CHECKERED,
                    [0.02, 0.93, 0.58],
                    [3, 2, 2],
                    [1, 2, 3],
                ),
            ],
            1,
        );
        assert!(
            unplaced.is_some_and(|gap| (gap - 0.7).abs() < 1e-6),
            "{unplaced:?}"
        );
    }

    /// A car most of a lap behind on the lead lap, over the line a moment before the (early) flag,
    /// isn't taken for the winner; and a car finished a lap down, reaching the winner's laps on its
    /// cool-down lap, has no finishing gap from it.
    #[test]
    fn only_the_winners_lap_finishes_with_it() {
        use super::super::sdk::{Value, Vars};
        use super::super::session::{Driver, Session};
        let driver = Driver {
            class_id: Some(1),
            class_lap_time: Some(90.0),
            ..Default::default()
        };
        let info = SessionInfo {
            track_length: Some(1000.0),
            player: Some(0),
            drivers: (0..3).map(|car| (car, driver.clone())).collect(),
            sessions: vec![Session {
                num: 0,
                kind: Some("race"),
                number: 1,
                results: HashMap::new(),
            }],
            ..Default::default()
        };
        // Updates through a race's end, and the last one's
        let run = |updates: &[(f64, i32, [f64; 3], [i32; 3])], f2: [f64; 3]| {
            let start = Instant::now();
            let mut converter = Converter::default();
            let mut last = None;
            for (t, state, pct, laps) in updates {
                let (vars, data) = Vars::of(&[
                    ("SessionTime", Value::Double(*t)),
                    ("SessionNum", Value::Int(0)),
                    ("SessionState", Value::Int(*state)),
                    ("PlayerCarIdx", Value::Int(0)),
                    ("CarIdxLapDistPct", Value::Doubles(pct.to_vec())),
                    ("CarIdxLapCompleted", Value::Ints(laps.to_vec())),
                    ("CarIdxPosition", Value::Ints(vec![1, 2, 3])),
                    ("CarIdxClassPosition", Value::Ints(vec![1, 2, 3])),
                    (
                        "CarIdxEstTime",
                        Value::Doubles(pct.iter().map(|p| p * 90.0).collect()),
                    ),
                    ("CarIdxF2Time", Value::Doubles(f2.to_vec())),
                ]);
                last = converter.convert(
                    &Frame::new(&vars, &data),
                    &info,
                    start + std::time::Duration::from_secs_f64(*t),
                );
            }
            last.unwrap()
        };
        let close = |got: Option<f64>, want: f64| {
            assert!(got.is_some_and(|got| (got - want).abs() < 1e-6), "{got:?}");
        };

        // Car 1, most of a lap behind car 0 on its lap, over the line just before the flag: still
        // racing, its gap to car 0 live (0.92 of a lap), not a finishing one
        let early = run(
            &[
                (1.0, STATE_RACING, [0.9, 0.97, 0.5], [2, 1, 1]),
                (1.5, STATE_RACING, [0.94, 0.02, 0.54], [2, 2, 1]),
                (2.0, STATE_CHECKERED, [0.98, 0.06, 0.58], [2, 2, 1]),
            ],
            [0.0, 0.7, 5.0],
        );
        let gap = early
            .telemetry
            .iter()
            .find(|t| t.vehicle_id.get() == Some(1))
            .unwrap()
            .gap_to_place_ahead
            .get();
        close(gap, 0.92 * 90.0);

        // Car 2 a lap down: over the line after the winner, then round its cool-down lap to the
        // winner's laps. Its time behind the leader is the game's, not one from that crossing
        let lapped = run(
            &[
                (1.0, STATE_RACING, [0.9, 0.85, 0.95], [2, 2, 1]),
                (1.5, STATE_CHECKERED, [0.94, 0.89, 0.99], [2, 2, 1]),
                (2.0, STATE_CHECKERED, [0.02, 0.93, 0.995], [3, 2, 1]),
                (2.5, STATE_CHECKERED, [0.06, 0.97, 0.04], [3, 2, 2]),
                (3.0, STATE_CHECKERED, [0.1, 0.02, 0.08], [3, 3, 2]),
                (7.0, STATE_CHECKERED, [0.55, 0.47, 0.53], [3, 3, 2]),
                (11.0, STATE_CHECKERED, [0.95, 0.9, 0.97], [3, 3, 2]),
                (11.5, STATE_CHECKERED, [0.99, 0.94, 0.02], [3, 3, 3]),
            ],
            [0.0, 0.7, 95.0],
        );
        let (_, standings) = lapped.scoring.as_ref().unwrap();
        let behind = |car: i64| {
            standings
                .iter()
                .find(|s| s.vehicle_id.get() == Some(car))
                .unwrap()
                .time_behind_leader
                .get()
        };
        assert_eq!(behind(2), Some(95.0));
        // Car 1, finished on the winner's lap: how long after it, over the line
        close(
            behind(1),
            (2.5 + 0.03 / 0.05 * 0.5) - (1.5 + 0.06 / 0.08 * 0.5),
        );
    }

    /// Updates of three cars in a race: time, session state, lap distance and laps completed each,
    /// with places 1 to 3 and the game's times behind the leader. The last update's.
    fn race_end(updates: &[(f64, i32, [f64; 3], [i32; 3])], f2: [f64; 3]) -> Update {
        race_of(updates, f2, None)
    }

    /// As `race_end`, in a race of `laps` laps when there are so many.
    fn race_of(
        updates: &[(f64, i32, [f64; 3], [i32; 3])],
        f2: [f64; 3],
        laps: Option<i32>,
    ) -> Update {
        use super::super::sdk::{Value, Vars};
        use super::super::session::{Driver, Session};
        let driver = Driver {
            class_id: Some(1),
            class_lap_time: Some(90.0),
            ..Default::default()
        };
        let info = SessionInfo {
            track_length: Some(1000.0),
            player: Some(0),
            drivers: (0..3).map(|car| (car, driver.clone())).collect(),
            sessions: vec![Session {
                num: 0,
                kind: Some("race"),
                number: 1,
                results: HashMap::new(),
            }],
            ..Default::default()
        };
        let start = Instant::now();
        let mut converter = Converter::default();
        let mut last = None;
        let total = laps;
        for (t, state, pct, laps) in updates {
            let mut values = vec![
                ("SessionTime", Value::Double(*t)),
                ("SessionNum", Value::Int(0)),
                ("SessionState", Value::Int(*state)),
                ("PlayerCarIdx", Value::Int(0)),
                ("CarIdxLapDistPct", Value::Doubles(pct.to_vec())),
                (
                    "CarIdxTrackSurface",
                    Value::Ints(
                        pct.iter()
                            .map(|p| if *p < 0.0 { NOT_IN_WORLD } else { 3 })
                            .collect(),
                    ),
                ),
                ("CarIdxLapCompleted", Value::Ints(laps.to_vec())),
                ("CarIdxPosition", Value::Ints(vec![1, 2, 3])),
                ("CarIdxClassPosition", Value::Ints(vec![1, 2, 3])),
                (
                    "CarIdxEstTime",
                    Value::Doubles(pct.iter().map(|p| p.max(0.0) * 90.0).collect()),
                ),
                ("CarIdxF2Time", Value::Doubles(f2.to_vec())),
            ];
            if let Some(total) = total {
                values.push(("SessionLapsTotal", Value::Int(total)));
            }
            let (vars, data) = Vars::of(&values);
            last = converter.convert(
                &Frame::new(&vars, &data),
                &info,
                start + std::time::Duration::from_secs_f64(*t),
            );
        }
        last.unwrap()
    }

    /// Updates every quarter second from `from` to `to`, the three cars going a tenth of a lap a
    /// second from `gone` (in laps) at 0, the flag from `flag`.
    fn steady(
        from: f64,
        to: f64,
        gone: [f64; 3],
        flag: f64,
    ) -> Vec<(f64, i32, [f64; 3], [i32; 3])> {
        let mut updates = vec![];
        let mut t = from;
        while t <= to {
            let p = gone.map(|g| g + 0.1 * t);
            let state = match t >= flag {
                true => STATE_CHECKERED,
                false => STATE_RACING,
            };
            updates.push((t, state, p.map(|p| p.fract()), p.map(|p| p.floor() as i32)));
            t += 0.25;
        }
        updates
    }

    /// Once finished, a car keeps the game's place: going further than the winner stopped past
    /// the line doesn't take its place.
    #[test]
    fn a_finished_car_keeps_the_games_place() {
        // Over the line, car 0 at 5.15 s and stopped just past it, car 2 (a lap down) at 6.15 s,
        // car 1 at 13.15 s and on by car 0 since
        let mut updates = steady(0.1, 14.85, [9.485, 8.685, 8.385], 4.1);
        let stopped = updates.iter().position(|u| u.0 >= 5.35).unwrap();
        let (pct, laps) = (updates[stopped].2[0], updates[stopped].3[0]);
        for update in &mut updates[stopped..] {
            (update.2[0], update.3[0]) = (pct, laps);
        }
        let end = race_end(&updates, [0.0, 72.0, 99.0]);
        let (_, standings) = end.scoring.as_ref().unwrap();
        let place = |car: i64| {
            let row = standings.iter().find(|s| s.vehicle_id.get() == Some(car));
            row.and_then(|row| row.position.get())
        };
        assert_eq!((place(0), place(1), place(2)), (Some(1), Some(2), Some(3)));
    }

    /// A pass for the lead on the last lap holds over the line, while the game's places catch up:
    /// the cars over it are in the order they crossed. Once they've had time to, the game's.
    #[test]
    fn a_last_lap_pass_holds_over_the_line() {
        // Car 1 by car 0 at 1 s and over the line first, at 1.83 s; car 0 at 2 s; car 2 a lap
        // down. The game's places stay as they were
        let updates: Vec<_> = (0..25)
            .map(|k| {
                let t = 0.1 + 0.25 * k as f64;
                let gone = [2.8 + 0.1 * t, 2.78 + 0.12 * t, 1.9 + 0.1 * t];
                let state = match t >= 0.5 {
                    true => STATE_CHECKERED,
                    false => STATE_RACING,
                };
                (
                    t,
                    state,
                    gone.map(f64::fract),
                    gone.map(|g| g.floor() as i32),
                )
            })
            .collect();
        let places = |at: f64| {
            let last = updates.iter().position(|u| u.0 >= at).unwrap();
            let end = race_of(&updates[..=last], [0.0, 0.3, 95.0], Some(3));
            let (_, standings) = end.scoring.as_ref().unwrap();
            let place = |car: i64| {
                let row = standings.iter().find(|s| s.vehicle_id.get() == Some(car));
                row.and_then(|row| row.position.get())
            };
            (place(0), place(1), place(2))
        };
        // Passed, both still racing; car 1 over the line; both over
        assert_eq!(places(1.5), (Some(2), Some(1), Some(3)));
        assert_eq!(places(1.85), (Some(2), Some(1), Some(3)));
        assert_eq!(places(2.35), (Some(2), Some(1), Some(3)));
        // Long enough over it for the game to have caught up
        assert_eq!(places(6.0), (Some(1), Some(2), Some(3)));
    }

    /// A car over the line with its count still to come is at the line, behind those over it
    /// before: not ahead of the winner by how far it's gone since.
    #[test]
    fn a_count_still_to_come_is_at_the_line() {
        // Car 0 over at 0.92 s and the winner; car 1 over at 1.3 s, its count an update later
        let updates = [
            (0.5, STATE_RACING, [0.95, 0.93, 0.5], [2, 2, 2]),
            (1.0, STATE_CHECKERED, [0.01, 0.97, 0.55], [3, 2, 2]),
            (1.5, STATE_CHECKERED, [0.05, 0.02, 0.6], [3, 2, 2]),
            (2.0, STATE_CHECKERED, [0.09, 0.06, 0.65], [3, 3, 2]),
        ];
        for last in [2, 3] {
            let end = race_of(&updates[..=last], [0.0, 0.3, 95.0], Some(3));
            let (_, standings) = end.scoring.as_ref().unwrap();
            let place = |car: i64| {
                let row = standings.iter().find(|s| s.vehicle_id.get() == Some(car));
                row.and_then(|row| row.position.get())
            };
            assert_eq!((place(0), place(1), place(2)), (Some(1), Some(2), Some(3)));
        }
    }

    /// Racing, a car over the line with its count still to come is as far on as it's gone: ahead
    /// of one over just after it.
    #[test]
    fn a_count_late_mid_race_is_as_far_as_gone() {
        // Car 0 over first, its count an update later; car 1 over after it, counted at once
        let updates = [
            (1.0, STATE_RACING, [0.97, 0.96, 0.5], [2, 2, 2]),
            (1.5, STATE_RACING, [0.02, 0.005, 0.55], [2, 3, 2]),
        ];
        let end = race_of(&updates, [0.0, 0.3, 95.0], None);
        let (_, standings) = end.scoring.as_ref().unwrap();
        let place = |car: i64| {
            let row = standings.iter().find(|s| s.vehicle_id.get() == Some(car));
            row.and_then(|row| row.position.get())
        };
        assert_eq!((place(0), place(1), place(2)), (Some(1), Some(2), Some(3)));
    }

    /// Where the count goes up past the line it's crossed at, a car over the line after the flag
    /// hasn't finished until its count says so: behind the car counted there before it.
    #[test]
    fn a_finish_past_the_line_is_at_the_count() {
        // A fifth of a lap past the line, car 0 counted at 4.1 s, car 1 at 4.4 s; the flag from
        // the second update
        let updates: Vec<_> = (0..20)
            .map(|k| {
                let t = 2.0 + 0.25 * k as f64;
                let gone = [2.79 + 0.1 * t, 2.76 + 0.1 * t, 2.0 + 0.1 * t];
                (
                    t,
                    match k {
                        0 => STATE_RACING,
                        _ => STATE_CHECKERED,
                    },
                    gone.map(f64::fract),
                    gone.map(|g| (g - 0.2).floor() as i32),
                )
            })
            .collect();
        for at in [3.0, 4.25] {
            let last = updates.iter().position(|u| u.0 >= at).unwrap();
            let end = race_of(&updates[..=last], [0.0, 0.3, 95.0], Some(3));
            let (_, standings) = end.scoring.as_ref().unwrap();
            let place = |car: i64| {
                let row = standings.iter().find(|s| s.vehicle_id.get() == Some(car));
                row.and_then(|row| row.position.get())
            };
            assert_eq!(
                (place(0), place(1), place(2)),
                (Some(1), Some(2), Some(3)),
                "{at}"
            );
        }
    }

    /// A car with the winner's laps done whose finish wasn't timed (its count came while the
    /// recording held up) keeps the game's place: not ahead by how far it's gone since.
    #[test]
    fn an_untimed_finish_keeps_the_games_place() {
        // Car 1 over the line in the hold-up; car 0 over after it, timed, and the winner
        let updates = [
            (1.0, STATE_RACING, [0.9, 0.95, 0.5], [2, 2, 2]),
            (1.5, STATE_CHECKERED, [0.92, 0.97, 0.52], [2, 2, 2]),
            (7.5, STATE_CHECKERED, [0.99, 0.5, 0.8], [2, 3, 2]),
            (8.0, STATE_CHECKERED, [0.03, 0.55, 0.85], [3, 3, 2]),
        ];
        let end = race_of(&updates, [0.0, 0.3, 95.0], None);
        let (_, standings) = end.scoring.as_ref().unwrap();
        let place = |car: i64| {
            let row = standings.iter().find(|s| s.vehicle_id.get() == Some(car));
            row.and_then(|row| row.position.get())
        };
        assert_eq!((place(0), place(1), place(2)), (Some(1), Some(2), Some(3)));
    }

    /// A car towed back to its stall, just short of the line, isn't ahead of the cars it was
    /// behind until it's driven over the line.
    #[test]
    fn a_towed_car_keeps_the_games_place() {
        let updates = [
            (1.0, STATE_RACING, [0.5, 0.36, 0.2], [5, 5, 5]),
            (1.5, STATE_RACING, [0.55, -1.0, 0.25], [5, 5, 5]),
            (2.0, STATE_RACING, [0.6, 0.97, 0.3], [5, 5, 5]),
        ];
        let end = race_of(&updates, [0.0, 0.3, 95.0], None);
        let (_, standings) = end.scoring.as_ref().unwrap();
        let place = |car: i64| {
            let row = standings.iter().find(|s| s.vehicle_id.get() == Some(car));
            row.and_then(|row| row.position.get())
        };
        assert_eq!((place(0), place(1), place(2)), (Some(1), Some(2), Some(3)));
    }

    /// The winner's count coming a moment before its line: it stays at the line, ahead, over it
    /// too, while the game's places catch up.
    #[test]
    fn an_early_count_holds_at_the_line() {
        // Car 1 by car 0, counted at 1 s and over the line at 1.4 s; car 0 over after it
        let updates = [
            (0.5, STATE_RACING, [0.9, 0.92, 0.5], [2, 2, 2]),
            (1.0, STATE_CHECKERED, [0.94, 0.96, 0.55], [2, 3, 2]),
            (1.5, STATE_CHECKERED, [0.98, 0.01, 0.6], [2, 3, 2]),
            (2.0, STATE_CHECKERED, [0.02, 0.05, 0.65], [3, 3, 2]),
        ];
        for last in [1, 2, 3] {
            let end = race_of(&updates[..=last], [0.0, 0.3, 95.0], Some(3));
            let (_, standings) = end.scoring.as_ref().unwrap();
            let place = |car: i64| {
                let row = standings.iter().find(|s| s.vehicle_id.get() == Some(car));
                row.and_then(|row| row.position.get())
            };
            assert_eq!(
                (place(0), place(1), place(2)),
                (Some(2), Some(1), Some(3)),
                "{last}"
            );
        }
    }

    /// A car finished a lap down, over the line again on its cool-down lap, keeps the game's place
    /// rather than going ahead of a car still racing.
    #[test]
    fn a_cool_down_line_isnt_a_finish() {
        // Car 0 wins at 1.75 s; car 2, a lap down, finishes at 2.25 s and is over again at 11.3 s;
        // car 1, ahead of it on the lead lap, stopped short of the line
        let updates = [
            (1.0, STATE_RACING, [0.9, 0.85, 0.95], [2, 2, 1]),
            (1.5, STATE_CHECKERED, [0.94, 0.89, 0.99], [2, 2, 1]),
            (2.0, STATE_CHECKERED, [0.02, 0.93, 0.995], [3, 2, 1]),
            (2.5, STATE_CHECKERED, [0.06, 0.93, 0.04], [3, 2, 2]),
            (7.0, STATE_CHECKERED, [0.55, 0.93, 0.53], [3, 2, 2]),
            (11.0, STATE_CHECKERED, [0.95, 0.93, 0.97], [3, 2, 2]),
            (11.5, STATE_CHECKERED, [0.99, 0.93, 0.02], [3, 2, 3]),
        ];
        let end = race_of(&updates, [0.0, 0.7, 95.0], None);
        let (_, standings) = end.scoring.as_ref().unwrap();
        let place = |car: i64| {
            let row = standings.iter().find(|s| s.vehicle_id.get() == Some(car));
            row.and_then(|row| row.position.get())
        };
        assert_eq!((place(0), place(1), place(2)), (Some(1), Some(2), Some(3)));
    }

    /// When who won can't be known, who has finished can't either: after the flag the places
    /// are the game's, not how far each has gone on its cool-down lap.
    #[test]
    fn an_unknown_winner_leaves_the_games_places() {
        // Car 0 over before the recording began and stopped just past the line, car 1 over at
        // 1.2 s and on by it since
        let mut updates = steady(0.1, 4.1, [3.05, 2.88, 2.0], 0.6);
        let stopped = updates.iter().position(|u| u.0 >= 0.35).unwrap();
        let (pct, laps) = (updates[stopped].2[0], updates[stopped].3[0]);
        for update in &mut updates[stopped..] {
            (update.2[0], update.3[0]) = (pct, laps);
        }
        let end = race_of(&updates, [0.0, 0.3, 50.0], Some(3));
        let (_, standings) = end.scoring.as_ref().unwrap();
        let place = |car: i64| {
            let row = standings.iter().find(|s| s.vehicle_id.get() == Some(car));
            row.and_then(|row| row.position.get())
        };
        assert_eq!((place(0), place(1), place(2)), (Some(1), Some(2), Some(3)));
    }

    /// A car a lap down, finished, behind a car on the lead lap still racing to the line: a lap
    /// behind it once that car finishes too, however long after.
    #[test]
    fn laps_down_wait_for_the_car_ahead_to_finish() {
        // Over the line, car 0 at 5.15 s, car 2 (a lap down) at 6.15 s, car 1 at 13.15 s
        let updates = steady(0.1, 14.85, [9.485, 8.685, 8.385], 4.1);
        let end = race_end(&updates, [0.0, 72.0, 99.0]);
        let (_, standings) = end.scoring.as_ref().unwrap();
        let laps = |car: i64| {
            let s = standings
                .iter()
                .find(|s| s.vehicle_id.get() == Some(car))
                .unwrap();
            (s.laps_behind_leader.get(), s.laps_behind_next.get())
        };
        assert_eq!(laps(1), (Some(0), Some(0)));
        assert_eq!(laps(2), (Some(1), Some(1)));
    }

    /// A car out of the world (towed, disconnected) isn't racing on track: the leader out of it
    /// leaves the times behind it the game's, not 0 for every car round from where it is. Nor is
    /// the game's time for a car out of it and back taken for the first lap's placeholder.
    #[test]
    fn a_car_out_of_the_world_isnt_on_track() {
        let mut updates = steady(0.1, 2.1, [5.1, 4.9, 4.7], 99.0);
        updates.push((2.35, STATE_RACING, [-1.0, 0.135, 0.935], [5, 5, 4]));
        let end = race_end(&updates, [0.0, 12.0, 24.0]);
        let (_, standings) = end.scoring.as_ref().unwrap();
        let behind = |car: i64| {
            standings
                .iter()
                .find(|s| s.vehicle_id.get() == Some(car))
                .unwrap()
                .time_behind_leader
                .get()
        };
        assert_eq!(behind(1), Some(12.0));
        assert_eq!(behind(2), Some(24.0));

        // A car out of the world (its count -1) and back keeps the game's time it had
        let mut updates = steady(0.1, 1.1, [5.1, 4.9, 4.7], 99.0);
        updates.push((1.35, STATE_RACING, [0.235, -1.0, 0.835], [5, -1, 4]));
        updates.push((1.6, STATE_RACING, [-1.0, 0.3, 0.86], [5, 5, 4]));
        let end = race_end(&updates, [0.0, 12.0, 24.0]);
        let (_, standings) = end.scoring.as_ref().unwrap();
        let behind = standings
            .iter()
            .find(|s| s.vehicle_id.get() == Some(1))
            .unwrap()
            .time_behind_leader
            .get();
        assert_eq!(behind, Some(12.0));
    }

    /// Over the line all but together, the second car's count going up an update before it's
    /// there and the winner's an update after it was: the winner is still the first over, and
    /// the gap between them how far apart they were over the line.
    #[test]
    fn a_dead_heat_finishes_in_the_order_over_the_line() {
        let end = race_end(
            &[
                (0.5, STATE_RACING, [0.94, 0.92, 0.5], [2, 2, 1]),
                (0.75, STATE_RACING, [0.965, 0.945, 0.525], [2, 2, 1]),
                (1.0, STATE_CHECKERED, [0.99, 0.97, 0.55], [2, 2, 1]),
                // Car 0 over at 1.1 s, its count not up yet; car 1's up, over at 1.3 s
                (1.25, STATE_CHECKERED, [0.015, 0.995, 0.575], [2, 3, 1]),
                (1.5, STATE_CHECKERED, [0.04, 0.02, 0.6], [3, 3, 1]),
            ],
            [0.0, 0.3, 50.0],
        );
        let close = |got: Option<f64>, want: f64| {
            assert!(got.is_some_and(|got| (got - want).abs() < 1e-6), "{got:?}");
        };
        let (_, standings) = end.scoring.as_ref().unwrap();
        let standing = standings
            .iter()
            .find(|s| s.vehicle_id.get() == Some(1))
            .unwrap();
        close(standing.time_behind_leader.get(), 0.2);
        let telemetry = end
            .telemetry
            .iter()
            .find(|t| t.vehicle_id.get() == Some(1))
            .unwrap();
        close(telemetry.gap_to_place_ahead.get(), 0.2);
    }

    /// A count timed ahead of the line (the car slowing to it) is timed at the line once the car
    /// is over; the winner's count can come after the flag and the next car's; and a car the
    /// winner passed on the way to the line, still placed first, has its finishing gap too.
    #[test]
    fn finishing_gaps_are_over_the_line() {
        let close = |got: Option<f64>, want: f64| {
            assert!(got.is_some_and(|got| (got - want).abs() < 1e-6), "{got:?}");
        };
        let behind = |end: &Update, car: i64| {
            let (_, standings) = end.scoring.as_ref().unwrap();
            standings
                .iter()
                .find(|s| s.vehicle_id.get() == Some(car))
                .unwrap()
                .time_behind_leader
                .get()
        };
        // Car 0 over at 0.7083 s; car 1's count up before it's over, at 1.4583 s
        let end = race_end(
            &[
                (0.25, STATE_RACING, [0.95, 0.93, 0.5], [2, 2, 1]),
                (0.5, STATE_CHECKERED, [0.975, 0.95, 0.525], [2, 2, 1]),
                (0.75, STATE_CHECKERED, [0.005, 0.975, 0.55], [3, 2, 1]),
                (1.0, STATE_CHECKERED, [0.035, 0.99, 0.575], [3, 2, 1]),
                (1.25, STATE_CHECKERED, [0.065, 0.995, 0.6], [3, 3, 1]),
                (1.5, STATE_CHECKERED, [0.095, 0.001, 0.625], [3, 3, 1]),
            ],
            [0.0, 0.3, 50.0],
        );
        close(
            behind(&end, 1),
            (1.25 + 0.005 / 0.006 * 0.25) - (0.5 + 0.025 / 0.03 * 0.25),
        );

        // Car 0 over at 1.05 s, its count up an update after car 1's (over at 1.15 s) and the
        // flag: still the winner
        let end = race_end(
            &[
                (0.75, STATE_RACING, [0.97, 0.96, 0.5], [2, 2, 1]),
                (1.0, STATE_RACING, [0.995, 0.985, 0.525], [2, 2, 1]),
                (1.25, STATE_CHECKERED, [0.02, 0.01, 0.55], [2, 3, 1]),
                (1.5, STATE_CHECKERED, [0.045, 0.035, 0.575], [3, 3, 1]),
            ],
            [0.0, 0.3, 50.0],
        );
        close(behind(&end, 1), 0.1);

        // Car 1 by car 0 before the line, over it at 2.5 s, car 0 at 3 s, still placed first
        let end = race_end(&steady(0.1, 3.35, [2.7, 2.75, 1.5], 2.0), [0.0, 0.3, 50.0]);
        close(behind(&end, 0), 0.5);
        close(behind(&end, 1), 0.0);
    }

    /// The winner's count up well before the line, the car slowing then speeding up to it: its
    /// finish is timed at the line once it's over, past the moment the winner can change.
    #[test]
    fn the_winners_early_count_is_timed_at_the_line() {
        let close = |got: Option<f64>, want: f64| {
            assert!(got.is_some_and(|got| (got - want).abs() < 1e-6), "{got:?}");
        };
        // Car 0's count up at 1 s, over at 1.8 s; car 1 over at 2.5 s
        let end = race_end(
            &[
                (0.5, STATE_RACING, [0.895, 0.8, 0.3], [2, 2, 1]),
                (0.75, STATE_CHECKERED, [0.898, 0.825, 0.31], [2, 2, 1]),
                (1.0, STATE_CHECKERED, [0.9, 0.85, 0.32], [3, 2, 1]),
                (1.25, STATE_CHECKERED, [0.95, 0.875, 0.33], [3, 2, 1]),
                (1.5, STATE_CHECKERED, [0.99, 0.9, 0.34], [3, 2, 1]),
                (1.75, STATE_CHECKERED, [0.995, 0.925, 0.35], [3, 2, 1]),
                (2.05, STATE_CHECKERED, [0.025, 0.955, 0.36], [3, 2, 1]),
                (2.35, STATE_CHECKERED, [0.055, 0.985, 0.37], [3, 2, 1]),
                (2.6, STATE_CHECKERED, [0.08, 0.01, 0.38], [3, 3, 1]),
            ],
            [0.0, 0.3, 50.0],
        );
        let (_, standings) = end.scoring.as_ref().unwrap();
        let behind = |car: i64| {
            standings
                .iter()
                .find(|s| s.vehicle_id.get() == Some(car))
                .unwrap()
                .time_behind_leader
                .get()
        };
        close(behind(0), 0.0);
        close(behind(1), 2.5 - 1.8);
    }

    /// Both over the line before the flag, the first passed just after it: the first over is
    /// the winner, not the car furthest on when the flag comes.
    #[test]
    fn the_winner_over_before_the_flag_is_the_first_over() {
        let end = race_end(
            &[
                (0.25, STATE_RACING, [0.97, 0.965, 0.3], [2, 2, 1]),
                (0.5, STATE_RACING, [0.005, 0.99, 0.31], [3, 2, 1]),
                (0.75, STATE_RACING, [0.025, 0.02, 0.32], [3, 3, 1]),
                (1.0, STATE_CHECKERED, [0.045, 0.05, 0.33], [3, 3, 1]),
                (1.25, STATE_CHECKERED, [0.065, 0.08, 0.34], [3, 3, 1]),
            ],
            [0.0, 0.3, 50.0],
        );
        let (_, standings) = end.scoring.as_ref().unwrap();
        let behind = standings
            .iter()
            .find(|s| s.vehicle_id.get() == Some(1))
            .unwrap()
            .time_behind_leader
            .get();
        let want = (0.5 + 0.01 / 0.03 * 0.25) - (0.25 + 0.03 / 0.035 * 0.25);
        assert!(
            behind.is_some_and(|got| (got - want).abs() < 1e-6),
            "{behind:?}"
        );
    }

    /// In a race of so many laps, the winner is the first over the line with them all done
    /// however long after the flag comes: not missed, nor the car over just behind it.
    #[test]
    fn a_lap_races_winner_is_the_first_with_every_lap_done() {
        // Car 0 over at 4.15 s, car 1 at 4.95 s, the flag at 9 s
        let updates = steady(0.1, 10.1, [2.585, 2.505, 1.6], 9.0);
        let end = race_of(&updates, [0.0, 0.3, 50.0], Some(3));
        let (_, standings) = end.scoring.as_ref().unwrap();
        let behind = |car: i64| {
            standings
                .iter()
                .find(|s| s.vehicle_id.get() == Some(car))
                .unwrap()
                .time_behind_leader
                .get()
        };
        assert_eq!(behind(0), Some(0.0));
        let got = behind(1);
        assert!(got.is_some_and(|got| (got - 0.8).abs() < 1e-6), "{got:?}");
    }

    /// The recording held up across the flag (3 s without an update): each car over the line in
    /// the gap is timed at its crossing, and in a race of so many laps the winner found among
    /// them. A car a lap down over just before the winner is still racing its last lap, not
    /// finished. In a timed race, who won isn't known: the first over in the gap may have started
    /// its last lap rather than finished it, and the car after it didn't win.
    #[test]
    fn a_hold_up_across_the_flag_times_each_crossing() {
        // Car 2 over at 3.7 s, car 0 at 4.15 s, car 1 at 4.95 s; no update from 3.6 s to 6.6 s
        let gone = [2.585, 2.505, 1.63];
        let mut updates = steady(0.1, 3.61, gone, 99.0);
        updates.extend(steady(6.6, 8.1, gone, 6.6));
        let standing = |end: &Update, car: i64| {
            let (_, standings) = end.scoring.as_ref().unwrap();
            standings
                .iter()
                .find(|s| s.vehicle_id.get() == Some(car))
                .unwrap()
                .clone()
        };
        let end = race_of(&updates, [0.0, 0.3, 50.0], Some(3));
        let got = standing(&end, 1).time_behind_leader.get();
        assert!(got.is_some_and(|got| (got - 0.8).abs() < 1e-6), "{got:?}");
        assert_eq!(standing(&end, 2).laps_behind_leader.get(), Some(0));

        // The winner's count an update after the one that saw it over the line: still timed at
        // its crossing
        let mut lagging = updates.clone();
        let after = lagging.iter_mut().find(|u| u.0 == 6.6).unwrap();
        after.3[0] = 2;
        let end = race_of(&lagging, [0.0, 0.3, 50.0], Some(3));
        let got = standing(&end, 1).time_behind_leader.get();
        assert!(got.is_some_and(|got| (got - 0.8).abs() < 1e-6), "{got:?}");

        // Still racing: Bono's live time behind (0.08 of a 90 s lap), no finishing gap. The
        // winner's count an update late too: the car over after it was taken for the winner a
        // moment, and isn't
        for updates in [&updates, &lagging] {
            let end = race_end(updates, [0.0, 0.3, 50.0]);
            let got = standing(&end, 1).time_behind_leader.get();
            assert!(got.is_some_and(|got| (got - 7.2).abs() < 1e-6), "{got:?}");
            assert_eq!(standing(&end, 2).laps_behind_leader.get(), Some(0));
        }
    }

    /// The first count of the laps done comes ahead of its car's line (timed at the line it's
    /// due at), and the car over the line first has its count come a little after its crossing,
    /// more than a moment after the first count (not after the first car's crossing, nor the
    /// update that saw it): still the winner, the first over.
    #[test]
    fn the_first_over_wins_however_early_another_count_comes() {
        // Car 0's count at 10 s, over at 10.875 s; car 1 over at 10.625 s, its count at 11.5 s
        let end = race_of(
            &[
                (9.75, STATE_RACING, [0.955, 0.86, 0.5], [2, 2, 1]),
                (10.0, STATE_CHECKERED, [0.965, 0.9, 0.51], [3, 2, 1]),
                (10.25, STATE_CHECKERED, [0.975, 0.94, 0.52], [3, 2, 1]),
                (10.5, STATE_CHECKERED, [0.985, 0.98, 0.53], [3, 2, 1]),
                (10.75, STATE_CHECKERED, [0.995, 0.02, 0.54], [3, 2, 1]),
                (11.0, STATE_CHECKERED, [0.005, 0.06, 0.55], [3, 2, 1]),
                (11.25, STATE_CHECKERED, [0.015, 0.1, 0.56], [3, 2, 1]),
                (11.5, STATE_CHECKERED, [0.025, 0.14, 0.57], [3, 3, 1]),
                (11.75, STATE_CHECKERED, [0.035, 0.18, 0.58], [3, 3, 1]),
            ],
            [0.0, 0.3, 50.0],
            Some(3),
        );
        let (_, standings) = end.scoring.as_ref().unwrap();
        let behind = |car: i64| {
            standings
                .iter()
                .find(|s| s.vehicle_id.get() == Some(car))
                .unwrap()
                .time_behind_leader
                .get()
        };
        let got = behind(0);
        assert!(got.is_some_and(|got| (got - 0.25).abs() < 1e-6), "{got:?}");
        assert_eq!(behind(1), Some(0.0));
    }

    /// In a race of so many laps, a car with them all done before the recording began (its
    /// crossing not seen) leaves who won unknown: the car over the line after it didn't win.
    #[test]
    fn a_lap_races_winner_done_before_the_recording_isnt_known() {
        // Car 0 over before 0.1 s, car 1 over at 1.2 s
        let updates = steady(0.1, 2.1, [3.05, 2.88, 2.0], 0.6);
        let end = race_of(&updates, [0.0, 0.3, 50.0], Some(3));
        let (_, standings) = end.scoring.as_ref().unwrap();
        let behind = standings
            .iter()
            .find(|s| s.vehicle_id.get() == Some(1))
            .unwrap()
            .time_behind_leader
            .get();
        // Still racing: Bono's live time behind (0.17 of a 90 s lap), no finishing gap of 0
        assert!(
            behind.is_some_and(|got| (got - 15.3).abs() < 1e-6),
            "{behind:?}"
        );
    }

    /// In a race of so many laps, a car already past them when the flag comes (a flag a lap
    /// late) leaves who won unknown: the next car over with them all done didn't win.
    #[test]
    fn a_lap_races_winner_past_its_laps_at_the_flag_isnt_known() {
        // Car 0 over for its fourth lap at 0.5 s, car 1 for its third at 0.7 s, the flag at 1 s
        let updates = steady(0.1, 2.1, [3.95, 2.93, 2.0], 1.0);
        let end = race_of(&updates, [0.0, 0.3, 50.0], Some(3));
        let (_, standings) = end.scoring.as_ref().unwrap();
        let behind = standings
            .iter()
            .find(|s| s.vehicle_id.get() == Some(1))
            .unwrap()
            .time_behind_leader
            .get();
        // Still racing: Bono's live time behind (a 90 s lap and 0.02 of one), not a finish
        assert!(
            behind.is_some_and(|got| (got - 91.8).abs() < 1e-6),
            "{behind:?}"
        );
    }

    /// In a race of so many laps, a car with them done but not timed after the winner is taken
    /// (its count up out of the world) leaves the winner as it is: it can't have been over first.
    #[test]
    fn a_lap_races_winner_stays_with_a_car_untimed_after_it() {
        // Car 0 over at 10.05 s, car 2 at 10.2 s; car 1 counted out of the world at 10.5 s
        let end = race_of(
            &[
                (9.75, STATE_RACING, [0.97, 0.9, 0.955], [2, 2, 2]),
                (10.0, STATE_CHECKERED, [0.995, 0.92, 0.98], [2, 2, 2]),
                (10.25, STATE_CHECKERED, [0.02, 0.94, 0.005], [3, 2, 3]),
                (10.5, STATE_CHECKERED, [0.045, -1.0, 0.03], [3, 3, 3]),
                (10.75, STATE_CHECKERED, [0.07, -1.0, 0.055], [3, 3, 3]),
            ],
            [0.0, 0.3, 50.0],
            Some(3),
        );
        let (_, standings) = end.scoring.as_ref().unwrap();
        let behind = standings
            .iter()
            .find(|s| s.vehicle_id.get() == Some(2))
            .unwrap()
            .time_behind_leader
            .get();
        assert!(
            behind.is_some_and(|got| (got - 0.15).abs() < 1e-6),
            "{behind:?}"
        );
    }

    /// The recording held up just after the update that saw two cars over the line, the winner's
    /// count still to come: it's timed at its crossing when it does, and wins.
    #[test]
    fn a_count_after_a_hold_up_just_after_its_crossing_is_timed_at_it() {
        // Car 1 over at 9.875 s, car 0 at 9.9375 s (counted at 10 s); car 1 counted at 13 s
        let end = race_of(
            &[
                (9.75, STATE_RACING, [0.985, 0.99, 0.5], [2, 2, 1]),
                (10.0, STATE_CHECKERED, [0.005, 0.01, 0.52], [3, 2, 1]),
                (13.0, STATE_CHECKERED, [0.245, 0.25, 0.76], [3, 3, 1]),
            ],
            [0.0, 0.3, 50.0],
            Some(3),
        );
        let (_, standings) = end.scoring.as_ref().unwrap();
        let behind = |car: i64| {
            standings
                .iter()
                .find(|s| s.vehicle_id.get() == Some(car))
                .unwrap()
                .time_behind_leader
                .get()
        };
        let got = behind(0);
        assert!(
            got.is_some_and(|got| (got - 0.0625).abs() < 1e-6),
            "{got:?}"
        );
        assert_eq!(behind(1), Some(0.0));
    }

    /// The winner taken at its count ahead of the line, and the car over first counted after a
    /// hold-up that also takes the winner's count to its line: the winner competes at its
    /// crossing, and the first over wins.
    #[test]
    fn the_winner_taken_competes_at_its_crossing() {
        // Car 0 counted at 10 s, over at 10.75 s; car 1 over at 10.667 s, counted at 13 s
        let end = race_of(
            &[
                (9.75, STATE_RACING, [0.89, 0.86, 0.5], [2, 2, 1]),
                (10.0, STATE_CHECKERED, [0.9, 0.9, 0.51], [3, 2, 1]),
                (13.0, STATE_CHECKERED, [0.3, 0.35, 0.6], [3, 3, 1]),
                (13.25, STATE_CHECKERED, [0.325, 0.3875, 0.61], [3, 3, 1]),
            ],
            [0.0, 0.3, 50.0],
            Some(3),
        );
        let (_, standings) = end.scoring.as_ref().unwrap();
        let behind = |car: i64| {
            standings
                .iter()
                .find(|s| s.vehicle_id.get() == Some(car))
                .unwrap()
                .time_behind_leader
                .get()
        };
        let got = behind(0);
        assert!(
            got.is_some_and(|got| (got - (10.75 - 10.0 - 2.0 / 3.0)).abs() < 1e-6),
            "{got:?}"
        );
        assert_eq!(behind(1), Some(0.0));
    }

    /// The winner taken at its count ahead of the line, a hold-up over both cars' crossings, and
    /// the car over first counted an update after the one after the hold-up: it wins.
    #[test]
    fn a_count_an_update_after_a_hold_up_still_wins() {
        // Car 0 counted at 10 s, car 1 over first; both over in the hold-up to 13 s, car 1
        // counted at 13.25 s
        let end = race_of(
            &[
                (9.75, STATE_RACING, [0.97, 0.975, 0.5], [2, 2, 1]),
                (10.0, STATE_CHECKERED, [0.99, 0.995, 0.51], [3, 2, 1]),
                (13.0, STATE_CHECKERED, [0.3, 0.32, 0.6], [3, 2, 1]),
                (13.25, STATE_CHECKERED, [0.326, 0.347, 0.61], [3, 3, 1]),
            ],
            [0.0, 0.3, 50.0],
            Some(3),
        );
        let (_, standings) = end.scoring.as_ref().unwrap();
        let behind = |car: i64| {
            standings
                .iter()
                .find(|s| s.vehicle_id.get() == Some(car))
                .unwrap()
                .time_behind_leader
                .get()
        };
        let want = (10.0 + 0.01 / 0.31 * 3.0) - (10.0 + 0.005 / 0.325 * 3.0);
        let got = behind(0);
        assert!(got.is_some_and(|got| (got - want).abs() < 1e-6), "{got:?}");
        assert_eq!(behind(1), Some(0.0));
    }

    /// A car finished stays finished, with its finishing gap, though its next count isn't timed
    /// (the recording held up longer than a car's driving can be told from a move).
    #[test]
    fn a_finished_car_stays_finished_over_a_long_hold_up() {
        // Car 0 over at 1.15 s, car 1 at 1.95 s; no update from 2.1 s to 13.1 s, both over again
        let gone = [2.885, 2.805, 1.5];
        let mut updates = steady(0.1, 2.11, gone, 0.6);
        updates.extend(steady(13.1, 13.6, gone, 0.6));
        let end = race_of(&updates, [0.0, 0.3, 50.0], Some(3));
        let (_, standings) = end.scoring.as_ref().unwrap();
        let behind = standings
            .iter()
            .find(|s| s.vehicle_id.get() == Some(1))
            .unwrap()
            .time_behind_leader
            .get();
        assert!(
            behind.is_some_and(|got| (got - 0.8).abs() < 1e-6),
            "{behind:?}"
        );
    }

    /// A car the cool-down finishes, its crossing not seen (the recording stalled over it), has
    /// no finishing gap from the crossing before: the game's time behind the leader.
    #[test]
    fn the_cool_down_finishes_without_a_crossing() {
        let end = race_end(
            &[
                (0.25, STATE_RACING, [0.925, 0.99, 0.3], [2, 1, 1]),
                (0.5, STATE_RACING, [0.95, 0.015, 0.325], [2, 2, 1]),
                (0.75, STATE_CHECKERED, [0.975, 0.04, 0.35], [2, 2, 1]),
                (1.0, STATE_CHECKERED, [0.005, 0.065, 0.375], [3, 2, 1]),
                (7.5, STATE_COOL_DOWN, [0.3, 0.1, 0.4], [3, 3, 1]),
            ],
            [0.0, 30.0, 50.0],
        );
        let (_, standings) = end.scoring.as_ref().unwrap();
        let behind = standings
            .iter()
            .find(|s| s.vehicle_id.get() == Some(1))
            .unwrap()
            .time_behind_leader
            .get();
        assert_eq!(behind, Some(30.0));
    }

    /// iRacing shows the flag a few seconds before the leader is over the line: until it is, the
    /// leader and the cars behind it are still racing, live.
    #[test]
    fn the_flag_comes_before_the_leader_is_over_the_line() {
        use super::super::sdk::{Value, Vars};
        use super::super::session::{Driver, Session};
        let driver = Driver {
            class_id: Some(1),
            class_lap_time: Some(90.0),
            ..Default::default()
        };
        let info = SessionInfo {
            track_length: Some(1000.0),
            player: Some(0),
            drivers: (0..3).map(|car| (car, driver.clone())).collect(),
            sessions: vec![Session {
                num: 0,
                kind: Some("race"),
                number: 1,
                results: HashMap::new(),
            }],
            ..Default::default()
        };
        let start = Instant::now();
        let mut converter = Converter::default();
        let mut gap_ahead = |t: f64, state: i32, pct: [f64; 3], laps: [i32; 3]| {
            let (vars, data) = Vars::of(&[
                ("SessionTime", Value::Double(t)),
                ("SessionNum", Value::Int(0)),
                ("SessionState", Value::Int(state)),
                ("PlayerCarIdx", Value::Int(0)),
                ("CarIdxLapDistPct", Value::Doubles(pct.to_vec())),
                ("CarIdxLapCompleted", Value::Ints(laps.to_vec())),
                ("CarIdxPosition", Value::Ints(vec![2, 3, 1])),
                ("CarIdxClassPosition", Value::Ints(vec![2, 3, 1])),
                (
                    "CarIdxEstTime",
                    Value::Doubles(pct.iter().map(|p| p * 90.0).collect()),
                ),
                ("CarIdxF2Time", Value::Doubles(vec![2.0, 5.0, 0.0])),
            ]);
            let update = converter
                .convert(
                    &Frame::new(&vars, &data),
                    &info,
                    start + std::time::Duration::from_secs_f64(t),
                )
                .unwrap();
            let row = update
                .telemetry
                .iter()
                .find(|t| t.vehicle_id.get() == Some(0))
                .unwrap();
            row.gap_to_place_ahead.get()
        };
        gap_ahead(1.0, STATE_RACING, [0.85, 0.8, 0.9], [3, 3, 3]);
        // The flag, the leader still short of the line: live
        let live = gap_ahead(2.0, STATE_CHECKERED, [0.9, 0.85, 0.95], [3, 3, 3]);
        assert!(live.is_some_and(|gap| (gap - 4.5).abs() < 1e-6), "{live:?}");
        // The leader over it: finished, its finishing gap
        let done = gap_ahead(3.0, STATE_CHECKERED, [0.95, 0.9, 0.02], [3, 3, 4]);
        assert_eq!(done, Some(2.0));
    }

    /// A car over the line in the same update as the leader's flag has finished with it.
    #[test]
    fn a_car_over_the_line_with_the_leader_has_finished() {
        use super::super::sdk::{Value, Vars};
        use super::super::session::{Driver, Session};
        let driver = Driver {
            class_id: Some(1),
            class_lap_time: Some(90.0),
            ..Default::default()
        };
        let info = SessionInfo {
            track_length: Some(1000.0),
            player: Some(0),
            drivers: (0..3).map(|car| (car, driver.clone())).collect(),
            sessions: vec![Session {
                num: 0,
                kind: Some("race"),
                number: 1,
                results: HashMap::new(),
            }],
            ..Default::default()
        };
        let start = Instant::now();
        let mut converter = Converter::default();
        let mut frame = |t: f64, state: i32, pct: [f64; 3], laps: [i32; 3]| {
            let (vars, data) = Vars::of(&[
                ("SessionTime", Value::Double(t)),
                ("SessionNum", Value::Int(0)),
                ("SessionState", Value::Int(state)),
                ("PlayerCarIdx", Value::Int(0)),
                ("CarIdxLapDistPct", Value::Doubles(pct.to_vec())),
                ("CarIdxLapCompleted", Value::Ints(laps.to_vec())),
                ("CarIdxPosition", Value::Ints(vec![2, 3, 1])),
                ("CarIdxClassPosition", Value::Ints(vec![2, 3, 1])),
                (
                    "CarIdxEstTime",
                    Value::Doubles(pct.iter().map(|p| p * 90.0).collect()),
                ),
                ("CarIdxF2Time", Value::Doubles(vec![2.0, 5.0, 0.0])),
            ]);
            converter
                .convert(
                    &Frame::new(&vars, &data),
                    &info,
                    start + std::time::Duration::from_secs_f64(t),
                )
                .unwrap()
        };
        frame(1.0, STATE_RACING, [0.9, 0.8, 0.95], [3, 3, 3]);
        // The leader and car 0 both over the line as the flag comes: car 1's gap to car 0 is its
        // finishing one, not live
        let flag = frame(2.0, STATE_CHECKERED, [0.01, 0.85, 0.02], [4, 3, 4]);
        let row = flag
            .telemetry
            .iter()
            .find(|t| t.vehicle_id.get() == Some(1))
            .unwrap();
        assert_eq!(row.gap_to_place_ahead.get(), Some(3.0));

        // Recording from after the flag: who has finished isn't known, so nobody's racing (the
        // game's gaps) and nobody's laps down are known
        let mut late = Converter::default();
        let (vars, data) = Vars::of(&[
            ("SessionTime", Value::Double(50.0)),
            ("SessionNum", Value::Int(0)),
            ("SessionState", Value::Int(STATE_CHECKERED)),
            ("PlayerCarIdx", Value::Int(0)),
            ("CarIdxLapDistPct", Value::Doubles(vec![0.6, 0.55, 0.7])),
            ("CarIdxLapCompleted", Value::Ints(vec![4, 4, 5])),
            ("CarIdxPosition", Value::Ints(vec![2, 3, 1])),
            ("CarIdxClassPosition", Value::Ints(vec![2, 3, 1])),
            ("CarIdxEstTime", Value::Doubles(vec![54.0, 49.5, 63.0])),
            ("CarIdxF2Time", Value::Doubles(vec![2.0, 5.0, 0.0])),
        ]);
        let update = late
            .convert(&Frame::new(&vars, &data), &info, start)
            .unwrap();
        let row = update
            .telemetry
            .iter()
            .find(|t| t.vehicle_id.get() == Some(1))
            .unwrap();
        assert_eq!(row.gap_to_place_ahead.get(), Some(3.0));
        let (_, standings) = update.scoring.as_ref().unwrap();
        assert!(
            standings
                .iter()
                .all(|s| s.laps_behind_leader.get().is_none())
        );
    }

    /// Before a race's green the places are the grid's; and a car that hasn't finished a lap (the
    /// game's time behind the leader not a time yet) has no gaps rather than its placeholder.
    #[test]
    fn races_start_from_the_grid_and_untimed_cars_have_no_gaps() {
        use super::super::sdk::{Value, Vars};
        use super::super::session::{Driver, Session};
        let mut info = SessionInfo {
            track_length: Some(1000.0),
            player: Some(0),
            drivers: (0..3).map(|car| (car, Driver::default())).collect(),
            sessions: vec![Session {
                num: 0,
                kind: Some("race"),
                number: 1,
                results: HashMap::new(),
            }],
            qualifying: HashMap::from([(2, 1), (0, 2), (1, 3)]),
            ..Default::default()
        };
        let update =
            |info: &SessionInfo, state: i32, places: [i32; 3], behind: [f64; 3], laps: i32| {
                let (vars, data) = Vars::of(&[
                    ("SessionTime", Value::Double(10.0)),
                    ("SessionNum", Value::Int(0)),
                    ("SessionState", Value::Int(state)),
                    ("PlayerCarIdx", Value::Int(0)),
                    ("CarIdxLapDistPct", Value::Doubles(vec![0.5, 0.4, 0.6])),
                    ("CarIdxLapCompleted", Value::Ints(vec![laps; 3])),
                    ("CarIdxPosition", Value::Ints(places.to_vec())),
                    ("CarIdxClassPosition", Value::Ints(places.to_vec())),
                    ("CarIdxF2Time", Value::Doubles(behind.to_vec())),
                ]);
                Converter::default()
                    .convert(&Frame::new(&vars, &data), info, Instant::now())
                    .unwrap()
            };
        let standing = |update: &Update, car: i64| {
            let (_, standings) = update.scoring.as_ref().unwrap();
            let row = standings
                .iter()
                .find(|s| s.vehicle_id.get() == Some(car))
                .unwrap();
            (
                row.position.get(),
                row.class_position.get(),
                row.time_behind_leader.get(),
            )
        };
        let ahead = |update: &Update, car: i64| {
            let row = update
                .telemetry
                .iter()
                .find(|t| t.vehicle_id.get() == Some(car))
                .unwrap();
            row.gap_to_place_ahead.get()
        };

        // On the parade laps: the grid, and nobody's timed but the leader
        let parade = update(&info, STATE_PARADE_LAPS, [0; 3], [0.0; 3], 0);
        assert_eq!(standing(&parade, 2), (Some(1), Some(1), Some(0.0)));
        let (_, standings) = parade.scoring.as_ref().unwrap();
        assert!(
            standings
                .iter()
                .all(|s| s.laps_behind_leader.get().is_none())
        );
        assert_eq!(standing(&parade, 0), (Some(2), Some(2), None));
        assert_eq!(standing(&parade, 1), (Some(3), Some(3), None));
        assert_eq!(ahead(&parade, 0), None);

        // Green, the game's places, but no lap done yet: no gaps (not its placeholder)
        let green = update(&info, STATE_RACING, [2, 3, 1], [0.001, 0.002, 0.0], 0);
        assert_eq!(standing(&green, 0), (Some(2), Some(2), None));
        assert_eq!((ahead(&green, 0), ahead(&green, 1)), (None, None));
        // Over the line
        let timed = update(&info, STATE_RACING, [2, 3, 1], [1.5, 3.0, 0.0], 1);
        assert_eq!(standing(&timed, 0), (Some(2), Some(2), Some(1.5)));
        assert_eq!((ahead(&timed, 0), ahead(&timed, 1)), (Some(1.5), Some(1.5)));
        // After the green, a car without a place has none, not the grid's
        let unplaced = update(&info, STATE_RACING, [0, 3, 1], [1.5, 3.0, 0.0], 1);
        assert_eq!(standing(&unplaced, 0).0, None);

        // Heats then a feature: their grids aren't qualifying's, so no places before the green
        info.sessions.push(Session {
            num: 1,
            kind: Some("race"),
            number: 2,
            results: HashMap::new(),
        });
        let heat = update(&info, STATE_PARADE_LAPS, [0; 3], [0.0; 3], 0);
        assert_eq!(standing(&heat, 2), (None, None, None));
        info.sessions.pop();

        // The game placing a car already: its places only, none of the grid's beside them
        let some = update(&info, STATE_PARADE_LAPS, [1, 0, 0], [0.0; 3], 0);
        assert_eq!(standing(&some, 0).0, Some(1));
        assert_eq!((standing(&some, 1).0, standing(&some, 2).0), (None, None));
    }

    /// The fuel per lap is the fuel used over the last lap's distance driven: none until a whole
    /// lap's been driven, and none again after the pit lane, a move or taking on fuel.
    #[test]
    fn fuel_per_lap_is_the_last_laps_distance() {
        let close = |got: Option<f64>, want: f64| {
            assert!(got.is_some_and(|got| (got - want).abs() < 1e-9), "{got:?}");
        };
        let mut track = CarTrack {
            driven: true,
            ..CarTrack::default()
        };
        let on_track = Some(false);
        // 2 l a lap, read every tenth of a lap: a lap's worth once a lap's been driven
        let drive = |track: &mut CarTrack, from: f64, tenths: usize, per_lap: f64, fuel: f64| {
            let mut out = Vec::new();
            for i in 0..=tenths {
                track.distance = from + i as f64 * 0.1;
                out.push(track.fuel_per_lap(Some(fuel - per_lap * i as f64 * 0.1), on_track));
            }
            out
        };
        let out = drive(&mut track, 0.0, 15, 2.0, 50.0);
        assert!(out[..10].iter().all(Option::is_none), "{out:?}");
        close(out[10], 2.0);
        // Mid-sample a lap back, it's worked out between the samples either side
        track.distance = 1.55;
        close(track.fuel_per_lap(Some(50.0 - 2.0 * 1.55), on_track), 2.0);
        // Thirstier running: the window follows
        let out = drive(&mut track, 1.6, 10, 3.0, 50.0 - 2.0 * 1.6);
        close(out[10], 3.0);

        // Taking on fuel a little at a time, standing still: it starts again
        let fuel = 50.0 - 2.0 * 1.6 - 3.0;
        assert!(track.fuel_per_lap(Some(fuel - 0.5), on_track).is_some());
        assert_eq!(track.fuel_per_lap(Some(fuel - 0.49), on_track), None);
        assert_eq!(track.fuel_window.len(), 1);

        // Taking on fuel, the pit lane and a move each start again
        let out = drive(&mut track, 2.7, 10, 2.0, 80.0);
        assert!(out[..10].iter().all(Option::is_none), "{out:?}");
        close(out[10], 2.0);
        assert_eq!(track.fuel_per_lap(Some(60.0), Some(true)), None);
        track.distance += 0.1;
        assert_eq!(track.fuel_per_lap(Some(59.9), on_track), None);
        track.driven = false;
        assert_eq!(track.fuel_per_lap(Some(59.8), on_track), None);
        assert!(track.fuel_window.is_empty());
        assert_eq!(track.fuel_per_lap(None, on_track), None);
    }

    /// A replay isn't converted: what's known of the live session carries on from where it was.
    #[test]
    fn a_replay_leaves_the_live_session_as_it_was() {
        use super::super::sdk::{Value, Vars};
        use super::super::session::{Driver, Session};
        let info = SessionInfo {
            track_length: Some(1000.0),
            player: Some(0),
            drivers: HashMap::from([(0, Driver::default())]),
            sessions: vec![Session {
                num: 0,
                kind: Some("race"),
                number: 1,
                results: HashMap::new(),
            }],
            ..Default::default()
        };
        let frame = |t: f64, replay: bool| {
            Vars::of(&[
                ("SessionTime", Value::Double(t)),
                ("SessionNum", Value::Int(0)),
                ("SessionState", Value::Int(STATE_RACING)),
                ("PlayerCarIdx", Value::Int(0)),
                ("IsReplayPlaying", Value::Bool(replay)),
                ("CarIdxLapDistPct", Value::Doubles(vec![0.5])),
                ("CarIdxLapCompleted", Value::Ints(vec![2])),
            ])
        };
        let mut converter = Converter::default();
        let start = Instant::now();
        let (vars, data) = frame(500.0, false);
        assert!(
            converter
                .convert(&Frame::new(&vars, &data), &info, start)
                .is_some()
        );
        // Rewound
        let (vars, data) = frame(100.0, true);
        let replayed = converter.convert(
            &Frame::new(&vars, &data),
            &info,
            start + std::time::Duration::from_secs(1),
        );
        assert!(replayed.is_none());
        assert_eq!(converter.last_time, Some(500.0));
        assert!(converter.cars.contains_key(&0));
    }

    /// The clock going back is scored at once, however soon after the last scoring.
    #[test]
    fn a_clock_going_back_is_scored_at_once() {
        use super::super::sdk::{Value, Vars};
        use super::super::session::{Driver, Session};
        let info = SessionInfo {
            track_length: Some(1000.0),
            player: Some(0),
            drivers: HashMap::from([(0, Driver::default())]),
            sessions: vec![Session {
                num: 0,
                kind: Some("race"),
                number: 1,
                results: HashMap::new(),
            }],
            ..Default::default()
        };
        let frame = |t: f64| {
            Vars::of(&[
                ("SessionTime", Value::Double(t)),
                ("SessionNum", Value::Int(0)),
                ("SessionState", Value::Int(STATE_RACING)),
                ("PlayerCarIdx", Value::Int(0)),
                ("CarIdxLapDistPct", Value::Doubles(vec![0.5])),
                ("CarIdxLapCompleted", Value::Ints(vec![2])),
            ])
        };
        let mut converter = Converter::default();
        let start = Instant::now();
        let (vars, data) = frame(100.0);
        let first = converter.convert(&Frame::new(&vars, &data), &info, start);
        assert!(first.unwrap().scoring.is_some());
        let soon = start + std::time::Duration::from_millis(20);
        let (vars, data) = frame(100.1);
        let on = converter.convert(&Frame::new(&vars, &data), &info, soon);
        assert!(on.unwrap().scoring.is_none());
        let (vars, data) = frame(98.9);
        let back = converter.convert(&Frame::new(&vars, &data), &info, soon);
        assert!(back.unwrap().scoring.is_some());
    }

    /// Fast repair alone in the pit menu is a service selected, not none; unticked, it's no word on
    /// repairs.
    #[test]
    fn fast_repair_is_a_pit_service() {
        use super::super::sdk::{Value, Vars};
        use super::super::session::{Driver, Session};
        let info = SessionInfo {
            track_length: Some(1000.0),
            player: Some(0),
            drivers: HashMap::from([(0, Driver::default())]),
            sessions: vec![Session {
                num: 0,
                kind: Some("race"),
                number: 1,
                results: HashMap::new(),
            }],
            ..Default::default()
        };
        let (vars, data) = Vars::of(&[
            ("SessionTime", Value::Double(10.0)),
            ("SessionNum", Value::Int(0)),
            ("SessionState", Value::Int(STATE_RACING)),
            ("PlayerCarIdx", Value::Int(0)),
            ("CarIdxLapDistPct", Value::Doubles(vec![0.5])),
            ("CarIdxLapCompleted", Value::Ints(vec![2])),
            ("PitSvFlags", Value::Int(SERVICE_FAST_REPAIR as i32)),
        ]);
        let update = Converter::default()
            .convert(&Frame::new(&vars, &data), &info, Instant::now())
            .unwrap();
        let (_, standings) = update.scoring.as_ref().unwrap();
        let me = standings
            .iter()
            .find(|s| s.vehicle_id.get() == Some(0))
            .unwrap();
        assert_eq!(me.pit_service_body_repair_selected.get(), Some(true));
        assert_eq!(me.pit_service_refuel_selected.get(), Some(false));
        assert_eq!(me.pit_service_front_tires_selected.get(), Some(false));

        // Unticked, it isn't "no repair": a stop still repairs, slowly
        let (vars, data) = Vars::of(&[
            ("SessionTime", Value::Double(10.0)),
            ("SessionNum", Value::Int(0)),
            ("SessionState", Value::Int(STATE_RACING)),
            ("PlayerCarIdx", Value::Int(0)),
            ("CarIdxLapDistPct", Value::Doubles(vec![0.5])),
            ("CarIdxLapCompleted", Value::Ints(vec![2])),
            ("PitSvFlags", Value::Int(SERVICE_FUEL as i32)),
        ]);
        let update = Converter::default()
            .convert(&Frame::new(&vars, &data), &info, Instant::now())
            .unwrap();
        let (_, standings) = update.scoring.as_ref().unwrap();
        let me = standings
            .iter()
            .find(|s| s.vehicle_id.get() == Some(0))
            .unwrap();
        assert_eq!(me.pit_service_body_repair_selected.get(), None);
        assert_eq!(me.pit_service_refuel_selected.get(), Some(true));
    }

    /// The setup goes to the recorder when it's first seen, when the session info's changes, and
    /// again in a new event (the recording starts again), each time with where it was read.
    #[test]
    fn the_setup_is_sent_once_until_it_changes() {
        use super::super::sdk::{Value, Vars};
        use super::super::session::Driver;
        let arb = |value: &str| PlayerSetup {
            update_count: Some(1),
            file_name: Some("race.sto".into()),
            modified: Some(false),
            settings: vec![("Chassis/Front/ArbSize".into(), value.into())],
        };
        let mut info = SessionInfo {
            track_length: Some(1000.0),
            player: Some(0),
            drivers: HashMap::from([(0, Driver::default())]),
            setup: Some(arb("Soft")),
            ..Default::default()
        };
        let start = Instant::now();
        let mut converter = Converter::default();
        let mut time = 0.0;
        let mut convert = |info: &SessionInfo, garage: bool, pit_road: bool, in_world: bool| {
            time += 1.0;
            let (vars, data) = Vars::of(&[
                ("SessionTime", Value::Double(time)),
                ("SessionNum", Value::Int(0)),
                ("PlayerCarIdx", Value::Int(0)),
                ("CarIdxLapDistPct", Value::Doubles(vec![0.5])),
                ("CarIdxLapCompleted", Value::Ints(vec![2])),
                ("IsInGarage", Value::Bool(garage)),
                ("OnPitRoad", Value::Bool(pit_road)),
                (
                    "PlayerTrackSurface",
                    Value::Int(if in_world { 3 } else { NOT_IN_WORLD }),
                ),
            ]);
            converter
                .convert(&Frame::new(&vars, &data), info, start)
                .unwrap()
                .setup
        };
        let setup = |value: &str, source: Option<SetupSource>| Setup {
            file_name: Some("race.sto".into()),
            modified: Some(false),
            source,
            settings: vec![Setting {
                name: "Chassis/Front/ArbSize".into(),
                value: value.into(),
                ..Default::default()
            }],
        };

        // The first is the garage's, wherever the car is
        assert_eq!(
            convert(&info, false, false, true),
            Some(setup("Soft", Some(SetupSource::Garage)))
        );
        assert_eq!(convert(&info, false, false, true), None);

        info.setup = Some(arb("Medium"));
        assert_eq!(
            convert(&info, false, true, true),
            Some(setup("Medium", Some(SetupSource::PitStop)))
        );
        assert_eq!(convert(&info, false, true, true), None);
        info.setup = Some(arb("Hard"));
        assert_eq!(
            convert(&info, true, false, true),
            Some(setup("Hard", Some(SetupSource::Garage)))
        );
        // Watching between the garage and the car: not in the world, not out on track
        assert_eq!(convert(&info, false, false, false), None);
        // Out of the garage the car is in its stall, on pit road: still the garage's
        info.setup = Some(arb("Medium"));
        assert_eq!(
            convert(&info, false, true, true),
            Some(setup("Medium", Some(SetupSource::Garage)))
        );
        // Out on track, where it was changed isn't known
        info.setup = Some(arb("Soft"));
        assert_eq!(
            convert(&info, false, false, true),
            Some(setup("Soft", None))
        );
        // Back on pit road from the track: a stop
        info.setup = Some(arb("Hard"));
        assert_eq!(
            convert(&info, false, true, true),
            Some(setup("Hard", Some(SetupSource::PitStop)))
        );
        info.setup = Some(arb("Soft"));
        assert_eq!(
            convert(&info, false, false, true),
            Some(setup("Soft", None))
        );

        // A new event clears the recording: the same setup goes again
        info.event = (Some(1), None, None);
        assert_eq!(
            convert(&info, false, false, true),
            Some(setup("Soft", Some(SetupSource::Garage)))
        );
        assert_eq!(convert(&info, false, false, true), None);

        // Setup logging turned off: nothing to send
        info.setup = None;
        assert_eq!(convert(&info, false, false, true), None);
    }

    /// Leaving the garage for the grid, the car is in the world and off pit road at once: the setup
    /// sent then is still the garage's, until the race goes green.
    #[test]
    fn leaving_the_garage_for_the_grid_is_the_garages() {
        use super::super::sdk::{Value, Vars};
        use super::super::session::Driver;
        let arb = |value: &str| PlayerSetup {
            settings: vec![("Chassis/Front/ArbSize".into(), value.into())],
            ..Default::default()
        };
        let mut info = SessionInfo {
            track_length: Some(1000.0),
            player: Some(0),
            drivers: HashMap::from([(0, Driver::default())]),
            setup: Some(arb("Soft")),
            ..Default::default()
        };
        let start = Instant::now();
        let mut converter = Converter::default();
        let mut time = 0.0;
        let mut source = |info: &SessionInfo, garage: bool, pit_road: bool, state: i32| {
            time += 1.0;
            let (vars, data) = Vars::of(&[
                ("SessionTime", Value::Double(time)),
                ("SessionNum", Value::Int(0)),
                ("SessionState", Value::Int(state)),
                ("PlayerCarIdx", Value::Int(0)),
                ("CarIdxLapDistPct", Value::Doubles(vec![0.5])),
                ("CarIdxLapCompleted", Value::Ints(vec![0])),
                ("IsInGarage", Value::Bool(garage)),
                ("OnPitRoad", Value::Bool(pit_road)),
                ("PlayerTrackSurface", Value::Int(3)),
            ]);
            converter
                .convert(&Frame::new(&vars, &data), info, start)
                .unwrap()
                .setup
                .map(|setup| setup.source)
        };
        assert_eq!(
            source(&info, true, false, STATE_GET_IN_CAR),
            Some(Some(SetupSource::Garage))
        );
        // On the grid, before the green
        assert_eq!(source(&info, false, false, STATE_GET_IN_CAR), None);
        info.setup = Some(arb("Medium"));
        assert_eq!(
            source(&info, false, false, STATE_PARADE_LAPS),
            Some(Some(SetupSource::Garage))
        );
        // Into the pit lane from the pace laps: a stop, even before the green
        info.setup = Some(arb("Soft"));
        assert_eq!(
            source(&info, false, true, STATE_PARADE_LAPS),
            Some(Some(SetupSource::PitStop))
        );
        // Racing: out on track, where it was changed isn't known
        assert_eq!(source(&info, false, false, STATE_RACING), None);
        info.setup = Some(arb("Hard"));
        assert_eq!(source(&info, false, false, STATE_RACING), Some(None));
    }

    /// A new event's first update has scoring, whenever the last one went: the recording is cleared
    /// with it, and the scoring says which car is the player's before the setup is written.
    #[test]
    fn a_new_event_starts_with_scoring() {
        use super::super::sdk::{Value, Vars};
        use super::super::session::Driver;
        let mut info = SessionInfo {
            track_length: Some(1000.0),
            player: Some(0),
            drivers: HashMap::from([(0, Driver::default())]),
            ..Default::default()
        };
        let start = Instant::now();
        let mut converter = Converter::default();
        let mut convert = |info: &SessionInfo, time: f64, num: i32| {
            let (vars, data) = Vars::of(&[
                ("SessionTime", Value::Double(time)),
                ("SessionNum", Value::Int(num)),
                ("PlayerCarIdx", Value::Int(0)),
                ("CarIdxLapDistPct", Value::Doubles(vec![0.5])),
                ("CarIdxLapCompleted", Value::Ints(vec![2])),
            ]);
            converter
                .convert(&Frame::new(&vars, &data), info, start)
                .unwrap()
        };
        assert!(convert(&info, 1.0, 0).scoring.is_some());
        // The same instant: no scoring yet
        assert!(convert(&info, 2.0, 0).scoring.is_none());
        info.event = (Some(1), None, None);
        let update = convert(&info, 3.0, 0);
        assert!(update.new_event);
        assert!(update.scoring.is_some());
        // Another session of the event: its scoring names it before a setup sent with it is
        // written
        assert!(convert(&info, 4.0, 0).scoring.is_none());
        let update = convert(&info, 5.0, 1);
        assert!(!update.new_event);
        assert!(update.scoring.is_some());
    }

    /// The out lap is the game's -1 laps done: the standings keep it, so the line that ends the out
    /// lap is a lap ending there, but the telemetry's lap and laps behind go from none done.
    #[test]
    fn the_out_lap_is_no_lap_done_yet() {
        use super::super::sdk::{Value, Vars};
        use super::super::session::{Driver, Session};
        let info = SessionInfo {
            track_length: Some(1000.0),
            player: Some(0),
            drivers: (0..2).map(|car| (car, Driver::default())).collect(),
            sessions: vec![Session {
                num: 0,
                kind: Some("qualifying"),
                number: 1,
                results: HashMap::new(),
            }],
            ..Default::default()
        };
        let (vars, data) = Vars::of(&[
            ("SessionTime", Value::Double(10.0)),
            ("SessionNum", Value::Int(0)),
            ("SessionState", Value::Int(STATE_RACING)),
            ("PlayerCarIdx", Value::Int(0)),
            ("CarIdxLapDistPct", Value::Doubles(vec![0.1, 0.5])),
            ("CarIdxLapCompleted", Value::Ints(vec![-1, 3])),
        ]);
        let update = Converter::default()
            .convert(&Frame::new(&vars, &data), &info, Instant::now())
            .unwrap();
        let (_, standings) = update.scoring.as_ref().unwrap();
        let laps = |car: i64| {
            let standing = standings.iter().find(|s| s.vehicle_id.get() == Some(car));
            let telemetry = update
                .telemetry
                .iter()
                .find(|t| t.vehicle_id.get() == Some(car));
            (
                standing.unwrap().laps_completed.get(),
                telemetry.unwrap().lap.get(),
            )
        };
        assert_eq!(laps(0), (Some(-1), Some(0)));
        assert_eq!(laps(1), (Some(3), Some(3)));
    }

    #[test]
    fn a_best_lap_not_timed_whole_has_no_sectors() {
        let mut track = CarTrack::default();
        track.current = vec![None; 3];
        track.best_lap = vec![Some(30.0), Some(30.0), Some(30.0)];
        track.best_lap_total = Some(90.0);
        track.best_official = Some((90.0, 50.0));
        let sectors = vec![Some(29.0), Some(30.0), Some(29.0)];
        track.recent = vec![(88.0, sectors.clone(), 100.0)];
        // A best this timed none of
        track.best_lap_matching(Some(89.0), 101.0);
        assert_eq!(track.best_lap_total, None);
        assert!(track.best_lap.iter().all(Option::is_none));
        track.best_lap_matching(Some(88.05), 101.0);
        assert_eq!(track.best_lap_total, Some(88.0));

        // A little better: the lap that just set it, not the one before within as little
        track.recent.push((87.97, sectors.clone(), 150.0));
        track.best_lap_matching(Some(87.98), 150.5);
        assert_eq!(track.best_lap_total, Some(87.97));
        // Looked for a moment only: a lap finished long after isn't it
        track.best_lap_matching(Some(87.9), 160.0);
        track.recent.push((87.9, sectors, 200.0));
        track.best_lap_matching(Some(87.9), 200.0);
        assert_eq!(track.best_lap_total, None);
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
        assert_eq!(track.lap_offset(), 0);
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

    /// Push to pass on and, in races, how much is left: from each car's arrays (the player's own
    /// variables where there are none), once the car's shown it has it.
    #[test]
    fn push_to_pass_left_only_in_races() {
        use super::super::sdk::{Value, Vars};
        use super::super::session::{Driver, Session};
        let info = |kind: &'static str| SessionInfo {
            track_length: Some(1000.0),
            player: Some(0),
            drivers: HashMap::from([(0, Driver::default()), (1, Driver::default())]),
            sessions: vec![Session {
                num: 0,
                kind: Some(kind),
                number: 1,
                results: HashMap::new(),
            }],
            ..Default::default()
        };
        let left = |update: &Update, car: i64| {
            let row = update
                .telemetry
                .iter()
                .find(|t| t.vehicle_id.get() == Some(car))
                .unwrap();
            (
                row.state.push_to_pass_engaged.get(),
                row.state.push_to_pass_activations_left.get(),
            )
        };

        // The player's own variables, without arrays
        let (vars, data) = Vars::of(&[
            ("SessionTime", Value::Double(10.0)),
            ("SessionNum", Value::Int(0)),
            ("PlayerCarIdx", Value::Int(0)),
            ("LapDistPct", Value::Double(0.5)),
            ("P2P_Status", Value::Bool(false)),
            ("P2P_Count", Value::Int(5)),
        ]);
        let frame = Frame::new(&vars, &data);
        for (kind, expected) in [("race", Some(5)), ("practice", None)] {
            let update = Converter::default()
                .convert(&frame, &info(kind), Instant::now())
                .unwrap();
            assert_eq!(left(&update, 0), (Some(false), expected), "{kind}");
        }

        // Every car's from the arrays: the second car on with 3 left; the first shows nothing (no
        // push to pass, or none yet): off, and nothing left said
        let (vars, data) = Vars::of(&[
            ("SessionTime", Value::Double(10.0)),
            ("SessionNum", Value::Int(0)),
            ("PlayerCarIdx", Value::Int(0)),
            ("CarIdxLapDistPct", Value::Doubles(vec![0.5, 0.6])),
            ("CarIdxP2P_Status", Value::Bools(vec![false, true])),
            ("CarIdxP2P_Count", Value::Ints(vec![0, 3])),
        ]);
        let mut converter = Converter::default();
        let update = converter
            .convert(&Frame::new(&vars, &data), &info("race"), Instant::now())
            .unwrap();
        assert_eq!(left(&update, 0), (Some(false), None));
        assert_eq!(left(&update, 1), (Some(true), Some(3)));
        // Shown once, its count's what's left from then on, used up too
        let (vars, data) = Vars::of(&[
            ("SessionTime", Value::Double(10.1)),
            ("SessionNum", Value::Int(0)),
            ("PlayerCarIdx", Value::Int(0)),
            ("CarIdxLapDistPct", Value::Doubles(vec![0.5, 0.601])),
            ("CarIdxP2P_Status", Value::Bools(vec![false, false])),
            ("CarIdxP2P_Count", Value::Ints(vec![0, 0])),
        ]);
        let update = converter
            .convert(&Frame::new(&vars, &data), &info("race"), Instant::now())
            .unwrap();
        assert_eq!(left(&update, 1), (Some(false), Some(0)));
    }

    /// The carcass temperatures and tread left are a reading, stamped with when it last changed:
    /// the first update seen, then whenever any of a wheel's values changes, and held otherwise,
    /// the car in the world or not. Another car's are its own.
    #[test]
    fn stamps_the_tyres_last_reading() {
        use super::super::sdk::{Value, Vars};
        use super::super::session::Driver;
        let info = SessionInfo {
            track_length: Some(1000.0),
            player: Some(0),
            drivers: HashMap::from([(0, Driver::default()), (1, Driver::default())]),
            ..Default::default()
        };
        let mut converter = Converter::default();
        // The player's front and left rear readings at `time`, the front left's tread in the
        // middle `lf_wear`, nothing for the rear right; out of the world without `in_world`
        let mut update = |time: f64, lf_wear: f64, in_world: bool, player: i32| {
            let mut values: Vec<(String, Value)> = vec![
                ("SessionTime".into(), Value::Double(time)),
                ("SessionNum".into(), Value::Int(0)),
                ("PlayerCarIdx".into(), Value::Int(player)),
                (
                    "LapDistPct".into(),
                    Value::Double(if in_world { 0.5 } else { -1.0 }),
                ),
            ];
            for prefix in ["LF", "RF", "LR"] {
                let wear = if prefix == "LF" { lf_wear } else { 0.96 };
                for (name, value) in [
                    ("tempCL", 80.0),
                    ("tempCM", 84.0),
                    ("tempCR", 88.0),
                    ("wearL", 0.97),
                    ("wearM", wear),
                    ("wearR", 0.95),
                    ("coldPressure", 172.0),
                ] {
                    values.push((format!("{prefix}{name}"), Value::Double(value)));
                }
            }
            let values: Vec<(&str, Value)> = values
                .iter()
                .map(|(n, v)| (n.as_str(), v.clone()))
                .collect();
            let (vars, data) = Vars::of(&values);
            let update = converter
                .convert(&Frame::new(&vars, &data), &info, Instant::now())
                .unwrap();
            update
                .telemetry
                .first()
                .map(|t| t.wheels.map(|w| w.tire_reading_time.get()))
        };
        let (vars, data) = Vars::doubles(&[
            ("LFtempCL", 80.0),
            ("LFtempCM", 84.0),
            ("LFtempCR", 88.0),
            ("LFwearL", 0.97),
            ("LFwearM", 0.96),
            ("LFwearR", 0.95),
            ("LFcoldPressure", 172.0),
        ]);
        let mut out = bono_telemetry::default();
        player_telemetry(&Frame::new(&vars, &data), &SessionInfo::default(), &mut out);
        let wheel = &out.wheels[0];
        assert_eq!(wheel.tire_carcass_temperature.get(), Some(84.0));
        assert_eq!(wheel.tire_inner_temperature_left.get(), Some(80.0));
        assert!((wheel.tire_wear.get().unwrap() - 0.96).abs() < 1e-9);
        assert_eq!(wheel.tire_cold_pressure.get(), Some(172.0));
        assert_eq!(wheel.tire_reading_time.get(), None);

        // Seen first: from now. The rear right has no reading
        let first = [Some(10.0), Some(10.0), Some(10.0), None];
        assert_eq!(update(10.0, 0.96, true, 0), Some(first));
        // Held while nothing changes
        assert_eq!(update(10.1, 0.96, true, 0), Some(first));
        // One value of one wheel changes: that wheel's reading is new, the others' held
        let changed = [Some(10.2), Some(10.0), Some(10.0), None];
        assert_eq!(update(10.2, 0.9, true, 0), Some(changed));
        assert_eq!(update(30.0, 0.9, true, 0), Some(changed));
        // Changed out of the world (in the garage, say): from then, not from when it's back
        assert_eq!(update(40.0, 0.8, false, 0), None);
        let garage = [Some(40.0), Some(10.0), Some(10.0), None];
        assert_eq!(update(50.0, 0.8, true, 0), Some(garage));
        // Another car, with the same values: from when it's first seen
        assert_eq!(
            update(60.0, 0.8, true, 1),
            Some([Some(60.0), Some(60.0), Some(60.0), None])
        );
    }

    /// The player's axes and rotations, from iRacing's directions (see the module's doc) to the
    /// recording's.
    #[test]
    fn turns_the_players_axes_round() {
        use std::f64::consts::PI;
        let player = |values: &[(&str, f64)]| {
            let (vars, data) = super::super::sdk::Vars::doubles(values);
            let mut out = bono_telemetry::default();
            player_telemetry(&Frame::new(&vars, &data), &SessionInfo::default(), &mut out);
            out
        };
        // Turning left at speed, sliding outwards, nose down, wheel a quarter turn left of a
        // span of two turns, the wheel pulling back right, heading west
        let out = player(&[
            ("VelocityX", 30.0),
            ("VelocityY", -3.0),
            ("VelocityZ", 0.5),
            ("LongAccel", -4.0),
            ("LatAccel", 12.0),
            ("VertAccel", 9.8),
            ("Pitch", 0.02),
            ("PitchRate", 0.1),
            ("YawRate", 0.4),
            ("YawNorth", 1.5 * PI),
            ("SteeringWheelAngle", PI / 2.0),
            ("SteeringWheelAngleMax", 4.0 * PI),
            ("SteeringWheelTorque", -5.0),
        ]);
        let get = |v: bono_sim::sys::bono_opt_f64| v.get().unwrap();
        assert_eq!(get(out.velocity_longitudinal), 30.0);
        assert_eq!(get(out.velocity_lateral), 3.0);
        assert_eq!(get(out.velocity_vertical), 0.5);
        assert_eq!(get(out.acceleration_longitudinal), -4.0);
        assert_eq!(get(out.acceleration_lateral), -12.0);
        assert_eq!(get(out.acceleration_vertical), 9.8);
        assert_eq!(get(out.pitch), -0.02);
        assert_eq!(get(out.pitch_rate), -0.1);
        assert_eq!(get(out.yaw_rate), -0.4);
        assert!((get(out.heading) + PI / 2.0).abs() < 1e-9);
        // Sliding right of where it points: travel's a little right of west
        let slip = (3.0f64).atan2(30.0);
        assert!((get(out.course) - (-PI / 2.0 + slip)).abs() < 1e-9);
        assert!((get(out.steering) + 0.25).abs() < 1e-9);
        assert_eq!(get(out.steering_shaft_torque), 5.0);
        assert_eq!(out.roll.get(), None);
        assert_eq!(out.roll_rate.get(), None);

        // Reversing: travel's behind the nose. Too slow, and there's no telling
        let out = player(&[("VelocityX", -5.0), ("VelocityY", 0.0), ("YawNorth", 0.0)]);
        assert!((get(out.course).abs() - PI).abs() < 1e-9);
        let out = player(&[("VelocityX", 0.5), ("VelocityY", 0.0), ("YawNorth", 0.0)]);
        assert_eq!(out.course.get(), None);
        // No span, no steering; past the lock, at the lock
        let out = player(&[("SteeringWheelAngle", 1.0)]);
        assert_eq!(out.steering.get(), None);
        let out = player(&[("SteeringWheelAngle", -9.0), ("SteeringWheelAngleMax", 4.0)]);
        assert_eq!(get(out.steering), 1.0);
    }

    #[test]
    fn the_wind_blows_from_its_bearing() {
        use std::f64::consts::PI;
        let wind = |speed: f64, from: f64| {
            let (vars, data) =
                super::super::sdk::Vars::doubles(&[("WindVel", speed), ("WindDir", from)]);
            steady_wind(None, wind(&Frame::new(&vars, &data)))
        };
        // From the north: the air moves south, +z
        assert_eq!(wind(8.0, 0.0), Some((0.0, 8.0)));
        // From the east: west, +x
        assert_eq!(wind(8.0, PI / 2.0), Some((8.0, 0.0)));
        // From the south-west: north-east, -x and -z
        assert_eq!(wind(4.0, 1.25 * PI), Some((-3.0, -3.0)));
        // Nothing west or east is no wind either way (not -0)
        let (x, _) = wind(8.0, -0.01).unwrap();
        assert!(x == 0.0 && x.is_sign_positive());
        assert_eq!(wind(-1.0, 0.0), None);

        // On the edge between two steps, it stays on one; a step away, it moves
        let mut last = steady_wind(None, Some((0.0, 8.249)));
        assert_eq!(last, Some((0.0, 8.0)));
        for z in [8.251, 8.249, 8.3, 8.45] {
            last = steady_wind(last, Some((0.0, z)));
            assert_eq!(last, Some((0.0, 8.0)), "{z}");
        }
        assert_eq!(steady_wind(last, Some((0.0, 8.6))), Some((0.0, 8.5)));
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
