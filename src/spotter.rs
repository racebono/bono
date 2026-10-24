//! The spotter: follows the running simulator's telemetry about thirty times a second and calls
//! cars alongside, flags, the car's own alerts and the weather the moment they happen, without
//! anyone asking. A port of my-engineer's `spotter.cpp`.
//!
//! It reads the database on a thread of its own, with a read-only connection of its own (closed as
//! soon as the game stops, so the plugin can delete the file), and turns what changed into calls.
//! They're said in the engineer's voice: urgent ones (traffic, the pit limiter, full course
//! yellows, red flags) cut in over the engineer, which carries on afterwards; the rest wait for it
//! to finish.

use crate::{
    arrstr,
    config::{ArrayStr, Config, SpotterConfig},
    engineer::Engineer,
    sim::Sim,
    synthesizer::{Drain, Drained, Effects, Speech, Synthesizer, plays_on},
};
use bono_i18n::t;
use rusqlite::{Connection, OpenFlags, OptionalExtension, Row};
use sdl3::audio::{AudioDevice, AudioStreamOwner};
use std::{
    collections::{HashMap, HashSet, VecDeque},
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    time::{Duration, Instant},
};
use tokio::{runtime::Handle, sync::oneshot, sync::watch};

/// Rows searched either side of the player's row for the other cars: the most cars a simulator
/// maps, plus slack.
const FRAME_WINDOW: i64 = 110;
/// Another car's row further than this (in session time) from the player's isn't used.
const MAX_FRAME_AGE: f64 = 0.5;
/// How long the database may be busy (the recorder committing) before a poll gives up.
const BUSY_TIMEOUT: Duration = Duration::from_millis(200);
/// How long a call is still worth saying.
const URGENT_FOR: Duration = Duration::from_millis(1500);
const OTHER_FOR: Duration = Duration::from_secs(10);
/// A gap behind called this close is "close behind" rather than "within a second".
const CLOSE_BEHIND: f64 = 0.15;
/// The gap behind isn't called in the session's first seconds (the start, the field nose to tail).
const GAP_BEHIND_AFTER: f64 = 20.0;
/// Calls synthesized ahead, so the first of each is said without waiting for the voice.
const WARM: [&str; 6] = [
    "car left",
    "car right",
    "three wide",
    "clear left",
    "clear right",
    "clear all round",
];

/// Something the spotter says. `text` is English (a `{}` in it takes `number`); [`Raised::words`]
/// gives it in the pilot's language.
#[derive(Debug, Clone, PartialEq)]
pub struct Raised {
    pub text: &'static str,
    pub number: Option<i64>,
    /// Traffic, the limiter and flags that change what the pilot does right now: stale within a
    /// second or two, so they interrupt the engineer
    pub urgent: bool,
    /// When it happened, from the player's car
    pub session_time: f64,
}

impl Raised {
    fn valid_at(&self, at: Instant, now: Instant) -> bool {
        now.duration_since(at) < if self.urgent { URGENT_FOR } else { OTHER_FOR }
    }

    /// Traffic updates replace the state of the sides they describe. Unrelated alerts do not.
    fn traffic_sides(&self) -> u8 {
        match self.text {
            "car left" | "still there left" | "clear left" => 1,
            "car right" | "still there right" | "clear right" => 2,
            "three wide" | "clear all round" => 3,
            _ => 0,
        }
    }

    /// What's said, in `lang` (or in English when there's no translation).
    pub fn words(&self, lang: ArrayStr<2>) -> String {
        let text = translate(self.text, lang);
        match self.number {
            Some(number) => text.replacen("{}", &number.to_string(), 1),
            None => text.to_owned(),
        }
    }
}

/// Urgent state updates supersede only warnings about the same thing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WarningCategory {
    Traffic,
    Limiter,
    Flag,
    Other(&'static str),
}

impl Raised {
    fn warning_category(&self) -> WarningCategory {
        match self.text {
            "car left" | "car right" | "three wide" | "clear left" | "clear right"
            | "clear all round" | "still there left" | "still there right" => {
                WarningCategory::Traffic
            }
            "limiter, limiter" => WarningCategory::Limiter,
            "full course yellow" | "green flag, green flag" | "red flag" => WarningCategory::Flag,
            text => WarningCategory::Other(text),
        }
    }

    /// A traffic update replaces only the sides it describes; other urgent updates
    /// replace the older state in their category.
    fn supersedes(&self, older: &Raised) -> bool {
        if !self.urgent || !older.urgent {
            return false;
        }
        match (self.warning_category(), older.warning_category()) {
            (WarningCategory::Traffic, WarningCategory::Traffic) => {
                self.traffic_sides() & older.traffic_sides() != 0
            }
            (new, old) => new == old,
        }
    }
}

pub struct Spotter {
    /// Following the database at this path
    running: Option<Running>,
    /// Calls to say, oldest first, with when they came in
    queue: VecDeque<(Raised, Instant)>,
    /// The call being said
    saying: Option<Saying>,
    /// Calls already synthesized, by (language, voice, radio effect, words)
    cache: HashMap<CacheKey, Arc<Speech>>,
    /// Calls being synthesized ahead
    warming: Vec<(CacheKey, oneshot::Receiver<anyhow::Result<Speech>>)>,
}

type CacheKey = (ArrayStr<2>, PathBuf, u32, String);

struct Running {
    db: PathBuf,
    settings: watch::Sender<SpotterConfig>,
    calls: mpsc::Receiver<Raised>,
    stop: Arc<AtomicBool>,
}

impl Drop for Running {
    fn drop(&mut self) {
        // The thread notices within a poll, and closes its connection as it ends
        self.stop.store(true, Ordering::Relaxed);
    }
}

enum Saying {
    Synthesizing {
        key: CacheKey,
        recv: oneshot::Receiver<anyhow::Result<Speech>>,
        call: Raised,
        at: Instant,
    },
    /// Dropping the stream stops it
    Playing(AudioStreamOwner, Drain),
}

impl Spotter {
    pub fn new() -> Self {
        Self {
            running: None,
            queue: VecDeque::new(),
            saying: None,
            cache: HashMap::new(),
            warming: Vec::new(),
        }
    }

    /// Called every frame: follows the running simulator's database while the spotter is on, and
    /// says its calls, holding the engineer while it does.
    pub fn update(
        &mut self,
        rt: &Handle,
        sim: Option<&mut Sim>,
        config: &Config,
        synth: &mut Synthesizer,
        speaker: Option<&AudioDevice>,
        engineer: &mut Engineer,
    ) {
        let settings = config.get().spotter.clone();
        let db = sim
            .filter(|_| *settings.enabled())
            .and_then(|sim| sim.database())
            .map(|pool| pool.database().path().to_owned());

        match (&mut self.running, db) {
            (Some(running), Some(db)) if running.db == db => {
                running.settings.send_if_modified(|current| {
                    let changed = *current != settings;
                    *current = settings.clone();
                    changed
                });
            }
            (_, Some(db)) => {
                self.running = Some(Running::start(db, settings.clone()));
                self.queue.clear();
            }
            (_, None) => {
                if self.running.take().is_some() {
                    self.queue.clear();
                    self.saying = None;
                }
            }
        }

        let lang = self.language(config, &settings, engineer);
        if self.running.is_some() {
            while let Some(call) = self
                .running
                .as_ref()
                .and_then(|running| running.calls.try_recv().ok())
            {
                let at = Instant::now();
                self.discard_stale_synthesis(at, Some(&call));
                Self::enqueue(&mut self.queue, call, at);
            }
            self.warm(rt, config, synth, lang);
        }

        self.say(rt, config, synth, speaker, engineer, lang);
    }

    /// The speaker `which` is gone (unplugged): the call on it is dropped, so the spotter says the
    /// next ones and the engineer is let go.
    pub fn speaker_removed(&mut self, which: u32) {
        if let Some(Saying::Playing(stream, _)) = &self.saying
            && plays_on(stream, which)
        {
            log::warn!("The speaker was removed while the spotter was speaking");
            self.saying = None;
        }
    }

    fn enqueue(queue: &mut VecDeque<(Raised, Instant)>, call: Raised, at: Instant) {
        queue.retain(|(queued, _)| !call.supersedes(queued));
        queue.push_back((call, at));
    }

    /// The calls' language: the configured one, or the pilot's; the engineer's fallback without
    /// a voice for it (English without any voice, when nothing can say them anyway).
    fn language(
        &self,
        config: &Config,
        settings: &SpotterConfig,
        engineer: &Engineer,
    ) -> ArrayStr<2> {
        let lang = settings.language().unwrap_or(engineer.language());
        engineer.voice(config, lang).unwrap_or(arrstr!("en"))
    }

    fn key(config: &Config, lang: ArrayStr<2>, words: String) -> CacheKey {
        let config = config.get();
        let voice = config
            .voices()
            .get(&lang)
            .map(|path| path.as_std_path().to_owned());
        (
            lang,
            voice.unwrap_or_default(),
            config.radio_effect().to_bits(),
            words,
        )
    }

    fn effects(config: &Config) -> Effects {
        // Calls are short and come in bursts: no roger beep after each
        Effects {
            beep_volume: 0.0,
            ..config.get().synth_effects()
        }
    }

    /// Synthesizes the traffic calls ahead, so the first ones aren't late.
    fn warm(&mut self, rt: &Handle, config: &Config, synth: &mut Synthesizer, lang: ArrayStr<2>) {
        self.warming
            .retain_mut(|(key, recv)| match recv.try_recv() {
                Ok(Ok(speech)) => {
                    self.cache.insert(key.clone(), Arc::new(speech));
                    false
                }
                Ok(Err(_)) | Err(oneshot::error::TryRecvError::Closed) => false,
                Err(oneshot::error::TryRecvError::Empty) => true,
            });

        for text in WARM {
            let words = translate(text, lang).to_owned();
            let key = Self::key(config, lang, words.clone());
            if self.cache.contains_key(&key) || self.warming.iter().any(|(k, _)| *k == key) {
                continue;
            }
            let recv = synth.synthesize(rt, lang, words, Self::effects(config));
            self.warming.push((key, recv));
        }
    }

    /// Drop the receiver promptly, so an obsolete call no longer holds the engineer.
    fn discard_stale_synthesis(&mut self, now: Instant, newer: Option<&Raised>) {
        if let Some(Saying::Synthesizing { call, at, .. }) = &self.saying {
            let superseded = newer.is_some_and(|newer| newer.supersedes(call));
            if !call.valid_at(*at, now) || superseded {
                self.saying = None;
            }
        }
    }

    fn say(
        &mut self,
        rt: &Handle,
        config: &Config,
        synth: &mut Synthesizer,
        speaker: Option<&AudioDevice>,
        engineer: &mut Engineer,
        lang: ArrayStr<2>,
    ) {
        self.discard_stale_synthesis(Instant::now(), None);

        // The call being said
        match &mut self.saying {
            Some(Saying::Synthesizing { key, recv, .. }) => match recv.try_recv() {
                Ok(Ok(speech)) => {
                    let speech = Arc::new(speech);
                    self.cache.insert(key.clone(), speech.clone());
                    self.saying = self.play(config, speaker, &speech);
                }
                Ok(Err(e)) => {
                    log::warn!("Error synthesizing a spotter call: {e:#}");
                    self.saying = None;
                }
                Err(oneshot::error::TryRecvError::Closed) => self.saying = None,
                Err(oneshot::error::TryRecvError::Empty) => {}
            },
            // A speaker that stops taking audio doesn't keep the spotter (and the engineer) waiting
            Some(Saying::Playing(stream, drain)) => {
                match drain.check(stream.queued_bytes().ok(), false, Instant::now()) {
                    Drained::Playing => {}
                    Drained::Done => self.saying = None,
                    Drained::Stalled => {
                        log::warn!("The speaker stopped playing a spotter call");
                        self.saying = None;
                    }
                }
            }
            None => {}
        }

        // The next call: urgent ones first, the rest once the engineer is quiet
        if self.saying.is_none() {
            let now = Instant::now();
            self.queue.retain(|(call, at)| call.valid_at(*at, now));

            let next = match self.queue.iter().position(|(call, _)| call.urgent) {
                Some(urgent) => Some(urgent),
                None if !engineer.is_speaking() => (!self.queue.is_empty()).then_some(0),
                None => None,
            };

            if let Some((call, at)) = next.and_then(|next| self.queue.remove(next)) {
                let words = call.words(lang);
                let key = Self::key(config, lang, words.clone());
                self.saying = match self.cache.get(&key).cloned() {
                    Some(speech) => self.play(config, speaker, &speech),
                    None => {
                        let recv = synth.synthesize(rt, lang, words, Self::effects(config));
                        Some(Saying::Synthesizing {
                            key,
                            recv,
                            call,
                            at,
                        })
                    }
                };
            }
        }

        // The engineer waits while the spotter speaks, and carries on after
        engineer.hold(self.saying.is_some());
    }

    fn play(
        &self,
        config: &Config,
        speaker: Option<&AudioDevice>,
        speech: &Speech,
    ) -> Option<Saying> {
        let speaker = speaker?;
        let gain = {
            let config = config.get();
            *config.master_volume() * *config.spotter_volume()
        };
        let speech = Speech {
            samples: speech.samples.iter().map(|sample| sample * gain).collect(),
            sample_rate: speech.sample_rate,
        };
        match speech.play(speaker) {
            Ok(stream) => Some(Saying::Playing(stream, Drain::new(Instant::now()))),
            Err(e) => {
                log::warn!("Error playing a spotter call: {e}");
                None
            }
        }
    }
}

impl Running {
    fn start(db: PathBuf, settings: SpotterConfig) -> Self {
        let (settings, settings_recv) = watch::channel(settings);
        let (send, calls) = mpsc::channel();
        let stop = Arc::new(AtomicBool::new(false));

        let thread_db = db.clone();
        let thread_stop = stop.clone();
        let spawned = std::thread::Builder::new()
            .name("spotter".into())
            .spawn(move || follow(&thread_db, settings_recv, send, &thread_stop));
        if let Err(e) = spawned {
            log::error!("Couldn't start the spotter: {e}");
        }

        log::info!("Spotter following {}", db.display());
        Self {
            db,
            settings,
            calls,
            stop,
        }
    }
}

/// Polls the database until stopped, sending the calls.
fn follow(
    db: &Path,
    settings: watch::Receiver<SpotterConfig>,
    calls: mpsc::Sender<Raised>,
    stop: &AtomicBool,
) {
    let conn = match open(db) {
        Ok(conn) => conn,
        Err(e) => {
            log::error!("The spotter couldn't open {}: {e}", db.display());
            return;
        }
    };

    let mut logic = Logic::default();
    let mut last_id = None;
    let mut failing = false;
    while !stop.load(Ordering::Relaxed) {
        let start = Instant::now();
        let config = settings.borrow().clone();

        match poll(&conn, &mut logic, &config, &mut last_id) {
            Ok(raised) => {
                failing = false;
                for call in raised {
                    if calls.send(call).is_err() {
                        return;
                    }
                }
            }
            // The recorder is busy, or hasn't made its tables yet: the next poll tries again
            Err(e) => {
                if !std::mem::replace(&mut failing, true) {
                    log::debug!("Spotter poll failed: {e}");
                }
            }
        }

        let period = Duration::from_secs_f64(config.poll_interval().clamp(0.005, 1.0));
        std::thread::sleep(period.saturating_sub(start.elapsed()));
    }
}

fn open(db: &Path) -> rusqlite::Result<Connection> {
    let conn = Connection::open_with_flags(
        db,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    conn.busy_timeout(BUSY_TIMEOUT)?;
    Ok(conn)
}

/// The calls for the player's newest row, if it's new.
fn poll(
    conn: &Connection,
    logic: &mut Logic,
    config: &SpotterConfig,
    last_id: &mut Option<i64>,
) -> rusqlite::Result<Vec<Raised>> {
    let Some(car) = latest(conn)? else {
        return Ok(Vec::new());
    };
    if Some(car.id) == *last_id {
        return Ok(Vec::new());
    }
    *last_id = Some(car.id);

    let player = player(conn, car)?;
    let others = frame(conn, &player.car)?;
    let wheels = wheels(conn, player.car.id)?;
    let scoring = scoring(conn, None)?;
    Ok(logic.update(config, &player, &others, &wheels, scoring.as_ref()))
}

/// Runs the spotter over a recording, as fast as it can, from the player's first row to the last:
/// every call it would have made, for testing without the game.
#[cfg_attr(not(test), allow(dead_code))]
pub fn replay(db: &Path, config: &SpotterConfig) -> rusqlite::Result<Vec<Raised>> {
    let conn = open(db)?;
    let mut statement = conn.prepare(&format!(
        "SELECT {CAR_COLUMNS} FROM telemetry WHERE is_player = 1 ORDER BY id"
    ))?;
    let cars = statement
        .query_map([], car)?
        .collect::<rusqlite::Result<Vec<_>>>()?;

    let mut logic = Logic::default();
    let mut calls = Vec::new();
    for car in cars.into_iter().filter(|car| car.time.is_some()) {
        let time = car.time;
        let player = player(&conn, car)?;
        let others = frame(&conn, &player.car)?;
        let wheels = wheels(&conn, player.car.id)?;
        let scoring = scoring(&conn, time)?;
        calls.extend(logic.update(config, &player, &others, &wheels, scoring.as_ref()));
    }
    Ok(calls)
}

// --------------------------------------------------------------------------- what it reads

const CAR_COLUMNS: &str =
    "id, vehicle_id, session_time, in_pit_lane, speed, position_x, position_y, position_z, heading";

#[derive(Debug, Clone, Default)]
struct Car {
    id: i64,
    vehicle: i64,
    time: Option<f64>,
    in_pit_lane: bool,
    speed: Option<f64>,
    x: Option<f64>,
    y: Option<f64>,
    z: Option<f64>,
    heading: Option<f64>,
}

/// The player's car, with what only the player's own alerts need.
#[derive(Debug, Clone, Default)]
struct Player {
    car: Car,
    lap_invalidated: bool,
    engine_overheating: bool,
    gap_behind: Option<f64>,
    /// Unknown isn't off: some simulators only report the limiter while the pilot drives
    limiter: Option<bool>,
    parts_detached: bool,
}

#[derive(Debug, Clone, Default)]
struct Wheels {
    /// Wheel (0 front left .. 3 rear right) -> flat
    flat: [Option<bool>; 4],
    /// How many tyres are on a wet surface
    wet: usize,
}

fn car(row: &Row) -> rusqlite::Result<Car> {
    Ok(Car {
        id: row.get(0)?,
        vehicle: row.get::<_, Option<i64>>(1)?.unwrap_or(-1),
        time: row.get(2)?,
        in_pit_lane: row.get::<_, Option<bool>>(3)?.unwrap_or(false),
        speed: row.get(4)?,
        x: row.get(5)?,
        y: row.get(6)?,
        z: row.get(7)?,
        heading: row.get(8)?,
    })
}

#[derive(Debug, Clone, Default, PartialEq)]
struct Scoring {
    full_course_yellow: Option<String>,
    phase: Option<String>,
    /// A local yellow in any sector
    yellow: bool,
    /// The flag shown to the player
    flag: Option<String>,
    penalties: Option<i64>,
    /// 0-1
    rain: Option<f64>,
    /// 0-1
    wetness: Option<f64>,
    session_type: Option<String>,
    session_end_time: Option<f64>,
    /// The newest forecast for this session: (how far through it, 0-1; chance of rain, 0-1), in
    /// order
    forecast: Vec<(f64, f64)>,
}

/// The player's newest row.
fn latest(conn: &Connection) -> rusqlite::Result<Option<Car>> {
    conn.prepare_cached(&format!(
        "SELECT {CAR_COLUMNS} FROM telemetry WHERE is_player = 1 ORDER BY id DESC LIMIT 1"
    ))?
    .query_row([], car)
    .optional()
}

/// The player's row with its own alerts, and the car's state in force at it.
fn player(conn: &Connection, car: Car) -> rusqlite::Result<Player> {
    let (lap_invalidated, engine_overheating, gap_behind) = conn
        .prepare_cached(
            "SELECT lap_invalidated, engine_overheating, gap_to_place_behind FROM telemetry \
             WHERE id = ?1",
        )?
        .query_row([car.id], |row| {
            Ok((
                row.get::<_, Option<bool>>(0)?.unwrap_or(false),
                row.get::<_, Option<bool>>(1)?.unwrap_or(false),
                row.get(2)?,
            ))
        })?;

    let state = conn
        .prepare_cached(
            "SELECT speed_limiter_on, parts_detached FROM car_state \
             WHERE vehicle_id = ?1 AND telemetry_id <= ?2 ORDER BY telemetry_id DESC LIMIT 1",
        )?
        .query_row([car.vehicle, car.id], |row| {
            Ok((
                row.get::<_, Option<bool>>(0)?,
                row.get::<_, Option<bool>>(1)?,
            ))
        })
        .optional()?;
    let (limiter, parts_detached) = state.unwrap_or_default();

    Ok(Player {
        car,
        lap_invalidated,
        engine_overheating,
        gap_behind,
        limiter,
        parts_detached: parts_detached.unwrap_or(false),
    })
}

/// The player's tyres at their row.
fn wheels(conn: &Connection, telemetry_id: i64) -> rusqlite::Result<Wheels> {
    let mut statement = conn.prepare_cached(
        "SELECT wheel, tire_flat, surface_type FROM telemetry_wheel WHERE telemetry_id = ?1",
    )?;
    let mut rows = statement.query([telemetry_id])?;
    let mut wheels = Wheels::default();
    while let Some(row) = rows.next()? {
        let wheel = row.get::<_, Option<i64>>(0)?;
        if let Some(flat) = wheel
            .and_then(|wheel| usize::try_from(wheel).ok())
            .and_then(|wheel| wheels.flat.get_mut(wheel))
        {
            *flat = Some(row.get::<_, Option<bool>>(1)?.unwrap_or(false));
        }
        // 1 is a wet surface
        if row.get::<_, Option<i64>>(2)? == Some(1) {
            wheels.wet += 1;
        }
    }
    Ok(wheels)
}

/// Every other car near the player's row, moved to the player's time.
fn frame(conn: &Connection, player: &Car) -> rusqlite::Result<Vec<Car>> {
    let Some(time) = player.time else {
        return Ok(Vec::new());
    };

    let mut statement = conn.prepare_cached(&format!(
        "SELECT {CAR_COLUMNS} FROM telemetry WHERE id BETWEEN ?1 AND ?2"
    ))?;
    let rows = statement.query_map([player.id - FRAME_WINDOW, player.id + FRAME_WINDOW], car)?;

    // Per car, the row closest in time (then in id) to the player's
    let mut best = HashMap::<i64, ((f64, i64), Car)>::new();
    for row in rows {
        let car = row?;
        let Some(car_time) = car.time else {
            continue;
        };
        if car.vehicle == player.vehicle {
            continue;
        }
        let key = ((car_time - time).abs(), (car.id - player.id).abs());
        if key.0 >= MAX_FRAME_AGE {
            continue;
        }
        if best.get(&car.vehicle).is_none_or(|(best, _)| key < *best) {
            best.insert(car.vehicle, (key, car));
        }
    }

    Ok(best
        .into_values()
        .map(|(_, car)| at_time(car, time))
        .collect())
}

/// The newest session row, the player's newest standing and the session's newest forecast (at or
/// before `upto`, when replaying).
fn scoring(conn: &Connection, upto: Option<f64>) -> rusqlite::Result<Option<Scoring>> {
    let session = conn
        .prepare_cached(
            "SELECT full_course_yellow, phase, local_yellow_1, local_yellow_2, local_yellow_3, \
             rain, track_wetness_average, session_type, session_end_time \
             FROM session WHERE ?1 IS NULL OR session_time <= ?1 ORDER BY id DESC LIMIT 1",
        )?
        .query_row([upto], |row| {
            let yellow = |i| row.get::<_, Option<bool>>(i).map(|y| y.unwrap_or(false));
            Ok(Scoring {
                full_course_yellow: row.get(0)?,
                phase: row.get(1)?,
                yellow: yellow(2)? || yellow(3)? || yellow(4)?,
                rain: row.get(5)?,
                wetness: row.get(6)?,
                session_type: row.get(7)?,
                session_end_time: row.get(8)?,
                ..Scoring::default()
            })
        })
        .optional()?;
    let Some(mut scoring) = session else {
        return Ok(None);
    };

    // Live, only the newest rows are looked through: the player's standing changes every sector
    let standing = match upto {
        None => conn
            .prepare_cached(
                "SELECT flag, penalties FROM standings WHERE is_player = 1 \
                 AND id > (SELECT coalesce(max(id), 0) - 5000 FROM standings) \
                 ORDER BY id DESC LIMIT 1",
            )?
            .query_row([], |row| Ok((row.get(0)?, row.get(1)?)))
            .optional()?,
        Some(upto) => conn
            .prepare_cached(
                "SELECT flag, penalties FROM standings WHERE is_player = 1 AND session_time <= ?1 \
                 ORDER BY id DESC LIMIT 1",
            )?
            .query_row([upto], |row| Ok((row.get(0)?, row.get(1)?)))
            .optional()?,
    };
    if let Some((flag, penalties)) = standing {
        scoring.flag = flag;
        scoring.penalties = penalties;
    }

    // The forecast is LMU's only, written as a whole set of rows (same `session_time`) when it
    // changes
    if let Some(kind) = &scoring.session_type {
        let mut statement = conn.prepare_cached(
            "SELECT session_fraction, rain_chance FROM forecast WHERE session_type = ?1 \
             AND session_time = (SELECT session_time FROM forecast \
             WHERE ?2 IS NULL OR session_time <= ?2 ORDER BY id DESC LIMIT 1) \
             ORDER BY session_fraction",
        )?;
        let points = statement.query_map(rusqlite::params![kind, upto], |row| {
            Ok((row.get::<_, Option<f64>>(0)?, row.get::<_, Option<f64>>(1)?))
        })?;
        for point in points {
            if let (Some(fraction), Some(chance)) = point? {
                scoring.forecast.push((fraction, chance));
            }
        }
    }
    Ok(Some(scoring))
}

/// The other car relative to the player in metres: (+ahead, +right). Forward is (-sin h, -cos h)
/// and right is (-cos h, sin h) in world x/z.
fn relative(player: &Car, other: &Car) -> Option<(f64, f64)> {
    let h = player.heading?;
    let dx = other.x? - player.x?;
    let dz = other.z? - player.z?;
    Some((dx * -h.sin() + dz * -h.cos(), dx * -h.cos() + dz * h.sin()))
}

/// A car moved along its heading at its speed to session time `t`: opponents are recorded less
/// often than the player's car, and at racing speed a tenth of a second is several metres.
fn at_time(mut car: Car, t: f64) -> Car {
    if let (Some(time), Some(x), Some(z), Some(heading), Some(speed)) =
        (car.time, car.x, car.z, car.heading, car.speed)
    {
        let (x, z) = extrapolate(x, z, heading, speed, t - time);
        car.x = Some(x);
        car.z = Some(z);
    }
    car
}

/// World x/z moved `dt` seconds along `heading` at `speed` (forward is (-sin h, -cos h)).
pub(crate) fn extrapolate(x: f64, z: f64, heading: f64, speed: f64, dt: f64) -> (f64, f64) {
    (
        x - heading.sin() * speed * dt,
        z - heading.cos() * speed * dt,
    )
}

/// The band `value` is in, moving from `current` one band at a time and only `margin` past an
/// edge, so a value sitting on an edge stays put.
fn band(value: Option<f64>, edges: &[f64], current: Option<usize>, margin: f64) -> Option<usize> {
    let Some(value) = value else {
        return current;
    };
    let Some(mut band) = current else {
        return Some(edges.iter().filter(|edge| value >= **edge).count());
    };
    while band < edges.len() && value >= edges[band] + margin {
        band += 1;
    }
    while band > 0 && value < edges[band - 1] - margin {
        band -= 1;
    }
    Some(band)
}

// --------------------------------------------------------------------------- logic

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum Side {
    Left,
    Right,
}

impl Side {
    const BOTH: [Side; 2] = [Side::Left, Side::Right];

    fn index(self) -> usize {
        self as usize
    }
}

struct Logic {
    /// Cars beside the player, not yet for long enough: vehicle -> (side, since)
    pending: HashMap<i64, (Side, f64)>,
    /// Cars beside the player: vehicle -> side
    overlap: HashMap<i64, Side>,
    /// Since when each side has been taken, and when "still there" was last said for it
    side_since: [Option<f64>; 2],
    last_still: [Option<f64>; 2],
    prev: Option<Player>,
    prev_flat: [Option<bool>; 4],
    /// The gap behind opened up since it was last called
    gap_armed: bool,
    prev_scoring: Option<Scoring>,
    /// The last bands called
    rain_band: Option<usize>,
    track_band: Option<usize>,
    last_rain_call: f64,
    last_track_call: f64,
    last_patch_call: f64,
    prev_wet: usize,
    /// Forecast points already called: (session type, how far through it)
    forecast_called: HashSet<(String, u64)>,
}

impl Default for Logic {
    fn default() -> Self {
        Self {
            pending: HashMap::new(),
            overlap: HashMap::new(),
            side_since: [None; 2],
            last_still: [None; 2],
            prev: None,
            prev_flat: [None; 4],
            gap_armed: true,
            prev_scoring: None,
            rain_band: None,
            track_band: None,
            last_rain_call: f64::NEG_INFINITY,
            last_track_call: f64::NEG_INFINITY,
            last_patch_call: f64::NEG_INFINITY,
            prev_wet: 0,
            forecast_called: HashSet::new(),
        }
    }
}

/// The calls of one update.
struct Calls {
    t: f64,
    calls: Vec<Raised>,
}

impl Calls {
    fn raise(&mut self, text: &'static str, urgent: bool) {
        self.raise_number(text, None, urgent);
    }

    fn raise_number(&mut self, text: &'static str, number: Option<i64>, urgent: bool) {
        self.calls.push(Raised {
            text,
            number,
            urgent,
            session_time: self.t,
        });
    }
}

impl Logic {
    /// The calls for the player's newest row, given the other cars at the same time.
    fn update(
        &mut self,
        config: &SpotterConfig,
        player: &Player,
        others: &[Car],
        wheels: &Wheels,
        scoring: Option<&Scoring>,
    ) -> Vec<Raised> {
        let p = &player.car;
        let Some(t) = p.time else {
            return Vec::new();
        };
        let mut calls = Calls {
            t,
            calls: Vec::new(),
        };

        // A new session
        if self
            .prev
            .as_ref()
            .and_then(|prev| prev.car.time)
            .is_some_and(|prev| t < prev - 1.0)
        {
            *self = Logic::default();
        }

        let stopped = p.speed.unwrap_or(0.0) < *config.min_speed()
            || p.in_pit_lane
            || p.x.is_none()
            || p.z.is_none()
            || p.heading.is_none();
        // On the formation lap cars weave and run side by side on purpose
        let formation_lap = scoring.is_some_and(|s| s.phase.as_deref() == Some("formation_lap"));
        let quiet = stopped || formation_lap;

        self.traffic(config, p, others, t, quiet, &mut calls);
        if let Some(prev) = self.prev.take() {
            self.alerts(config, player, &prev, wheels, t, formation_lap, &mut calls);
        }
        if let Some(scoring) = scoring {
            self.flags(scoring, &mut calls);
            self.weather(config, scoring, wheels.wet, t, stopped, &mut calls);
        }

        self.prev_flat = wheels.flat;
        self.prev = Some(player.clone());
        calls.calls
    }

    /// Cars alongside: who came, who went, and who's still there.
    fn traffic(
        &mut self,
        config: &SpotterConfig,
        p: &Car,
        others: &[Car],
        t: f64,
        quiet: bool,
        calls: &mut Calls,
    ) {
        let before = self.sides();
        if quiet {
            self.reset_traffic();
        } else {
            self.track(config, p, others, t);
        }
        let after = self.sides();

        let became = Side::BOTH
            .into_iter()
            .filter(|s| after[s.index()] > 0 && before[s.index()] == 0)
            .collect::<Vec<_>>();
        let cleared = Side::BOTH
            .into_iter()
            .filter(|s| before[s.index()] > 0 && after[s.index()] == 0)
            .collect::<Vec<_>>();

        for side in &became {
            self.side_since[side.index()] = Some(t);
            self.last_still[side.index()] = Some(t);
        }
        if let Some(first) = became.first() {
            match (after[0] > 0 && after[1] > 0, first) {
                (true, _) => calls.raise("three wide", true),
                (false, Side::Left) => calls.raise("car left", true),
                (false, Side::Right) => calls.raise("car right", true),
            }
        }

        let mut clears = Vec::new();
        for side in cleared {
            let held = self.side_since[side.index()].map_or(0.0, |since| t - since);
            self.side_since[side.index()] = None;
            self.last_still[side.index()] = None;
            if held >= *config.min_hold_for_clear() && !quiet {
                clears.push(side);
            }
        }
        match clears.as_slice() {
            [_, _] => calls.raise("clear all round", true),
            [Side::Left] => calls.raise("clear left", true),
            [Side::Right] => calls.raise("clear right", true),
            _ => {}
        }

        for side in Side::BOTH {
            if after[side.index()] > 0
                && let Some(last) = self.last_still[side.index()]
                && t - last >= *config.still_there_every()
            {
                self.last_still[side.index()] = Some(t);
                calls.raise(
                    match side {
                        Side::Left => "still there left",
                        Side::Right => "still there right",
                    },
                    true,
                );
            }
        }
    }

    /// Follows the cars beside the player: they count once they've been there `debounce`, and
    /// stop counting past the (wider) exit distances.
    fn track(&mut self, config: &SpotterConfig, player: &Car, others: &[Car], t: f64) {
        let mut seen = Vec::new();
        for other in others {
            if other.x.is_none() || other.z.is_none() || other.in_pit_lane {
                continue;
            }
            if (other.y.unwrap_or(0.0) - player.y.unwrap_or(0.0)).abs()
                > *config.max_height_difference()
            {
                continue;
            }
            let Some((along, across)) = relative(player, other) else {
                continue;
            };
            let side = if across > 0.0 {
                Side::Right
            } else {
                Side::Left
            };
            let (along, across) = (along.abs(), across.abs());

            if let Some(overlap) = self.overlap.get(&other.vehicle) {
                if along <= *config.exit_along()
                    && (*config.min_across()..=*config.exit_across()).contains(&across)
                    && side == *overlap
                {
                    seen.push(other.vehicle);
                }
                continue;
            }

            if along <= *config.enter_along()
                && (*config.min_across()..=*config.enter_across()).contains(&across)
            {
                match self.pending.get(&other.vehicle) {
                    Some(&(pending, since)) if pending == side => {
                        if t - since >= *config.debounce() {
                            self.overlap.insert(other.vehicle, side);
                            self.pending.remove(&other.vehicle);
                            seen.push(other.vehicle);
                        }
                    }
                    _ => {
                        self.pending.insert(other.vehicle, (side, t));
                    }
                }
            } else {
                self.pending.remove(&other.vehicle);
            }
        }
        self.overlap.retain(|vehicle, _| seen.contains(vehicle));
    }

    /// Rising edges of the player's own alerts.
    #[allow(clippy::too_many_arguments)]
    fn alerts(
        &mut self,
        config: &SpotterConfig,
        p: &Player,
        prev: &Player,
        wheels: &Wheels,
        t: f64,
        formation_lap: bool,
        calls: &mut Calls,
    ) {
        let rose = |now: bool, before: bool| now && !before;

        for (wheel, flat) in wheels.flat.iter().enumerate() {
            if flat.unwrap_or(false) && !self.prev_flat[wheel].unwrap_or(false) {
                calls.raise(PUNCTURES[wheel], false);
            }
        }
        if rose(p.parts_detached, prev.parts_detached) {
            calls.raise("parts off the car", false);
        }
        if rose(p.engine_overheating, prev.engine_overheating) {
            calls.raise("engine overheating", false);
        }
        // In the pit lane with the limiter off: on the way in, or switched off on the way through
        if p.car.in_pit_lane
            && p.limiter == Some(false)
            && (rose(p.car.in_pit_lane, prev.car.in_pit_lane) || prev.limiter.unwrap_or(false))
        {
            calls.raise("limiter, limiter", true);
        }
        if rose(p.lap_invalidated, prev.lap_invalidated) {
            calls.raise("lap invalidated", false);
        }

        // A car closing to within a second, called again only once it's dropped back. Not on the
        // formation lap, where the field runs nose to tail
        let gap = p.gap_behind.unwrap_or(0.0);
        if gap != 0.0 && gap >= *config.gap_behind_off() {
            self.gap_armed = true;
        } else if gap > 0.0
            && gap <= *config.gap_behind_on()
            && self.gap_armed
            && !p.car.in_pit_lane
            && t > GAP_BEHIND_AFTER
            && !formation_lap
        {
            self.gap_armed = false;
            calls.raise(
                match gap < CLOSE_BEHIND {
                    true => "car close behind",
                    false => "car within a second behind",
                },
                false,
            );
        }
    }

    /// Rising edges of flags and penalties.
    fn flags(&mut self, now: &Scoring, calls: &mut Calls) {
        let Some(last) = self.prev_scoring.replace(now.clone()) else {
            return;
        };
        let caution = |fcy: &Option<String>| fcy.as_deref().is_some_and(|fcy| fcy != "none");

        if caution(&now.full_course_yellow) && !caution(&last.full_course_yellow) {
            calls.raise("full course yellow", true);
        } else if caution(&last.full_course_yellow)
            && now.full_course_yellow.as_deref() == Some("none")
        {
            calls.raise("green flag, green flag", true);
        }
        if now.phase.as_deref() == Some("stopped") && last.phase.as_deref() != Some("stopped") {
            calls.raise("red flag", true);
        }
        if now.yellow && !last.yellow {
            calls.raise("yellow flag on track", false);
        }
        if now.flag.as_deref() == Some("blue") && last.flag.as_deref() != Some("blue") {
            calls.raise("blue flag", false);
        }
        if now.penalties.unwrap_or(0) > last.penalties.unwrap_or(0) {
            calls.raise("penalty", false);
        }
    }

    /// Rain and track wetness changing band, and running onto a wet patch while the track is dry.
    /// A change within `weather_repeat` of the last call is called once that's passed, if it still
    /// holds.
    fn weather(
        &mut self,
        config: &SpotterConfig,
        scoring: &Scoring,
        wet_wheels: usize,
        t: f64,
        stopped: bool,
        calls: &mut Calls,
    ) {
        let rain = band(
            scoring.rain,
            &*config.rain_bands(),
            self.rain_band,
            *config.band_margin(),
        );
        match (self.rain_band, rain) {
            (None, _) => self.rain_band = rain,
            (Some(was), Some(now))
                if now != was && t - self.last_rain_call >= *config.weather_repeat() =>
            {
                calls.raise(
                    match () {
                        _ if was == 0 => "rain starting",
                        _ if now == 0 => "rain stopped",
                        _ if now > was => "rain getting heavier",
                        _ => "rain easing",
                    },
                    false,
                );
                self.rain_band = rain;
                self.last_rain_call = t;
            }
            _ => {}
        }

        let track = band(
            scoring.wetness,
            &*config.wet_bands(),
            self.track_band,
            *config.band_margin(),
        );
        match (self.track_band, track) {
            (None, _) => self.track_band = track,
            (Some(was), Some(now))
                if now != was && t - self.last_track_call >= *config.weather_repeat() =>
            {
                calls.raise(
                    match () {
                        _ if was == 0 => "track getting wet",
                        _ if now == 0 => "track dry",
                        _ if now > was => "track getting wetter",
                        _ => "track drying",
                    },
                    false,
                );
                self.track_band = track;
                self.last_track_call = t;
            }
            _ => {}
        }

        // On a wet track every tyre is on a wet surface: only news while it's dry or damp
        if wet_wheels >= 2
            && self.prev_wet < 2
            && !stopped
            && self.track_band.unwrap_or(0) <= 1
            && t - self.last_patch_call >= *config.wet_patch_repeat()
        {
            calls.raise("wet patch", false);
            self.last_patch_call = t;
        }
        self.prev_wet = wet_wheels;

        self.forecast(config, scoring, t, calls);
    }

    /// Rain on the way: the first forecast point ahead likely to bring rain, once it's a few
    /// minutes away, while it's still dry. Each point is called once.
    fn forecast(&mut self, config: &SpotterConfig, scoring: &Scoring, t: f64, calls: &mut Calls) {
        // Until the session's under way, LMU's end time is the end of the current phase (the
        // countdown, the formation lap), not of the session
        if !matches!(
            scoring.phase.as_deref(),
            Some("green" | "full_course_yellow")
        ) {
            return;
        }
        let Some(end) = scoring.session_end_time.filter(|end| *end > 0.0) else {
            return;
        };
        // Raining already
        if self.rain_band.unwrap_or(0) > 0 {
            return;
        }

        for &(fraction, chance) in &scoring.forecast {
            let at = fraction * end;
            if at <= t || chance < *config.forecast_rain_chance() {
                continue;
            }
            let minutes = (at - t) / 60.0;
            // The next wet point is still far off
            if minutes > *config.forecast_rain_lead_minutes() {
                return;
            }
            let point = (
                scoring.session_type.clone().unwrap_or_default(),
                fraction.to_bits(),
            );
            if !self.forecast_called.insert(point) {
                return;
            }
            match minutes.round() as i64 {
                ..=1 => calls.raise("rain likely in about a minute", false),
                minutes => {
                    calls.raise_number("rain likely in about {} minutes", Some(minutes), false)
                }
            }
            return;
        }
    }

    fn reset_traffic(&mut self) {
        self.pending.clear();
        self.overlap.clear();
        self.side_since = [None; 2];
        self.last_still = [None; 2];
    }

    /// How many cars are beside the player on each side.
    fn sides(&self) -> [usize; 2] {
        let mut count = [0; 2];
        for side in self.overlap.values() {
            count[side.index()] += 1;
        }
        count
    }
}

const PUNCTURES: [&str; 4] = [
    "puncture front left",
    "puncture front right",
    "puncture rear left",
    "puncture rear right",
];

// --------------------------------------------------------------------------- words

/// A call in `lang`, or in English when there's no translation for it.
pub fn translate(text: &'static str, lang: ArrayStr<2>) -> &'static str {
    let lang = &*lang;
    match text {
        "car left" => t!("call.car_left": lang),
        "car right" => t!("call.car_right": lang),
        "three wide" => t!("call.three_wide": lang),
        "clear left" => t!("call.clear_left": lang),
        "clear right" => t!("call.clear_right": lang),
        "clear all round" => t!("call.clear_all_round": lang),
        "still there left" => t!("call.still_there_left": lang),
        "still there right" => t!("call.still_there_right": lang),
        "puncture front left" => t!("call.puncture_front_left": lang),
        "puncture front right" => t!("call.puncture_front_right": lang),
        "puncture rear left" => t!("call.puncture_rear_left": lang),
        "puncture rear right" => t!("call.puncture_rear_right": lang),
        "parts off the car" => t!("call.parts_off": lang),
        "engine overheating" => t!("call.engine_overheating": lang),
        "limiter, limiter" => t!("call.limiter": lang),
        "lap invalidated" => t!("call.lap_invalidated": lang),
        "car close behind" => t!("call.car_close_behind": lang),
        "car within a second behind" => t!("call.car_within_a_second": lang),
        "full course yellow" => t!("call.full_course_yellow": lang),
        "green flag, green flag" => t!("call.green_flag": lang),
        "red flag" => t!("call.red_flag": lang),
        "yellow flag on track" => t!("call.yellow_flag": lang),
        "blue flag" => t!("call.blue_flag": lang),
        "penalty" => t!("call.penalty": lang),
        "rain starting" => t!("call.rain_starting": lang),
        "rain getting heavier" => t!("call.rain_heavier": lang),
        "rain easing" => t!("call.rain_easing": lang),
        "rain stopped" => t!("call.rain_stopped": lang),
        "track getting wet" => t!("call.track_wet": lang),
        "track getting wetter" => t!("call.track_wetter": lang),
        "track drying" => t!("call.track_drying": lang),
        "track dry" => t!("call.track_dry": lang),
        "wet patch" => t!("call.wet_patch": lang),
        "rain likely in about {} minutes" => t!("call.rain_in_minutes": lang),
        "rain likely in about a minute" => t!("call.rain_in_a_minute": lang),
        _ => text,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn queued(calls: &[(&'static str, bool)]) -> Vec<(Raised, Instant)> {
        let mut queue = VecDeque::new();
        for &(text, urgent) in calls {
            Spotter::enqueue(
                &mut queue,
                Raised {
                    text,
                    urgent,
                    number: None,
                    session_time: 0.0,
                },
                Instant::now(),
            );
        }
        queue.into_iter().collect()
    }

    #[test]
    fn queued_urgent_categories_survive_in_either_order() {
        for calls in [
            [
                ("car right", true),
                ("limiter, limiter", true),
                ("full course yellow", true),
            ],
            [
                ("full course yellow", true),
                ("limiter, limiter", true),
                ("car right", true),
            ],
        ] {
            let queue = queued(&calls);
            assert_eq!(
                queue
                    .iter()
                    .map(|(call, _)| (call.text, call.urgent))
                    .collect::<Vec<_>>(),
                calls
            );
        }
    }

    #[test]
    fn queued_updates_replace_only_their_category() {
        let queue = queued(&[
            ("blue flag", false),
            ("car right", true),
            ("full course yellow", true),
            ("limiter, limiter", true),
            ("clear right", true),
            ("green flag, green flag", true),
            ("red flag", true),
        ]);
        assert_eq!(
            queue.iter().map(|(call, _)| call.text).collect::<Vec<_>>(),
            ["blue flag", "limiter, limiter", "clear right", "red flag"]
        );
        assert_eq!(queue.iter().position(|(call, _)| call.urgent), Some(1));
    }

    #[test]
    fn queued_replacement_refreshes_age_and_preserves_unrelated_age() {
        let old = Instant::now();
        let new = old + Duration::from_millis(100);
        let call = |text| Raised {
            text,
            urgent: true,
            number: None,
            session_time: 0.0,
        };
        let mut queue = VecDeque::new();
        Spotter::enqueue(&mut queue, call("car left"), old);
        Spotter::enqueue(&mut queue, call("limiter, limiter"), old);
        Spotter::enqueue(&mut queue, call("still there left"), new);
        assert_eq!(
            queue
                .iter()
                .map(|(call, at)| (call.text, *at))
                .collect::<Vec<_>>(),
            [("limiter, limiter", old), ("still there left", new)]
        );
    }

    #[test]
    fn opposite_side_traffic_updates_do_not_erase_each_other() {
        for calls in [
            [("car right", true), ("clear left", true)],
            [("car left", true), ("clear right", true)],
            [("still there left", true), ("still there right", true)],
        ] {
            let queue = queued(&calls);
            assert_eq!(
                queue
                    .iter()
                    .map(|(call, _)| (call.text, call.urgent))
                    .collect::<Vec<_>>(),
                calls,
            );
        }
        let queue = queued(&[
            ("car right", true),
            ("car left", true),
            ("clear all round", true),
        ]);
        assert_eq!(
            queue.iter().map(|(call, _)| call.text).collect::<Vec<_>>(),
            ["clear all round"]
        );
    }

    #[test]
    fn newer_flag_state_cancels_inflight_synthesis() {
        let at = Instant::now();
        let (mut spotter, send) = pending_call("full course yellow", true, at);
        let newer = Raised {
            text: "green flag, green flag",
            urgent: true,
            number: None,
            session_time: 1.0,
        };
        spotter.discard_stale_synthesis(at, Some(&newer));
        Spotter::enqueue(&mut spotter.queue, newer, at);
        assert!(spotter.saying.is_none());
        assert!(send.is_closed());
        assert_eq!(
            spotter.queue.front().unwrap().0.text,
            "green flag, green flag"
        );
    }

    fn pending_call(
        text: &'static str,
        urgent: bool,
        at: Instant,
    ) -> (Spotter, oneshot::Sender<anyhow::Result<Speech>>) {
        let mut spotter = Spotter::new();
        let (send, recv) = oneshot::channel();
        spotter.saying = Some(Saying::Synthesizing {
            key: (arrstr!("en"), PathBuf::new(), 0, text.into()),
            recv,
            call: Raised {
                text,
                urgent,
                number: None,
                session_time: 0.0,
            },
            at,
        });
        (spotter, send)
    }

    #[test]
    fn synthesis_expires_at_the_original_queue_deadline() {
        let at = Instant::now();
        for (urgent, lifetime) in [(true, URGENT_FOR), (false, OTHER_FOR)] {
            let (mut spotter, send) = pending_call("car right", urgent, at);
            spotter.discard_stale_synthesis(at + lifetime - Duration::from_nanos(1), None);
            assert!(spotter.saying.is_some());
            // Completion at the deadline must not reach playback, even if already received.
            assert!(
                send.send(Ok(Speech {
                    samples: vec![],
                    sample_rate: 24000,
                }))
                .is_ok()
            );
            spotter.discard_stale_synthesis(at + lifetime, None);
            assert!(spotter.saying.is_none());
        }
    }

    #[test]
    fn expired_pending_synthesis_releases_the_call_without_waiting_for_completion() {
        let at = Instant::now();
        let (mut spotter, send) = pending_call("car right", true, at);
        spotter.discard_stale_synthesis(at + URGENT_FOR, None);
        assert!(spotter.saying.is_none());
        assert!(send.is_closed());
    }

    #[test]
    fn newer_traffic_cancels_pending_or_completed_synthesis() {
        let at = Instant::now();
        for completed in [false, true] {
            for (old, new) in [
                ("car right", "clear right"),
                ("still there left", "clear left"),
                ("three wide", "clear right"),
                ("car left", "clear all round"),
                ("clear right", "car right"),
            ] {
                let (mut spotter, send) = pending_call(old, true, at);
                if completed {
                    assert!(
                        send.send(Ok(Speech {
                            samples: vec![],
                            sample_rate: 24000,
                        }))
                        .is_ok()
                    );
                }
                let newer = Raised {
                    text: new,
                    urgent: true,
                    number: None,
                    session_time: 1.0,
                };
                spotter.discard_stale_synthesis(at, Some(&newer));
                assert!(spotter.saying.is_none(), "{old} superseded by {new}");
            }
        }
    }

    #[test]
    fn unrelated_calls_preserve_fresh_synthesis() {
        let at = Instant::now();
        for new in ["car left", "limiter, limiter", "red flag", "rain starting"] {
            let (mut spotter, _send) = pending_call("car right", true, at);
            let newer = Raised {
                text: new,
                urgent: true,
                number: None,
                session_time: 1.0,
            };
            spotter.discard_stale_synthesis(at, Some(&newer));
            assert!(spotter.saying.is_some(), "unrelated {new}");
        }
    }

    /// The player at `t`, heading along -z (heading 0) at 50 m/s.
    fn player(t: f64) -> Player {
        Player {
            car: Car {
                id: 1,
                vehicle: 1,
                time: Some(t),
                speed: Some(50.0),
                x: Some(0.0),
                y: Some(0.0),
                z: Some(0.0),
                heading: Some(0.0),
                ..Car::default()
            },
            limiter: Some(false),
            ..Player::default()
        }
    }

    /// Another car `ahead` metres ahead and `right` metres to the right of the player.
    fn other(vehicle: i64, ahead: f64, right: f64) -> Car {
        Car {
            vehicle,
            time: Some(0.0),
            speed: Some(50.0),
            // Forward is -z and right is -x at heading 0
            x: Some(-right),
            y: Some(0.0),
            z: Some(-ahead),
            heading: Some(0.0),
            ..Car::default()
        }
    }

    fn texts(calls: Vec<Raised>) -> Vec<&'static str> {
        calls.into_iter().map(|call| call.text).collect()
    }

    /// Runs the logic at 30 Hz from `from` to `to` with the same cars, returning the calls.
    fn run(logic: &mut Logic, from: f64, to: f64, others: &[Car]) -> Vec<&'static str> {
        let config = SpotterConfig::default();
        let mut calls = Vec::new();
        let mut t = from;
        while t < to {
            calls.extend(texts(logic.update(
                &config,
                &player(t),
                others,
                &Wheels::default(),
                None,
            )));
            t += 1.0 / 30.0;
        }
        calls
    }

    #[test]
    fn calls_a_car_alongside_and_clear() {
        let mut logic = Logic::default();
        // Alongside on the right, for a second
        assert_eq!(
            run(&mut logic, 0.0, 1.0, &[other(2, 1.0, 3.0)]),
            ["car right"]
        );
        // Gone
        assert_eq!(run(&mut logic, 1.0, 1.5, &[]), ["clear right"]);
    }

    #[test]
    fn waits_for_the_debounce() {
        let mut logic = Logic::default();
        // Alongside for less than the debounce: nothing
        assert!(run(&mut logic, 0.0, 0.08, &[other(2, 1.0, -3.0)]).is_empty());
        assert!(run(&mut logic, 0.08, 0.5, &[]).is_empty());
    }

    #[test]
    fn calls_three_wide_and_clear_all_round() {
        let mut logic = Logic::default();
        assert_eq!(
            run(&mut logic, 0.0, 1.0, &[other(2, 0.0, 3.0)]),
            ["car right"]
        );
        assert_eq!(
            run(
                &mut logic,
                1.0,
                2.0,
                &[other(2, 0.0, 3.0), other(3, 0.0, -3.0)]
            ),
            ["three wide"]
        );
        assert_eq!(run(&mut logic, 2.0, 2.5, &[]), ["clear all round"]);
    }

    #[test]
    fn keeps_a_car_at_the_edge_without_chattering() {
        let mut logic = Logic::default();
        assert_eq!(
            run(&mut logic, 0.0, 1.0, &[other(2, 4.0, 3.0)]),
            ["car right"]
        );
        // Past the way in (4.8 m) but within the way out (5.8 m): still there
        assert!(run(&mut logic, 1.0, 3.0, &[other(2, 5.3, 3.0)]).is_empty());
        assert_eq!(
            run(&mut logic, 3.0, 3.5, &[other(2, 6.5, 3.0)]),
            ["clear right"]
        );
    }

    #[test]
    fn ignores_cars_in_line_and_says_still_there() {
        let mut logic = Logic::default();
        // Right behind, in line: not alongside
        assert!(run(&mut logic, 0.0, 1.0, &[other(2, -3.0, 0.2)]).is_empty());

        let mut logic = Logic::default();
        let calls = run(&mut logic, 0.0, 7.0, &[other(2, 0.0, -3.0)]);
        assert_eq!(calls, ["car left", "still there left"]);
    }

    #[test]
    fn stays_quiet_when_slow_or_on_the_formation_lap() {
        let config = SpotterConfig::default();
        let mut logic = Logic::default();
        for i in 0..30 {
            let mut p = player(i as f64 / 30.0);
            p.car.speed = Some(2.0);
            let calls = logic.update(&config, &p, &[other(2, 0.0, 3.0)], &Wheels::default(), None);
            assert!(calls.is_empty());
        }

        let formation = Scoring {
            phase: Some("formation_lap".into()),
            ..Scoring::default()
        };
        let mut logic = Logic::default();
        for i in 0..30 {
            let p = player(i as f64 / 30.0);
            let calls = logic.update(
                &config,
                &p,
                &[other(2, 0.0, 3.0)],
                &Wheels::default(),
                Some(&formation),
            );
            assert!(calls.is_empty());
        }
    }

    #[test]
    fn calls_flags_as_they_change() {
        let config = SpotterConfig::default();
        let mut logic = Logic::default();
        let mut scoring = Scoring {
            full_course_yellow: Some("none".into()),
            phase: Some("green".into()),
            flag: Some("green".into()),
            penalties: Some(0),
            ..Scoring::default()
        };
        let mut step = |t: f64, scoring: &Scoring| {
            logic
                .update(&config, &player(t), &[], &Wheels::default(), Some(scoring))
                .into_iter()
                .map(|call| (call.text, call.urgent))
                .collect::<Vec<_>>()
        };

        assert!(step(0.0, &scoring).is_empty());
        scoring.full_course_yellow = Some("pending".into());
        assert_eq!(step(0.1, &scoring), [("full course yellow", true)]);
        assert!(step(0.2, &scoring).is_empty());
        scoring.full_course_yellow = Some("none".into());
        assert_eq!(step(0.3, &scoring), [("green flag, green flag", true)]);
        scoring.yellow = true;
        scoring.flag = Some("blue".into());
        scoring.penalties = Some(1);
        assert_eq!(
            step(0.4, &scoring),
            [
                ("yellow flag on track", false),
                ("blue flag", false),
                ("penalty", false)
            ]
        );
        scoring.phase = Some("stopped".into());
        assert_eq!(step(0.5, &scoring), [("red flag", true)]);
    }

    #[test]
    fn calls_the_cars_own_alerts() {
        let config = SpotterConfig::default();
        let mut logic = Logic::default();
        let mut step =
            |p: &Player, wheels: &Wheels| texts(logic.update(&config, p, &[], wheels, None));

        let mut p = player(30.0);
        assert!(step(&p, &Wheels::default()).is_empty());

        // A flat front right, parts off, the engine overheating and the lap invalidated
        p.car.time = Some(30.1);
        p.parts_detached = true;
        p.engine_overheating = true;
        p.lap_invalidated = true;
        let flat = Wheels {
            flat: [Some(false), Some(true), Some(false), Some(false)],
            wet: 0,
        };
        assert_eq!(
            step(&p, &flat),
            [
                "puncture front right",
                "parts off the car",
                "engine overheating",
                "lap invalidated"
            ]
        );
        // Called once, while they last
        p.car.time = Some(30.2);
        assert!(step(&p, &flat).is_empty());

        // Into the pit lane with the limiter off
        p.car.time = Some(30.3);
        p.car.in_pit_lane = true;
        assert_eq!(step(&p, &flat), ["limiter, limiter"]);
    }

    #[test]
    fn calls_a_car_closing_behind_once() {
        let config = SpotterConfig::default();
        let mut logic = Logic::default();
        let mut at = |t: f64, gap: f64| {
            let mut p = player(t);
            p.gap_behind = Some(gap);
            texts(logic.update(&config, &p, &[], &Wheels::default(), None))
        };

        assert!(at(30.0, 2.0).is_empty());
        assert_eq!(at(30.1, 0.8), ["car within a second behind"]);
        // Still there: not again
        assert!(at(30.2, 0.5).is_empty());
        // Dropped back and closed again
        assert!(at(30.3, 1.6).is_empty());
        assert_eq!(at(30.4, 0.1), ["car close behind"]);
    }

    #[test]
    fn calls_the_weather_changing() {
        let config = SpotterConfig::default();
        let mut logic = Logic::default();
        let mut at = |t: f64, rain: f64, wetness: f64, wet_wheels: usize| {
            let scoring = Scoring {
                rain: Some(rain),
                wetness: Some(wetness),
                ..Scoring::default()
            };
            let wheels = Wheels {
                wet: wet_wheels,
                ..Wheels::default()
            };
            texts(logic.update(&config, &player(t), &[], &wheels, Some(&scoring)))
        };

        assert!(at(0.0, 0.0, 0.0, 0).is_empty());
        // On an edge (0.05), within the margin: nothing
        assert!(at(1.0, 0.06, 0.0, 0).is_empty());
        assert_eq!(at(2.0, 0.1, 0.0, 0), ["rain starting"]);
        // Heavier within a minute of the last call: waits
        assert!(at(30.0, 0.4, 0.0, 0).is_empty());
        assert_eq!(at(62.0, 0.4, 0.0, 0), ["rain getting heavier"]);
        assert_eq!(at(63.0, 0.4, 0.1, 0), ["track getting wet"]);
        // Two tyres on a wet surface while the track's only damp
        assert_eq!(at(64.0, 0.4, 0.1, 2), ["wet patch"]);
        assert_eq!(at(130.0, 0.0, 0.1, 0), ["rain stopped"]);
    }

    #[test]
    fn calls_rain_on_the_way() {
        let config = SpotterConfig::default();
        let mut logic = Logic::default();
        let scoring = Scoring {
            rain: Some(0.0),
            phase: Some("green".into()),
            session_type: Some("race".into()),
            session_end_time: Some(3600.0),
            // Rain likely at the end, an hour in
            forecast: vec![(0.0, 0.0), (0.5, 0.1), (1.0, 0.8)],
            ..Scoring::default()
        };
        let mut at = |t: f64| {
            logic
                .update(&config, &player(t), &[], &Wheels::default(), Some(&scoring))
                .into_iter()
                .map(|call| call.words(arrstr!("en")))
                .collect::<Vec<_>>()
        };

        // Too far off
        assert!(at(2000.0).is_empty());
        assert_eq!(at(3060.0), ["rain likely in about 9 minutes"]);
        // Once
        assert!(at(3100.0).is_empty());
    }

    #[test]
    fn no_rain_on_the_way_before_the_start() {
        let config = SpotterConfig::default();
        let mut logic = Logic::default();
        // As recorded on the grid: the end time is the countdown's, 5 s in
        let mut scoring = Scoring {
            rain: Some(0.0),
            phase: Some("before_start".into()),
            session_type: Some("race".into()),
            session_end_time: Some(5.0),
            forecast: vec![(0.0, 0.0), (0.25, 0.0), (0.5, 0.9), (0.75, 0.0), (1.0, 0.0)],
            ..Scoring::default()
        };
        let mut at = |t: f64, scoring: &Scoring| {
            logic
                .update(&config, &player(t), &[], &Wheels::default(), Some(scoring))
                .into_iter()
                .map(|call| call.words(arrstr!("en")))
                .collect::<Vec<_>>()
        };

        assert!(at(0.2, &scoring).is_empty());
        // The formation lap's end
        scoring.phase = Some("formation_lap".into());
        scoring.session_end_time = Some(83.0);
        assert!(at(30.0, &scoring).is_empty());
        // Under way, the rain's 15 minutes off
        scoring.phase = Some("green".into());
        scoring.session_end_time = Some(1883.0);
        assert!(at(90.0, &scoring).is_empty());
    }

    #[test]
    fn moves_cars_to_the_players_time() {
        // Recorded 0.1 s earlier at 50 m/s along -z: 5 m further on by now
        let car = at_time(other(2, 0.0, 3.0), 0.1);
        assert!((car.z.unwrap() - -5.0).abs() < 1e-9);
    }

    #[test]
    fn says_calls_in_the_pilots_language() {
        let call = Raised {
            text: "rain likely in about {} minutes",
            number: Some(4),
            urgent: false,
            session_time: 0.0,
        };
        assert_eq!(
            call.words(arrstr!("es")),
            "probable lluvia en unos 4 minutos"
        );
        assert_eq!(call.words(arrstr!("fr")), "rain likely in about 4 minutes");
        assert_eq!(translate("car left", arrstr!("es")), "coche a la izquierda");
        assert_eq!(translate("car left", arrstr!("ca")), "cotxe a l'esquerra");
    }

    /// Prints every call the spotter makes over a recording (`EVAL_DB`), e.g. my-engineer's race
    /// fixture, which ends with a lapped car alongside on the right.
    #[test]
    #[ignore]
    fn replays_a_recording() {
        let db = PathBuf::from(std::env::var("EVAL_DB").expect("EVAL_DB isn't set"));
        let calls = replay(&db, &SpotterConfig::default()).unwrap();
        for call in &calls {
            println!(
                "{:>8.1} {}{}",
                call.session_time,
                call.words(arrstr!("en")),
                if call.urgent { " (urgent)" } else { "" }
            );
        }
    }
}
