//! The player's aero and suspension damage, from LMU's own local web API (the one its UI uses): the
//! plugin interface only has the car's dents. A thread of its own asks about once a second while the
//! player drives, so the game's threads never wait on the web API: they only take the last answer.
//! It's only asked in realtime: after realtime ends the game may unload the car, and asking about
//! it then may crash the game.
//!
//! The same thread asks for the player's garage setup (see `garage`), only at two moments: when the
//! car leaves the garage (realtime starts, or a new player or event) and when it leaves the pit lane
//! after coming in from the track. Nothing else changes the garage setup, and cockpit changes are in
//! the telemetry, so it's a handful of questions a session.

use crate::garage;
use bono_sim::{
    setup::{Setup, SetupSource},
    sys::bono_telemetry,
};
use serde_json::Value;
use std::{
    panic::{AssertUnwindSafe, catch_unwind},
    sync::{
        Arc, Condvar, Mutex, MutexGuard, PoisonError,
        atomic::{AtomicBool, Ordering},
    },
    thread::JoinHandle,
    time::{Duration, Instant},
};

const URL: &str = "http://localhost:6397/rest/garage/getVehicleCondition";
/// How often it's asked while the player drives.
const POLL: Duration = Duration::from_secs(1);
/// An answer older than this isn't used: the web API stopped answering, so it may be out of date.
const STALE: Duration = Duration::from_secs(5);
/// It's only asked this long after the player's last telemetry update, so not from the menus.
const DRIVING: Duration = Duration::from_secs(5);
/// How many times the setup is asked at a moment, a poll apart, before it's given up until the next.
const SETUP_ATTEMPTS: u8 = 3;

/// What one answer says, as fractions: 0 none to 1 destroyed.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
struct Condition {
    aero: Option<f64>,
    suspension: Option<f64>,
}

#[derive(Default)]
struct State {
    /// The last answer, and when it came
    answer: Option<(Condition, Instant)>,
    /// When the player's telemetry last arrived
    driving: Option<Instant>,
    /// Whether the game is in realtime (see `set_realtime`)
    realtime: bool,
    /// The player's car the answer is for
    vehicle: Option<i64>,
    /// Bumped by `clear`, so an answer asked for before it is dropped
    generation: u64,
    /// The setup's moment: where the car is coming from, and how many attempts are left
    setup_wanted: Option<(SetupSource, u8)>,
    /// The setup read, until the game's thread takes it
    setup: Option<Setup>,
    /// Whether the player's car was in the pit lane in its last telemetry
    in_pit_lane: Option<bool>,
    /// Whether it came into the pit lane from the track, so leaving it is after a stop
    came_in: bool,
}

#[derive(Default)]
struct Shared {
    state: Mutex<State>,
    wake: Condvar,
    /// Set without the state's lock, as `Recorder`'s is: a wake-up sent just before the thread
    /// waits is missed, and it stops at its next poll instead
    stop: AtomicBool,
}

/// The player's damage the web API reports, as of its last answer.
pub struct VehicleCondition {
    shared: Arc<Shared>,
    thread: Option<JoinHandle<()>>,
}

impl VehicleCondition {
    /// Starts asking the web API. If its thread can't start, the damage stays unknown.
    pub fn start() -> Self {
        // Asked every second: anything slow means it isn't answering. About 1.5 s in all
        let agent = crate::local_agent(Duration::from_millis(400));
        // The setup's answer is longer, and it's asked rarely. About 3 s in all
        let setup_agent = crate::local_agent(Duration::from_millis(750));
        Self::with(
            POLL,
            move || fetch(&agent),
            move || garage::fetch(&setup_agent),
        )
    }

    fn with(
        poll: Duration,
        mut fetch: impl FnMut() -> Option<Condition> + Send + 'static,
        mut fetch_setup: impl FnMut() -> Option<Setup> + Send + 'static,
    ) -> Self {
        let shared = Arc::new(Shared::default());
        let thread = {
            let shared = shared.clone();
            std::thread::Builder::new()
                .name("bono-lmu-condition".into())
                .spawn(move || shared.run(poll, &mut fetch, &mut fetch_setup))
                .ok()
        };
        Self { shared, thread }
    }

    /// Forgets the last answer, for a new event: it isn't asked again until the player drives.
    pub fn clear(&self) {
        let mut state = lock(&self.shared.state);
        state.forget();
        state.driving = None;
        state.vehicle = None;
        state.leave_garage();
    }

    /// Whether the game is in realtime, where the car can be driven. Outside it, it isn't asked. A
    /// request already in flight when realtime ends isn't waited for (that would hold the game's
    /// thread): it normally takes milliseconds, well before the game unloads the car.
    ///
    /// Realtime starting is the car leaving the garage, so its setup is asked again. Answers asked
    /// for before then are dropped: the car may have changed in the garage.
    pub fn set_realtime(&self, realtime: bool) {
        let mut state = lock(&self.shared.state);
        if realtime && !state.realtime {
            state.forget();
            state.leave_garage();
        }
        state.realtime = realtime;
    }

    /// Takes the player's car from a scoring update (`None`: no car is theirs). Another car, or none,
    /// drops the last answer and stops asking until the player drives.
    pub fn set_player(&self, vehicle: Option<i64>) {
        let mut state = lock(&self.shared.state);
        if state.vehicle != vehicle {
            state.forget();
            state.driving = None;
            state.vehicle = vehicle;
            state.leave_garage();
        }
    }

    /// The setup read since the last call, if any. The game's thread takes it, never waiting on the
    /// web API.
    pub fn take_setup(&self) -> Option<Setup> {
        lock(&self.shared.state).setup.take()
    }

    /// Fills in the player's aero and suspension damage, when the web API answered recently.
    /// Other cars' stay unknown: the web API only has the player's.
    pub fn fill_telemetry(&self, telemetry: &mut bono_telemetry, is_player: bool) {
        self.fill_at(telemetry, is_player, Instant::now());
    }

    fn fill_at(&self, telemetry: &mut bono_telemetry, is_player: bool, now: Instant) {
        if !is_player {
            return;
        }
        let answer = {
            let mut state = lock(&self.shared.state);
            // Another car's answer isn't this one's, and its setup is read as `set_player` would
            // (telemetry can name the new car before scoring does)
            let vehicle = telemetry.vehicle_id.get();
            if state.vehicle != vehicle {
                state.forget();
                state.leave_garage();
                state.vehicle = vehicle;
            }
            state.driving = Some(now);
            let in_pit_lane = telemetry.in_pit_lane.get();
            match (state.in_pit_lane, in_pit_lane) {
                (Some(false), Some(true)) => state.came_in = true,
                // Leaving the garage goes down the pit lane too, but isn't a stop
                (Some(true), Some(false)) if state.came_in => {
                    state.came_in = false;
                    // A garage read still wanted stays one
                    if state.setup_wanted.is_none() {
                        state.setup_wanted = Some((SetupSource::PitStop, SETUP_ATTEMPTS));
                    }
                }
                _ => {}
            }
            state.in_pit_lane = in_pit_lane;
            state.answer
        };
        let Some((condition, _)) =
            answer.filter(|(_, at)| now.saturating_duration_since(*at) < STALE)
        else {
            return;
        };
        telemetry.state.aerodynamics_damage = condition.aero.into();
        telemetry.state.suspension_damage = condition.suspension.into();
    }
}

impl VehicleCondition {
    /// Asks the thread to stop after any request in flight, without waiting for it.
    pub fn stop(&self) {
        self.shared.stop.store(true, Ordering::Relaxed);
        self.shared.wake.notify_all();
    }
}

impl Drop for VehicleCondition {
    /// Waits for the thread, as the DLL may be unloaded next. This runs on the game's thread when the
    /// game closes: a request in flight holds it for up to the request's timeouts, about 1.5 s (3 s
    /// for the setup's), and
    /// only if the web API stops answering just then. Not waiting would leave the thread running code
    /// that is gone.
    fn drop(&mut self) {
        self.stop();
        if let Some(thread) = self.thread.take() {
            // Caught, as in `Recorder`'s drop: joining a thread the system already ended can panic
            let _ = catch_unwind(AssertUnwindSafe(|| thread.join()));
        }
    }
}

impl State {
    /// Drops the answer, and any answer asked for before now.
    fn forget(&mut self) {
        self.answer = None;
        self.generation = self.generation.wrapping_add(1);
    }

    /// Wants the garage setup, as the car is leaving the garage: any setup read and pit lane seen
    /// before are about before.
    fn leave_garage(&mut self) {
        self.setup_wanted = Some((SetupSource::Garage, SETUP_ATTEMPTS));
        self.setup = None;
        self.in_pit_lane = None;
        self.came_in = false;
    }
}

impl Shared {
    fn run(
        &self,
        poll: Duration,
        fetch: &mut impl FnMut() -> Option<Condition>,
        fetch_setup: &mut impl FnMut() -> Option<Setup>,
    ) {
        let mut state = lock(&self.state);
        loop {
            state = self
                .wake
                .wait_timeout_while(state, poll, |_| !self.stop.load(Ordering::Relaxed))
                .unwrap_or_else(PoisonError::into_inner)
                .0;
            if self.stop.load(Ordering::Relaxed) {
                return;
            }
            if !state.realtime || !state.driving.is_some_and(|at| at.elapsed() < DRIVING) {
                continue;
            }

            // Asked without the lock: the game's threads never wait for the answer
            let generation = state.generation;
            drop(state);
            let answer = fetch();
            state = lock(&self.state);
            if let Some(answer) = answer
                && state.generation == generation
            {
                state.answer = Some((answer, Instant::now()));
            }

            // The setup, at its moments, also only in realtime while the player drives. Not once
            // asked to stop, so closing waits for one request at most
            let Some((source, attempts)) = state.setup_wanted else {
                continue;
            };
            if self.stop.load(Ordering::Relaxed)
                || !state.realtime
                || !state.driving.is_some_and(|at| at.elapsed() < DRIVING)
            {
                continue;
            }
            let generation = state.generation;
            drop(state);
            let setup = fetch_setup();
            state = lock(&self.state);
            // A new moment came while it was asked: the answer may be from before it
            if state.generation != generation || state.setup_wanted != Some((source, attempts)) {
                continue;
            }
            state.setup_wanted = match setup {
                Some(setup) => {
                    state.setup = Some(Setup {
                        source: Some(source),
                        ..setup
                    });
                    None
                }
                None => attempts
                    .checked_sub(1)
                    .filter(|left| *left > 0)
                    .map(|left| (source, left)),
            };
        }
    }
}

fn fetch(agent: &ureq::Agent) -> Option<Condition> {
    let body = agent
        .get(URL)
        .call()
        .ok()?
        .body_mut()
        .read_to_string()
        .ok()?;
    parse(&body)
}

/// `{"vehicleDamage": 0.22, "suspensionDamage": [0.05, 0.04, 0.0, 0.0], "brakeCondition": [...],
/// "tireCondition": [...]}`, with a value per wheel. `vehicleDamage` is the one damage figure LMU keeps for the car (its repair screen calls it aero).
/// The web API doesn't document its values, so anything outside 0-1 counts as unknown; an answer
/// with neither isn't an answer.
fn parse(body: &str) -> Option<Condition> {
    let Ok(Value::Object(json)) = serde_json::from_str::<Value>(body) else {
        return None;
    };
    let fraction = |value: &Value| value.as_f64().filter(|v| (0.0..=1.0).contains(v));
    let aero = json.get("vehicleDamage").and_then(fraction);
    let suspension = json
        .get("suspensionDamage")
        .and_then(Value::as_array)
        .filter(|wheels| !wheels.is_empty())
        .and_then(|wheels| wheels.iter().map(fraction).collect::<Option<Vec<_>>>())
        .map(|wheels| wheels.into_iter().fold(0.0, f64::max));
    (aero.is_some() || suspension.is_some()).then_some(Condition { aero, suspension })
}

/// A panic on another thread mustn't stop the damage: the state behind the lock stays valid.
fn lock(mutex: &Mutex<State>) -> MutexGuard<'_, State> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use super::*;
    use bono_sim::setup::Setting;
    use std::sync::mpsc;

    const BODY: &str = r#"{
        "brakeCondition": [0.99, 0.99, 0.99, 0.99],
        "suspensionDamage": [0.05, 0.04, 0.0, 0.0],
        "tireCondition": [0.99, 0.99, 0.94, 0.84],
        "vehicleDamage": 0.22
    }"#;

    fn damage(
        condition: &VehicleCondition,
        is_player: bool,
        now: Instant,
    ) -> (Option<f64>, Option<f64>) {
        damage_of(condition, 1, is_player, now)
    }

    fn damage_of(
        condition: &VehicleCondition,
        vehicle: i64,
        is_player: bool,
        now: Instant,
    ) -> (Option<f64>, Option<f64>) {
        let mut telemetry = bono_telemetry::default();
        telemetry.vehicle_id = Some(vehicle).into();
        condition.fill_at(&mut telemetry, is_player, now);
        (
            telemetry.state.aerodynamics_damage.get(),
            telemetry.state.suspension_damage.get(),
        )
    }

    /// Without its thread, holding `answer` as if it had just come.
    fn answered(answer: Condition, at: Instant) -> VehicleCondition {
        let condition = VehicleCondition {
            shared: Arc::new(Shared::default()),
            thread: None,
        };
        let mut state = lock(&condition.shared.state);
        state.answer = Some((answer, at));
        state.vehicle = Some(1);
        drop(state);
        condition
    }

    /// Telemetry can name a new player's car before scoring does: that car's setup is still read.
    #[test]
    fn a_new_car_first_seen_in_telemetry_reads_its_setup() {
        let condition = answered(parse(BODY).unwrap(), Instant::now());
        lock(&condition.shared.state).setup_wanted = None;
        let mut telemetry = bono_telemetry::default();
        telemetry.vehicle_id = Some(2).into();
        condition.fill_at(&mut telemetry, true, Instant::now());
        let state = lock(&condition.shared.state);
        assert_eq!(state.vehicle, Some(2));
        assert_eq!(
            state.setup_wanted,
            Some((SetupSource::Garage, SETUP_ATTEMPTS))
        );
    }

    #[test]
    fn suspension_is_the_worst_wheel() {
        assert_eq!(
            parse(BODY),
            Some(Condition {
                aero: Some(0.22),
                suspension: Some(0.05),
            })
        );
    }

    #[test]
    fn values_outside_fractions_are_unknown() {
        let body = r#"{"vehicleDamage": 1.5, "suspensionDamage": [0.1, -1.0, 0.0, 0.0]}"#;
        assert_eq!(parse(body), None);
        let body = r#"{"vehicleDamage": 0.3, "suspensionDamage": []}"#;
        assert_eq!(
            parse(body),
            Some(Condition {
                aero: Some(0.3),
                suspension: None,
            })
        );
        for body in ["", "not json", "[]", r#"{"status": "unavailable"}"#] {
            assert_eq!(parse(body), None, "{body}");
        }
    }

    #[test]
    fn only_the_player_gets_a_recent_answer() {
        let now = Instant::now();
        let condition = answered(parse(BODY).unwrap(), now);
        assert_eq!(damage(&condition, true, now), (Some(0.22), Some(0.05)));
        assert_eq!(damage(&condition, false, now), (None, None));
        assert_eq!(damage(&condition, true, now + STALE), (None, None));
    }

    #[test]
    fn another_car_does_not_get_the_last_answer() {
        let now = Instant::now();
        let condition = answered(parse(BODY).unwrap(), now);
        assert_eq!(damage_of(&condition, 2, true, now), (None, None));
        assert_eq!(damage_of(&condition, 1, true, now), (None, None));
    }

    #[test]
    fn changing_the_player_drops_the_answer_and_stops_asking() {
        let condition = answered(parse(BODY).unwrap(), Instant::now());
        condition.set_player(Some(1));
        assert!(lock(&condition.shared.state).answer.is_some());
        lock(&condition.shared.state).driving = Some(Instant::now());
        // Spectating, then back in the same car: the answer may have been about another one
        condition.set_player(None);
        condition.set_player(Some(1));
        let state = lock(&condition.shared.state);
        assert!(state.answer.is_none() && state.driving.is_none());
    }

    #[test]
    fn clearing_stops_asking_until_the_player_drives() {
        let condition = answered(parse(BODY).unwrap(), Instant::now());
        lock(&condition.shared.state).driving = Some(Instant::now());
        condition.clear();
        let state = lock(&condition.shared.state);
        assert!(state.answer.is_none() && state.driving.is_none());
    }

    #[test]
    fn an_answer_asked_for_before_clearing_is_dropped() {
        let (asked_tx, asked) = mpsc::channel();
        let (answer, answer_rx) = mpsc::channel::<Condition>();
        let condition = VehicleCondition::with(
            Duration::from_millis(1),
            move || {
                let _ = asked_tx.send(());
                answer_rx.recv().ok()
            },
            || None,
        );
        let first = Condition {
            aero: Some(0.1),
            suspension: Some(0.2),
        };

        // Only asked once the player drives
        condition.set_realtime(true);
        damage(&condition, true, Instant::now());
        asked.recv().unwrap();
        condition.clear();
        answer.send(first).unwrap();
        // Asked again once the player drives: the first answer has been dealt with
        damage(&condition, true, Instant::now());
        asked.recv().unwrap();
        assert_eq!(damage(&condition, true, Instant::now()), (None, None));

        answer.send(first).unwrap();
        asked.recv().unwrap();
        assert_eq!(
            damage(&condition, true, Instant::now()),
            (Some(0.1), Some(0.2))
        );
        // The thread's next question gets no answer, so dropping `condition` can join it
        drop(answer);
    }

    #[test]
    fn only_asked_in_realtime() {
        let (asked_tx, asked) = mpsc::channel();
        let (answer, answer_rx) = mpsc::channel::<Condition>();
        // Bounded, so a failing assertion can't leave dropping `condition` waiting on the question
        let condition = VehicleCondition::with(
            Duration::from_millis(1),
            move || {
                let _ = asked_tx.send(());
                answer_rx.recv_timeout(Duration::from_secs(5)).ok()
            },
            || None,
        );
        damage(&condition, true, Instant::now());
        assert!(asked.recv_timeout(Duration::from_millis(50)).is_err());

        condition.set_realtime(true);
        damage(&condition, true, Instant::now());
        asked.recv_timeout(Duration::from_secs(5)).unwrap();

        // Realtime ends while a question is being asked: it's answered, and no other is asked
        condition.set_realtime(false);
        damage(&condition, true, Instant::now());
        answer.send(Condition::default()).unwrap();
        assert!(asked.recv_timeout(Duration::from_millis(50)).is_err());
        drop(answer);
    }

    /// The player's telemetry in car 1, in the pit lane or not.
    fn drive(condition: &VehicleCondition, in_pit_lane: bool) {
        drive_car(condition, 1, in_pit_lane);
    }

    fn drive_car(condition: &VehicleCondition, vehicle: i64, in_pit_lane: bool) {
        let mut telemetry = bono_telemetry {
            vehicle_id: Some(vehicle).into(),
            in_pit_lane: Some(in_pit_lane).into(),
            ..bono_telemetry::default()
        };
        condition.fill_at(&mut telemetry, true, Instant::now());
    }

    /// A condition whose setup reads send on `asked` and answer `answer`, and whose damage reads
    /// get nothing.
    fn asking_setup(answer: Option<Setup>) -> (VehicleCondition, mpsc::Receiver<()>) {
        let (asked_tx, asked) = mpsc::channel();
        let condition = VehicleCondition::with(
            Duration::from_millis(1),
            || None,
            move || {
                let _ = asked_tx.send(());
                answer.clone()
            },
        );
        (condition, asked)
    }

    /// The setup the thread stored, waiting for it a while.
    fn taken(condition: &VehicleCondition) -> Option<Setup> {
        for _ in 0..500 {
            if let Some(setup) = condition.take_setup() {
                return Some(setup);
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        None
    }

    fn not_asked(asked: &mpsc::Receiver<()>) -> bool {
        asked.recv_timeout(Duration::from_millis(50)).is_err()
    }

    fn wing() -> Setup {
        Setup {
            settings: vec![Setting {
                name: "VM_REAR_WING".into(),
                value: "P5".into(),
                ..Setting::default()
            }],
            ..Setup::default()
        }
    }

    #[test]
    fn the_setup_is_asked_when_realtime_starts_and_after_the_pit_lane() {
        let (condition, asked) = asking_setup(Some(wing()));
        condition.set_realtime(true);
        drive(&condition, true);
        asked.recv_timeout(Duration::from_secs(5)).unwrap();
        let setup = taken(&condition).unwrap();
        assert_eq!(setup.source, Some(SetupSource::Garage));
        assert_eq!(setup.settings, wing().settings);
        assert_eq!(condition.take_setup(), None);
        assert!(not_asked(&asked));

        // Out of the garage onto the track: not a stop
        drive(&condition, false);
        assert!(not_asked(&asked));
        // In from the track and out again
        drive(&condition, true);
        assert!(not_asked(&asked));
        drive(&condition, false);
        asked.recv_timeout(Duration::from_secs(5)).unwrap();
        let setup = taken(&condition).unwrap();
        assert_eq!(setup.source, Some(SetupSource::PitStop));

        drive(&condition, false);
        drive(&condition, false);
        assert!(not_asked(&asked));

        // Back to the garage and out again
        condition.set_realtime(false);
        condition.set_realtime(true);
        drive(&condition, true);
        asked.recv_timeout(Duration::from_secs(5)).unwrap();
        assert_eq!(taken(&condition).unwrap().source, Some(SetupSource::Garage));
        drive(&condition, false);
        assert!(not_asked(&asked));
    }

    #[test]
    fn a_failed_setup_read_is_retried_then_given_up() {
        let (condition, asked) = asking_setup(None);
        condition.set_realtime(true);
        drive(&condition, false);
        for _ in 0..SETUP_ATTEMPTS {
            asked.recv_timeout(Duration::from_secs(5)).unwrap();
        }
        assert!(not_asked(&asked));
        assert_eq!(condition.take_setup(), None);

        // The next moment asks again
        drive(&condition, true);
        drive(&condition, false);
        asked.recv_timeout(Duration::from_secs(5)).unwrap();
    }

    #[test]
    fn the_setup_is_never_asked_outside_realtime() {
        let (condition, asked) = asking_setup(Some(wing()));
        condition.set_player(Some(1));
        drive(&condition, false);
        drive(&condition, true);
        drive(&condition, false);
        assert!(not_asked(&asked));

        // Realtime starting is the moment it waited for
        condition.set_realtime(true);
        drive(&condition, false);
        asked.recv_timeout(Duration::from_secs(5)).unwrap();
        assert_eq!(taken(&condition).unwrap().source, Some(SetupSource::Garage));
    }

    #[test]
    fn a_new_player_or_event_asks_for_the_garage_setup_again() {
        let condition = answered(parse(BODY).unwrap(), Instant::now());
        let mut state = lock(&condition.shared.state);
        state.setup = Some(wing());
        state.in_pit_lane = Some(false);
        drop(state);
        condition.set_player(Some(2));
        let state = lock(&condition.shared.state);
        assert_eq!(
            state.setup_wanted,
            Some((SetupSource::Garage, SETUP_ATTEMPTS))
        );
        assert!(state.setup.is_none() && state.in_pit_lane.is_none());
        drop(state);
        // Leaving the pit lane before it's read doesn't make it a pit stop's
        for in_pit_lane in [false, true, false] {
            drive_car(&condition, 2, in_pit_lane);
        }
        let state = lock(&condition.shared.state);
        assert_eq!(
            state.setup_wanted,
            Some((SetupSource::Garage, SETUP_ATTEMPTS))
        );
        drop(state);

        let mut state = lock(&condition.shared.state);
        state.setup = Some(wing());
        state.setup_wanted = None;
        drop(state);
        condition.clear();
        let state = lock(&condition.shared.state);
        assert_eq!(
            state.setup_wanted,
            Some((SetupSource::Garage, SETUP_ATTEMPTS))
        );
        assert!(state.setup.is_none());
    }
}
