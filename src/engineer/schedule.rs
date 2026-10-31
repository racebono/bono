//! Requests the pilot wants answered later ("tell me when the tyres are warm", "remind me in five
//! laps"): the `schedule` tool sets one up with a trigger, which [`watch`] checks off the UI thread
//! about once a second. Once it's met, the engineer is asked the request again, unprompted, and
//! answers it from the telemetry of that moment.

use crate::{provider::ToolHandler, sqlite::pool::Pool};
use anyhow::{Context as _, bail};
use rusqlite::{Connection, OptionalExtension, types::Value};
use serde::Deserialize;
use std::{
    fmt::Write,
    sync::{Arc, Mutex, MutexGuard, PoisonError},
    time::{Duration, Instant},
};

/// How often the triggers are checked
const POLL: Duration = Duration::from_secs(1);
/// A `when` condition still running after this is stopped: it runs every poll
const WHEN_TIME: Duration = Duration::from_millis(500);
/// Requests waiting at once
const MAX_WAITING: usize = 8;
/// The longest request, in characters
const MAX_REQUEST: usize = 500;
/// The furthest ahead `in_seconds` reaches: a day of session time
const MAX_SECONDS: f64 = 24.0 * 3600.0;
/// A `when` condition failing this many polls in a row is given up
const MAX_ERRORS: u32 = 5;
/// Times a due request is asked before it's given up (the pilot talked over it, or asking failed)
const MAX_ATTEMPTS: u32 = 2;

/// The requests set up in a conversation, shared by its tool, its watch and the engineer.
#[derive(Default)]
pub struct Schedule {
    next_id: u32,
    requests: Vec<Request>,
}

struct Request {
    id: u32,
    /// The pilot's request, as the model wrote it down
    text: String,
    trigger: Trigger,
    /// The game session it was set in, and the latest session time the player was seen at since:
    /// a request from another session (or before a restart) has nothing to be about
    game: Option<GameKey>,
    seen: f64,
    state: State,
    /// The condition's failures in a row
    errors: u32,
    /// Times it was asked
    attempts: u32,
}

/// When a request is due.
#[derive(Debug, Clone, PartialEq)]
enum Trigger {
    /// The player's session time reaches it
    Time(f64),
    /// The player's lap (telemetry `lap`) reaches it
    Lap(i64),
    /// A read-only query's first value is nonzero
    When(String),
}

#[derive(Debug, Clone, PartialEq)]
enum State {
    Waiting,
    /// Its trigger was met: the engineer answers it
    Met,
    /// Its condition kept failing: the engineer says it can't watch for it anymore
    Failed(String),
    /// The game session changed, or restarted, before it was met
    Expired,
}

/// What the engineer does next about the schedule.
#[derive(Debug, PartialEq)]
pub enum Next {
    /// Ask the model `prompt`: it answers the request `id`, which is removed once answered
    Ask {
        id: u32,
        request: String,
        prompt: String,
        failed: bool,
    },
    /// The request was dropped, its session gone: the pilot is told in the chat
    Expired(String),
    /// The request was asked too often without an answer, and dropped
    GaveUp(String),
}

/// A request's outcome at one check.
#[derive(Debug, PartialEq)]
enum Outcome {
    Pending,
    Met,
    Expired,
    Error(String),
}

type GameKey = (Option<String>, Option<String>, Option<i64>);

impl Schedule {
    /// The first due request to ask (its attempt counted), or one dropped to tell about. Called
    /// until `None` while the engineer is free.
    pub fn next(&mut self) -> Option<Next> {
        let i = self
            .requests
            .iter()
            .position(|r| r.state != State::Waiting)?;
        let request = &mut self.requests[i];
        let failed = match &request.state {
            State::Waiting => unreachable!(),
            State::Expired => return Some(Next::Expired(self.requests.remove(i).text)),
            _ if request.attempts >= MAX_ATTEMPTS => {
                return Some(Next::GaveUp(self.requests.remove(i).text));
            }
            State::Met => None,
            State::Failed(error) => Some(error.clone()),
        };
        request.attempts += 1;
        let (id, text) = (request.id, &request.text);
        let prompt = match &failed {
            None => format!(
                "[SCHEDULED REQUEST #{id}] Earlier the pilot asked to be told this later: \
                 {text:?}. Its trigger is met now. Answer it now from fresh telemetry, as a radio \
                 call the pilot didn't just ask for. It is no longer scheduled."
            ),
            Some(error) => format!(
                "[SCHEDULED REQUEST #{id} STOPPED] Earlier the pilot asked to be told this later: \
                 {text:?}, but its trigger can't be checked anymore ({error}). Tell the pilot \
                 briefly that you can't watch for it anymore. It is no longer scheduled."
            ),
        };
        Some(Next::Ask {
            id,
            request: text.clone(),
            prompt,
            failed: failed.is_some(),
        })
    }

    /// The request `id` was answered (or cancelled): it's done.
    pub fn remove(&mut self, id: u32) -> bool {
        let len = self.requests.len();
        self.requests.retain(|r| r.id != id);
        self.requests.len() != len
    }

    /// Forgets every request: the pilot started over.
    pub fn clear(&mut self) {
        self.requests.clear();
    }

    fn waiting_len(&self) -> usize {
        (self.requests.iter())
            .filter(|r| r.state == State::Waiting)
            .count()
    }

    /// The requests to check, without holding the lock: the waiting ones' triggers, and whether
    /// the due ones still belong to the session (`None` trigger).
    fn to_check(&self) -> Vec<Check> {
        (self.requests.iter())
            .filter(|r| r.state != State::Expired)
            .map(|r| Check {
                id: r.id,
                trigger: (r.state == State::Waiting).then(|| r.trigger.clone()),
                game: r.game.clone(),
                seen: r.seen,
            })
            .collect()
    }

    /// The outcomes of a check, the player seen at session time `time`.
    fn apply(&mut self, time: f64, outcomes: Vec<(u32, Outcome)>) {
        for (id, outcome) in outcomes {
            // Cancelled meanwhile, or expired already
            let Some(request) =
                (self.requests.iter_mut()).find(|r| r.id == id && r.state != State::Expired)
            else {
                continue;
            };
            if outcome == Outcome::Expired {
                request.state = State::Expired;
                continue;
            }
            request.seen = request.seen.max(time);
            // Due meanwhile: only its session is checked
            if request.state != State::Waiting {
                continue;
            }
            match outcome {
                Outcome::Pending => request.errors = 0,
                Outcome::Met => request.state = State::Met,
                Outcome::Expired => unreachable!(),
                Outcome::Error(error) => {
                    request.errors += 1;
                    if request.errors >= MAX_ERRORS {
                        request.state = State::Failed(error);
                    }
                }
            }
        }
    }
}

/// A request to check: its trigger, unless it's due already.
struct Check {
    id: u32,
    trigger: Option<Trigger>,
    game: Option<GameKey>,
    seen: f64,
}

fn lock(schedule: &Mutex<Schedule>) -> MutexGuard<'_, Schedule> {
    schedule.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Checks the schedule's waiting requests about once a second, until dropped (aborted).
pub async fn watch(schedule: Arc<Mutex<Schedule>>, pool: Pool) {
    let mut interval = tokio::time::interval(POLL);
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        interval.tick().await;
        let checks = lock(&schedule).to_check();
        if checks.is_empty() {
            continue;
        }
        let checked = async {
            let conn = pool.get().await?;
            conn.run(move |conn| check(conn, &checks)).await
        };
        match checked.await {
            Ok((time, outcomes)) => lock(&schedule).apply(time, outcomes),
            // The recorder is busy, or the player isn't in the car: the next poll tries again
            Err(e) => log::debug!("Error checking the engineer's schedule: {e:#}"),
        }
    }
}

/// Where the player is now.
struct Now {
    time: f64,
    lap: i64,
    game: Option<GameKey>,
}

impl Now {
    fn read(conn: &Connection) -> rusqlite::Result<Option<Self>> {
        let player = conn
            .prepare_cached(
                "SELECT session_time, lap FROM telemetry WHERE is_player = 1 \
                 ORDER BY id DESC LIMIT 1",
            )?
            .query_row([], |row| {
                Ok((row.get::<_, Option<f64>>(0)?, row.get::<_, Option<i64>>(1)?))
            })
            .optional()?;
        let Some((Some(time), Some(lap))) = player else {
            return Ok(None);
        };
        let game = conn
            .prepare_cached(
                "SELECT track, session_type, session_number FROM session ORDER BY id DESC LIMIT 1",
            )?
            .query_row([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
            .optional()?;
        Ok(Some(Self { time, lap, game }))
    }
}

/// The player's session time now, and each request's outcome. `Err` when where the player is
/// can't be read: nothing is decided then.
fn check(conn: &Connection, checks: &[Check]) -> anyhow::Result<(f64, Vec<(u32, Outcome)>)> {
    let now = Now::read(conn)?.context("no player telemetry")?;
    let outcomes = checks
        .iter()
        .map(
            |Check {
                 id,
                 trigger,
                 game,
                 seen,
             }| {
                // A new session, or the same one restarted
                if *game != now.game || now.time < seen - 1.0 {
                    return (*id, Outcome::Expired);
                }
                let Some(trigger) = trigger else {
                    return (*id, Outcome::Pending);
                };
                let outcome = match trigger {
                    Trigger::Time(at) => match now.time >= *at {
                        true => Outcome::Met,
                        false => Outcome::Pending,
                    },
                    Trigger::Lap(lap) => match now.lap >= *lap {
                        true => Outcome::Met,
                        false => Outcome::Pending,
                    },
                    Trigger::When(query) => match condition(conn, query) {
                        Ok(true) => Outcome::Met,
                        Ok(false) => Outcome::Pending,
                        Err(e) if busy(&e) => Outcome::Pending,
                        Err(e) => Outcome::Error(format!("{e:#}")),
                    },
                };
                (*id, outcome)
            },
        )
        .collect();
    Ok((now.time, outcomes))
}

/// The logger is mid-write: not the condition's fault.
fn busy(error: &anyhow::Error) -> bool {
    matches!(
        error.downcast_ref::<rusqlite::Error>(),
        Some(rusqlite::Error::SqliteFailure(failure, _))
            if failure.code == rusqlite::ErrorCode::DatabaseBusy
    )
}

/// Whether the read-only `query`'s first value is nonzero (no row, or NULL, is not).
fn condition(conn: &Connection, query: &str) -> anyhow::Result<bool> {
    let query = query.trim().trim_end_matches(';').trim_end();
    let lower = query.to_lowercase();
    if !lower.starts_with("select") && !lower.starts_with("with") {
        bail!("`when` must be one SELECT or WITH query");
    }
    let mut stmt = conn.prepare(query)?;
    if !stmt.readonly() {
        bail!("`when` may only read");
    }

    // Stopped from inside SQLite: it runs every second, beside the engineer's own queries
    let deadline = Instant::now() + WHEN_TIME;
    conn.progress_handler(1_000, Some(move || Instant::now() > deadline))?;
    let value = stmt.query_row([], |row| row.get::<_, Value>(0)).optional();
    conn.progress_handler(0, None::<fn() -> bool>)?;

    let value = match value {
        Err(rusqlite::Error::SqliteFailure(e, _))
            if e.code == rusqlite::ErrorCode::OperationInterrupted =>
        {
            bail!(
                "the condition took longer than {} ms: read only the newest rows (ORDER BY id DESC \
                 LIMIT ...)",
                WHEN_TIME.as_millis()
            )
        }
        value => value?,
    };
    Ok(match value {
        None | Some(Value::Null) => false,
        Some(Value::Integer(n)) => n != 0,
        Some(Value::Real(x)) => x != 0.0,
        Some(Value::Text(_) | Value::Blob(_)) => {
            bail!("the condition must return a number (nonzero when it's time), not text")
        }
    })
}

/// The `schedule` tool: sets up, lists and cancels the requests the pilot wants answered later.
#[derive(Clone)]
pub struct ScheduleTool {
    schedule: Arc<Mutex<Schedule>>,
    pool: Pool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Action {
    Add,
    List,
    Cancel,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ScheduleInput {
    action: Action,
    #[serde(default)]
    request: Option<String>,
    #[serde(default)]
    in_seconds: Option<f64>,
    #[serde(default)]
    in_laps: Option<i64>,
    #[serde(default)]
    at_lap: Option<i64>,
    #[serde(default)]
    when: Option<String>,
    #[serde(default)]
    id: Option<u32>,
}

/// A trigger as the model gave it, before it's set against where the player is.
enum Asked {
    InSeconds(f64),
    InLaps(i64),
    AtLap(i64),
    When(String),
}

impl ScheduleTool {
    pub const NAME: &'static str = "schedule";

    pub fn new(schedule: Arc<Mutex<Schedule>>, pool: Pool) -> Self {
        Self { schedule, pool }
    }

    pub fn description() -> String {
        "Answer a pilot's request later, when it's due: \"tell me when the tyres are warm\", \
         \"remind me in five laps\", \"at lap 20 tell me if we can make it\", \"tell me if Fischer \
         gets within a second\". When its trigger is met the app gives you the request again, \
         unprompted, to answer from the telemetry of that moment with the `engineer` tool. \
         `action` add needs `request` (the pilot's request, in their language, self-contained: \
         name what pronouns like \"they\" refer to, and the rival by name) and exactly one \
         trigger: `in_seconds` (session clock), `in_laps`, `at_lap` (the player's telemetry \
         `lap`), or `when` (one read-only SELECT over the telemetry database returning a number, \
         nonzero when it's due; read `schema` first, and keep it cheap: it runs every second, so \
         read only the newest rows by id). Requests are one-shot and end with the session: to \
         keep updating the pilot, schedule the next one when answering, only if they asked for \
         repeated updates. Tell the pilot it's set only once this tool confirms it; if it fails, \
         say you can't watch for that. `list` shows what's set; `cancel` removes `id`."
            .into()
    }

    pub fn input_schema() -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "action": {"type": "string", "enum": ["add", "list", "cancel"]},
                "request": {
                    "type": "string",
                    "description": "add: what to answer when it's due, self-contained."
                },
                "in_seconds": {
                    "type": "number",
                    "description": "add: due after this many seconds of session time."
                },
                "in_laps": {
                    "type": "integer",
                    "description": "add: due this many laps from the player's current lap."
                },
                "at_lap": {
                    "type": "integer",
                    "description": "add: due when the player's telemetry `lap` reaches it."
                },
                "when": {
                    "type": "string",
                    "description": "add: one read-only SELECT whose first value is nonzero when it's due."
                },
                "id": {"type": "integer", "description": "cancel: the request to remove."}
            },
            "required": ["action"]
        })
    }

    async fn run(&self, input: ScheduleInput) -> anyhow::Result<String> {
        match input.action {
            Action::Add => self.add(input).await,
            Action::List => Ok(self.list()),
            Action::Cancel => {
                let id = input.id.context("cancel needs an `id`")?;
                match lock(&self.schedule).remove(id) {
                    true => Ok(format!("Cancelled #{id}.")),
                    false => bail!("nothing scheduled as #{id}"),
                }
            }
        }
    }

    async fn add(&self, input: ScheduleInput) -> anyhow::Result<String> {
        let text = input.request.as_deref().map(str::trim).unwrap_or_default();
        if text.is_empty() {
            bail!("add needs a `request`");
        }
        if text.chars().count() > MAX_REQUEST {
            bail!("the request is longer than {MAX_REQUEST} characters");
        }
        let text = text.to_owned();
        let asked = match (input.in_seconds, input.in_laps, input.at_lap, input.when) {
            (Some(s), None, None, None) if s.is_finite() && s > 0.0 && s <= MAX_SECONDS => {
                Asked::InSeconds(s)
            }
            (Some(_), None, None, None) => {
                bail!("`in_seconds` must be more than 0 and at most {MAX_SECONDS}")
            }
            (None, Some(laps), None, None) if laps > 0 => Asked::InLaps(laps),
            (None, Some(_), None, None) => bail!("`in_laps` must be at least 1"),
            (None, None, Some(lap), None) => Asked::AtLap(lap),
            (None, None, None, Some(query)) => Asked::When(query),
            _ => bail!("add needs exactly one of `in_seconds`, `in_laps`, `at_lap` or `when`"),
        };
        if lock(&self.schedule).waiting_len() >= MAX_WAITING {
            bail!("{MAX_WAITING} requests are already scheduled: cancel one first");
        }

        let conn = self.pool.get().await?;
        let set = conn
            .run(move |conn| -> anyhow::Result<Option<(Trigger, Now)>> {
                let now = Now::read(conn)?.context(
                    "no player telemetry yet (the player isn't in the car): nothing can be watched",
                )?;
                let trigger = match asked {
                    Asked::InSeconds(s) => Trigger::Time(now.time + s),
                    Asked::InLaps(laps) => Trigger::Lap(
                        now.lap
                            .checked_add(laps)
                            .context("`in_laps` is too large")?,
                    ),
                    Asked::AtLap(lap) if lap > now.lap => Trigger::Lap(lap),
                    Asked::AtLap(lap) => {
                        bail!("the player is already on lap {} (asked for {lap})", now.lap)
                    }
                    // Checked now: a broken condition is never promised, a met one is answered
                    Asked::When(query) => match condition(conn, &query)? {
                        true => return Ok(None),
                        false => Trigger::When(query.trim().to_owned()),
                    },
                };
                Ok(Some((trigger, now)))
            })
            .await?;
        let Some((trigger, now)) = set else {
            return Ok(
                "The condition is already true now, so nothing was scheduled: answer the \
                       request now instead."
                    .into(),
            );
        };

        let mut schedule = lock(&self.schedule);
        if schedule.waiting_len() >= MAX_WAITING {
            bail!("{MAX_WAITING} requests are already scheduled: cancel one first");
        }
        schedule.next_id += 1;
        let id = schedule.next_id;
        let due = describe(&trigger, &now);
        schedule.requests.push(Request {
            id,
            text,
            trigger,
            game: now.game,
            seen: now.time,
            state: State::Waiting,
            errors: 0,
            attempts: 0,
        });
        Ok(format!(
            "Scheduled #{id}: due {due}. You'll be given the request when it is; tell the pilot \
             it's set."
        ))
    }

    fn list(&self) -> String {
        let schedule = lock(&self.schedule);
        if schedule.requests.is_empty() {
            return "Nothing is scheduled.".into();
        }
        let mut out = String::new();
        for r in &schedule.requests {
            let state = match &r.state {
                State::Waiting => "waiting",
                State::Met => "due now",
                State::Failed(_) => "can't be checked anymore",
                State::Expired => "expired",
            };
            let trigger = match &r.trigger {
                Trigger::Time(t) => format!("at session time {t:.0} s"),
                Trigger::Lap(lap) => format!("at lap {lap}"),
                Trigger::When(query) => format!("when {query:?}"),
            };
            let _ = writeln!(out, "#{}: {:?}, {trigger} ({state})", r.id, r.text);
        }
        out
    }
}

/// When `trigger` is due, from `now`.
fn describe(trigger: &Trigger, now: &Now) -> String {
    match trigger {
        Trigger::Time(t) => format!(
            "in {:.0} s of session time (session time {t:.0} s)",
            t - now.time
        ),
        Trigger::Lap(lap) => format!("at lap {lap} (the player is on lap {} now)", now.lap),
        Trigger::When(_) => "once the condition is true (it's false now)".into(),
    }
}

impl ToolHandler for ScheduleTool {
    type Input = ScheduleInput;

    fn handle(
        &self,
        input: ScheduleInput,
    ) -> impl Future<Output = anyhow::Result<String>> + Send + 'static {
        let tool = self.clone();
        async move { tool.run(input).await }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sqlite::Database;
    use bono_sim::{
        recorder::{DATABASE, Recorder},
        sys::{bono_session_info, bono_standing, bono_telemetry},
    };
    use std::num::NonZeroUsize;

    /// The player (vehicle 1) driving a `session_type` session, a lap every 100 s, up to `until`
    /// seconds.
    fn drive(recorder: &Recorder, session_type: &str, until: f64) {
        let mut session = bono_session_info::default();
        session.track = Some("Test Ring".to_owned()).into();
        session.session_type = Some(session_type.to_owned()).into();
        session.session_number = Some(1).into();
        let mut standing = bono_standing::default();
        standing.vehicle_id = Some(1).into();
        standing.is_player = Some(true).into();
        recorder.push_scoring(session, vec![standing]);
        let mut t = 0.0;
        while t <= until {
            let mut telemetry = bono_telemetry::default();
            telemetry.vehicle_id = Some(1).into();
            telemetry.session_time = Some(t).into();
            telemetry.lap = Some((t / 100.0).floor() as i64).into();
            telemetry.wheels[0].tire_surface_temperature_center = Some(60.0 + t / 10.0).into();
            if recorder.wants_telemetry(1, t, 10.0) {
                recorder.push_telemetry(&telemetry);
            }
            t += 1.0;
        }
    }

    fn pool(dir: &std::path::Path) -> Pool {
        let two = NonZeroUsize::new(2).unwrap();
        Pool::new(Database::read_only(Arc::new(dir.join(DATABASE))), two, two)
    }

    fn add(fields: serde_json::Value) -> ScheduleInput {
        let mut input = serde_json::json!({"action": "add", "request": "tell me"});
        input
            .as_object_mut()
            .unwrap()
            .extend(fields.as_object().unwrap().clone());
        serde_json::from_value(input).unwrap()
    }

    async fn check_now(tool: &ScheduleTool) {
        let checks = lock(&tool.schedule).to_check();
        let conn = tool.pool.get().await.unwrap();
        let (time, outcomes) = conn.run(move |conn| check(conn, &checks)).await.unwrap();
        lock(&tool.schedule).apply(time, outcomes);
    }

    /// The recorder writes on its own thread: waits for the player's row at `t`.
    async fn wait_for(pool: &Pool, t: f64) {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            // Not even created yet, at first
            if let Ok(conn) = pool.get().await
                && let Some(now) = conn.run(|conn| Now::read(conn).ok().flatten()).await
                && now.time >= t
            {
                return;
            }
            assert!(Instant::now() < deadline, "the recording never reached {t}");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn sets_triggers_from_where_the_player_is() {
        let dir = tempfile::tempdir().unwrap();
        let recorder = Recorder::new(dir.path(), None).unwrap();
        drive(&recorder, "race", 150.0);
        let pool = pool(dir.path());
        wait_for(&pool, 150.0).await;
        let tool = ScheduleTool::new(Default::default(), pool);

        let out = tool
            .run(add(serde_json::json!({"in_laps": 2})))
            .await
            .unwrap();
        assert!(
            out.contains("at lap 3 (the player is on lap 1 now)"),
            "{out}"
        );
        let out = tool
            .run(add(serde_json::json!({"in_seconds": 30})))
            .await
            .unwrap();
        assert!(out.contains("session time 180 s"), "{out}");
        // The front left is at 75 C
        let warm = "SELECT tire_surface_temperature_center > 70 FROM telemetry_wheel \
                    WHERE wheel = 0 ORDER BY telemetry_id DESC LIMIT 1";
        let out = tool
            .run(add(serde_json::json!({"when": warm})))
            .await
            .unwrap();
        assert!(out.contains("already true"), "{out}");
        let cold = warm.replace("> 70", "> 90");
        tool.run(add(serde_json::json!({"when": cold})))
            .await
            .unwrap();
        assert_eq!(lock(&tool.schedule).requests.len(), 3);

        // Nothing is promised that can't be watched
        for bad in [
            serde_json::json!({"at_lap": 1}),
            serde_json::json!({"in_laps": 0}),
            serde_json::json!({"in_laps": i64::MAX}),
            serde_json::json!({"in_seconds": -5}),
            serde_json::json!({"in_laps": 1, "at_lap": 5}),
            serde_json::json!({}),
            serde_json::json!({"when": "SELECT no_such_column FROM telemetry"}),
            serde_json::json!({"when": "SELECT 'yes'"}),
            serde_json::json!({"when": "DELETE FROM telemetry"}),
            serde_json::json!({"when": "SELECT 0; SELECT 1"}),
        ] {
            assert!(tool.run(add(bad.clone())).await.is_err(), "{bad}");
        }
        let empty = serde_json::json!({"action": "add", "request": " ", "in_laps": 1});
        assert!(
            tool.run(serde_json::from_value(empty).unwrap())
                .await
                .is_err()
        );

        check_now(&tool).await;
        assert_eq!(lock(&tool.schedule).next(), None);
        assert!(
            tool.list()
                .contains("#2: \"tell me\", at session time 180 s (waiting)")
        );

        let cancel = serde_json::json!({"action": "cancel", "id": 2});
        tool.run(serde_json::from_value(cancel.clone()).unwrap())
            .await
            .unwrap();
        assert!(
            tool.run(serde_json::from_value(cancel).unwrap())
                .await
                .is_err()
        );
        assert_eq!(lock(&tool.schedule).requests.len(), 2);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn asks_due_requests_and_drops_them_with_their_session() {
        let dir = tempfile::tempdir().unwrap();
        let recorder = Recorder::new(dir.path(), None).unwrap();
        drive(&recorder, "race", 150.0);
        let pool = pool(dir.path());
        wait_for(&pool, 150.0).await;
        let schedule = Arc::new(Mutex::new(Schedule::default()));
        let tool = ScheduleTool::new(schedule.clone(), pool.clone());
        tool.run(add(
            serde_json::json!({"request": "box now?", "in_seconds": 10}),
        ))
        .await
        .unwrap();
        tool.run(add(serde_json::json!({"request": "lap 5", "at_lap": 5})))
            .await
            .unwrap();
        // Not due yet
        check_now(&tool).await;
        assert_eq!(lock(&schedule).next(), None);

        // Fakes the trigger being met: asked, at most twice
        lock(&schedule).apply(150.0, vec![(1, Outcome::Met)]);
        for _ in 0..MAX_ATTEMPTS {
            let Some(Next::Ask {
                id, prompt, failed, ..
            }) = lock(&schedule).next()
            else {
                panic!("not asked");
            };
            assert_eq!((id, failed), (1, false));
            assert!(prompt.contains("\"box now?\""), "{prompt}");
        }
        assert_eq!(
            lock(&schedule).next(),
            Some(Next::GaveUp("box now?".into()))
        );

        // A condition failing is given up, once it's failed for a while
        for _ in 1..MAX_ERRORS {
            lock(&schedule).apply(150.0, vec![(2, Outcome::Error("broken".into()))]);
        }
        lock(&schedule).apply(150.0, vec![(2, Outcome::Pending)]);
        for _ in 0..MAX_ERRORS {
            assert_eq!(lock(&schedule).next(), None);
            lock(&schedule).apply(150.0, vec![(2, Outcome::Error("broken".into()))]);
        }
        let Some(Next::Ask { failed, prompt, .. }) = lock(&schedule).next() else {
            panic!("not told");
        };
        assert!(failed && prompt.contains("broken"), "{prompt}");
        assert!(lock(&schedule).remove(2));

        // Another session: whatever was waiting is dropped, and whatever was due but not asked yet
        for request in ["later", "due"] {
            tool.run(add(serde_json::json!({"request": request, "in_laps": 1})))
                .await
                .unwrap();
        }
        lock(&schedule).apply(150.0, vec![(4, Outcome::Met)]);
        drive(&recorder, "practice", 5.0);
        let deadline = Instant::now() + Duration::from_secs(5);
        while !(lock(&schedule).requests.iter()).all(|r| r.state == State::Expired) {
            assert!(Instant::now() < deadline, "never expired");
            tokio::time::sleep(Duration::from_millis(20)).await;
            check_now(&tool).await;
        }
        assert_eq!(lock(&schedule).next(), Some(Next::Expired("later".into())));
        assert_eq!(lock(&schedule).next(), Some(Next::Expired("due".into())));
        assert_eq!(lock(&schedule).next(), None);
    }

    #[test]
    fn a_restarted_session_expires_requests_set_at_its_start() {
        let mut schedule = Schedule::default();
        schedule.requests.push(Request {
            id: 1,
            text: "in five minutes".into(),
            trigger: Trigger::Time(300.0),
            game: None,
            seen: 0.0,
            state: State::Waiting,
            errors: 0,
            attempts: 0,
        });
        schedule.apply(200.0, vec![(1, Outcome::Pending)]);
        let checks = schedule.to_check();
        assert_eq!(checks[0].seen, 200.0);
        // Back at the start of the same session
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE telemetry (id INTEGER PRIMARY KEY, is_player, session_time, lap);
             CREATE TABLE session (id INTEGER PRIMARY KEY, track, session_type, session_number);
             INSERT INTO telemetry VALUES (1, 1, 10.0, 0);",
        )
        .unwrap();
        let (time, outcomes) = check(&conn, &checks).unwrap();
        assert_eq!(outcomes, vec![(1, Outcome::Expired)]);
        schedule.apply(time, outcomes);
        assert_eq!(
            schedule.next(),
            Some(Next::Expired("in five minutes".into()))
        );
    }
}
