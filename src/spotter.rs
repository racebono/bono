//! The spotter: follows the running simulator's telemetry about thirty times a second and calls
//! cars alongside, flags, the car's own alerts and the weather the moment they happen, without
//! anyone asking. A port of my-engineer's `spotter.cpp`.
//!
//! It reads the database on a thread of its own, with a read-only connection of its own (closed as
//! soon as the game stops, so the plugin can delete the file), and turns what changed into calls.
//! They're said in the engineer's voice: urgent ones (traffic, the pit limiter, full course
//! yellows, red flags) cut in over the engineer, which carries on afterwards; the rest wait for it
//! to finish. A call a newer one makes out of date (a car called on a side that's just cleared) is
//! dropped before it's said, or cut off while it is, and the newer call is said in its place.

use crate::{
    arrstr,
    config::{ArrayStr, Config, SpotterConfig},
    engineer::Engineer,
    sim::Sim,
    synthesizer::{Drain, Drained, Effects, Speech, Synthesizer, plays_on},
};
use bono_i18n::t;
use bono_sim::sql::{Epoch, active_epoch};
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
const WARM_RETRY_AFTER: Duration = Duration::from_secs(5);
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
            "three wide left" => 1,
            "three wide right" => 2,
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

/// What a call is about. A newer call about the same thing makes an older one out of date; calls
/// about anything else are left alone.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WarningCategory {
    Traffic,
    Limiter,
    Flag,
    GapBehind,
    /// One-off news (a puncture, a penalty, the weather changing): never out of date by another
    Other(&'static str),
}

impl Raised {
    fn warning_category(&self) -> WarningCategory {
        match self.text {
            "car left" | "car right" | "three wide" | "three wide left" | "three wide right"
            | "clear left" | "clear right" | "clear all round" | "still there left"
            | "still there right" => WarningCategory::Traffic,
            "limiter, limiter" => WarningCategory::Limiter,
            "full course yellow" | "green flag, green flag" | "red flag" => WarningCategory::Flag,
            "car close behind" | "car within a second behind" => WarningCategory::GapBehind,
            text => WarningCategory::Other(text),
        }
    }

    /// What's still true of a pending call this replaces: the side still taken when a clear
    /// replaces three wide, and the side still clear when a car comes back to the other after
    /// clear all round.
    fn remaining_traffic(&self, older: &Raised) -> Option<Raised> {
        if !self.supersedes(older) {
            return None;
        }
        let text = match (older.text, self.text) {
            ("three wide", "clear left") => "car right",
            ("three wide", "clear right") => "car left",
            ("clear all round", _) => match self.traffic_sides() {
                1 => "clear right",
                2 => "clear left",
                _ => return None,
            },
            _ => return None,
        };
        Some(Raised {
            text,
            ..older.clone()
        })
    }

    /// A traffic update replaces only the sides it describes; other updates replace the older
    /// state in their category, as urgent as they are. Nothing but a red flag replaces a red
    /// flag: the session's stopped until it restarts, which isn't called.
    fn supersedes(&self, older: &Raised) -> bool {
        if self.urgent != older.urgent {
            return false;
        }
        match (self.warning_category(), older.warning_category()) {
            (WarningCategory::Traffic, WarningCategory::Traffic) => {
                self.traffic_sides() & older.traffic_sides() != 0
            }
            (WarningCategory::Flag, WarningCategory::Flag) => {
                older.text != "red flag" || self.text == "red flag"
            }
            (WarningCategory::Other(_), _) | (_, WarningCategory::Other(_)) => false,
            (new, old) => new == old,
        }
    }

    /// The same words as `other` (three wide is said the same whichever side has two cars).
    fn says_the_same_as(&self, other: &Raised) -> bool {
        let en = arrstr!("en");
        self.words(en) == other.words(en)
    }

    /// Cuts `playing` off while it's being said: it's out of date, and this says something else
    /// (the same words again would only restart it). Only urgent calls cut in; the rest take
    /// their turn.
    fn interrupts(&self, playing: &Raised) -> bool {
        self.urgent && self.supersedes(playing) && !self.says_the_same_as(playing)
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
    /// Failed warm-ups wait before retrying the same voice, language and effects
    warm_failed: HashMap<CacheKey, Instant>,
}

type CacheKey = (ArrayStr<2>, PathBuf, u32, String);

struct Running {
    db: PathBuf,
    settings: watch::Sender<(SpotterConfig, u32)>,
    calls: mpsc::Receiver<Followed>,
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
    Playing {
        /// Dropping it stops the call, mid-word if need be
        stream: AudioStreamOwner,
        drain: Drain,
        call: Raised,
    },
}

impl Spotter {
    pub fn new() -> Self {
        Self {
            running: None,
            queue: VecDeque::new(),
            saying: None,
            cache: HashMap::new(),
            warming: Vec::new(),
            warm_failed: HashMap::new(),
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
        let frequency = config.get().opponent_frequency().get();
        let db = sim
            .filter(|_| *settings.enabled())
            .and_then(|sim| sim.database())
            .map(|pool| pool.database().path().to_owned());

        match (&mut self.running, db) {
            (Some(running), Some(db)) if running.db == db => {
                running.settings.send_if_modified(|current| {
                    let changed = *current != (settings.clone(), frequency);
                    *current = (settings.clone(), frequency);
                    changed
                });
            }
            (_, Some(db)) => {
                self.running = Some(Running::start(db, settings.clone(), frequency));
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
            while let Some(followed) = self
                .running
                .as_ref()
                .and_then(|running| running.calls.try_recv().ok())
            {
                match followed {
                    Followed::Call(call) => self.hear(call, Instant::now()),
                    // What was called is about another session's cars
                    Followed::Epoch => {
                        self.queue.clear();
                        self.saying = None;
                    }
                }
            }
            self.warm(rt, config, synth, lang);
        }

        self.say(rt, config, synth, speaker, engineer, lang);
    }

    /// The speaker `which` is gone (unplugged): the call on it is dropped, so the spotter says the
    /// next ones and the engineer is let go.
    pub fn speaker_removed(&mut self, which: u32) {
        if let Some(Saying::Playing { stream, .. }) = &self.saying
            && plays_on(stream, which)
        {
            log::warn!("The speaker was removed while the spotter was speaking");
            self.saying = None;
        }
    }

    /// A new call: whatever it makes out of date goes before it's said (or, when it's being said,
    /// is cut off), and it waits its turn. Replacing the call being said, it takes that call's
    /// turn: next, after what that call still had to say.
    fn hear(&mut self, call: Raised, at: Instant) {
        // The words being said already say it: they're now about the newer state
        if let Some(Saying::Playing { call: playing, .. }) = &mut self.saying
            && call.supersedes(playing)
            && call.says_the_same_as(playing)
        {
            Self::retire(&mut self.queue, &call);
            *playing = call;
            return;
        }
        let place = self.drop_outdated(at, Some(&call));
        let queued = Self::enqueue(&mut self.queue, call, at);
        if let Some(place) = place.filter(|place| *place < queued)
            && let Some(call) = self.queue.remove(queued)
        {
            self.queue.insert(place, call);
        }
    }

    /// Queues `call`, in the turn of the first call it replaces (after what that call still had
    /// to say), or else at the back. Where it went.
    fn enqueue(queue: &mut VecDeque<(Raised, Instant)>, call: Raised, at: Instant) -> usize {
        let place = Self::retire(queue, &call).unwrap_or(queue.len());
        queue.insert(place, (call, at));
        place
    }

    /// Drops the queued calls `call` makes out of date, keeping what's still true of them. Where
    /// the first of them was, for `call` to take its turn.
    fn retire(queue: &mut VecDeque<(Raised, Instant)>, call: &Raised) -> Option<usize> {
        let mut place = None;
        let mut kept = 0;
        queue.retain_mut(|(queued, _)| {
            let keep = if let Some(remaining) = call.remaining_traffic(queued) {
                *queued = remaining;
                place.get_or_insert(kept + 1);
                true
            } else if call.supersedes(queued) {
                place.get_or_insert(kept);
                false
            } else {
                true
            };
            kept += usize::from(keep);
            keep
        });
        place
    }

    /// The calls' language: the configured one, or the pilot's; the engineer's fallback without
    /// a voice for it (English without any voice, when nothing can say them anyway).
    fn language(
        &self,
        config: &Config,
        settings: &SpotterConfig,
        engineer: &Engineer,
    ) -> ArrayStr<2> {
        let lang = settings
            .language()
            .unwrap_or_else(|| engineer.language(config));
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
        self.finish_warming(Instant::now());

        for text in WARM {
            let words = translate(text, lang).to_owned();
            let key = Self::key(config, lang, words.clone());
            if self.cache.contains_key(&key)
                || self.warm_failed.contains_key(&key)
                || self.warming.iter().any(|(k, _)| *k == key)
            {
                continue;
            }
            let recv = synth.synthesize(rt, lang, words, Self::effects(config));
            self.warming.push((key, recv));
        }
    }

    fn finish_warming(&mut self, now: Instant) {
        self.warm_failed
            .retain(|_, at| now.duration_since(*at) < WARM_RETRY_AFTER);
        self.warming
            .retain_mut(|(key, recv)| match recv.try_recv() {
                Ok(Ok(speech)) => {
                    self.cache.insert(key.clone(), Arc::new(speech));
                    false
                }
                Ok(Err(_)) | Err(oneshot::error::TryRecvError::Closed) => {
                    self.warm_failed.insert(key.clone(), now);
                    false
                }
                Err(oneshot::error::TryRecvError::Empty) => true,
            });
    }

    /// Drops the call being said once it's out of date (expired before it was heard, or replaced by
    /// `newer`), so it no longer holds the engineer and the newer call is said straight away.
    /// Replaced by `newer`, where in the queue `newer` goes to take its turn.
    fn drop_outdated(&mut self, now: Instant, newer: Option<&Raised>) -> Option<usize> {
        match &self.saying {
            Some(Saying::Synthesizing { call, at, .. }) => {
                let valid = call.valid_at(*at, now);
                let superseded = newer.is_some_and(|newer| newer.supersedes(call));
                if !valid || superseded {
                    let mut place = None;
                    if valid && let Some(newer) = newer {
                        place = Some(0);
                        if let Some(remaining) = newer.remaining_traffic(call) {
                            self.queue.push_front((remaining, *at));
                            place = Some(1);
                        }
                    }
                    self.saying = None;
                    return place;
                }
                None
            }
            // Started, so not restated: the pilot heard (part of) it, and the newer call is what
            // matters now
            Some(Saying::Playing { call, .. })
                if newer.is_some_and(|newer| newer.interrupts(call)) =>
            {
                log::debug!("Cutting off the spotter's \"{}\": out of date", call.text);
                self.saying = None;
                Some(0)
            }
            _ => None,
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
        self.drop_outdated(Instant::now(), None);

        // The call being said
        match &mut self.saying {
            Some(Saying::Synthesizing { key, recv, .. }) => match recv.try_recv() {
                Ok(Ok(speech)) => {
                    let speech = Arc::new(speech);
                    self.cache.insert(key.clone(), speech.clone());
                    if let Some(Saying::Synthesizing { call, .. }) = self.saying.take() {
                        self.saying = self.play(config, speaker, &speech, call);
                    }
                }
                Ok(Err(e)) => {
                    log::warn!("Error synthesizing a spotter call: {e:#}");
                    self.saying = None;
                }
                Err(oneshot::error::TryRecvError::Closed) => self.saying = None,
                Err(oneshot::error::TryRecvError::Empty) => {}
            },
            // A speaker that stops taking audio doesn't keep the spotter (and the engineer) waiting
            Some(Saying::Playing { stream, drain, .. }) => {
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
                    Some(speech) => self.play(config, speaker, &speech, call),
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
        call: Raised,
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
            Ok(stream) => Some(Saying::Playing {
                stream,
                drain: Drain::new(Instant::now()),
                call,
            }),
            Err(e) => {
                log::warn!("Error playing a spotter call: {e}");
                None
            }
        }
    }
}

impl Running {
    fn start(db: PathBuf, settings: SpotterConfig, frequency: u32) -> Self {
        let (settings, settings_recv) = watch::channel((settings, frequency));
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

/// What `follow` sends.
enum Followed {
    Call(Raised),
    /// The recording moved on to another epoch: the calls before are out of date
    Epoch,
}

/// Polls the database until stopped, sending the calls.
fn follow(
    db: &Path,
    settings: watch::Receiver<(SpotterConfig, u32)>,
    calls: mpsc::Sender<Followed>,
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
    let mut epoch = None;
    let mut failing = false;
    while !stop.load(Ordering::Relaxed) {
        let start = Instant::now();
        let (config, frequency) = settings.borrow().clone();

        let followed = epoch;
        let polled = poll(
            &conn,
            &mut logic,
            &config,
            frequency,
            &mut last_id,
            &mut epoch,
        );
        if epoch != followed && calls.send(Followed::Epoch).is_err() {
            return;
        }
        match polled {
            Ok(raised) => {
                failing = false;
                for call in raised {
                    if calls.send(Followed::Call(call)).is_err() {
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

/// The calls for the player's newest row, if it's new. Only the newest epoch's rows count (see
/// `bono_sim::sql`): in another session, what was called before starts over, and until the player
/// has a row in it (spectating, or a replay, which isn't recorded) there's nothing to call. The
/// epoch and its rows are read in one transaction, so they're as of the same write.
fn poll(
    conn: &Connection,
    logic: &mut Logic,
    config: &SpotterConfig,
    frequency: u32,
    last_id: &mut Option<i64>,
    seen_epoch: &mut Option<i64>,
) -> rusqlite::Result<Vec<Raised>> {
    let _snapshot = conn.unchecked_transaction()?;
    let epoch = active_epoch(conn)?;
    if epoch.id != *seen_epoch {
        *seen_epoch = epoch.id;
        *logic = Logic::default();
        // A cleared recording numbers its rows from 1 again
        *last_id = None;
    }
    let Some(car) = latest(conn, &epoch)? else {
        return Ok(Vec::new());
    };
    if Some(car.id) == *last_id {
        return Ok(Vec::new());
    }
    *last_id = Some(car.id);

    let player = player(conn, car)?;
    let others = frame(conn, &player.car, frequency, &epoch)?;
    let wheels = wheels(conn, player.car.id)?;
    let scoring = scoring(conn, None, &epoch)?;
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
        let others = frame(&conn, &player.car, 5, &Epoch::ALL)?;
        let wheels = wheels(&conn, player.car.id)?;
        let scoring = scoring(&conn, time, &Epoch::ALL)?;
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
    /// Cars alongside (left, right) as the simulator itself reports them, for simulators that
    /// don't give the other cars' positions. `None` when it doesn't: they're worked out from the
    /// positions instead.
    alongside: Option<[u8; 2]>,
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

/// The player's newest row in `epoch`.
fn latest(conn: &Connection, epoch: &Epoch) -> rusqlite::Result<Option<Car>> {
    conn.prepare_cached(&format!(
        "SELECT {CAR_COLUMNS} FROM telemetry WHERE is_player = 1 AND id >= ?1 \
         ORDER BY id DESC LIMIT 1"
    ))?
    .query_row([epoch.telemetry_from], car)
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
        alongside: alongside(conn, car.id)?,
        car,
        lap_invalidated,
        engine_overheating,
        gap_behind,
        limiter,
        parts_detached: parts_detached.unwrap_or(false),
    })
}

/// The cars alongside the simulator reports at the player's row, when it reports them (both
/// sides). Recordings made before the columns existed don't have them.
fn alongside(conn: &Connection, telemetry_id: i64) -> rusqlite::Result<Option<[u8; 2]>> {
    let columns = conn
        .prepare_cached(
            "SELECT count(*) FROM pragma_table_info('telemetry') \
             WHERE name IN ('cars_left', 'cars_right')",
        )?
        .query_row([], |row| row.get::<_, i64>(0))?;
    if columns != 2 {
        return Ok(None);
    }
    let (left, right) = conn
        .prepare_cached("SELECT cars_left, cars_right FROM telemetry WHERE id = ?1")?
        .query_row([telemetry_id], |row| {
            Ok((row.get::<_, Option<i64>>(0)?, row.get::<_, Option<i64>>(1)?))
        })?;
    let count = |cars: i64| u8::try_from(cars.clamp(0, 2)).unwrap_or(0);
    Ok(left
        .zip(right)
        .map(|(left, right)| [count(left), count(right)]))
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

/// Every other car near the player's row in `epoch`, moved to the player's time.
fn frame(
    conn: &Connection,
    player: &Car,
    frequency: u32,
    epoch: &Epoch,
) -> rusqlite::Result<Vec<Car>> {
    let Some(time) = player.time else {
        return Ok(Vec::new());
    };

    // Keep the default frame unchanged; sparse recordings need a full opponent cycle,
    // with slack for polling and rows for the rest of the field.
    let (window, max_age) = if frequency > 5 {
        let previous = conn
            .prepare_cached(
                "SELECT session_time FROM telemetry WHERE is_player = 1 AND id < ?1 \
                 AND id >= ?2 ORDER BY id DESC LIMIT 1",
            )?
            .query_row([player.id, epoch.telemetry_from], |row| {
                row.get::<_, Option<f64>>(0)
            })
            .optional()?
            .flatten();
        let interval = previous.map_or(0.0, |previous| (time - previous).max(0.0));
        (
            FRAME_WINDOW * ((i64::from(frequency) + 4) / 5),
            MAX_FRAME_AGE.max(interval * f64::from(frequency) * 1.5),
        )
    } else {
        (FRAME_WINDOW, MAX_FRAME_AGE)
    };
    let mut statement = conn.prepare_cached(&format!(
        "SELECT {CAR_COLUMNS} FROM telemetry WHERE id BETWEEN ?1 AND ?2"
    ))?;
    let rows = statement.query_map(
        [
            (player.id - window).max(epoch.telemetry_from),
            player.id + window,
        ],
        car,
    )?;

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
        if key.0 >= max_age {
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
/// before `upto`, when replaying), in `epoch`.
fn scoring(
    conn: &Connection,
    upto: Option<f64>,
    epoch: &Epoch,
) -> rusqlite::Result<Option<Scoring>> {
    let session = conn
        .prepare_cached(
            "SELECT full_course_yellow, phase, \
             EXISTS (SELECT 1 FROM session_sector WHERE session_id = session.id AND local_yellow), \
             rain, track_wetness_average, session_type, session_end_time \
             FROM session WHERE (?1 IS NULL OR session_time <= ?1) AND id >= ?2 \
             ORDER BY id DESC LIMIT 1",
        )?
        .query_row(rusqlite::params![upto, epoch.session_from], |row| {
            Ok(Scoring {
                full_course_yellow: row.get(0)?,
                phase: row.get(1)?,
                yellow: row.get(2)?,
                rain: row.get(3)?,
                wetness: row.get(4)?,
                session_type: row.get(5)?,
                session_end_time: row.get(6)?,
                ..Scoring::default()
            })
        })
        .optional()?;
    let Some(mut scoring) = session else {
        return Ok(None);
    };

    // Live, try the newest rows first, then look further back if the player has been in the garage
    let standing = match upto {
        None => conn
            .prepare_cached(
                "SELECT flag, penalties FROM standings WHERE is_player = 1 \
                 AND id > (SELECT coalesce(max(id), 0) - 5000 FROM standings) AND id >= ?1 \
                 ORDER BY id DESC LIMIT 1",
            )?
            .query_row([epoch.standings_from], |row| Ok((row.get(0)?, row.get(1)?)))
            .optional()?
            .map(|standing| Ok(Some(standing)))
            .unwrap_or_else(|| {
                conn.prepare_cached(
                    "SELECT flag, penalties FROM standings WHERE is_player = 1 AND id >= ?1 \
                     ORDER BY id DESC LIMIT 1",
                )?
                .query_row([epoch.standings_from], |row| Ok((row.get(0)?, row.get(1)?)))
                .optional()
            })?,
        Some(upto) => conn
            .prepare_cached(
                "SELECT flag, penalties FROM standings WHERE is_player = 1 AND session_time <= ?1 \
                 AND id >= ?2 ORDER BY id DESC LIMIT 1",
            )?
            .query_row(rusqlite::params![upto, epoch.standings_from], |row| {
                Ok((row.get(0)?, row.get(1)?))
            })
            .optional()?,
    };
    if let Some((flag, penalties)) = standing {
        scoring.flag = flag;
        scoring.penalties = penalties;
    }

    // The forecast is LMU's only, written as a whole set of rows when it changes
    if let Some(kind) = &scoring.session_type {
        // Older recordings have no set identity beyond the session clock.
        let set = if conn.column_exists(None, "forecast", "generation")? {
            "generation"
        } else {
            "session_time"
        };
        let mut statement = conn.prepare_cached(&format!(
            "SELECT session_fraction, rain_chance FROM forecast WHERE session_type = ?1 \
             AND {set} = (SELECT {set} FROM forecast \
             WHERE ?2 IS NULL OR session_time <= ?2 ORDER BY id DESC LIMIT 1) \
             ORDER BY session_fraction",
        ))?;
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

/// What's beside the player, however it's known: worked out from the cars' positions
/// ([`Geometry`]) or reported by the simulator ([`Player::alongside`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Proximity {
    /// Can't tell (no positions, or the simulator isn't reporting): never a reason to say clear
    Unavailable,
    /// Cars alongside on each side, at most two
    Known { left: u8, right: u8 },
}

/// Works out the cars beside the player from everyone's positions, following each car: it counts
/// once it's been there `debounce`, and stops counting past the (wider) exit distances.
#[derive(Debug, Default)]
struct Geometry {
    /// Cars beside the player, not yet for long enough: vehicle -> (side, since)
    pending: HashMap<i64, (Side, f64)>,
    /// Cars beside the player: vehicle -> side
    overlap: HashMap<i64, Side>,
}

impl Geometry {
    fn reset(&mut self) {
        self.pending.clear();
        self.overlap.clear();
    }

    /// Positions can't tell two cars side by side on one side from two in line there, so a side
    /// counts one car at most.
    fn update(
        &mut self,
        config: &SpotterConfig,
        player: &Car,
        others: &[Car],
        t: f64,
    ) -> Proximity {
        if player.x.is_none() || player.z.is_none() || player.heading.is_none() {
            self.reset();
            return Proximity::Unavailable;
        }
        self.track(config, player, others, t);
        let mut sides = [0; 2];
        for side in self.overlap.values() {
            sides[side.index()] = 1;
        }
        Proximity::Known {
            left: sides[Side::Left.index()],
            right: sides[Side::Right.index()],
        }
    }

    fn track(&mut self, config: &SpotterConfig, player: &Car, others: &[Car], t: f64) {
        self.pending
            .retain(|vehicle, _| others.iter().any(|other| other.vehicle == *vehicle));
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
}

struct Logic {
    geometry: Geometry,
    /// Cars alongside on each side as last called (0 while it can't be told)
    alongside: [u8; 2],
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
            geometry: Geometry::default(),
            alongside: [0; 2],
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

        let stopped = p.speed.unwrap_or(0.0) < *config.min_speed() || p.in_pit_lane;
        // On the formation lap cars weave and run side by side on purpose
        let formation_lap = scoring.is_some_and(|s| s.phase.as_deref() == Some("formation_lap"));

        let proximity = if stopped || formation_lap {
            self.geometry.reset();
            Proximity::Unavailable
        } else if let Some([left, right]) = player.alongside {
            self.geometry.reset();
            Proximity::Known { left, right }
        } else {
            self.geometry.update(config, p, others, t)
        };
        self.traffic(config, proximity, t, &mut calls);
        if let Some(prev) = self.prev.take() {
            let race = scoring.is_some_and(|s| s.session_type.as_deref() == Some("race"));
            self.alerts(
                config,
                player,
                &prev,
                wheels,
                t,
                race && !formation_lap,
                &mut calls,
            );
        }
        if let Some(scoring) = scoring {
            self.flags(scoring, &mut calls);
            self.weather(config, scoring, wheels.wet, t, stopped, &mut calls);
        }

        self.prev_flat = wheels.flat;
        self.prev = Some(player.clone());
        calls.calls
    }

    /// Cars alongside: who came, who went, and who's still there. While it can't be told the
    /// sides empty silently, and calls start again from what's next seen.
    fn traffic(&mut self, config: &SpotterConfig, proximity: Proximity, t: f64, calls: &mut Calls) {
        let before = self.alongside;
        let (after, known) = match proximity {
            Proximity::Unavailable => ([0; 2], false),
            Proximity::Known { left, right } => ([left.min(2), right.min(2)], true),
        };
        self.alongside = after;

        let became = Side::BOTH
            .into_iter()
            .filter(|s| after[s.index()] > 0 && before[s.index()] == 0)
            .collect::<Vec<_>>();
        let cleared = Side::BOTH
            .into_iter()
            .filter(|s| before[s.index()] > 0 && after[s.index()] == 0)
            .collect::<Vec<_>>();
        // Three wide: a car each side, or two on one side
        let three_wide = |sides: [u8; 2]| sides[0] + sides[1] >= 2;
        // Two on one side is said the same but is only about that side, so a clear of the
        // other doesn't leave a car there in the queue
        let three_wide_call = |sides: [u8; 2]| match sides {
            [0, _] => "three wide right",
            [_, 0] => "three wide left",
            _ => "three wide",
        };

        let mut clears = Vec::new();
        for &side in &cleared {
            let held = self.side_since[side.index()].map_or(0.0, |since| t - since);
            self.side_since[side.index()] = None;
            self.last_still[side.index()] = None;
            if held >= *config.min_hold_for_clear() && known {
                clears.push(side);
            }
        }
        for side in &became {
            self.side_since[side.index()] = Some(t);
            self.last_still[side.index()] = Some(t);
        }
        let side_call = |side: Side| match side {
            Side::Left => "car left",
            Side::Right => "car right",
        };
        // What's alongside was said this update
        let mut stated = true;
        if let Some(&first) = became.first() {
            calls.raise(
                if three_wide(after) {
                    three_wide_call(after)
                } else {
                    side_call(first)
                },
                true,
            );
        } else if three_wide(after) && !three_wide(before) {
            // A second car joined one already alongside on the same side
            calls.raise(three_wide_call(after), true);
        } else if three_wide(before) && !three_wide(after) && clears.is_empty() {
            // One of two went (from one side, or too soon to call clear): the other's still there
            if let Some(side) = Side::BOTH.into_iter().find(|s| after[s.index()] > 0) {
                calls.raise(side_call(side), true);
            }
        } else {
            stated = false;
        }

        match clears.as_slice() {
            // Both went: one that went too soon to call clear is clear all the same, and a
            // one-sided clear would leave a queued three wide to it
            [_, ..] if cleared.len() == 2 => calls.raise("clear all round", true),
            [Side::Left] => calls.raise("clear left", true),
            [Side::Right] => calls.raise("clear right", true),
            _ => {}
        }
        // A car each side became two on one: still three wide
        if became.is_empty() && !cleared.is_empty() && three_wide(after) {
            calls.raise(three_wide_call(after), true);
            stated = true;
        }
        // A reminder due now would replace what was just said in the queue: it waits
        if stated {
            for side in Side::BOTH {
                if after[side.index()] > 0 {
                    self.last_still[side.index()] = Some(t);
                }
            }
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

    /// Rising edges of the player's own alerts.
    #[allow(clippy::too_many_arguments)]
    fn alerts(
        &mut self,
        config: &SpotterConfig,
        p: &Player,
        prev: &Player,
        wheels: &Wheels,
        t: f64,
        race_gap: bool,
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
        // formation lap, where the field runs nose to tail. Standings gaps outside races
        // do not describe proximity on track
        let gap = p.gap_behind.unwrap_or(0.0);
        if gap != 0.0 && gap >= *config.gap_behind_off() {
            self.gap_armed = true;
        } else if gap > 0.0
            && gap <= *config.gap_behind_on()
            && self.gap_armed
            && !p.car.in_pit_lane
            && t > GAP_BEHIND_AFTER
            && race_gap
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
            // The caution ending as the session's stopped isn't a restart: it's still red
            && now.phase.as_deref() != Some("stopped")
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
        "three wide" | "three wide left" | "three wide right" => t!("call.three_wide": lang),
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

    #[test]
    fn scoring_uses_newest_forecast_set_with_frozen_session_time() {
        use bono_sim::{sql::ForecastWriter, sys::bono_forecast_point};

        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(&format!(
            "CREATE TABLE forecast (id INTEGER PRIMARY KEY{})",
            <bono_forecast_point as bono_sim::sql::Columns>::DEFINITIONS
        ))
        .unwrap();
        bono_sim::sql::recreate_tables(&conn).unwrap();
        conn.execute(
            "INSERT INTO session (session_time, session_type) VALUES (0, 'race')",
            [],
        )
        .unwrap();
        let point = |time, kind: &str, fraction, chance| {
            let mut point = bono_forecast_point::default();
            point.session_time = Some(time).into();
            point.session_type = Some(kind.to_owned()).into();
            point.session_fraction = Some(fraction).into();
            point.rain_chance = Some(chance).into();
            point
        };
        let mut writer = ForecastWriter::new(&conn).unwrap();
        for time in [0.0, 20.0] {
            writer
                .write(vec![
                    point(time, "race", 0.0, 0.0),
                    point(time, "race", 0.5, 0.9),
                    point(time, "race", 1.0, 0.8),
                ])
                .unwrap();
            writer
                .write(vec![
                    point(time, "qualifying", 0.0, 0.7),
                    point(time, "race", 0.0, 0.0),
                    point(time, "race", 1.0, 0.1),
                ])
                .unwrap();
            for upto in [None, Some(time)] {
                assert_eq!(
                    scoring(&conn, upto, &Epoch::ALL).unwrap().unwrap().forecast,
                    vec![(0.0, 0.0), (1.0, 0.1)]
                );
            }
        }
        assert_eq!(
            scoring(&conn, Some(0.0), &Epoch::ALL)
                .unwrap()
                .unwrap()
                .forecast,
            vec![(0.0, 0.0), (1.0, 0.1)]
        );
        writer
            .write(vec![point(30.0, "qualifying", 0.0, 0.6)])
            .unwrap();
        assert!(
            scoring(&conn, None, &Epoch::ALL)
                .unwrap()
                .unwrap()
                .forecast
                .is_empty()
        );
    }

    #[test]
    fn failed_warmups_wait_for_backoff_for_the_same_key() {
        let now = Instant::now();
        for closed in [false, true] {
            let mut spotter = Spotter::new();
            let key = (arrstr!("en"), PathBuf::new(), 0, "car left".into());
            let (send, recv) = oneshot::channel();
            spotter.warming.push((key.clone(), recv));
            if closed {
                drop(send);
            } else {
                assert!(send.send(Err(anyhow::anyhow!("No voice loaded"))).is_ok());
            }
            spotter.finish_warming(now);
            assert!(spotter.warming.is_empty());
            assert!(spotter.cache.is_empty());
            assert!(spotter.warm_failed.contains_key(&key));
            for changed in [
                (arrstr!("es"), key.1.clone(), key.2, key.3.clone()),
                (key.0, PathBuf::from("voice.onnx"), key.2, key.3.clone()),
                (key.0, key.1.clone(), 1.0_f32.to_bits(), key.3.clone()),
            ] {
                assert!(!spotter.warm_failed.contains_key(&changed));
            }
            spotter.finish_warming(now + WARM_RETRY_AFTER - Duration::from_nanos(1));
            assert!(spotter.warm_failed.contains_key(&key));
            spotter.finish_warming(now + WARM_RETRY_AFTER);
            assert!(!spotter.warm_failed.contains_key(&key));
        }
    }

    #[test]
    fn warmups_keep_pending_jobs_and_cache_successes() {
        let mut spotter = Spotter::new();
        let key = (arrstr!("en"), PathBuf::new(), 0, "car left".into());
        let (send, recv) = oneshot::channel();
        spotter.warming.push((key.clone(), recv));
        spotter.finish_warming(Instant::now());
        assert_eq!(spotter.warming.len(), 1);
        assert!(
            send.send(Ok(Speech {
                samples: vec![0.5],
                sample_rate: 24000,
            }))
            .is_ok()
        );
        spotter.finish_warming(Instant::now());
        assert!(spotter.warming.is_empty());
        assert_eq!(spotter.cache[&key].samples, [0.5]);
        assert!(spotter.warm_failed.is_empty());
    }

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
        // Each replacement takes the turn of the call it replaces
        assert_eq!(
            queue.iter().map(|(call, _)| call.text).collect::<Vec<_>>(),
            ["blue flag", "clear right", "red flag", "limiter, limiter"]
        );
        assert_eq!(queue.iter().position(|(call, _)| call.urgent), Some(1));
    }

    #[test]
    fn only_a_red_flag_replaces_a_red_flag() {
        for newer in ["full course yellow", "green flag, green flag"] {
            let queue = queued(&[("red flag", true), (newer, true)]);
            assert_eq!(
                queue.iter().map(|(call, _)| call.text).collect::<Vec<_>>(),
                ["red flag", newer]
            );
        }
        let queue = queued(&[("red flag", true), ("red flag", true)]);
        assert_eq!(queue.len(), 1);
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
            [("still there left", new), ("limiter, limiter", old)]
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
        spotter.drop_outdated(at, Some(&newer));
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
            spotter.drop_outdated(at + lifetime - Duration::from_nanos(1), None);
            assert!(spotter.saying.is_some());
            // Completion at the deadline must not reach playback, even if already received.
            assert!(
                send.send(Ok(Speech {
                    samples: vec![],
                    sample_rate: 24000,
                }))
                .is_ok()
            );
            spotter.drop_outdated(at + lifetime, None);
            assert!(spotter.saying.is_none());
        }
    }

    #[test]
    fn expired_pending_synthesis_releases_the_call_without_waiting_for_completion() {
        let at = Instant::now();
        let (mut spotter, send) = pending_call("car right", true, at);
        spotter.drop_outdated(at + URGENT_FOR, None);
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
                spotter.drop_outdated(at, Some(&newer));
                assert!(spotter.saying.is_none(), "{old} superseded by {new}");
            }
        }
    }

    #[test]
    fn single_side_clear_preserves_remaining_queued_traffic() {
        // The first car call has already left the queue for playback.
        for (first, clear, remaining) in [
            ("car left", "clear left", "car right"),
            ("car right", "clear right", "car left"),
        ] {
            let mut queue: VecDeque<_> = queued(&[(first, true)]).into();
            queue.pop_front();
            let at = Instant::now();
            for text in ["three wide", clear] {
                Spotter::enqueue(
                    &mut queue,
                    Raised {
                        text,
                        number: None,
                        urgent: true,
                        session_time: 1.0,
                    },
                    at,
                );
            }
            assert_eq!(
                queue.iter().map(|(call, _)| call.text).collect::<Vec<_>>(),
                [remaining, clear],
            );
        }
    }

    #[test]
    fn clearing_two_on_one_side_leaves_no_car_on_the_other() {
        for (three_wide, clear) in [
            ("three wide right", "clear right"),
            ("three wide left", "clear left"),
        ] {
            let mut queue = VecDeque::new();
            for text in [three_wide, clear] {
                Spotter::enqueue(
                    &mut queue,
                    Raised {
                        text,
                        number: None,
                        urgent: true,
                        session_time: 1.0,
                    },
                    Instant::now(),
                );
            }
            assert_eq!(
                queue.iter().map(|(call, _)| call.text).collect::<Vec<_>>(),
                [clear],
            );
        }
    }

    #[test]
    fn single_side_clear_preserves_remaining_synthesizing_traffic() {
        for completed in [false, true] {
            for (clear, remaining) in [("clear left", "car right"), ("clear right", "car left")] {
                let at = Instant::now();
                let (mut spotter, send) = pending_call("three wide", true, at);
                if completed {
                    assert!(
                        send.send(Ok(Speech {
                            samples: vec![],
                            sample_rate: 24000
                        }))
                        .is_ok()
                    );
                }
                let newer = Raised {
                    text: clear,
                    number: None,
                    urgent: true,
                    session_time: 1.0,
                };
                spotter.drop_outdated(at, Some(&newer));
                Spotter::enqueue(&mut spotter.queue, newer, at);
                assert!(spotter.saying.is_none());
                assert_eq!(
                    spotter
                        .queue
                        .iter()
                        .map(|(call, queued_at)| (call.text, *queued_at))
                        .collect::<Vec<_>>(),
                    [(remaining, at), (clear, at)],
                );
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
            spotter.drop_outdated(at, Some(&newer));
            assert!(spotter.saying.is_some(), "unrelated {new}");
        }
    }

    fn call(text: &'static str, urgent: bool) -> Raised {
        Raised {
            text,
            urgent,
            number: None,
            session_time: 1.0,
        }
    }

    #[test]
    fn a_change_of_the_same_state_cuts_off_the_call_being_said() {
        for (playing, newer) in [
            ("car right", "clear right"),
            ("car left", "clear left"),
            ("car left", "clear all round"),
            ("still there right", "clear right"),
            ("three wide", "clear left"),
            ("three wide", "car right"),
            ("three wide right", "clear right"),
            ("car right", "three wide"),
            ("clear right", "car right"),
            ("full course yellow", "green flag, green flag"),
            ("green flag, green flag", "red flag"),
        ] {
            assert!(
                call(newer, true).interrupts(&call(playing, true)),
                "{newer} over {playing}"
            );
        }
    }

    #[test]
    fn other_news_waits_for_the_call_being_said() {
        for (playing, newer) in [
            // Another side, or something else altogether
            ("car right", "clear left"),
            ("car left", "car right"),
            ("three wide left", "clear right"),
            ("car right", "limiter, limiter"),
            ("limiter, limiter", "full course yellow"),
            ("full course yellow", "car left"),
            // Saying it again would only restart it
            ("car right", "car right"),
            ("limiter, limiter", "limiter, limiter"),
            ("three wide left", "three wide"),
            ("three wide", "three wide right"),
        ] {
            assert!(
                !call(newer, true).interrupts(&call(playing, true)),
                "{newer} over {playing}"
            );
        }
        // Calls that aren't urgent take their turn
        assert!(!call("car close behind", false).interrupts(&call("car right", true)));
        assert!(
            !call("car close behind", false).interrupts(&call("car within a second behind", false))
        );
        assert!(!call("rain stopped", false).interrupts(&call("rain starting", false)));
    }

    fn texts_of(queue: &VecDeque<(Raised, Instant)>) -> Vec<&'static str> {
        queue.iter().map(|(call, _)| call.text).collect()
    }

    #[test]
    fn a_replacement_takes_the_turn_of_the_call_it_replaces() {
        let at = Instant::now();
        for (pending, newer, expected) in [
            (
                "car right",
                "clear right",
                &["clear right", "limiter, limiter"][..],
            ),
            (
                "three wide",
                "clear left",
                &["car right", "clear left", "limiter, limiter"][..],
            ),
            (
                "clear all round",
                "car right",
                &["clear left", "car right", "limiter, limiter"][..],
            ),
        ] {
            let (mut spotter, _send) = pending_call(pending, true, at);
            spotter.hear(call("limiter, limiter", true), at);
            spotter.hear(call(newer, true), at);
            assert!(spotter.saying.is_none());
            assert_eq!(texts_of(&spotter.queue), expected, "{newer} over {pending}");
        }
        // A second replacement before the first is said keeps the turn
        let (mut spotter, _send) = pending_call("green flag, green flag", true, at);
        for newer in ["limiter, limiter", "full course yellow", "red flag"] {
            spotter.hear(call(newer, true), at);
        }
        assert_eq!(texts_of(&spotter.queue), ["red flag", "limiter, limiter"]);
        // Unrelated news still waits its turn
        let (mut spotter, _send) = pending_call("car right", true, at);
        spotter.hear(call("limiter, limiter", true), at);
        spotter.hear(call("red flag", true), at);
        assert!(spotter.saying.is_some());
        assert_eq!(texts_of(&spotter.queue), ["limiter, limiter", "red flag"]);
    }

    #[test]
    fn a_car_back_on_one_side_keeps_the_other_side_clear() {
        for (newer, remaining) in [
            ("car right", "clear left"),
            ("three wide right", "clear left"),
            ("car left", "clear right"),
            ("three wide left", "clear right"),
        ] {
            let queue = queued(&[("clear all round", true), (newer, true)]);
            assert_eq!(
                queue.iter().map(|(call, _)| call.text).collect::<Vec<_>>(),
                [remaining, newer],
            );
        }
        let queue = queued(&[("clear all round", true), ("three wide", true)]);
        assert_eq!(
            queue.iter().map(|(call, _)| call.text).collect::<Vec<_>>(),
            ["three wide"],
        );
    }

    #[test]
    fn a_newer_gap_behind_replaces_the_queued_one() {
        let queue = queued(&[
            ("car within a second behind", false),
            ("penalty", false),
            ("car close behind", false),
            ("penalty", false),
        ]);
        assert_eq!(
            queue.iter().map(|(call, _)| call.text).collect::<Vec<_>>(),
            ["car close behind", "penalty", "penalty"]
        );
    }

    #[test]
    fn a_newer_gap_behind_replaces_the_one_being_synthesized() {
        let at = Instant::now();
        let (mut spotter, send) = pending_call("car within a second behind", false, at);
        spotter.hear(call("car close behind", false), at);
        assert!(spotter.saying.is_none());
        assert!(send.is_closed());
        assert_eq!(
            spotter
                .queue
                .iter()
                .map(|(call, _)| call.text)
                .collect::<Vec<_>>(),
            ["car close behind"]
        );
    }

    #[test]
    #[ignore = "run alone with SDL_AUDIO_DRIVER=dummy"]
    fn a_clear_cuts_off_the_car_call_being_said_and_is_said_instead() {
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let _entered = runtime.enter();
        let dir = tempfile::tempdir().unwrap();
        let config = runtime
            .block_on(Config::new(dir.path().join("config.json")))
            .unwrap();
        let mut synth = Synthesizer::new(runtime.handle(), &config.get());
        let mut engineer = Engineer::new(reqwest::Client::new(), None);
        let sdl = sdl3::init().unwrap();
        let audio = sdl.audio().unwrap();
        let lang = arrstr!("en");
        // Long enough to still be playing when the clear comes
        let speech = || Speech {
            samples: vec![0.0; 24000 * 5],
            sample_rate: 24000,
        };
        let device = audio.open_playback_device(&speech().spec()).unwrap();

        let mut spotter = Spotter::new();
        spotter.cache.insert(
            Spotter::key(&config, lang, call("clear right", true).words(lang)),
            Arc::new(speech()),
        );
        spotter.saying = spotter.play(&config, Some(&device), &speech(), call("car right", true));
        let playing = |spotter: &Spotter| match &spotter.saying {
            Some(Saying::Playing { call, .. }) => Some(call.text),
            _ => None,
        };
        assert_eq!(playing(&spotter), Some("car right"));

        // News about something else waits for it
        spotter.hear(call("limiter, limiter", true), Instant::now());
        assert_eq!(playing(&spotter), Some("car right"));

        // The clear takes its turn, ahead of what was waiting
        spotter.hear(call("clear right", true), Instant::now());
        assert_eq!(playing(&spotter), None);
        assert_eq!(
            texts_of(&spotter.queue),
            ["clear right", "limiter, limiter"]
        );
        spotter.say(
            runtime.handle(),
            &config,
            &mut synth,
            Some(&device),
            &mut engineer,
            lang,
        );
        assert_eq!(playing(&spotter), Some("clear right"));
        assert_eq!(texts_of(&spotter.queue), ["limiter, limiter"]);
    }

    #[test]
    #[ignore = "run alone with SDL_AUDIO_DRIVER=dummy"]
    fn the_same_words_carry_on_about_the_newer_state() {
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let _entered = runtime.enter();
        let dir = tempfile::tempdir().unwrap();
        let config = runtime
            .block_on(Config::new(dir.path().join("config.json")))
            .unwrap();
        let sdl = sdl3::init().unwrap();
        let audio = sdl.audio().unwrap();
        let speech = Speech {
            samples: vec![0.0; 24000 * 5],
            sample_rate: 24000,
        };
        let device = audio.open_playback_device(&speech.spec()).unwrap();
        let mut spotter = Spotter::new();
        spotter.saying = spotter.play(
            &config,
            Some(&device),
            &speech,
            call("three wide left", true),
        );
        let playing = |spotter: &Spotter| match &spotter.saying {
            Some(Saying::Playing { call, .. }) => Some(call.text),
            _ => None,
        };

        // Two cars on the left become one each side: still three wide, said once
        spotter.hear(call("three wide", true), Instant::now());
        assert_eq!(playing(&spotter), Some("three wide"));
        assert!(spotter.queue.is_empty());

        // So the right clearing cuts it off
        spotter.hear(call("clear right", true), Instant::now());
        assert_eq!(playing(&spotter), None);
        assert_eq!(texts_of(&spotter.queue), ["clear right"]);
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
    fn missing_car_restarts_the_debounce() {
        let mut logic = Logic::default();
        let config = SpotterConfig::default();
        let others = [other(2, 1.0, 3.0)];
        logic
            .geometry
            .track(&config, &player(0.0).car, &others, 0.0);
        logic.geometry.track(&config, &player(0.1).car, &[], 0.1);
        assert!(logic.geometry.pending.is_empty());
        logic
            .geometry
            .track(&config, &player(1.0).car, &others, 1.0);
        assert!(logic.geometry.overlap.is_empty());
        logic
            .geometry
            .track(&config, &player(1.2).car, &others, 1.2);
        assert!(logic.geometry.overlap.get(&2) == Some(&Side::Right));
    }

    #[test]
    fn sparse_frames_keep_traffic_until_it_really_clears() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE telemetry (
                id INTEGER PRIMARY KEY, vehicle_id INTEGER, session_time REAL,
                is_player INTEGER, in_pit_lane INTEGER, speed REAL,
                position_x REAL, position_y REAL, position_z REAL, heading REAL
            );",
        )
        .unwrap();
        let mut logic = Logic::default();
        let mut calls = Vec::new();
        let mut default_expired = false;
        for tick in 0..240 {
            let t = tick as f64 / 60.0;
            // Sixty opponents, recorded once a second; the player is kept at 60 Hz.
            if tick % 60 == 0 {
                for vehicle in 2..=61 {
                    let x = if vehicle == 2 { -3.0 } else { -100.0 };
                    let z = -50.0 * t + if tick < 120 { 0.0 } else { -100.0 };
                    conn.execute(
                        "INSERT INTO telemetry VALUES (NULL, ?1, ?2, 0, 0, 50, ?3, 0, ?4, 0)",
                        rusqlite::params![vehicle, t, x, z],
                    )
                    .unwrap();
                }
            }
            conn.execute(
                "INSERT INTO telemetry VALUES (NULL, 1, ?1, 1, 0, 50, 0, 0, 0, 0)",
                [t],
            )
            .unwrap();
            let mut p = player(t);
            p.car.id = conn.last_insert_rowid();
            p.car.z = Some(-50.0 * t);
            let others = frame(&conn, &p.car, 60, &Epoch::ALL).unwrap();
            assert_eq!(others.len(), 60);
            if tick == 59 {
                default_expired = frame(&conn, &p.car, 5, &Epoch::ALL).unwrap().is_empty();
            }
            calls.extend(texts(logic.update(
                &SpotterConfig::default(),
                &p,
                &others,
                &Wheels::default(),
                None,
            )));
        }
        assert!(default_expired);
        assert_eq!(calls, ["car right", "clear right"]);
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

    /// Runs the logic at 30 Hz from `from` to `to` with the simulator reporting the cars
    /// `alongside` and nobody's position, returning the calls.
    fn run_reported(
        logic: &mut Logic,
        from: f64,
        to: f64,
        alongside: Option<[u8; 2]>,
    ) -> Vec<&'static str> {
        let config = SpotterConfig::default();
        let mut calls = Vec::new();
        let mut t = from;
        while t < to {
            let mut p = player(t);
            (p.car.x, p.car.z, p.car.heading) = (None, None, None);
            p.alongside = alongside;
            calls.extend(texts(logic.update(
                &config,
                &p,
                &[],
                &Wheels::default(),
                None,
            )));
            t += 1.0 / 30.0;
        }
        calls
    }

    #[test]
    fn calls_reported_cars_alongside_without_waiting() {
        let mut logic = Logic::default();
        assert_eq!(
            run_reported(&mut logic, 0.0, 1.0 / 60.0, Some([0, 1])),
            ["car right"]
        );
        assert!(run_reported(&mut logic, 1.0 / 60.0, 1.0, Some([0, 1])).is_empty());
        assert_eq!(
            run_reported(&mut logic, 1.0, 1.5, Some([0, 0])),
            ["clear right"]
        );
    }

    #[test]
    fn calls_three_wide_with_two_reported_on_one_side() {
        let mut logic = Logic::default();
        assert_eq!(
            run_reported(&mut logic, 0.0, 1.0, Some([1, 0])),
            ["car left"]
        );
        assert_eq!(
            run_reported(&mut logic, 1.0, 2.0, Some([2, 0])),
            ["three wide left"]
        );
        // One of the two went: the other's still there
        assert_eq!(
            run_reported(&mut logic, 2.0, 3.0, Some([1, 0])),
            ["car left"]
        );
        assert_eq!(
            run_reported(&mut logic, 3.0, 4.0, Some([2, 0])),
            ["three wide left"]
        );
        assert_eq!(
            run_reported(&mut logic, 4.0, 4.5, Some([0, 0])),
            ["clear left"]
        );
    }

    #[test]
    fn calls_three_wide_with_a_car_reported_each_side() {
        let mut logic = Logic::default();
        assert_eq!(
            run_reported(&mut logic, 0.0, 1.0, Some([1, 0])),
            ["car left"]
        );
        assert_eq!(
            run_reported(&mut logic, 1.0, 2.0, Some([1, 1])),
            ["three wide"]
        );
        assert_eq!(
            run_reported(&mut logic, 2.0, 3.0, Some([0, 1])),
            ["clear left"]
        );
        assert_eq!(
            run_reported(&mut logic, 3.0, 3.5, Some([0, 0])),
            ["clear right"]
        );
    }

    #[test]
    fn a_car_each_side_becoming_two_on_one_is_still_three_wide() {
        let mut logic = Logic::default();
        run_reported(&mut logic, 0.0, 1.0, Some([1, 1]));
        assert_eq!(
            run_reported(&mut logic, 1.0, 2.0, Some([0, 2])),
            ["clear left", "three wide right"]
        );

        // Even when the side went too soon to call clear
        let mut logic = Logic::default();
        run_reported(&mut logic, 0.0, 0.5, Some([1, 0]));
        run_reported(&mut logic, 0.5, 0.6, Some([1, 1]));
        assert_eq!(
            run_reported(&mut logic, 0.6, 0.8, Some([2, 0])),
            ["three wide left"]
        );

        // Back to one car too soon to call clear: that car is said again
        let mut logic = Logic::default();
        run_reported(&mut logic, 0.0, 0.1, Some([1, 0]));
        run_reported(&mut logic, 0.1, 0.3, Some([1, 1]));
        assert_eq!(
            run_reported(&mut logic, 0.3, 0.5, Some([1, 0])),
            ["car left"]
        );

        // Both go, one too soon to call clear: both are clear all the same
        let mut logic = Logic::default();
        run_reported(&mut logic, 0.0, 1.0, Some([1, 0]));
        run_reported(&mut logic, 1.0, 1.1, Some([1, 1]));
        assert_eq!(
            run_reported(&mut logic, 1.1, 1.5, Some([0, 0])),
            ["clear all round"]
        );

        // Both at once from one car: three wide is said once
        let mut logic = Logic::default();
        run_reported(&mut logic, 0.0, 1.0, Some([1, 0]));
        assert_eq!(
            run_reported(&mut logic, 1.0, 2.0, Some([0, 2])),
            ["three wide right", "clear left"]
        );
    }

    #[test]
    fn three_wide_is_not_replaced_by_a_reminder_due_at_once() {
        let mut logic = Logic::default();
        let from_start = run_reported(&mut logic, 0.0, 6.0 - 1.0 / 60.0, Some([1, 0]));
        assert_eq!(from_start, ["car left"]);
        // The reminder for the left falls due as a second car joins it
        assert_eq!(
            run_reported(&mut logic, 6.0, 6.5, Some([2, 0])),
            ["three wide left"]
        );
    }

    #[test]
    fn unknown_alongside_is_never_clear() {
        let mut logic = Logic::default();
        assert_eq!(
            run_reported(&mut logic, 0.0, 1.0, Some([1, 0])),
            ["car left"]
        );
        // The simulator stops reporting: nothing said, and no reminders
        assert!(run_reported(&mut logic, 1.0, 8.0, None).is_empty());
        // Reporting again: called afresh, and a clear side is no news
        assert_eq!(
            run_reported(&mut logic, 8.0, 9.0, Some([1, 0])),
            ["car left"]
        );
        assert!(run_reported(&mut logic, 9.0, 9.5, None).is_empty());
        assert!(run_reported(&mut logic, 9.5, 10.0, Some([0, 0])).is_empty());
    }

    #[test]
    fn reads_reported_cars_alongside_when_recorded() {
        let conn = Connection::open_in_memory().unwrap();
        // A recording from before the columns existed
        conn.execute_batch(
            "CREATE TABLE telemetry (id INTEGER PRIMARY KEY, vehicle_id INTEGER);
             INSERT INTO telemetry VALUES (1, 1);",
        )
        .unwrap();
        assert_eq!(alongside(&conn, 1).unwrap(), None);

        conn.execute_batch(
            "ALTER TABLE telemetry ADD COLUMN cars_left INTEGER;
             ALTER TABLE telemetry ADD COLUMN cars_right INTEGER;
             INSERT INTO telemetry VALUES (2, 1, 2, 0), (3, 1, 1, NULL), (4, 1, 5, -1);",
        )
        .unwrap();
        assert_eq!(alongside(&conn, 1).unwrap(), None);
        assert_eq!(alongside(&conn, 2).unwrap(), Some([2, 0]));
        assert_eq!(alongside(&conn, 3).unwrap(), None);
        assert_eq!(alongside(&conn, 4).unwrap(), Some([2, 0]));
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

        // A caution ending once the session's stopped is no green flag
        scoring.phase = Some("green".into());
        assert!(step(0.6, &scoring).is_empty());
        scoring.full_course_yellow = Some("pending".into());
        assert_eq!(step(0.7, &scoring), [("full course yellow", true)]);
        scoring.phase = Some("stopped".into());
        assert_eq!(step(0.8, &scoring), [("red flag", true)]);
        scoring.full_course_yellow = Some("none".into());
        assert!(step(0.9, &scoring).is_empty());
    }

    #[test]
    fn keeps_flags_and_penalties_outside_the_standing_window() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE session (id INTEGER PRIMARY KEY, session_time REAL,
             full_course_yellow TEXT, phase TEXT, sector_count INTEGER, rain REAL,
             track_wetness_average REAL, session_type TEXT, session_end_time REAL);
             CREATE TABLE session_sector (session_id INTEGER, sector INTEGER,
             local_yellow INTEGER);
             CREATE TABLE standings (id INTEGER PRIMARY KEY, is_player INTEGER,
             session_time REAL, flag TEXT, penalties INTEGER);
             INSERT INTO session (session_time) VALUES (0);
             INSERT INTO standings VALUES (1, 1, 0, 'blue', 1);",
        )
        .unwrap();
        let config = SpotterConfig::default();
        let mut logic = Logic::default();
        let mut step = |t| {
            let scoring = scoring(&conn, None, &Epoch::ALL).unwrap().unwrap();
            assert_eq!(scoring.flag.as_deref(), Some("blue"));
            assert_eq!(scoring.penalties, Some(1));
            texts(logic.update(&config, &player(t), &[], &Wheels::default(), Some(&scoring)))
        };
        assert!(step(0.0).is_empty());
        conn.execute_batch(
            "WITH RECURSIVE ids(id) AS (SELECT 2 UNION ALL SELECT id + 1 FROM ids WHERE id < 5001)
             INSERT INTO standings SELECT id, 0, 1, 'green', 0 FROM ids;",
        )
        .unwrap();
        assert!(step(1800.0).is_empty());
        conn.execute_batch("INSERT INTO standings VALUES (5002, 1, 1801, 'blue', 1);")
            .unwrap();
        assert!(step(1801.0).is_empty());
        // The recorder clears the tables for a new session.
        conn.execute_batch("DELETE FROM standings; DELETE FROM session; INSERT INTO session (session_time) VALUES (0);")
            .unwrap();
        let latest = scoring(&conn, None, &Epoch::ALL).unwrap().unwrap();
        assert_eq!(latest.flag, None);
        assert_eq!(latest.penalties, None);
        assert!(!latest.yellow);
        // A local yellow in any of the session's sectors
        conn.execute_batch(
            "INSERT INTO session_sector SELECT MAX(id), 0, 0 FROM session;
             INSERT INTO session_sector SELECT MAX(id), 4, 1 FROM session;",
        )
        .unwrap();
        assert!(scoring(&conn, None, &Epoch::ALL).unwrap().unwrap().yellow);
    }

    #[test]
    fn follows_the_newest_epoch_only() {
        let conn = Connection::open_in_memory().unwrap();
        bono_sim::sql::create_tables(&conn).unwrap();
        bono_sim::sql::begin_epoch(&conn, 1, bono_sim::sql::EpochReason::Start).unwrap();
        conn.execute_batch(
            "INSERT INTO telemetry (vehicle_id, is_player, session_time, speed) VALUES (1, 1, 100, 50);
             INSERT INTO session (session_time, session_type) VALUES (100, 'practice');
             INSERT INTO standings (vehicle_id, is_player, session_time, flag) VALUES (1, 1, 100, 'blue');",
        )
        .unwrap();
        let config = SpotterConfig::default();
        let mut logic = Logic::default();
        let (mut last_id, mut epoch) = (None, None);
        poll(&conn, &mut logic, &config, 5, &mut last_id, &mut epoch).unwrap();
        assert_eq!((last_id, epoch), (Some(1), Some(1)));
        assert!(logic.prev.is_some());

        // Another session, spectating: its rows say nothing of the player, and the last session's
        // aren't taken for now
        bono_sim::sql::begin_epoch(&conn, 2, bono_sim::sql::EpochReason::Session).unwrap();
        conn.execute_batch(
            "INSERT INTO telemetry (vehicle_id, is_player, session_time, speed) VALUES (1, 0, 200, 50);
             INSERT INTO session (session_time, session_type) VALUES (200, 'qualifying');",
        )
        .unwrap();
        assert!(
            poll(&conn, &mut logic, &config, 5, &mut last_id, &mut epoch)
                .unwrap()
                .is_empty()
        );
        assert_eq!(epoch, Some(2));
        assert!(logic.prev.is_none(), "the calls start over");
        assert_eq!(
            last_id, None,
            "a cleared recording numbers its rows from 1 again"
        );
        let current = active_epoch(&conn).unwrap();
        assert!(latest(&conn, &current).unwrap().is_none());
        let scoring = scoring(&conn, None, &current).unwrap().unwrap();
        assert_eq!(scoring.session_type.as_deref(), Some("qualifying"));
        assert_eq!(scoring.flag, None);
    }

    #[test]
    fn resets_flag_and_penalty_edges_for_a_new_session() {
        let config = SpotterConfig::default();
        let mut logic = Logic::default();
        let mut step = |t, scoring: &Scoring| {
            texts(logic.update(&config, &player(t), &[], &Wheels::default(), Some(scoring)))
        };
        assert!(step(1800.0, &Scoring::default()).is_empty());
        let mut scoring = Scoring {
            flag: Some("blue".into()),
            penalties: Some(1),
            ..Scoring::default()
        };
        // The first row of a new session seeds the edges without calling them.
        assert!(step(0.0, &scoring).is_empty());
        scoring = Scoring::default();
        assert!(step(0.1, &scoring).is_empty());
        scoring.flag = Some("blue".into());
        scoring.penalties = Some(1);
        assert_eq!(step(0.2, &scoring), ["blue flag", "penalty"]);
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
        let scoring = Scoring {
            session_type: Some("race".into()),
            ..Scoring::default()
        };
        let mut at = |t: f64, gap: f64| {
            let mut p = player(t);
            p.gap_behind = Some(gap);
            texts(logic.update(&config, &p, &[], &Wheels::default(), Some(&scoring)))
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
    fn does_not_call_standings_gaps_outside_races() {
        let config = SpotterConfig::default();
        for kind in [Some("practice"), Some("qualifying"), Some("warmup"), None] {
            for gap in [0.8, 0.1] {
                let scoring = Scoring {
                    session_type: kind.map(str::to_owned),
                    ..Scoring::default()
                };
                for scoring in [Some(&scoring), None] {
                    let mut logic = Logic::default();
                    for (t, gap) in [(30.0, 2.0), (30.1, gap)] {
                        let mut p = player(t);
                        p.gap_behind = Some(gap);
                        assert!(
                            logic
                                .update(&config, &p, &[], &Wheels::default(), scoring)
                                .is_empty()
                        );
                    }
                }
            }
        }
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

    #[tokio::test]
    async fn fixed_input_language_applies_to_calls_before_any_question() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        std::fs::write(
            &path,
            r#"{
            "voices": {"en": "en.onnx", "es": "es.onnx"},
            "heard_language": "es", "spoken_language": "en"
        }"#,
        )
        .unwrap();
        let config = Config::new(&path).await.unwrap();
        let engineer = Engineer::new(reqwest::Client::new(), None);
        let spotter = Spotter::new();
        assert_eq!(
            spotter.language(&config, &config.get().spotter, &engineer),
            arrstr!("es")
        );
        config.update(|raw| raw.heard_language.set(Some(arrstr!("en"))));
        assert_eq!(
            spotter.language(&config, &config.get().spotter, &engineer),
            arrstr!("en")
        );
        config.update(|raw| {
            raw.heard_language.set(Some(arrstr!("es")));
            raw.spotter.language.set(Some(arrstr!("en")));
        });
        assert_eq!(
            spotter.language(&config, &config.get().spotter, &engineer),
            arrstr!("en")
        );
    }

    #[test]
    fn says_calls_in_italian() {
        for (text, expected) in WARM.into_iter().zip([
            "auto a sinistra",
            "auto a destra",
            "tre affiancati",
            "libero a sinistra",
            "libero a destra",
            "tutto libero",
        ]) {
            assert_eq!(translate(text, arrstr!("it")), expected);
        }
        assert_eq!(translate("red flag", arrstr!("it")), "bandiera rossa");
        let call = Raised {
            text: "rain likely in about {} minutes",
            number: Some(4),
            urgent: false,
            session_time: 0.0,
        };
        assert_eq!(
            call.words(arrstr!("it")),
            "pioggia probabile tra circa 4 minuti"
        );
    }

    #[test]
    fn every_spotter_call_has_italian_words() {
        let catalog: serde_json::Value =
            serde_json::from_str(include_str!("../i18n/intl.json")).unwrap();
        for (key, translations) in catalog.as_object().unwrap() {
            if key.starts_with("call.") {
                let italian = translations["it"].as_str().expect(key);
                let english = translations["en"].as_str().unwrap();
                assert!(!italian.is_empty(), "{key}");
                assert_ne!(italian, english, "{key}");
                assert_eq!(
                    italian.matches("{}").count(),
                    english.matches("{}").count(),
                    "{key}"
                );
                // Said through `translate`, by the right key
                assert_eq!(
                    translate(english.to_owned().leak(), arrstr!("it")),
                    italian,
                    "{key}"
                );
            }
        }
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
