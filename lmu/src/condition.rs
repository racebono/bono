//! The player's aero and suspension damage, from LMU's own local web API (the one its UI uses): the
//! plugin interface only has the car's dents. A thread of its own asks about once a second while the
//! player drives, so the game's threads never wait on the web API: they only take the last answer.
//! It's only asked in realtime: after realtime ends the game may unload the car, and asking about
//! it then may crash the game.

use bono_sim::sys::bono_telemetry;
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
        Self::with(POLL, move || fetch(&agent))
    }

    fn with(poll: Duration, mut fetch: impl FnMut() -> Option<Condition> + Send + 'static) -> Self {
        let shared = Arc::new(Shared::default());
        let thread = {
            let shared = shared.clone();
            std::thread::Builder::new()
                .name("bono-lmu-condition".into())
                .spawn(move || shared.run(poll, &mut fetch))
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
    }

    /// Whether the game is in realtime, where the car can be driven. Outside it, it isn't asked. A
    /// request already in flight when realtime ends isn't waited for (that would hold the game's
    /// thread): it normally takes milliseconds, well before the game unloads the car.
    pub fn set_realtime(&self, realtime: bool) {
        lock(&self.shared.state).realtime = realtime;
    }

    /// Takes the player's car from a scoring update (`None`: no car is theirs). Another car, or none,
    /// drops the last answer and stops asking until the player drives.
    pub fn set_player(&self, vehicle: Option<i64>) {
        let mut state = lock(&self.shared.state);
        if state.vehicle != vehicle {
            state.forget();
            state.driving = None;
            state.vehicle = vehicle;
        }
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
            // Another car's answer isn't this one's
            let vehicle = telemetry.vehicle_id.get();
            if state.vehicle != vehicle {
                state.forget();
                state.vehicle = vehicle;
            }
            state.driving = Some(now);
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
    /// game closes: a request in flight holds it for up to the request's timeouts, about 1.5 s, and
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
}

impl Shared {
    fn run(&self, poll: Duration, fetch: &mut impl FnMut() -> Option<Condition>) {
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
        let condition = VehicleCondition::with(Duration::from_millis(1), move || {
            let _ = asked_tx.send(());
            answer_rx.recv().ok()
        });
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
        let condition = VehicleCondition::with(Duration::from_millis(1), move || {
            let _ = asked_tx.send(());
            answer_rx.recv().ok()
        });
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
}
