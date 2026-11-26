//! The `schedule` tool: how the model adds, cancels and replaces the pilot's requests.
//!
//! The tool only stages changes for the turn holding the conversation. The engineer takes them
//! when the turn finishes and applies them to its [`Schedule`](super::Schedule), or drops them
//! with the turn.

use super::{
    Change, Checked, DEFAULT_STALE, MAX_REQUESTS, NewRequest, Repeat, RequestId, Trigger, describe,
    describe_stale,
};
use crate::{
    engineer::tool::{self, Progress},
    provider::ToolHandler,
};
use anyhow::{Context as _, bail};
use serde::Deserialize;
use std::{
    sync::{Arc, Mutex, MutexGuard, PoisonError},
    time::Duration,
};

/// How long a call waits for a progress snapshot when there's none: the data thread publishes
/// about once a second, and has none for a moment while its tables catch up or after a failed tick
const PROGRESS_WAIT: Duration = Duration::from_secs(3);

/// How often a call waiting for a progress snapshot looks for one
const PROGRESS_POLL: Duration = Duration::from_millis(100);

/// The fewest seconds of game clock between a repeating time trigger's firings: each firing is a
/// scheduled turn and an answer to speak
const MIN_REPEAT_S: f64 = 10.0;

/// The session types the simulator adapters write to `race_state.session_type`
const SESSION_TYPES: [&str; 5] = ["test", "practice", "qualifying", "warmup", "race"];

/// The tool, for one conversation. Cheap to clone; clones stage into the same turn.
#[derive(Clone)]
pub struct ScheduleTool {
    engineer: tool::Engineer,
    /// Locked only to push a change or take them, never across a wait
    staging: Arc<Mutex<Staging>>,
    /// How long a call waits for a progress snapshot when there's none
    progress_wait: Duration,
}

/// What the turn holding the conversation has staged
struct Staging {
    /// Goes up with every `begin`, so a call made for an earlier turn doesn't stage into this one
    turn: u64,
    /// The requests a change can refer to: those pending when the turn began, and those it added,
    /// less those it cancelled
    pending: Vec<RequestId>,
    /// The id of the turn's next add
    next_id: RequestId,
    changes: Vec<Change>,
}

impl ScheduleTool {
    pub const NAME: &'static str = "schedule";

    pub fn new(engineer: tool::Engineer) -> Self {
        Self {
            engineer,
            staging: Arc::new(Mutex::new(Staging {
                turn: 0,
                pending: Vec::new(),
                next_id: RequestId(1),
                changes: Vec::new(),
            })),
            progress_wait: PROGRESS_WAIT,
        }
    }

    /// With another wait for a progress snapshot, so a test without one needn't wait long
    #[cfg(test)]
    fn with_progress_wait(mut self, wait: Duration) -> Self {
        self.progress_wait = wait;
        self
    }

    pub fn description() -> String {
        "Adds, cancels or replaces scheduled pilot requests that fire in a future turn with fresh \
         telemetry. A replace keeps what's been counted: its `until` counts from when the request \
         was first made, and its next firing counts on from its last. To count from now, cancel it \
         and add a new one. Changes apply when your reply finishes."
            .into()
    }

    pub fn input_schema() -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "action": {
                    "type": "string",
                    "enum": ["add", "cancel", "replace"]
                },
                "id": {
                    "type": "string",
                    "description": "cancel, replace: the request's id, as the pending list gives it (\"#3\")."
                },
                "instruction": {
                    "type": "string",
                    "description": "add, replace: short self-contained note of what to check or say when it fires, with session context only if it matters."
                },
                "trigger": trigger_schema(
                    "add, replace: when it fires. time and laps count from now (and, when it \
                     repeats, from its last firing); session fires when the next session starts; \
                     condition fires when its query starts returning a row (one already true \
                     isn't added if it's answered once, and otherwise waits until it turns false \
                     and true again)."
                ),
                "repeat": {
                    "type": "string",
                    "enum": ["once", "every"],
                    "description": "add, replace: answered once (the default), or every time it fires. Needed with `until`."
                },
                "until": trigger_schema(
                    "add, replace: when the request stops (repeating or one-shot expiry), counted \
                     from when it was made. A firing on the same check still counts."
                ),
                "stale_after": trigger_schema(&format!(
                    "add, replace: when an answer stops being worth saying, counted from its \
                     firing: time or laps after it, the next session starting, or while a \
                     condition's query returns a row. Without it, {}.",
                    describe_stale(&DEFAULT_STALE)
                ))
            },
            "required": ["action"]
        })
    }

    /// Starts staging for a turn that now holds the conversation, discarding what an earlier turn
    /// staged: `pending` are the requests pending now, and the turn's adds are numbered from
    /// `next_id` ([`Schedule::next_id`](super::Schedule::next_id)), so the ids the model confirms
    /// are the ones applied.
    pub fn begin(&self, pending: Vec<RequestId>, next_id: RequestId) {
        let mut staging = self.lock();
        staging.turn += 1;
        staging.pending = pending;
        staging.next_id = next_id;
        let discarded = std::mem::take(&mut staging.changes);
        drop(staging);
        drop(discarded);
    }

    /// The changes staged since [`begin`](Self::begin), in the order they were made.
    pub fn take(&self) -> Vec<Change> {
        std::mem::take(&mut self.lock().changes)
    }

    fn lock(&self) -> MutexGuard<'_, Staging> {
        self.staging.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Runs `stage` on this turn's staging, unless another turn has begun since `turn`.
    fn stage<T>(
        &self,
        turn: u64,
        stage: impl FnOnce(&mut Staging) -> anyhow::Result<T>,
    ) -> anyhow::Result<T> {
        let mut staging = self.lock();
        if staging.turn != turn {
            bail!("the turn moved on, so nothing was changed");
        }
        stage(&mut staging)
    }

    /// Runs a call made in `turn`
    async fn run(&self, turn: u64, input: ScheduleInput) -> anyhow::Result<String> {
        let replacing = match input.action {
            Action::Cancel => {
                let id = input.id()?;
                self.stage(turn, |staging| {
                    staging.find(id)?;
                    staging.pending.retain(|pending| *pending != id);
                    staging.changes.push(Change::Cancel(id));
                    Ok(())
                })?;
                return Ok(format!("Cancelled {id}."));
            }
            Action::Add => None,
            Action::Replace => Some(input.id()?),
        };
        let (instruction, trigger, repeat, until, stale_after) = input.request()?;
        // What can be checked at once, before waiting for progress or running conditions. Checked
        // again when it's staged: another call may have changed the staging meanwhile.
        self.stage(turn, |staging| match replacing {
            Some(id) => staging.find(id),
            None => staging.room(),
        })?;
        let made = self.wait_for_progress().await?;
        let mut new = NewRequest {
            instruction,
            trigger,
            repeat,
            until,
            stale_after,
            made,
            checked: Checked::default(),
        };
        // Outside the lock: the data thread may take a while
        let notes = self.check_conditions(&mut new).await?;
        // A one-time request whose trigger already holds is answered by now: what the pilot asked
        // to hear is already the case, and it may never turn false and true again to fire
        if new.repeat == Repeat::Once && new.checked.trigger == Some(true) {
            const ANSWER: &str = "Tell them that now in one sentence, from the telemetry; they \
                                  can ask again for the next time.";
            return Ok(match replacing {
                Some(id) => format!(
                    "Nothing was changed, so {id} stays as it was: what the pilot asked to be \
                     told about is already the case. {ANSWER}"
                ),
                None => format!(
                    "Nothing was added: what the pilot asked to be told about is already the \
                     case. {ANSWER}"
                ),
            });
        }
        let summary = summary(&new);
        let kept = kept(&new);
        let id = self.stage(turn, |staging| match replacing {
            Some(id) => {
                staging.find(id)?;
                staging.changes.push(Change::Replace(id, new));
                Ok(id)
            }
            None => {
                staging.room()?;
                let id = staging.next_id;
                staging.next_id = RequestId(id.0 + 1);
                staging.pending.push(id);
                staging.changes.push(Change::Add(id, new));
                Ok(id)
            }
        })?;
        Ok(match replacing {
            Some(_) => format!("Replaced {id}: {summary}.{kept}{notes}"),
            None => format!("Added {id}: {summary}.{notes}"),
        })
    }

    /// The latest progress snapshot, waiting a little for one when there's none: the data thread
    /// has none for a moment while its tables catch up with the recording, or after a failed tick.
    /// Polls without blocking, since the snapshot is only ever copied out of shared state.
    async fn wait_for_progress(&self) -> anyhow::Result<Progress> {
        let deadline = tokio::time::Instant::now() + self.progress_wait;
        loop {
            if let Some(progress) = self.engineer.peek_progress() {
                return Ok(progress);
            }
            let now = tokio::time::Instant::now();
            if now >= deadline {
                bail!(
                    "telemetry isn't available right now, so nothing was changed: try again \
                     shortly"
                );
            }
            tokio::time::sleep((deadline - now).min(PROGRESS_POLL)).await;
        }
    }

    /// Checks each of the request's conditions once now. Returns a note for the model about those
    /// already true.
    /// Notes each result in `new.checked`.
    async fn check_conditions(&self, new: &mut NewRequest) -> anyhow::Result<String> {
        let until = new.until.as_ref();
        let mut notes = String::new();
        let mut checked = Checked::default();
        // The snapshot when each was checked: one after it was measured after that check
        let (mut trigger_seq, mut until_seq, mut stale_seq) = (0, 0, 0);
        for (field, trigger, note, result) in [
            (
                "trigger",
                Some(&new.trigger),
                "it fires only once that turns false and then true again",
                &mut checked.trigger,
            ),
            (
                "until",
                until,
                "it ends only once that turns false and then true again",
                &mut checked.until,
            ),
            (
                "stale_after",
                new.stale_after.as_ref(),
                "while it stays true, its answers are out of date and skipped",
                &mut checked.stale_after,
            ),
        ] {
            let Some(Trigger::Condition { query }) = trigger else {
                continue;
            };
            let (holds, at, epoch) = self
                .engineer
                .check_condition_at(query.clone())
                .await
                .with_context(|| {
                    format!("the `{field}` condition failed, so nothing was changed")
                })?;
            // Its result is where changes are counted from in the request's epoch, not another's
            if epoch != new.made.epoch {
                bail!(
                    "the recording moved on to another session while the `{field}` condition was \
                     checked, so nothing was changed: just retry"
                );
            }
            // Unknown (the tables don't show how things are now): its first check is its start
            *result = holds;
            match field {
                "trigger" => trigger_seq = at,
                "until" => until_seq = at,
                _ => stale_seq = at,
            }
            if holds == Some(true) {
                notes.push_str(&format!(
                    " Its `{field}` condition is already true: {note}."
                ));
            }
        }
        (checked.trigger_seq, checked.until_seq, checked.stale_seq) =
            (trigger_seq, until_seq, stale_seq);
        new.checked = checked;
        Ok(notes)
    }
}

impl Staging {
    /// Fails if the cap of pending requests is reached
    fn room(&self) -> anyhow::Result<()> {
        if self.pending.len() >= MAX_REQUESTS {
            bail!(
                "the cap of {MAX_REQUESTS} pending requests is reached, so this one wasn't added: \
                 ask the pilot which to cancel"
            );
        }
        Ok(())
    }

    /// Fails unless `id` is a request a change can refer to
    fn find(&self, id: RequestId) -> anyhow::Result<()> {
        if self.pending.contains(&id) {
            return Ok(());
        }
        let pending: Vec<_> = self.pending.iter().map(RequestId::to_string).collect();
        if pending.is_empty() {
            bail!("there's no pending request {id}: none are pending");
        }
        bail!(
            "there's no pending request {id}; the pending ones are {}",
            pending.join(", ")
        );
    }
}

impl ToolHandler for ScheduleTool {
    type Input = ScheduleInput;

    fn handle(
        &self,
        input: ScheduleInput,
    ) -> impl Future<Output = anyhow::Result<String>> + Send + 'static {
        let tool = self.clone();
        // When the call is made, not when it runs: a call made for a turn that's over by then
        // stages nothing
        let turn = self.lock().turn;
        async move { tool.run(turn, input).await }
    }
}

/// A trigger's schema, for when a request fires, ends or goes stale
fn trigger_schema(description: &str) -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "description": description,
        "properties": {
            "kind": {
                "type": "string",
                "enum": ["time", "laps", "session", "condition"]
            },
            "seconds": {
                "type": "number",
                "description": format!("time: seconds of game clock, more than 0; at least {MIN_REPEAT_S} for the trigger of a request that repeats.")
            },
            "count": {
                "type": "integer",
                "description": "laps: laps the pilot completes, 1 or more."
            },
            "session_type": {
                "type": "string",
                "enum": SESSION_TYPES,
                "description": "session: only a session of this type, as race_state.session_type gives it. Any session if left out."
            },
            "query": {
                "type": "string",
                "description": "condition: one SELECT or WITH statement over the tables `sql` reads, true when it returns a row."
            }
        },
        "required": ["kind"]
    })
}

/// What a replace keeps counting, for the model to confirm: only time and laps count towards a
/// firing, and only an `until` counts from when the request was made
fn kept(new: &NewRequest) -> String {
    let mut kept = String::new();
    if matches!(new.trigger, Trigger::Time { .. } | Trigger::Laps { .. }) {
        kept.push_str(
            " It counts from its last firing, or from when it was first made if it hasn't fired \
             yet.",
        );
    }
    if new.until.is_some() {
        kept.push_str(" Its `until` still counts from when it was first made.");
    }
    kept
}

/// A request in one line, for the model to confirm
fn summary(new: &NewRequest) -> String {
    let repeat = match (&new.repeat, &new.until) {
        (Repeat::Once, None) => "once".to_string(),
        (Repeat::Once, Some(until)) => {
            format!("once, until {} from when it was made", describe(until))
        }
        (Repeat::Every, None) => "every time, until cancelled".to_string(),
        (Repeat::Every, Some(until)) => format!(
            "every time, until {} from when it was made",
            describe(until)
        ),
    };
    format!(
        "fires {}, {repeat}; stale {}",
        describe(&new.trigger),
        describe_stale(new.stale_after.as_ref().unwrap_or(&DEFAULT_STALE)),
    )
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Action {
    Add,
    Cancel,
    Replace,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ScheduleInput {
    action: Action,
    /// Read by hand, so a malformed one gets a clear error
    #[serde(default)]
    id: Option<serde_json::Value>,
    #[serde(default)]
    instruction: Option<String>,
    #[serde(default)]
    trigger: Option<TriggerInput>,
    #[serde(default)]
    repeat: Option<RepeatInput>,
    #[serde(default)]
    until: Option<TriggerInput>,
    #[serde(default)]
    stale_after: Option<TriggerInput>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
enum RepeatInput {
    Once,
    Every,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum TriggerInput {
    Time {
        seconds: f64,
    },
    /// A number, so a whole one written as `1.0` is read too
    Laps {
        count: f64,
    },
    Session {
        #[serde(default)]
        session_type: Option<String>,
    },
    Condition {
        query: String,
    },
}

impl ScheduleInput {
    /// `"#3"` as the schema has it, or `"# 3"`, `"3"`, `3` or `3.0`, which a model may well send
    fn id(&self) -> anyhow::Result<RequestId> {
        let Some(id) = &self.id else {
            bail!("`id` is needed: the pending request's id, like #3");
        };
        let parsed = match id {
            serde_json::Value::Number(number) => number.as_u64().or_else(|| {
                number
                    .as_f64()
                    .filter(|n| n.fract() == 0.0 && (0.0..=f64::from(u32::MAX)).contains(n))
                    .map(|n| n as u64)
            }),
            serde_json::Value::String(text) => {
                let digits = text.trim();
                let digits = digits.strip_prefix('#').unwrap_or(digits).trim_start();
                digits.parse::<u32>().ok().map(u64::from)
            }
            _ => None,
        };
        match parsed.and_then(|id| u32::try_from(id).ok()) {
            Some(id) => Ok(RequestId(id)),
            None => bail!("`id` is a request's id, like #3, not {id}"),
        }
    }

    /// The request to add or replace with: its instruction, trigger, repeat, `until` and
    /// `stale_after`
    pub(crate) fn request(
        self,
    ) -> anyhow::Result<(String, Trigger, Repeat, Option<Trigger>, Option<Trigger>)> {
        let instruction = self.instruction.as_deref().map(str::trim).unwrap_or("");
        if instruction.is_empty() {
            bail!("`instruction` is needed: what to do when it fires");
        }
        let Some(trigger) = self.trigger else {
            bail!("`trigger` is needed: when it fires");
        };
        let trigger = trigger.parse("trigger")?;
        let until = self.until.map(|until| until.parse("until")).transpose()?;
        // Left out, "every lap for five laps" would be answered once
        if until.is_some() && self.repeat.is_none() {
            bail!("`until` needs `repeat`: every to stop repeating, or once to expire unanswered");
        }
        let ends_first = match (&trigger, &until) {
            (Trigger::Time { seconds }, Some(Trigger::Time { seconds: until })) => until < seconds,
            (Trigger::Laps { count }, Some(Trigger::Laps { count: until })) => until < count,
            _ => false,
        };
        if ends_first {
            bail!("`until` ends it before its `trigger` can fire: they count from the same start");
        }
        let repeat = match self.repeat.unwrap_or(RepeatInput::Once) {
            RepeatInput::Once => Repeat::Once,
            RepeatInput::Every => Repeat::Every,
        };
        if let (Repeat::Every, Trigger::Time { seconds }) = (repeat, &trigger)
            && *seconds < MIN_REPEAT_S
        {
            bail!(
                "`trigger`: a request that repeats needs at least {MIN_REPEAT_S} seconds of game \
                 clock between firings, not {seconds}; for something sooner, use a condition"
            );
        }
        let stale_after = self
            .stale_after
            .map(|stale| stale.parse("stale_after"))
            .transpose()?;
        Ok((instruction.to_owned(), trigger, repeat, until, stale_after))
    }
}

impl TriggerInput {
    /// The trigger, if it can ever count to anything: an empty period would fire on every check
    fn parse(self, field: &str) -> anyhow::Result<Trigger> {
        Ok(match self {
            TriggerInput::Time { seconds } => {
                if !(seconds.is_finite() && seconds > 0.0) {
                    bail!("`{field}`: `seconds` must be more than 0, not {seconds}");
                }
                Trigger::Time { seconds }
            }
            TriggerInput::Laps { count } => {
                if !(count.fract() == 0.0 && (1.0..=f64::from(u32::MAX)).contains(&count)) {
                    bail!(
                        "`{field}`: `count` must be a whole number of laps, 1 or more, not {count}"
                    );
                }
                Trigger::Laps {
                    count: count as u32,
                }
            }
            TriggerInput::Session { session_type } => {
                let kind = session_type
                    .as_deref()
                    .map(str::trim)
                    .filter(|kind| !kind.is_empty());
                let kind = match kind {
                    None => None,
                    Some(kind) => match SESSION_TYPES
                        .iter()
                        .find(|known| known.eq_ignore_ascii_case(kind))
                    {
                        Some(known) => Some((*known).to_owned()),
                        None => bail!(
                            "`{field}`: `session_type` is one of {}, not {kind:?}",
                            SESSION_TYPES.join(", ")
                        ),
                    },
                };
                Trigger::Session { kind }
            }
            TriggerInput::Condition { query } => {
                if query.trim().is_empty() {
                    bail!("`{field}`: `query` is needed, one SELECT or WITH statement");
                }
                Trigger::Condition { query }
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        engineer::{schedule::MAX_REQUESTS, tool},
        provider::Tools,
        sqlite::{Database, pool::Pool},
    };
    use bono_sim::{
        recorder::{DATABASE, Recorder},
        sys::{bono_session_info, bono_standing, bono_telemetry},
    };
    use serde_json::json;
    use std::{
        num::NonZeroUsize,
        sync::Arc,
        time::{Duration, Instant},
    };

    /// The player's car alone in a race, its laps recorded: going 36 km/h
    const TRUE: &str = "SELECT 1 FROM cars_now WHERE is_player = 1";
    const FALSE: &str = "SELECT 1 FROM cars_now WHERE is_player = 1 AND speed_kmh > 100";
    const BROKEN: &str = "SELECT 1 FROM nowhere";
    /// The session types `lmu.cpp` and `rrre.rs` write
    const WRITTEN: [&str; 5] = ["test", "practice", "qualifying", "warmup", "race"];

    fn open(dir: &tempfile::TempDir) -> tool::Engineer {
        let two = NonZeroUsize::new(2).unwrap();
        tool::Engineer::new(Pool::new(
            Database::read_only(Arc::new(dir.path().join(DATABASE))),
            two,
            two,
        ))
    }

    /// A tool over a recorded race, once the data thread has published progress
    async fn recorded() -> (tempfile::TempDir, tool::Engineer, ScheduleTool) {
        let dir = tempfile::tempdir().unwrap();
        let recorder = Recorder::new(dir.path(), None).unwrap();
        for t in 0..10 {
            let t = f64::from(t);
            let laps = 2;
            let mut session = bono_session_info::default();
            session.session_time = Some(t).into();
            session.track = Some("Test Ring".to_owned()).into();
            session.track_length = Some(1000.0).into();
            session.session_type = Some("race".to_owned()).into();
            session.phase = Some("green".to_owned()).into();
            let mut standing = bono_standing::default();
            standing.session_time = Some(t).into();
            standing.vehicle_id = Some(1).into();
            standing.is_player = Some(true).into();
            standing.driver = Some("Ana Lopes".to_owned()).into();
            standing.laps_completed = Some(laps).into();
            recorder.push_scoring(session, vec![standing]);
            let mut telemetry = bono_telemetry::default();
            telemetry.vehicle_id = Some(1).into();
            telemetry.session_time = Some(t).into();
            // On the lap after those done, as the game counts from lap 1
            telemetry.lap = Some(laps + 1).into();
            telemetry.lap_distance = Some(t * 10.0).into();
            telemetry.speed = Some(10.0).into();
            if recorder.wants_telemetry(1, t, 10.0) {
                recorder.push_telemetry(&telemetry);
            }
        }
        drop(recorder);
        let engineer = open(&dir);
        let deadline = Instant::now() + Duration::from_secs(10);
        while engineer.progress().is_none() {
            assert!(Instant::now() < deadline, "no progress published");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let tool = ScheduleTool::new(engineer.clone());
        tool.begin(Vec::new(), RequestId(1));
        (dir, engineer, tool)
    }

    /// Calls the tool as the model would, through a conversation's tools
    async fn call(tool: &ScheduleTool, input: serde_json::Value) -> anyhow::Result<String> {
        let mut tools = Tools::default();
        tools.insert(
            ScheduleTool::NAME,
            Some(ScheduleTool::description()),
            ScheduleTool::input_schema(),
            tool.clone(),
        );
        tools.handle(ScheduleTool::NAME, &input).await
    }

    fn add(trigger: serde_json::Value) -> serde_json::Value {
        json!({
            "action": "add",
            "instruction": "Tell the pilot the fuel left.",
            "trigger": trigger,
        })
    }

    fn laps(count: u32) -> serde_json::Value {
        json!({"kind": "laps", "count": count})
    }

    fn condition(query: &str) -> serde_json::Value {
        json!({"kind": "condition", "query": query})
    }

    fn ids(changes: &[Change]) -> Vec<(&'static str, RequestId)> {
        changes
            .iter()
            .map(|change| match change {
                Change::Add(id, _) => ("add", *id),
                Change::Cancel(id) => ("cancel", *id),
                Change::Replace(id, _) => ("replace", *id),
            })
            .collect()
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn adding_leaves_the_snapshot_unread() {
        use bono_sim::sql::{EpochReason, begin_epoch};

        let (dir, engineer, tool) = recorded().await;
        engineer.publish_progress().await.unwrap();
        engineer.progress().unwrap();
        let conn = rusqlite::Connection::open(dir.path().join(DATABASE)).unwrap();
        begin_epoch(&conn, i64::MAX, EpochReason::Session).unwrap();
        engineer.publish_progress().await.unwrap();
        // Its baseline is the snapshot the engineer hasn't stepped through yet
        call(&tool, add(laps(1))).await.unwrap();
        // So the engineer still hears of the session starting
        engineer.publish_progress().await.unwrap();
        assert_eq!(engineer.progress().unwrap().new_epochs, ["session"]);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn add_stages_with_the_current_progress() {
        let (_dir, engineer, tool) = recorded().await;
        let before = engineer.progress().unwrap();
        let reply = call(
            &tool,
            json!({
                "action": "add",
                "instruction": "Tell the pilot the fuel left.",
                "trigger": laps(1),
                "repeat": "every",
                "until": laps(5),
                "stale_after": {"kind": "time", "seconds": 20},
            }),
        )
        .await
        .unwrap();
        let after = engineer.progress().unwrap();
        assert!(reply.contains("#1"), "{reply}");
        assert!(!reply.contains("Tell the pilot the fuel left."), "{reply}");
        assert!(!reply.contains('\n'), "one line: {reply}");
        assert_eq!(
            reply,
            "Added #1: fires after 1 lap, every time, until after 5 laps from when it was made; \
             stale 0:20 of game clock after it fires."
        );

        let changes = tool.take();
        let [Change::Add(id, new)] = changes.as_slice() else {
            panic!("{changes:?}");
        };
        assert_eq!(*id, RequestId(1));
        assert_eq!(new.instruction, "Tell the pilot the fuel left.");
        assert_eq!(new.trigger, Trigger::Laps { count: 1 });
        assert_eq!(new.repeat, Repeat::Every);
        assert_eq!(new.until, Some(Trigger::Laps { count: 5 }));
        assert_eq!(new.stale_after, Some(Trigger::Time { seconds: 20.0 }));
        assert!((before.seq..=after.seq).contains(&new.made.seq), "{new:?}");
        assert_eq!(
            (new.made.weekend, new.made.epoch),
            (before.weekend, before.epoch)
        );
        assert!(new.made.weekend.is_some());
        // Taken: nothing is left for a second take
        assert!(tool.take().is_empty());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn condition_is_checked_when_added() {
        let (_dir, _engineer, tool) = recorded().await;

        // A query that fails goes back to the model, in the trigger, `until` or `stale_after`
        let fails = |field: &str, error: anyhow::Error| {
            let error = format!("{error:#}");
            assert!(
                error.contains(&format!("the `{field}` condition failed")),
                "{error}"
            );
            assert!(error.contains("no such table"), "{error}");
        };
        fails(
            "trigger",
            call(&tool, add(condition(BROKEN))).await.unwrap_err(),
        );
        let mut until = add(laps(1));
        until["repeat"] = json!("every");
        until["until"] = condition(BROKEN);
        fails("until", call(&tool, until).await.unwrap_err());
        let mut stale = add(laps(1));
        stale["stale_after"] = condition(BROKEN);
        fails("stale_after", call(&tool, stale).await.unwrap_err());
        assert!(tool.take().is_empty());

        // One that's false now is just added
        let reply = call(&tool, add(condition(FALSE))).await.unwrap();
        assert!(!reply.contains("already true"), "{reply}");
        // One answered once that's true now isn't added: the model answers it now
        let reply = call(&tool, add(condition(TRUE))).await.unwrap();
        assert_eq!(
            reply,
            "Nothing was added: what the pilot asked to be told about is already the case. Tell \
             them that now in one sentence, from the telemetry; they can ask again for the next \
             time."
        );
        // One that repeats waits for it to turn false and true again, and the reply says so
        let mut every = add(condition(TRUE));
        every["repeat"] = json!("every");
        let reply = call(&tool, every).await.unwrap();
        assert!(reply.contains("Added #2"), "{reply}");
        assert!(reply.contains("already true"), "{reply}");
        let mut stale = add(laps(1));
        stale["stale_after"] = condition(TRUE);
        let reply = call(&tool, stale).await.unwrap();
        assert!(reply.contains("already true"), "{reply}");
        assert_eq!(tool.take().len(), 3);

        // A replace is checked the same way
        tool.begin(vec![RequestId(1)], RequestId(2));
        let mut replace = add(condition(BROKEN));
        replace["action"] = json!("replace");
        replace["id"] = json!("#1");
        call(&tool, replace.clone()).await.unwrap_err();
        assert!(tool.take().is_empty());
        // And one answered once that's true now leaves the request as it was
        replace["trigger"] = condition(TRUE);
        let reply = call(&tool, replace).await.unwrap();
        assert!(
            reply.starts_with("Nothing was changed, so #1 stays as it was"),
            "{reply}"
        );
        assert!(tool.take().is_empty());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn cancel_and_replace_need_a_pending_id() {
        let (_dir, _engineer, tool) = recorded().await;
        tool.begin(vec![RequestId(2)], RequestId(3));
        let replace = |id: &str| {
            let mut replace = add(laps(2));
            replace["action"] = json!("replace");
            replace["id"] = json!(id);
            replace
        };

        let error = call(&tool, json!({"action": "cancel", "id": "#5"}))
            .await
            .unwrap_err();
        assert!(format!("{error:#}").contains("#5"), "{error:#}");
        call(&tool, replace("#5")).await.unwrap_err();
        call(&tool, json!({"action": "cancel"})).await.unwrap_err();
        call(&tool, json!({"action": "cancel", "id": "two"}))
            .await
            .unwrap_err();

        // A pending one, with or without the `#`
        let reply = call(&tool, replace("2")).await.unwrap();
        assert!(reply.contains("#2"), "{reply}");
        let reply = call(&tool, json!({"action": "cancel", "id": "#2"}))
            .await
            .unwrap();
        assert!(reply.contains("#2"), "{reply}");
        // Cancelled in this turn: no longer pending
        call(&tool, replace("#2")).await.unwrap_err();
        // One added in this turn can be changed in it
        call(&tool, add(laps(1))).await.unwrap();
        call(&tool, replace("#3")).await.unwrap();

        assert_eq!(
            ids(&tool.take()),
            [
                ("replace", RequestId(2)),
                ("cancel", RequestId(2)),
                ("add", RequestId(3)),
                ("replace", RequestId(3)),
            ]
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn cap_counts_staged_adds() {
        let (_dir, _engineer, tool) = recorded().await;
        let pending: Vec<_> = (1..MAX_REQUESTS as u32).map(RequestId).collect();
        assert_eq!(pending.len(), 49);
        tool.begin(pending, RequestId(50));

        call(&tool, add(laps(1))).await.unwrap();
        let error = call(&tool, add(laps(1))).await.unwrap_err();
        assert!(format!("{error:#}").contains("50"), "{error:#}");
        // A replace doesn't add one
        let mut replace = add(laps(2));
        replace["action"] = json!("replace");
        replace["id"] = json!("#50");
        call(&tool, replace).await.unwrap();
        // A cancel makes room
        call(&tool, json!({"action": "cancel", "id": "#1"}))
            .await
            .unwrap();
        call(&tool, add(laps(1))).await.unwrap();

        assert_eq!(
            ids(&tool.take()),
            [
                ("add", RequestId(50)),
                ("replace", RequestId(50)),
                ("cancel", RequestId(1)),
                ("add", RequestId(51)),
            ]
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn begin_discards_what_was_staged() {
        let (_dir, _engineer, tool) = recorded().await;
        call(&tool, add(laps(1))).await.unwrap();
        tool.begin(Vec::new(), RequestId(1));
        assert!(tool.take().is_empty());
        // Neither is the dropped turn's add pending any more
        call(&tool, json!({"action": "cancel", "id": "#1"}))
            .await
            .unwrap_err();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn ids_number_from_next_id() {
        let (_dir, _engineer, tool) = recorded().await;
        tool.begin(vec![RequestId(3)], RequestId(7));
        assert!(call(&tool, add(laps(1))).await.unwrap().contains("#7"));
        assert!(call(&tool, add(laps(2))).await.unwrap().contains("#8"));
        assert_eq!(
            ids(&tool.take()),
            [("add", RequestId(7)), ("add", RequestId(8))]
        );

        // That turn was dropped: the next one gives the same ids again
        tool.begin(vec![RequestId(3)], RequestId(7));
        assert!(call(&tool, add(laps(1))).await.unwrap().contains("#7"));
        assert_eq!(ids(&tool.take()), [("add", RequestId(7))]);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn refuses_without_progress() {
        // No recording: the data thread never publishes progress
        let dir = tempfile::tempdir().unwrap();
        let engineer = open(&dir);
        let wait = Duration::from_millis(200);
        let tool = ScheduleTool::new(engineer.clone()).with_progress_wait(wait);
        tool.begin(vec![RequestId(1)], RequestId(2));
        // It waits for a snapshot first, then says so without guessing why there's none
        let unavailable = |error: anyhow::Error| {
            let error = format!("{error:#}");
            assert!(error.contains("isn't available right now"), "{error}");
            assert!(error.contains("try again"), "{error}");
            assert!(!error.contains("recording"), "{error}");
        };
        let started = Instant::now();
        unavailable(call(&tool, add(laps(1))).await.unwrap_err());
        assert!(started.elapsed() >= wait);
        let mut replace = add(laps(1));
        replace["action"] = json!("replace");
        replace["id"] = json!("#1");
        unavailable(call(&tool, replace).await.unwrap_err());
        assert!(tool.take().is_empty());
        assert!(engineer.progress().is_none());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn waits_for_progress_through_a_gap() {
        let (_dir, engineer, tool) = recorded().await;
        // A tick fell behind the recording: no snapshot until the next one
        engineer.take_progress().unwrap();
        let reply = call(&tool, add(laps(1))).await.unwrap();
        assert!(reply.starts_with("Added #1"), "{reply}");
        assert_eq!(ids(&tool.take()), [("add", RequestId(1))]);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn cheap_checks_come_first() {
        // No recording, and a wait for progress the test would notice
        let dir = tempfile::tempdir().unwrap();
        let wait = Duration::from_secs(5);
        let tool = ScheduleTool::new(open(&dir)).with_progress_wait(wait);
        let started = Instant::now();
        tool.begin(vec![RequestId(2)], RequestId(3));
        let mut replace = add(condition(BROKEN));
        replace["action"] = json!("replace");
        replace["id"] = json!("#5");
        let error = call(&tool, replace).await.unwrap_err();
        assert!(
            format!("{error:#}").contains("no pending request #5"),
            "{error:#}"
        );
        tool.begin(
            (1..=MAX_REQUESTS as u32).map(RequestId).collect(),
            RequestId(51),
        );
        let error = call(&tool, add(condition(BROKEN))).await.unwrap_err();
        assert!(format!("{error:#}").contains("cap"), "{error:#}");
        assert!(started.elapsed() < wait);
        assert!(tool.take().is_empty());

        // Nor does a condition run for an unknown id
        let (_dir, _engineer, tool) = recorded().await;
        let mut replace = add(condition(BROKEN));
        replace["action"] = json!("replace");
        replace["id"] = json!("#5");
        let error = call(&tool, replace).await.unwrap_err();
        let error = format!("{error:#}");
        assert!(error.contains("no pending request #5"), "{error}");
        assert!(!error.contains("no such table"), "{error}");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_call_made_for_an_earlier_turn_stages_nothing() {
        let (_dir, _engineer, tool) = recorded().await;
        tool.begin(vec![RequestId(1)], RequestId(2));
        let input = |value| serde_json::from_value::<ScheduleInput>(value).unwrap();
        // Made in one turn, run once the next has begun
        let calls = [
            ToolHandler::handle(&tool, input(add(laps(1)))),
            ToolHandler::handle(&tool, input(json!({"action": "cancel", "id": "#1"}))),
        ];
        tool.begin(vec![RequestId(1)], RequestId(2));
        for call in calls {
            let error = call.await.unwrap_err();
            assert!(
                format!("{error:#}").contains("the turn moved on"),
                "{error:#}"
            );
        }
        assert!(tool.take().is_empty());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn replace_says_what_it_keeps() {
        let description = ScheduleTool::description();
        assert!(
            description.contains(
                "A replace keeps what's been counted: its `until` counts from when the request \
                 was first made, and its next firing counts on from its last. To count from now, \
                 cancel it and add a new one."
            ),
            "{description}"
        );

        let (_dir, _engineer, tool) = recorded().await;
        tool.begin(vec![RequestId(2)], RequestId(3));
        let mut replace = add(laps(2));
        replace["action"] = json!("replace");
        replace["id"] = json!(2);
        assert_eq!(
            call(&tool, replace.clone()).await.unwrap(),
            "Replaced #2: fires after 2 laps, once; stale 0:30 of game clock after it fires. \
             It counts from its last firing, or from when it was first made if it hasn't fired yet."
        );
        // A session trigger counts nothing; its `until` still counts from when it was made
        replace["trigger"] = json!({"kind": "session"});
        replace["repeat"] = json!("every");
        replace["until"] = laps(5);
        assert_eq!(
            call(&tool, replace).await.unwrap(),
            "Replaced #2: fires when the next session starts, every time, until after 5 laps from \
             when it was made; stale 0:30 of game clock after it fires. Its `until` still counts \
             from when it was first made."
        );
        assert_eq!(
            ids(&tool.take()),
            [("replace", RequestId(2)), ("replace", RequestId(2))]
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn session_type_is_one_the_simulators_give() {
        let (_dir, _engineer, tool) = recorded().await;
        let session = |kind: &str| add(json!({"kind": "session", "session_type": kind}));
        for wrong in ["quali", "Qualifying 1"] {
            let error = call(&tool, session(wrong)).await.unwrap_err();
            let error = format!("{error:#}");
            for kind in WRITTEN {
                assert!(error.contains(kind), "{error}");
            }
        }
        assert!(tool.take().is_empty());
        call(&tool, session("Race")).await.unwrap();
        call(&tool, session(" WARMUP ")).await.unwrap();
        let triggers: Vec<_> = tool
            .take()
            .into_iter()
            .map(|change| match change {
                Change::Add(_, new) => new.trigger,
                other => panic!("{other:?}"),
            })
            .collect();
        assert_eq!(
            triggers,
            [
                Trigger::Session {
                    kind: Some("race".into())
                },
                Trigger::Session {
                    kind: Some("warmup".into())
                }
            ]
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn repeating_time_triggers_have_a_minimum() {
        let (_dir, _engineer, tool) = recorded().await;
        let time = |seconds: f64| json!({"kind": "time", "seconds": seconds});
        let mut every = add(time(9.5));
        every["repeat"] = json!("every");
        let error = call(&tool, every.clone()).await.unwrap_err();
        assert!(format!("{error:#}").contains("10 seconds"), "{error:#}");
        assert!(tool.take().is_empty());
        every["trigger"] = time(10.0);
        call(&tool, every).await.unwrap();
        // A one-shot, an `until` and a `stale_after` only need more than 0
        call(&tool, add(time(1.0))).await.unwrap();
        let mut until = add(laps(1));
        until["repeat"] = json!("every");
        until["until"] = time(1.0);
        call(&tool, until).await.unwrap();
        let mut stale = add(laps(1));
        stale["repeat"] = json!("every");
        stale["stale_after"] = time(1.0);
        call(&tool, stale).await.unwrap();
        assert_eq!(tool.take().len(), 4);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn ids_and_counts_are_read_leniently() {
        let (_dir, _engineer, tool) = recorded().await;
        tool.begin(vec![RequestId(3)], RequestId(4));
        let replace = |id: serde_json::Value, count: serde_json::Value| {
            let mut replace = add(json!({"kind": "laps", "count": count}));
            replace["action"] = json!("replace");
            replace["id"] = id;
            replace
        };
        for id in [
            json!("# 3"),
            json!(" #3 "),
            json!("3"),
            json!(3),
            json!(3.0),
        ] {
            let reply = call(&tool, replace(id.clone(), json!(1.0))).await;
            assert!(reply.unwrap().starts_with("Replaced #3"), "{id}");
        }
        for id in [
            json!(3.5),
            json!(-3),
            json!("three"),
            json!("#"),
            json!("#3.5"),
            json!(true),
            json!([3]),
            json!(5_000_000_000_u64),
        ] {
            let error = call(&tool, replace(id.clone(), json!(1)))
                .await
                .unwrap_err();
            let error = format!("{error:#}");
            assert!(error.contains("like #3"), "{id}: {error}");
            assert!(!error.contains("variant"), "{id}: {error}");
        }
        for count in [json!(1.5), json!(-1), json!(0.0)] {
            let error = call(&tool, add(json!({"kind": "laps", "count": count})))
                .await
                .unwrap_err();
            assert!(
                format!("{error:#}").contains("`count`"),
                "{count}: {error:#}"
            );
        }
        let changes = tool.take();
        assert_eq!(changes.len(), 5);
        assert!(changes.iter().all(|change| matches!(
            change,
            Change::Replace(RequestId(3), new) if new.trigger == Trigger::Laps { count: 1 }
        )));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn rejects_empty_periods() {
        let (_dir, _engineer, tool) = recorded().await;
        let empty = [
            json!({"kind": "time", "seconds": 0}),
            json!({"kind": "time", "seconds": -5}),
            laps(0),
        ];
        for period in &empty {
            call(&tool, add(period.clone())).await.unwrap_err();
            let mut until = add(laps(1));
            until["repeat"] = json!("every");
            until["until"] = period.clone();
            call(&tool, until).await.unwrap_err();
            let mut stale = add(laps(1));
            stale["stale_after"] = period.clone();
            call(&tool, stale).await.unwrap_err();
        }
        // JSON can't carry them, but nothing else may either
        for seconds in [f64::INFINITY, f64::NEG_INFINITY, f64::NAN] {
            let input = ScheduleInput {
                action: Action::Add,
                id: None,
                instruction: Some("Tell the pilot the fuel left.".into()),
                trigger: Some(TriggerInput::Time { seconds }),
                repeat: None,
                until: None,
                stale_after: None,
            };
            ToolHandler::handle(&tool, input).await.unwrap_err();
        }
        assert!(tool.take().is_empty());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn rejects_malformed_input() {
        let (_dir, _engineer, tool) = recorded().await;
        // An unknown action, a missing instruction or trigger, an unknown trigger kind, a trigger
        // missing its field
        for input in [
            json!({"action": "pause", "id": "#1"}),
            json!({"action": "add", "trigger": laps(1)}),
            json!({"action": "add", "instruction": " ", "trigger": laps(1)}),
            json!({"action": "add", "instruction": "Fuel."}),
            add(json!({"kind": "sector", "count": 1})),
            add(json!({"kind": "time"})),
            add(json!({"kind": "condition"})),
        ] {
            call(&tool, input.clone()).await.unwrap_err();
        }
        assert!(tool.take().is_empty());

        // A session trigger, of any session or of one type
        call(&tool, add(json!({"kind": "session"}))).await.unwrap();
        call(
            &tool,
            add(json!({"kind": "session", "session_type": "race"})),
        )
        .await
        .unwrap();
        let changes = tool.take();
        let triggers: Vec<_> = changes
            .iter()
            .map(|change| match change {
                Change::Add(_, new) => new.trigger.clone(),
                other => panic!("{other:?}"),
            })
            .collect();
        assert_eq!(
            triggers,
            [
                Trigger::Session { kind: None },
                Trigger::Session {
                    kind: Some("race".into())
                }
            ]
        );
    }

    #[test]
    fn schema_names_every_action_and_trigger() {
        let schema = ScheduleTool::input_schema();
        assert_eq!(
            schema["properties"]["action"]["enum"],
            json!(["add", "cancel", "replace"])
        );
        for field in ["trigger", "until", "stale_after"] {
            assert_eq!(
                schema["properties"][field]["properties"]["kind"]["enum"],
                json!(["time", "laps", "session", "condition"]),
                "{field}"
            );
        }
        assert_eq!(schema["required"], json!(["action"]));
        // As the simulators write them
        for field in ["trigger", "until", "stale_after"] {
            assert_eq!(
                schema["properties"][field]["properties"]["session_type"]["enum"],
                json!(WRITTEN),
                "{field}"
            );
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn once_with_until_is_accepted_and_checks_until_condition() {
        let (_dir, _engineer, tool) = recorded().await;

        // 1. `add` with `repeat: "once"` + `until`; with `repeat` omitted, or an `until` before the
        // trigger, it's refused
        let mut explicit = add(condition(FALSE));
        explicit["repeat"] = json!("once");
        explicit["until"] = laps(5);
        let reply = call(&tool, explicit).await.unwrap();
        assert!(!reply.contains('\n'), "one line: {reply}");
        assert!(!reply.contains("Tell the pilot the fuel left."), "{reply}");
        assert!(reply.contains("Added #1"), "{reply}");
        assert!(reply.contains("once, until after 5 laps"), "{reply}");

        let mut omitted = add(condition(FALSE));
        omitted["until"] = laps(5);
        let error = format!("{:#}", call(&tool, omitted).await.unwrap_err());
        assert!(error.contains("`until` needs `repeat`"), "{error}");
        for (trigger, until) in [
            (laps(5), laps(1)),
            (
                json!({"kind": "time", "seconds": 600}),
                json!({"kind": "time", "seconds": 30}),
            ),
        ] {
            let mut never = add(trigger);
            never["repeat"] = json!("once");
            never["until"] = until;
            let error = format!("{:#}", call(&tool, never).await.unwrap_err());
            assert!(error.contains("before its `trigger` can fire"), "{error}");
        }

        let changes = tool.take();
        assert_eq!(changes.len(), 1);
        for change in changes {
            let Change::Add(_, new) = change else {
                panic!("{change:?}");
            };
            assert_eq!(new.repeat, Repeat::Once);
            assert_eq!(new.until, Some(Trigger::Laps { count: 5 }));
        }

        // 2. `add` with `repeat: "once"` + broken `until` condition fails clearly
        let mut broken = add(laps(1));
        broken["repeat"] = json!("once");
        broken["until"] = condition(BROKEN);
        let error = format!("{:#}", call(&tool, broken).await.unwrap_err());
        assert!(error.contains("the `until` condition failed"), "{error}");

        // 3. `add` with `repeat: "once"` + already-true `until` condition notes it
        let mut already_true = add(laps(1));
        already_true["repeat"] = json!("once");
        already_true["until"] = condition(TRUE);
        let reply = call(&tool, already_true).await.unwrap();
        assert!(reply.contains("already true"), "{reply}");
        let _ = tool.take();

        // 4. `replace` with `repeat: "once"` + `until` reports kept `until` baseline
        tool.begin(vec![RequestId(2)], RequestId(3));
        let mut replace = add(json!({"kind": "session"}));
        replace["action"] = json!("replace");
        replace["id"] = json!("#2");
        replace["repeat"] = json!("once");
        replace["until"] = laps(5);
        let reply = call(&tool, replace).await.unwrap();
        assert!(
            reply.contains("Its `until` still counts from when it was first made."),
            "{reply}"
        );
        assert!(!reply.contains("Tell the pilot the fuel left."), "{reply}");
    }
}
