//! The `schedule` tool: how the model adds, cancels and replaces the pilot's requests.
//!
//! The tool only stages changes for the turn holding the conversation. The engineer takes them
//! when the turn finishes and applies them to its [`Schedule`](super::Schedule), or drops them
//! with the turn.

use super::{
    Change, DEFAULT_STALE, MAX_REQUESTS, NewRequest, Repeat, RequestId, Trigger, describe,
    describe_stale,
};
use crate::{engineer::tool, provider::ToolHandler};
use anyhow::{Context as _, bail};
use serde::Deserialize;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

/// The tool, for one conversation. Cheap to clone; clones stage into the same turn.
// Inserted into the conversation's tools by the engineer, which a later task makes use it.
#[allow(dead_code)]
#[derive(Clone)]
pub struct ScheduleTool {
    engineer: tool::Engineer,
    /// Locked only to push a change or take them, never across a wait
    staging: Arc<Mutex<Staging>>,
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

// Inserted into the conversation's tools by the engineer, which a later task makes use it.
#[allow(dead_code)]
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
        }
    }

    pub fn description() -> String {
        "Schedules what the pilot asks you to do later: a check at the end of the lap, a number \
         every lap for a stint, a heads-up when something happens. When a request fires you get a \
         turn of your own, with fresh telemetry, to answer it. `action` picks the change: add a \
         request, cancel one, or replace one to change it. Changes take effect once your reply is \
         finished. Every question lists the pending requests and their ids."
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
                    "description": "add, replace: what to do when it fires, complete in itself for a conversation that never heard it asked: who asked, when, and what for."
                },
                "trigger": trigger_schema(
                    "add, replace: when it fires. time and laps count from now (and, when it \
                     repeats, from its last firing); session fires when the next session starts; \
                     condition fires when its query starts returning a row (one already true \
                     waits until it turns false and true again)."
                ),
                "repeat": {
                    "type": "string",
                    "enum": ["once", "every"],
                    "description": "add, replace: answered once (the default), or every time it fires."
                },
                "until": trigger_schema(
                    "add, replace, with repeat every: when it stops, counted from when it was \
                     made. A firing on the same check still counts. Without it, it repeats until \
                     cancelled."
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
            bail!("this turn is over, so nothing was changed");
        }
        stage(&mut staging)
    }

    async fn run(&self, input: ScheduleInput) -> anyhow::Result<String> {
        let turn = self.lock().turn;
        let replacing = match input.action {
            Action::Cancel => {
                let id = input.id()?;
                self.stage(turn, |staging| {
                    staging.find(id)?;
                    staging.pending.retain(|pending| *pending != id);
                    staging.changes.push(Change::Cancel(id));
                    Ok(())
                })?;
                return Ok(format!(
                    "Cancelled {id}. Confirm it to the pilot in one sentence."
                ));
            }
            Action::Add => None,
            Action::Replace => Some(input.id()?),
        };
        let (instruction, trigger, repeat, stale_after) = input.request()?;
        let Some(made) = self.engineer.progress() else {
            bail!("no telemetry yet: requests can be made once the simulator is recording");
        };
        let new = NewRequest {
            instruction,
            trigger,
            repeat,
            stale_after,
            made,
        };
        // Outside the lock: the data thread may take a while
        let notes = self.check_conditions(&new).await?;
        let summary = summary(&new);
        let id = self.stage(turn, |staging| match replacing {
            Some(id) => {
                staging.find(id)?;
                staging.changes.push(Change::Replace(id, new));
                Ok(id)
            }
            None => {
                if staging.pending.len() >= MAX_REQUESTS {
                    bail!(
                        "the cap of {MAX_REQUESTS} pending requests is reached, so this one \
                         wasn't added: ask the pilot which to cancel"
                    );
                }
                let id = staging.next_id;
                staging.next_id = RequestId(id.0 + 1);
                staging.pending.push(id);
                staging.changes.push(Change::Add(id, new));
                Ok(id)
            }
        })?;
        Ok(match replacing {
            Some(_) => format!(
                "Replaced {id}: {summary}. What it has counted carries on: its next firing from \
                 its last, its `until` from when it was first made.{notes} Confirm the change to \
                 the pilot in one sentence."
            ),
            None => {
                format!("Added {id}: {summary}.{notes} Confirm it to the pilot in one sentence.")
            }
        })
    }

    /// Checks each of the request's conditions once now. Returns a note for the model about those
    /// already true.
    async fn check_conditions(&self, new: &NewRequest) -> anyhow::Result<String> {
        let until = match &new.repeat {
            Repeat::Every { until } => until.as_ref(),
            Repeat::Once => None,
        };
        let mut notes = String::new();
        for (field, trigger, note) in [
            (
                "trigger",
                Some(&new.trigger),
                "it fires only once that turns false and then true again",
            ),
            (
                "until",
                until,
                "it ends only once that turns false and then true again",
            ),
            (
                "stale_after",
                new.stale_after.as_ref(),
                "while it stays true, its answers are out of date and skipped",
            ),
        ] {
            let Some(Trigger::Condition { query }) = trigger else {
                continue;
            };
            let holds = self
                .engineer
                .check_condition(query.clone())
                .await
                .with_context(|| {
                    format!("the `{field}` condition failed, so nothing was changed")
                })?;
            if holds {
                notes.push_str(&format!(
                    " Its `{field}` condition is already true: {note}."
                ));
            }
        }
        Ok(notes)
    }
}

impl Staging {
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
        async move { tool.run(input).await }
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
                "description": "time: seconds of game clock, more than 0."
            },
            "count": {
                "type": "integer",
                "description": "laps: laps the pilot completes, 1 or more."
            },
            "session_type": {
                "type": "string",
                "description": "session: only a session of this type, as race_state.session_type gives it (practice, qualifying, warmup or race). Any session if left out."
            },
            "query": {
                "type": "string",
                "description": "condition: one SELECT or WITH statement over the tables `sql` reads, true when it returns a row."
            }
        },
        "required": ["kind"]
    })
}

/// A request in one line, for the model to confirm
fn summary(new: &NewRequest) -> String {
    let repeat = match &new.repeat {
        Repeat::Once => "once".to_string(),
        Repeat::Every { until: None } => "every time, until cancelled".to_string(),
        Repeat::Every { until: Some(until) } => format!(
            "every time, until {} from when it was made",
            describe(until)
        ),
    };
    format!(
        "\"{}\" It fires {}, {repeat}; its answer is out of date {}",
        new.instruction,
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
    #[serde(default)]
    id: Option<IdInput>,
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

/// `"#3"` as the schema has it, or `"3"` or `3`, which a model may well send
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
enum IdInput {
    Number(u32),
    Text(String),
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
    Laps {
        count: u32,
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
    fn id(&self) -> anyhow::Result<RequestId> {
        match &self.id {
            None => bail!("`id` is needed: the pending request's id, like #3"),
            Some(IdInput::Number(id)) => Ok(RequestId(*id)),
            Some(IdInput::Text(text)) => {
                let digits = text.trim();
                let digits = digits.strip_prefix('#').unwrap_or(digits);
                match digits.parse() {
                    Ok(id) => Ok(RequestId(id)),
                    Err(_) => bail!("`id` is a request's id, like #3, not {text:?}"),
                }
            }
        }
    }

    /// The request to add or replace with: its instruction, trigger, repeat and `stale_after`
    fn request(self) -> anyhow::Result<(String, Trigger, Repeat, Option<Trigger>)> {
        let instruction = self.instruction.as_deref().map(str::trim).unwrap_or("");
        if instruction.is_empty() {
            bail!("`instruction` is needed: what to do when it fires");
        }
        let Some(trigger) = self.trigger else {
            bail!("`trigger` is needed: when it fires");
        };
        let trigger = trigger.parse("trigger")?;
        let until = self.until.map(|until| until.parse("until")).transpose()?;
        let repeat = match (self.repeat.unwrap_or(RepeatInput::Once), until) {
            (RepeatInput::Every, until) => Repeat::Every { until },
            (RepeatInput::Once, None) => Repeat::Once,
            (RepeatInput::Once, Some(_)) => {
                bail!("`until` ends a repeating request: set `repeat` to every, or leave it out")
            }
        };
        let stale_after = self
            .stale_after
            .map(|stale| stale.parse("stale_after"))
            .transpose()?;
        Ok((instruction.to_owned(), trigger, repeat, stale_after))
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
                if count == 0 {
                    bail!("`{field}`: `count` must be at least 1 lap");
                }
                Trigger::Laps { count }
            }
            TriggerInput::Session { session_type } => Trigger::Session {
                kind: session_type
                    .map(|kind| kind.trim().to_owned())
                    .filter(|kind| !kind.is_empty()),
            },
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
            telemetry.lap = Some(laps).into();
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
        assert!(reply.contains("Tell the pilot the fuel left."), "{reply}");
        assert!(!reply.contains('\n'), "one line: {reply}");
        assert_eq!(
            reply,
            "Added #1: \"Tell the pilot the fuel left.\" It fires after 1 lap, every time, until \
             after 5 laps from when it was made; its answer is out of date 0:20 of game clock \
             after it fires. Confirm it to the pilot in one sentence."
        );

        let changes = tool.take();
        let [Change::Add(id, new)] = changes.as_slice() else {
            panic!("{changes:?}");
        };
        assert_eq!(*id, RequestId(1));
        assert_eq!(new.instruction, "Tell the pilot the fuel left.");
        assert_eq!(new.trigger, Trigger::Laps { count: 1 });
        assert_eq!(
            new.repeat,
            Repeat::Every {
                until: Some(Trigger::Laps { count: 5 })
            }
        );
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
        let error = call(&tool, add(condition(BROKEN))).await.unwrap_err();
        assert!(format!("{error:#}").contains("no such table"), "{error:#}");
        let mut until = add(laps(1));
        until["repeat"] = json!("every");
        until["until"] = condition(BROKEN);
        call(&tool, until).await.unwrap_err();
        let mut stale = add(laps(1));
        stale["stale_after"] = condition(BROKEN);
        call(&tool, stale).await.unwrap_err();
        assert!(tool.take().is_empty());

        // One that's false now is just added
        let reply = call(&tool, add(condition(FALSE))).await.unwrap();
        assert!(!reply.contains("already true"), "{reply}");
        // One that's true now waits for it to turn false and true again, and the reply says so
        let reply = call(&tool, add(condition(TRUE))).await.unwrap();
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
        call(&tool, replace).await.unwrap_err();
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
        let tool = ScheduleTool::new(engineer.clone());
        tool.begin(vec![RequestId(1)], RequestId(2));
        let error = call(&tool, add(laps(1))).await.unwrap_err();
        assert!(
            format!("{error:#}").contains("no telemetry yet"),
            "{error:#}"
        );
        let mut replace = add(laps(1));
        replace["action"] = json!("replace");
        replace["id"] = json!("#1");
        let error = call(&tool, replace).await.unwrap_err();
        assert!(
            format!("{error:#}").contains("no telemetry yet"),
            "{error:#}"
        );
        assert!(tool.take().is_empty());
        assert!(engineer.progress().is_none());
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
        // `until` ends a repeating request only
        let mut once = add(laps(1));
        once["until"] = laps(5);
        let error = call(&tool, once.clone()).await.unwrap_err();
        assert!(format!("{error:#}").contains("every"), "{error:#}");
        once["repeat"] = json!("once");
        call(&tool, once).await.unwrap_err();
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
    }
}
