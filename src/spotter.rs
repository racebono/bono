//! The spotter: follows the running simulator's telemetry about thirty times a second and calls
//! cars alongside and flags the moment they happen, without anyone asking. A port of my-engineer's
//! `spotter.cpp` (proximity and flags so far; its own alerts and the weather are still to come).
//!
//! It reads the database on a thread of its own, with a read-only connection of its own (closed as
//! soon as the game stops, so the plugin can delete the file), and turns what changed into calls.
//! They're said in the engineer's voice: urgent ones (traffic, full course yellows, red flags) cut
//! in over the engineer, which carries on afterwards; the rest wait for it to finish.

use crate::{
    arrstr,
    config::{ArrayStr, Config, SpotterConfig},
    engineer::Engineer,
    sim::Sim,
    synthesizer::{Effects, Speech, Synthesizer},
};
use rusqlite::{Connection, OpenFlags, OptionalExtension, Row};
use sdl3::audio::{AudioDevice, AudioStreamOwner};
use std::{
    collections::{HashMap, VecDeque},
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
/// Calls synthesized ahead, so the first of each is said without waiting for the voice.
const WARM: [&str; 6] = [
    "car left",
    "car right",
    "three wide",
    "clear left",
    "clear right",
    "clear all round",
];

/// Something the spotter says. `text` is English; [`translate`] gives the pilot's language.
#[derive(Debug, Clone, PartialEq)]
pub struct Raised {
    pub text: &'static str,
    /// Traffic and flags that change what the pilot does right now: stale within a second or two,
    /// so they interrupt the engineer
    pub urgent: bool,
    /// When it happened, from the player's car
    pub session_time: f64,
}

pub struct Spotter {
    /// Following the database at this path
    running: Option<Running>,
    /// Calls to say, oldest first, with when they came in
    queue: VecDeque<(Raised, Instant)>,
    /// The call being said
    saying: Option<Saying>,
    /// Calls already synthesized, by (language, voice, radio effect, text)
    cache: HashMap<CacheKey, Arc<Speech>>,
    /// Calls being synthesized ahead
    warming: Vec<(CacheKey, oneshot::Receiver<anyhow::Result<Speech>>)>,
}

type CacheKey = (ArrayStr<2>, PathBuf, u32, &'static str);

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
    Synthesizing(CacheKey, oneshot::Receiver<anyhow::Result<Speech>>),
    /// Dropping it stops it
    Playing(AudioStreamOwner),
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
            .filter(|_| settings.enabled)
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
        if let Some(running) = &self.running {
            while let Ok(call) = running.calls.try_recv() {
                // A newer traffic call replaces one still waiting: "car left" is stale once it's
                // "clear left"
                if call.urgent {
                    self.queue.retain(|(queued, _)| !queued.urgent);
                }
                self.queue.push_back((call, Instant::now()));
            }
            self.warm(rt, config, synth, lang);
        }

        self.say(rt, config, synth, speaker, engineer, lang);
    }

    /// The calls' language: the configured one, or the pilot's; English without a voice for it.
    fn language(&self, config: &Config, settings: &SpotterConfig, engineer: &Engineer) -> ArrayStr<2> {
        let lang = settings.language.unwrap_or(engineer.language());
        match config.get().voices().contains_key(&lang) {
            true => lang,
            false => arrstr!("en"),
        }
    }

    fn key(config: &Config, lang: ArrayStr<2>, text: &'static str) -> CacheKey {
        let config = config.get();
        let voice = config.voices().get(&lang).map(|path| path.as_std_path().to_owned());
        (lang, voice.unwrap_or_default(), config.radio_effect.to_bits(), text)
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
        self.warming.retain_mut(|(key, recv)| match recv.try_recv() {
            Ok(Ok(speech)) => {
                self.cache.insert(key.clone(), Arc::new(speech));
                false
            }
            Ok(Err(_)) | Err(oneshot::error::TryRecvError::Closed) => false,
            Err(oneshot::error::TryRecvError::Empty) => true,
        });

        for text in WARM {
            let key = Self::key(config, lang, text);
            if self.cache.contains_key(&key) || self.warming.iter().any(|(k, _)| *k == key) {
                continue;
            }
            let recv = synth.synthesize(rt, lang, translate(text, lang).into(), Self::effects(config));
            self.warming.push((key, recv));
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
        // The call being said
        match &mut self.saying {
            Some(Saying::Synthesizing(key, recv)) => match recv.try_recv() {
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
            Some(Saying::Playing(stream)) => {
                if stream.queued_bytes().is_ok_and(|queued| queued == 0) {
                    self.saying = None;
                }
            }
            None => {}
        }

        // The next call: urgent ones first, the rest once the engineer is quiet
        if self.saying.is_none() {
            let now = Instant::now();
            self.queue.retain(|(call, at)| {
                now.duration_since(*at) < if call.urgent { URGENT_FOR } else { OTHER_FOR }
            });

            let next = match self.queue.iter().position(|(call, _)| call.urgent) {
                Some(urgent) => Some(urgent),
                None if !engineer.is_speaking() => (!self.queue.is_empty()).then_some(0),
                None => None,
            };

            if let Some((call, _)) = next.and_then(|next| self.queue.remove(next)) {
                let key = Self::key(config, lang, call.text);
                self.saying = match self.cache.get(&key).cloned() {
                    Some(speech) => self.play(config, speaker, &speech),
                    None => {
                        let text = translate(call.text, lang).to_owned();
                        let recv = synth.synthesize(rt, lang, text, Self::effects(config));
                        Some(Saying::Synthesizing(key, recv))
                    }
                };
            }
        }

        // The engineer waits while the spotter speaks, and carries on after
        engineer.hold(self.saying.is_some());
    }

    fn play(&self, config: &Config, speaker: Option<&AudioDevice>, speech: &Speech) -> Option<Saying> {
        let speaker = speaker?;
        let gain = {
            let config = config.get();
            config.master_volume * config.spotter_volume
        };
        let speech = Speech {
            samples: speech.samples.iter().map(|sample| sample * gain).collect(),
            sample_rate: speech.sample_rate,
        };
        match speech.play(speaker) {
            Ok(stream) => Some(Saying::Playing(stream)),
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

        let period = Duration::from_secs_f64(config.poll_interval.clamp(0.005, 1.0));
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
    let Some(player) = latest(conn)? else {
        return Ok(Vec::new());
    };
    if Some(player.id) == *last_id {
        return Ok(Vec::new());
    }
    *last_id = Some(player.id);

    let others = frame(conn, &player)?;
    let scoring = scoring(conn, None)?;
    Ok(logic.update(config, &player, &others, scoring.as_ref()))
}

/// Runs the spotter over a recording, as fast as it can, from the player's first row to the last:
/// every call it would have made, for testing without the game.
pub fn replay(db: &Path, config: &SpotterConfig) -> rusqlite::Result<Vec<Raised>> {
    let conn = open(db)?;
    let mut statement = conn.prepare(&format!(
        "SELECT {CAR_COLUMNS} FROM telemetry WHERE is_player = 1 ORDER BY id"
    ))?;
    let players = statement
        .query_map([], car)?
        .collect::<rusqlite::Result<Vec<_>>>()?;

    let mut logic = Logic::default();
    let mut calls = Vec::new();
    for player in players.iter().filter(|player| player.time.is_some()) {
        let others = frame(&conn, player)?;
        let scoring = scoring(&conn, player.time)?;
        calls.extend(logic.update(config, player, &others, scoring.as_ref()));
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
}

/// The player's newest row.
fn latest(conn: &Connection) -> rusqlite::Result<Option<Car>> {
    conn.prepare_cached(&format!(
        "SELECT {CAR_COLUMNS} FROM telemetry WHERE is_player = 1 ORDER BY id DESC LIMIT 1"
    ))?
    .query_row([], car)
    .optional()
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

    Ok(best.into_values().map(|(_, car)| at_time(car, time)).collect())
}

/// The newest session row and the player's newest standing (at or before `upto`, when replaying).
fn scoring(conn: &Connection, upto: Option<f64>) -> rusqlite::Result<Option<Scoring>> {
    let session = conn
        .prepare_cached(
            "SELECT full_course_yellow, phase, local_yellow_1, local_yellow_2, local_yellow_3 \
             FROM session WHERE ?1 IS NULL OR session_time <= ?1 ORDER BY id DESC LIMIT 1",
        )?
        .query_row([upto], |row| {
            let yellow = |i| row.get::<_, Option<bool>>(i).map(|y| y.unwrap_or(false));
            Ok(Scoring {
                full_course_yellow: row.get(0)?,
                phase: row.get(1)?,
                yellow: yellow(2)? || yellow(3)? || yellow(4)?,
                flag: None,
                penalties: None,
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
        let dt = t - time;
        car.x = Some(x - heading.sin() * speed * dt);
        car.z = Some(z - heading.cos() * speed * dt);
    }
    car
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

#[derive(Default)]
struct Logic {
    /// Cars beside the player, not yet for long enough: vehicle -> (side, since)
    pending: HashMap<i64, (Side, f64)>,
    /// Cars beside the player: vehicle -> side
    overlap: HashMap<i64, Side>,
    /// Since when each side has been taken, and when "still there" was last said for it
    side_since: [Option<f64>; 2],
    last_still: [Option<f64>; 2],
    prev_time: Option<f64>,
    prev_scoring: Option<Scoring>,
}

impl Logic {
    /// The calls for the player's newest row, given the other cars at the same time.
    fn update(
        &mut self,
        config: &SpotterConfig,
        player: &Car,
        others: &[Car],
        scoring: Option<&Scoring>,
    ) -> Vec<Raised> {
        let Some(t) = player.time else {
            return Vec::new();
        };
        let mut calls = Vec::new();
        let mut raise = |text, urgent| {
            calls.push(Raised {
                text,
                urgent,
                session_time: t,
            })
        };

        // A new session
        if self.prev_time.is_some_and(|prev| t < prev - 1.0) {
            *self = Logic::default();
        }
        self.prev_time = Some(t);

        let stopped = player.speed.unwrap_or(0.0) < config.min_speed
            || player.in_pit_lane
            || player.x.is_none()
            || player.z.is_none()
            || player.heading.is_none();
        // On the formation lap cars weave and run side by side on purpose
        let formation_lap = scoring.is_some_and(|s| s.phase.as_deref() == Some("formation_lap"));
        let quiet = stopped || formation_lap;

        let before = self.sides();
        if quiet {
            self.reset_traffic();
        } else {
            self.track(config, player, others, t);
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
                (true, _) => raise("three wide", true),
                (false, Side::Left) => raise("car left", true),
                (false, Side::Right) => raise("car right", true),
            }
        }

        let mut clears = Vec::new();
        for side in cleared {
            let held = self.side_since[side.index()].map_or(0.0, |since| t - since);
            self.side_since[side.index()] = None;
            self.last_still[side.index()] = None;
            if held >= config.min_hold_for_clear && !quiet {
                clears.push(side);
            }
        }
        match clears.as_slice() {
            [_, _] => raise("clear all round", true),
            [Side::Left] => raise("clear left", true),
            [Side::Right] => raise("clear right", true),
            _ => {}
        }

        for side in Side::BOTH {
            if after[side.index()] > 0
                && let Some(last) = self.last_still[side.index()]
                && t - last >= config.still_there_every
            {
                self.last_still[side.index()] = Some(t);
                raise(
                    match side {
                        Side::Left => "still there left",
                        Side::Right => "still there right",
                    },
                    true,
                );
            }
        }

        if let Some(scoring) = scoring {
            self.flags(scoring, &mut raise);
        }
        calls
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
                > config.max_height_difference
            {
                continue;
            }
            let Some((along, across)) = relative(player, other) else {
                continue;
            };
            let side = if across > 0.0 { Side::Right } else { Side::Left };
            let (along, across) = (along.abs(), across.abs());

            if let Some(overlap) = self.overlap.get(&other.vehicle) {
                if along <= config.exit_along
                    && (config.min_across..=config.exit_across).contains(&across)
                    && side == *overlap
                {
                    seen.push(other.vehicle);
                }
                continue;
            }

            if along <= config.enter_along
                && (config.min_across..=config.enter_across).contains(&across)
            {
                match self.pending.get(&other.vehicle) {
                    Some(&(pending, since)) if pending == side => {
                        if t - since >= config.debounce {
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

    /// Rising edges of flags and penalties.
    fn flags(&mut self, now: &Scoring, raise: &mut impl FnMut(&'static str, bool)) {
        let Some(last) = self.prev_scoring.replace(now.clone()) else {
            return;
        };
        let caution = |fcy: &Option<String>| fcy.as_deref().is_some_and(|fcy| fcy != "none");

        if caution(&now.full_course_yellow) && !caution(&last.full_course_yellow) {
            raise("full course yellow", true);
        } else if caution(&last.full_course_yellow) && now.full_course_yellow.as_deref() == Some("none")
        {
            raise("green flag, green flag", true);
        }
        if now.phase.as_deref() == Some("stopped") && last.phase.as_deref() != Some("stopped") {
            raise("red flag", true);
        }
        if now.yellow && !last.yellow {
            raise("yellow flag on track", false);
        }
        if now.flag.as_deref() == Some("blue") && last.flag.as_deref() != Some("blue") {
            raise("blue flag", false);
        }
        if now.penalties.unwrap_or(0) > last.penalties.unwrap_or(0) {
            raise("penalty", false);
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

// --------------------------------------------------------------------------- words

/// A call in `lang`, or in English when there's no translation for it.
pub fn translate(text: &'static str, lang: ArrayStr<2>) -> &'static str {
    let spanish = match &*lang {
        "es" => match text {
            "car left" => "coche a la izquierda",
            "car right" => "coche a la derecha",
            "three wide" => "tres en paralelo",
            "clear left" => "libre izquierda",
            "clear right" => "libre derecha",
            "clear all round" => "libre",
            "still there left" => "sigue a la izquierda",
            "still there right" => "sigue a la derecha",
            "full course yellow" => "amarilla en todo el circuito",
            "green flag, green flag" => "bandera verde, bandera verde",
            "red flag" => "bandera roja",
            "yellow flag on track" => "bandera amarilla en pista",
            "blue flag" => "bandera azul",
            "penalty" => "penalización",
            _ => text,
        },
        _ => text,
    };
    spanish
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The player at `t`, heading along -z (heading 0) at 50 m/s.
    fn player(t: f64) -> Car {
        Car {
            id: 1,
            vehicle: 1,
            time: Some(t),
            speed: Some(50.0),
            x: Some(0.0),
            y: Some(0.0),
            z: Some(0.0),
            heading: Some(0.0),
            ..Car::default()
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

    /// Runs the logic at 30 Hz from `from` to `to` with the same cars, returning the calls.
    fn run(logic: &mut Logic, from: f64, to: f64, others: &[Car]) -> Vec<&'static str> {
        let config = SpotterConfig::default();
        let mut calls = Vec::new();
        let mut t = from;
        while t < to {
            calls.extend(logic.update(&config, &player(t), others, None).into_iter().map(|c| c.text));
            t += 1.0 / 30.0;
        }
        calls
    }

    #[test]
    fn calls_a_car_alongside_and_clear() {
        let mut logic = Logic::default();
        // Alongside on the right, for a second
        assert_eq!(run(&mut logic, 0.0, 1.0, &[other(2, 1.0, 3.0)]), ["car right"]);
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
        assert_eq!(run(&mut logic, 0.0, 1.0, &[other(2, 0.0, 3.0)]), ["car right"]);
        assert_eq!(
            run(&mut logic, 1.0, 2.0, &[other(2, 0.0, 3.0), other(3, 0.0, -3.0)]),
            ["three wide"]
        );
        assert_eq!(run(&mut logic, 2.0, 2.5, &[]), ["clear all round"]);
    }

    #[test]
    fn keeps_a_car_at_the_edge_without_chattering() {
        let mut logic = Logic::default();
        assert_eq!(run(&mut logic, 0.0, 1.0, &[other(2, 4.0, 3.0)]), ["car right"]);
        // Past the way in (4.8 m) but within the way out (5.8 m): still there
        assert!(run(&mut logic, 1.0, 3.0, &[other(2, 5.3, 3.0)]).is_empty());
        assert_eq!(run(&mut logic, 3.0, 3.5, &[other(2, 6.5, 3.0)]), ["clear right"]);
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
        let mut slow = player(0.0);
        slow.speed = Some(2.0);
        for i in 0..30 {
            let mut p = slow.clone();
            p.time = Some(i as f64 / 30.0);
            assert!(logic.update(&config, &p, &[other(2, 0.0, 3.0)], None).is_empty());
        }

        let formation = Scoring {
            phase: Some("formation_lap".into()),
            ..Scoring::default()
        };
        let mut logic = Logic::default();
        for i in 0..30 {
            let calls = logic.update(&config, &player(i as f64 / 30.0), &[other(2, 0.0, 3.0)], Some(&formation));
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
                .update(&config, &player(t), &[], Some(scoring))
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
            [("yellow flag on track", false), ("blue flag", false), ("penalty", false)]
        );
        scoring.phase = Some("stopped".into());
        assert_eq!(step(0.5, &scoring), [("red flag", true)]);
    }

    #[test]
    fn moves_cars_to_the_players_time() {
        // Recorded 0.1 s earlier at 50 m/s along -z: 5 m further on by now
        let car = at_time(other(2, 0.0, 3.0), 0.1);
        assert!((car.z.unwrap() - -5.0).abs() < 1e-9);
    }

    #[test]
    fn translates_to_spanish() {
        assert_eq!(translate("car left", arrstr!("es")), "coche a la izquierda");
        assert_eq!(translate("car left", arrstr!("fr")), "car left");
    }

    /// Needs `EVAL_DB`: a recording, e.g. my-engineer's race fixture, whose last frame has a lapped
    /// car alongside on the right.
    #[test]
    #[ignore]
    fn replays_a_recording() {
        let db = PathBuf::from(std::env::var("EVAL_DB").expect("EVAL_DB isn't set"));
        let calls = replay(&db, &SpotterConfig::default()).unwrap();
        for call in &calls {
            println!("{:>8.1} {}{}", call.session_time, call.text, if call.urgent { " (urgent)" } else { "" });
        }
        assert!(calls.iter().any(|call| call.text == "car right"));
    }
}
