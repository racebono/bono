//! The pilot's scheduled requests, and the rules for when they fire, end and go stale.
//!
//! The rules are a plain function of the schedule, the data thread's latest [`Progress`] and the
//! wall clock: nothing here waits, reads the database or talks to the model.

use super::tool::{ConditionId, ConditionState, Progress};
use std::{
    fmt,
    time::{Duration, Instant},
};

pub mod tool;

/// How long a scheduled turn waits after the first request falls due, to take in requests that
/// fire together but are noticed a few checks apart
pub const GATHER: Duration = Duration::from_secs(1);

/// Failed checks in a row that drop a condition's request
pub const CONDITION_FAILURES: u32 = 5;

/// When an answer stops being worth saying, for a request made without its own
pub const DEFAULT_STALE: Trigger = Trigger::Time { seconds: 30.0 };

/// How long a failed scheduled turn waits before its requests are tried again
pub const RETRY_AFTER: Duration = Duration::from_secs(10);

/// What a scheduled turn replies, anywhere in its reply, when it has nothing worth saying
pub const SILENCE: &str = "[SILENCE]";

/// The most requests that may be pending, a guard against a model stuck adding them
pub const MAX_REQUESTS: usize = 50;

/// The game clock is a sum of float differences, so a time target counts as reached this close
/// to it
const TOLERANCE_S: f64 = 1e-6;

/// The epoch reasons after which an answer from before is out of date (`resumed` isn't one: the
/// same session carries on)
const BOUNDARIES: [&str; 3] = ["session", "clock_rollback", "replay"];

/// What the model and the pilot call a request by, shown as `#3`
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct RequestId(pub u32);

impl fmt::Display for RequestId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "#{}", self.0)
    }
}

/// When a request fires, ends or goes stale, counted from a baseline
#[derive(Clone, Debug, PartialEq)]
pub enum Trigger {
    /// Seconds of game clock
    Time { seconds: f64 },
    /// Laps the pilot completes
    Laps { count: u32 },
    /// The next session starting, of this type if given
    Session { kind: Option<String> },
    /// An SQL statement over the engineer tool's tables, true when it returns a row: firing and
    /// ending need it to turn true, staleness lasts while it is
    Condition { query: String },
}

/// Whether a request is answered once or again and again
#[derive(Clone, Debug, PartialEq)]
pub enum Repeat {
    /// Answered, then removed
    Once,
    /// Answered, then armed again, until this trigger fires (counted from when the request was
    /// made)
    Every { until: Option<Trigger> },
}

/// A request as the model asks for it
#[derive(Clone, Debug)]
pub struct NewRequest {
    /// Self-contained: it makes sense to a conversation that never heard it asked
    pub instruction: String,
    pub trigger: Trigger,
    pub repeat: Repeat,
    /// When its answer stops being worth saying, counted from its firing ([`DEFAULT_STALE`] if
    /// not given)
    pub stale_after: Option<Trigger>,
    /// The progress when it was asked for: the baseline of its trigger and its `until`
    pub made: Progress,
}

/// A change a turn makes to the schedule, applied once the turn finishes
#[derive(Clone, Debug)]
pub enum Change {
    /// With the id the tool gave it from [`Schedule::next_id`], which the model already confirmed
    Add(RequestId, NewRequest),
    Cancel(RequestId),
    /// Keeps the old request's `made` and, while it's armed, its armed baseline
    Replace(RequestId, NewRequest),
}

/// Where a request fired, which its answer carries to be checked for staleness
#[derive(Clone, Debug)]
pub struct Fired {
    pub id: RequestId,
    pub at: Progress,
}

/// The due requests one scheduled turn answers
#[derive(Clone, Debug)]
pub struct Batch {
    /// Each request's id, instruction, when it was made and why it fired now, as text for the
    /// model
    pub requests: Vec<(RequestId, String, String, String)>,
    pub fired: Vec<Fired>,
}

/// What [`Schedule::apply`] did with a turn's changes
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Applied {
    /// The requests added or replaced
    pub ids: Vec<RequestId>,
    /// The cancels' and replaces' requests that were gone by the time the turn finished (they
    /// ended, were dropped or went stale while it ran), so their change didn't apply
    pub missing: Vec<RequestId>,
    /// The adds made in another weekend than the one the turn finished in (the recording was
    /// cleared while it ran), so they weren't kept
    pub dropped: Vec<RequestId>,
}

/// What a step of the schedule did that the engineer tells the pilot about
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Advance {
    /// A new weekend cleared the requests
    pub cleared: bool,
    /// The requests dropped because a condition kept failing, with their instructions
    pub dropped: Vec<(RequestId, String)>,
    /// How many due requests went stale before a scheduled turn answered them
    pub skipped_stale: usize,
}

/// The pilot's requests, and every rule about when they fire, end and go stale
#[derive(Debug)]
pub struct Schedule {
    /// In the order they were made
    requests: Vec<Request>,
    /// The next request's id: ids are never reused, so an answer can't be taken for another
    /// request's
    next_id: u32,
    /// The next condition's id: never reused, so a result for a removed condition can't be taken
    /// for a new one's
    next_condition: ConditionId,
    /// The weekend of the last progress seen
    weekend: Option<Option<i64>>,
    /// The epoch of the last progress stepped through
    epoch: Option<Option<i64>>,
    /// The epoch current when the last boundary was seen: whatever fired in an earlier one is out
    /// of date
    boundary: Option<i64>,
    /// The epoch that started a session whose type no snapshot has given yet: an epoch can be
    /// written before its session's first scoring row, so a typed session trigger waits for it
    session_start: Option<i64>,
    /// The last session start seen, for a session trigger made before it but applied after it
    last_start: Option<SessionStart>,
    /// A weekend change seen by `apply`, for the next `advance` to report
    cleared: bool,
    /// A batch was taken and hasn't been answered, dropped or failed
    batch_out: bool,
    /// No batch is taken before this, after a failed one
    retry_at: Option<Instant>,
}

/// A session start `advance` has seen
#[derive(Debug)]
struct SessionStart {
    epoch: i64,
    /// Its type, once a snapshot of its epoch gives it
    kind: Option<String>,
}

#[derive(Debug)]
struct Request {
    id: RequestId,
    instruction: String,
    made: Progress,
    trigger: Trigger,
    repeat: Repeat,
    stale_after: Trigger,
    /// The conditions of its trigger, its `until` and its `stale_after`, where those are
    /// conditions
    trigger_slot: Option<Slot>,
    until_slot: Option<Slot>,
    stale_slot: Option<Slot>,
    state: State,
    /// Its `until` fired while it was due: it goes once this firing is answered
    ending: bool,
    /// Where it fired, for each of its answers written that hasn't played or been dropped yet
    waiting: Vec<Progress>,
    /// The epoch of a session start its session trigger missed: it was made before the start but
    /// applied after `advance` had seen it, so it's checked against that start on the next step
    missed_start: Option<i64>,
}

#[derive(Debug)]
enum State {
    /// Waiting for its trigger, counted from `since`
    Armed { since: Progress },
    /// Fired at `fired`, having counted from `since`, waiting for a scheduled turn
    Due {
        since: Progress,
        fired: Progress,
        at: Instant,
    },
    /// Fired, in the scheduled turn that's out
    InBatch {
        since: Progress,
        fired: Progress,
        at: Instant,
    },
    /// It won't fire again, and some of its answers are still waiting to play or be dropped: it
    /// goes once they're resolved
    Ended,
}

/// What a snapshot says about sessions starting, for session triggers
#[derive(Clone, Copy, Debug, Default)]
struct Starts {
    /// A session started since the last snapshot
    any: bool,
    /// This snapshot is the first to give the type of the session that started in its epoch
    typed: bool,
}

/// A condition the data thread checks for a request
#[derive(Debug)]
struct Slot {
    id: ConditionId,
    query: String,
    /// The last result, the starting state a change is measured against: none yet after it was
    /// registered or after an epoch change
    last: Option<bool>,
    /// Failed checks in a row
    failures: u32,
    /// It turned from false to true on the last step
    rose: bool,
}

impl Slot {
    fn observe(&mut self, progress: &Progress, epoch_changed: bool) {
        if epoch_changed {
            self.last = None;
        }
        self.rose = false;
        // Not checked for this snapshot (not yet, or not reached within the tick's budget): no change,
        // and an older failure isn't counted again
        if !progress.checked.contains(&self.id) {
            return;
        }
        match progress.conditions.get(&self.id) {
            None => {}
            Some(ConditionState::Failed) => self.failures += 1,
            Some(state) => {
                let now = *state == ConditionState::True;
                self.rose = self.last == Some(false) && now;
                self.last = Some(now);
                self.failures = 0;
            }
        }
    }
}

impl Request {
    fn slots(&self) -> impl Iterator<Item = &Slot> {
        [&self.trigger_slot, &self.until_slot, &self.stale_slot]
            .into_iter()
            .flatten()
    }

    fn slots_mut(&mut self) -> impl Iterator<Item = &mut Slot> {
        [
            &mut self.trigger_slot,
            &mut self.until_slot,
            &mut self.stale_slot,
        ]
        .into_iter()
        .flatten()
    }

    /// Goes once its current firing is answered
    fn last_firing(&self) -> bool {
        self.ending || self.repeat == Repeat::Once
    }

    fn pending(&self) -> bool {
        !matches!(self.state, State::Ended)
    }

    /// It won't fire again. Returns whether it stays, until its waiting answers are resolved.
    fn end(&mut self) -> bool {
        self.state = State::Ended;
        !self.waiting.is_empty()
    }

    /// Where counting starts again after its firing at `fired`, having counted from `since`. A
    /// timed one counts on from its target, so a firing noticed a little late (by up to a tick)
    /// doesn't push the next one back; but from the firing if it was noticed more than half a
    /// period late (an outage the clock jumped over), so the next one doesn't follow straight
    /// after. Laps are whole, so they're never late.
    fn rearm_from(&self, since: &Progress, fired: &Progress) -> Progress {
        let mut base = fired.clone();
        if let Trigger::Time { seconds } = self.trigger {
            let target = since.clock_s + seconds;
            if fired.clock_s - target <= seconds / 2.0 {
                base.clock_s = target;
            }
        }
        base
    }

    fn until(&self) -> Option<&Trigger> {
        match &self.repeat {
            Repeat::Every { until } => until.as_ref(),
            Repeat::Once => None,
        }
    }

    /// Whether an answer for its firing at `fired` is out of date at `progress`
    fn stale(&self, fired: &Progress, progress: &Progress, boundary: Option<i64>) -> bool {
        let past_boundary = match (fired.epoch, boundary) {
            (Some(fired), Some(boundary)) => fired < boundary,
            (None, Some(_)) => true,
            (_, None) => false,
        };
        past_boundary
            || match &self.stale_after {
                Trigger::Time { .. } | Trigger::Laps { .. } => {
                    reached(&self.stale_after, None, fired, progress, Starts::default())
                }
                // Any session starting is a boundary, so the rule above covers it
                Trigger::Session { .. } => false,
                // Stale while true; a failed or missing check isn't
                Trigger::Condition { .. } => self.stale_slot.as_ref().is_some_and(|slot| {
                    progress.conditions.get(&slot.id) == Some(&ConditionState::True)
                }),
            }
    }
}

/// Whether `trigger`, counted from `base`, fires at `progress`
fn reached(
    trigger: &Trigger,
    slot: Option<&Slot>,
    base: &Progress,
    progress: &Progress,
    starts: Starts,
) -> bool {
    match trigger {
        Trigger::Time { seconds } => progress.clock_s >= base.clock_s + seconds - TOLERANCE_S,
        Trigger::Laps { count } => progress.laps >= base.laps.saturating_add(u64::from(*count)),
        // A session that started after it was armed: one that started in the epoch it was armed in
        // had already begun
        Trigger::Session { kind: None } => starts.any && base.epoch != progress.epoch,
        // On the first snapshot that says what the session is, if it started after the request was
        // armed: one that started in the epoch it was armed in had already begun
        Trigger::Session { kind: Some(kind) } => {
            starts.typed
                && base.epoch != progress.epoch
                && progress
                    .session_type
                    .as_ref()
                    .is_some_and(|now| now.eq_ignore_ascii_case(kind))
        }
        Trigger::Condition { .. } => slot.is_some_and(|slot| slot.rose),
    }
}

// Owned by the engineer, which a later task makes use it.
#[allow(dead_code)]
impl Schedule {
    pub fn new() -> Self {
        Self {
            requests: Vec::new(),
            next_id: 1,
            next_condition: 1,
            weekend: None,
            epoch: None,
            boundary: None,
            session_start: None,
            last_start: None,
            cleared: false,
            batch_out: false,
            retry_at: None,
        }
    }

    /// Applies a finished turn's changes, at `progress`. Adds made in another weekend than
    /// `progress`'s are dropped, and reported as such; cancels and replaces of requests that
    /// aren't there anymore change nothing, and are reported as missing.
    ///
    /// A session trigger made before a session start that `advance` saw while the turn ran fires
    /// for that start on the next `advance` (if it's of the type asked for).
    pub fn apply(&mut self, changes: Vec<Change>, progress: &Progress) -> Applied {
        self.follow_weekend(progress.weekend);
        let mut ids = Vec::new();
        let mut missing = Vec::new();
        let mut dropped = Vec::new();
        for change in changes {
            match change {
                Change::Add(id, new) => {
                    if new.made.weekend != progress.weekend {
                        dropped.push(id);
                        continue;
                    }
                    if self.requests.iter().any(|r| r.id == id) {
                        continue;
                    }
                    self.next_id = self.next_id.max(id.0 + 1);
                    let since = new.made.clone();
                    let missed = self.missed_start(&new.trigger, &new.made, &since);
                    let mut request = self.request(id, new, since);
                    request.missed_start = missed;
                    self.requests.push(request);
                    ids.push(id);
                }
                // A change to an add this turn made and that was dropped goes with it
                Change::Cancel(id) | Change::Replace(id, _) if dropped.contains(&id) => {}
                Change::Cancel(id) => {
                    let before = self.requests.len();
                    self.requests.retain(|r| r.id != id);
                    if self.requests.len() == before {
                        missing.push(id);
                    }
                }
                Change::Replace(id, new) => {
                    let Some(index) = self.requests.iter().position(|r| r.id == id) else {
                        missing.push(id);
                        continue;
                    };
                    let old = self.requests.remove(index);
                    // Counting carries on from the last firing, if it has fired since it was armed
                    let since = match old.state {
                        State::Armed { since } => since,
                        State::Due { fired, .. } | State::InBatch { fired, .. } => fired,
                        State::Ended => old
                            .waiting
                            .last()
                            .cloned()
                            .unwrap_or_else(|| old.made.clone()),
                    };
                    // A start the old one missed and hasn't fired for yet still counts for a
                    // session trigger that replaces it
                    let missed =
                        self.missed_start(&new.trigger, &new.made, &since)
                            .or(match new.trigger {
                                Trigger::Session { .. } => old.missed_start,
                                _ => None,
                            });
                    let mut request = self.request(id, new, since);
                    request.made = old.made;
                    request.waiting = old.waiting;
                    request.missed_start = missed;
                    self.requests.insert(index, request);
                    ids.push(id);
                }
            }
        }
        Applied {
            ids,
            missing,
            dropped,
        }
    }

    /// Steps through a new progress snapshot: what fell due, what ended, what went stale, and
    /// whether a new weekend cleared everything
    pub fn advance(&mut self, progress: &Progress, now: Instant) -> Advance {
        let mut advance = Advance {
            cleared: self.follow_weekend(progress.weekend) || std::mem::take(&mut self.cleared),
            ..Default::default()
        };

        let epoch_changed = !progress.new_epochs.is_empty()
            || self.epoch.is_some_and(|epoch| epoch != progress.epoch);
        self.epoch = Some(progress.epoch);
        if progress
            .new_epochs
            .iter()
            .any(|reason| BOUNDARIES.contains(&reason.as_str()))
            && let Some(epoch) = progress.epoch
        {
            self.boundary = Some(epoch);
        }
        let session = progress.new_epochs.iter().any(|reason| reason == "session");
        if session {
            self.session_start = progress.epoch;
        } else if epoch_changed {
            self.session_start = None;
        }
        let starts = Starts {
            any: session,
            typed: self.session_start.is_some()
                && self.session_start == progress.epoch
                && progress.session_type.is_some(),
        };
        if starts.typed {
            self.session_start = None;
        }
        if session && let Some(epoch) = progress.epoch {
            self.last_start = Some(SessionStart { epoch, kind: None });
        }
        if let Some(start) = &mut self.last_start
            && start.kind.is_none()
            && progress.epoch == Some(start.epoch)
        {
            start.kind = progress.session_type.clone();
        }

        let mut kept = Vec::with_capacity(self.requests.len());
        for mut request in std::mem::take(&mut self.requests) {
            // Its answers on their way: nothing more happens to it
            if !request.pending() {
                kept.push(request);
                continue;
            }
            for slot in request.slots_mut() {
                slot.observe(progress, epoch_changed);
            }
            if request
                .slots()
                .any(|slot| slot.failures >= CONDITION_FAILURES)
            {
                advance.dropped.push((request.id, request.instruction));
                continue;
            }

            let until = request.until().is_some_and(|until| {
                reached(
                    until,
                    request.until_slot.as_ref(),
                    &request.made,
                    progress,
                    starts,
                )
            });
            if !matches!(request.state, State::Armed { .. }) {
                request.ending |= until;
            }
            let mut missed = matches!(request.state, State::Armed { .. })
                && self.fires_for_missed_start(&mut request, progress);
            // A due request gone stale is armed again and checked once more on this snapshot, so
            // a trigger that lasts one snapshot (a session starting) isn't lost
            let mut ended = false;
            for _ in 0..2 {
                if let State::Armed { since } = &request.state {
                    let fires = std::mem::take(&mut missed)
                        || reached(
                            &request.trigger,
                            request.trigger_slot.as_ref(),
                            since,
                            progress,
                            starts,
                        );
                    if !fires {
                        ended = until;
                        break;
                    }
                    // If it ends on the same step, this firing still counts
                    request.ending = until;
                    let since = since.clone();
                    request.state = State::Due {
                        since,
                        fired: progress.clone(),
                        at: now,
                    };
                }
                let State::Due { since, fired, .. } = &request.state else {
                    break;
                };
                if !request.stale(fired, progress, self.boundary) {
                    break;
                }
                advance.skipped_stale += 1;
                if request.last_firing() {
                    ended = true;
                    break;
                }
                // Fired on this very snapshot: checking again would fire it again, just as stale
                let fired_now = fired.seq == progress.seq;
                let since = request.rearm_from(since, fired);
                request.state = State::Armed { since };
                if fired_now {
                    break;
                }
            }
            if ended && !request.end() {
                continue;
            }
            kept.push(request);
        }
        self.requests = kept;
        advance
    }

    /// The due requests for a scheduled turn, once `GATHER` has passed since the first fell due,
    /// with no batch out and no retry pending
    pub fn take_batch(&mut self, now: Instant) -> Option<Batch> {
        if self.batch_out || self.retry_at.is_some_and(|retry| now < retry) {
            return None;
        }
        let first = self
            .requests
            .iter()
            .filter_map(|r| match r.state {
                State::Due { at, .. } => Some(at),
                _ => None,
            })
            .min()?;
        if now.saturating_duration_since(first) < GATHER {
            return None;
        }

        let mut batch = Batch {
            requests: Vec::new(),
            fired: Vec::new(),
        };
        for request in &mut self.requests {
            let State::Due { since, fired, at } = &request.state else {
                continue;
            };
            batch.requests.push((
                request.id,
                request.instruction.clone(),
                made_text(&request.made, fired),
                why_text(request, fired),
            ));
            batch.fired.push(Fired {
                id: request.id,
                at: fired.clone(),
            });
            request.state = State::InBatch {
                since: since.clone(),
                fired: fired.clone(),
                at: *at,
            };
        }
        self.batch_out = true;
        Some(batch)
    }

    /// The scheduled turn was dropped: its requests stay due
    pub fn batch_dropped(&mut self) {
        self.return_batch();
    }

    /// The scheduled turn failed: its requests stay due, and no batch is taken before `now +
    /// RETRY_AFTER`
    pub fn batch_failed(&mut self, now: Instant) {
        self.return_batch();
        self.retry_at = Some(now + RETRY_AFTER);
    }

    /// The scheduled turn answered `fired`, at `progress`: repeating requests are armed again from
    /// their firing (a timed one from its target), so a slow turn doesn't push the next one back,
    /// and the others end. Either way each waits for its answer to be resolved.
    pub fn answered(&mut self, fired: &[Fired], _progress: &Progress) {
        for request in &mut self.requests {
            let State::InBatch {
                since, fired: at, ..
            } = &request.state
            else {
                continue;
            };
            if !fired.iter().any(|f| f.id == request.id && same(&f.at, at)) {
                continue;
            }
            let at = at.clone();
            let next = if request.last_firing() {
                State::Ended
            } else {
                State::Armed {
                    since: request.rearm_from(since, &at),
                }
            };
            request.state = next;
            request.waiting.push(at);
        }
        // Anything else in the batch wasn't answered after all
        self.return_batch();
    }

    /// The answer to `fired` played or was dropped: the requests that ended go once none of their
    /// answers is waiting
    pub fn resolved(&mut self, fired: &[Fired]) {
        for request in &mut self.requests {
            let id = request.id;
            request
                .waiting
                .retain(|at| !fired.iter().any(|f| f.id == id && same(&f.at, at)));
        }
        self.requests
            .retain(|request| request.pending() || !request.waiting.is_empty());
    }

    /// Whether an answer to `fired` is out of date at `progress`: once the first of its requests
    /// is, or once one of them is gone.
    ///
    /// A request that ends (answered for the last time, or its `until` reached) stays until its
    /// waiting answers are resolved, so ending doesn't make them stale. One that's cancelled,
    /// cleared, or dropped because its condition kept failing is gone at once, and its waiting
    /// answers are stale with it: the pilot asked for it to stop, or it can't be checked anymore.
    pub fn is_stale(&self, fired: &[Fired], progress: &Progress) -> bool {
        fired.iter().any(|f| {
            self.requests
                .iter()
                .find(|r| r.id == f.id)
                .is_none_or(|r| r.stale(&f.at, progress, self.boundary))
        })
    }

    /// Drops every request, and the batch out with them
    pub fn clear(&mut self) {
        self.requests.clear();
        self.batch_out = false;
        self.retry_at = None;
        self.cleared = false;
        self.last_start = None;
    }

    /// Every condition the data thread should check, by id
    pub fn conditions(&self) -> Vec<(ConditionId, String)> {
        self.requests
            .iter()
            .flat_map(Request::slots)
            .map(|slot| (slot.id, slot.query.clone()))
            .collect()
    }

    /// The pending requests, for the model: id, instruction, trigger, repeat and how far along
    /// they are at `progress`
    pub fn pending_text(&self, progress: &Progress) -> String {
        let pending: Vec<_> = self.requests.iter().filter(|r| r.pending()).collect();
        if pending.is_empty() {
            return "Scheduled requests: none.".into();
        }
        let mut text = String::from("Scheduled requests:");
        for request in pending {
            let fires = match &request.state {
                State::Armed { since } => with_count(&request.trigger, since, progress),
                _ => format!(
                    "{} (fired, waiting to be answered)",
                    describe(&request.trigger)
                ),
            };
            let repeat = match &request.repeat {
                Repeat::Once => "once".to_string(),
                Repeat::Every { until: None } => "every time".to_string(),
                Repeat::Every { until: Some(until) } => format!(
                    "every time, until {} from when it was made",
                    with_count(until, &request.made, progress)
                ),
            };
            text.push_str(&format!(
                "\n- {}: {}\n  Fires {fires}, {repeat}. Its answer is out of date {}.",
                request.id,
                request.instruction,
                describe_stale(&request.stale_after),
            ));
        }
        text
    }

    /// The id the next request added will have. The tool numbers a turn's adds from it, so the ids
    /// the model confirms are the ones applied: a dropped turn's ids are given out again
    pub fn next_id(&self) -> RequestId {
        RequestId(self.next_id)
    }

    /// Pending requests, for the cap
    pub fn len(&self) -> usize {
        self.requests.iter().filter(|r| r.pending()).count()
    }

    /// The pending requests' ids, the ones [`len`](Self::len) counts, in the order they were made
    pub fn pending_ids(&self) -> Vec<RequestId> {
        self.requests
            .iter()
            .filter(|r| r.pending())
            .map(|r| r.id)
            .collect()
    }

    /// How many of request `id`'s answers are waiting to play or be dropped
    #[cfg(test)]
    pub fn waiting(&self, id: RequestId) -> usize {
        (self.requests.iter())
            .find(|r| r.id == id)
            .map_or(0, |r| r.waiting.len())
    }

    fn request(&mut self, id: RequestId, new: NewRequest, since: Progress) -> Request {
        let stale_after = new.stale_after.unwrap_or(DEFAULT_STALE);
        let until = match &new.repeat {
            Repeat::Every { until: Some(until) } => self.slot(until),
            _ => None,
        };
        Request {
            id,
            instruction: new.instruction,
            trigger_slot: self.slot(&new.trigger),
            until_slot: until,
            stale_slot: self.slot(&stale_after),
            made: new.made,
            trigger: new.trigger,
            repeat: new.repeat,
            stale_after,
            state: State::Armed { since },
            ending: false,
            waiting: Vec::new(),
            missed_start: None,
        }
    }

    /// The epoch of the last session start, if a session trigger made at `made`, counting from
    /// `since`, came too late for it: both are from before it
    fn missed_start(&self, trigger: &Trigger, made: &Progress, since: &Progress) -> Option<i64> {
        let Trigger::Session { .. } = trigger else {
            return None;
        };
        let start = self.last_start.as_ref()?.epoch;
        let before = |progress: &Progress| progress.epoch.is_none_or(|epoch| epoch < start);
        (before(made) && before(since)).then_some(start)
    }

    /// Whether an armed request fires now for the session start it missed. A typed one waits for
    /// that session's type while the session lasts; once it's settled either way, it's forgotten.
    fn fires_for_missed_start(&self, request: &mut Request, progress: &Progress) -> bool {
        let Some(epoch) = request.missed_start else {
            return false;
        };
        // A later start is one `advance` sees in time
        let start = self
            .last_start
            .as_ref()
            .filter(|start| start.epoch == epoch);
        let fires = match (&request.trigger, start) {
            (Trigger::Session { kind: None }, Some(_)) => true,
            (Trigger::Session { kind: Some(kind) }, Some(start)) => match &start.kind {
                Some(now) => now.eq_ignore_ascii_case(kind),
                None if progress.epoch == Some(epoch) => return false,
                None => false,
            },
            _ => false,
        };
        request.missed_start = None;
        fires
    }

    fn slot(&mut self, trigger: &Trigger) -> Option<Slot> {
        let Trigger::Condition { query } = trigger else {
            return None;
        };
        let id = self.next_condition;
        self.next_condition += 1;
        Some(Slot {
            id,
            query: query.clone(),
            last: None,
            failures: 0,
            rose: false,
        })
    }

    /// Notes `weekend` as the current one; a different one from before clears the requests.
    /// Returns whether it cleared any.
    fn follow_weekend(&mut self, weekend: Option<i64>) -> bool {
        let changed = self.weekend.is_some_and(|last| last != weekend);
        self.weekend = Some(weekend);
        if !changed {
            return false;
        }
        let any = !self.requests.is_empty() || self.batch_out;
        self.clear();
        // Reported by the next `advance`, if `apply` saw it first
        self.cleared = any;
        any
    }

    /// Puts the batch's requests back to due
    fn return_batch(&mut self) {
        for request in &mut self.requests {
            if let State::InBatch { since, fired, at } = &request.state {
                request.state = State::Due {
                    since: since.clone(),
                    fired: fired.clone(),
                    at: *at,
                };
            }
        }
        self.batch_out = false;
    }
}

/// Whether two snapshots are the same one
fn same(a: &Progress, b: &Progress) -> bool {
    a.seq == b.seq && a.epoch == b.epoch
}

/// How long ago, counted in laps and game clock, a request was made, as of its firing
fn made_text(made: &Progress, fired: &Progress) -> String {
    let laps = fired.laps.saturating_sub(made.laps);
    let session = made
        .session_type
        .as_deref()
        .map(|session| format!(", during {session}"))
        .unwrap_or_default();
    format!(
        "{} and {} of game clock before it fired{session}",
        plural(laps, "lap"),
        duration(fired.clock_s - made.clock_s),
    )
}

/// Why a request fired at `fired`, for the model
fn why_text(request: &Request, fired: &Progress) -> String {
    let why = match &request.trigger {
        Trigger::Time { seconds } => format!("{} of game clock passed", duration(*seconds)),
        Trigger::Laps { count } => format!("{} completed", plural(u64::from(*count), "lap")),
        Trigger::Session { .. } => match &fired.session_type {
            Some(session) => format!("a new session started ({session})"),
            None => "a new session started".into(),
        },
        Trigger::Condition { query } => format!("this query started returning a row: {query}"),
    };
    match &request.repeat {
        Repeat::Once => why,
        Repeat::Every { .. } if request.ending => format!("{why}; this is its last time"),
        Repeat::Every { .. } => format!("{why}; it repeats"),
    }
}

/// A trigger, and how far it has counted from `base` where it counts
fn with_count(trigger: &Trigger, base: &Progress, progress: &Progress) -> String {
    let count = match trigger {
        Trigger::Time { seconds } => format!(
            "{} of {}",
            duration((progress.clock_s - base.clock_s).min(*seconds)),
            duration(*seconds)
        ),
        Trigger::Laps { count } => format!(
            "{} of {} laps",
            progress
                .laps
                .saturating_sub(base.laps)
                .min(u64::from(*count)),
            count
        ),
        Trigger::Session { .. } | Trigger::Condition { .. } => return describe(trigger),
    };
    format!("{} ({count} so far)", describe(trigger))
}

fn describe(trigger: &Trigger) -> String {
    match trigger {
        Trigger::Time { seconds } => format!("after {} of game clock", duration(*seconds)),
        Trigger::Laps { count } => format!("after {}", plural(u64::from(*count), "lap")),
        Trigger::Session { kind: Some(kind) } => format!("when the next {kind} session starts"),
        Trigger::Session { kind: None } => "when the next session starts".into(),
        Trigger::Condition { query } => format!("when this query starts returning a row: {query}"),
    }
}

fn describe_stale(trigger: &Trigger) -> String {
    match trigger {
        Trigger::Time { seconds } => format!("{} of game clock after it fires", duration(*seconds)),
        Trigger::Laps { count } => format!("{} after it fires", plural(u64::from(*count), "lap")),
        Trigger::Session { .. } => "once the next session starts".into(),
        Trigger::Condition { query } => format!("while this query returns a row: {query}"),
    }
}

/// Seconds as `m:ss`, or `h:mm:ss`
fn duration(seconds: f64) -> String {
    let total = seconds.max(0.0).round() as u64;
    let (hours, minutes, seconds) = (total / 3600, total / 60 % 60, total % 60);
    if hours > 0 {
        format!("{hours}:{minutes:02}:{seconds:02}")
    } else {
        format!("{minutes}:{seconds:02}")
    }
}

fn plural(count: u64, noun: &str) -> String {
    if count == 1 {
        format!("1 {noun}")
    } else {
        format!("{count} {noun}s")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engineer::tool::{ConditionId, ConditionState, Progress};
    use std::time::{Duration, Instant};

    const WEEKEND: i64 = 1;

    /// A snapshot in the first epoch of the test weekend
    fn at(clock_s: f64, laps: u64) -> Progress {
        Progress {
            weekend: Some(WEEKEND),
            epoch: Some(1),
            clock_s,
            laps,
            ..Default::default()
        }
    }

    fn laps(laps: u64) -> Progress {
        at(0.0, laps)
    }

    fn with(mut progress: Progress, id: ConditionId, state: ConditionState) -> Progress {
        progress.conditions.insert(id, state);
        progress.checked.insert(id);
        progress
    }

    fn new_epoch(mut progress: Progress, epoch: i64, reasons: &[&str]) -> Progress {
        progress.epoch = Some(epoch);
        progress.new_epochs = reasons.iter().map(|r| r.to_string()).collect();
        progress
    }

    fn request(trigger: Trigger, repeat: Repeat, made: &Progress) -> NewRequest {
        NewRequest {
            instruction: "Tell the pilot the fuel left.".into(),
            trigger,
            repeat,
            stale_after: None,
            made: made.clone(),
        }
    }

    fn every() -> Repeat {
        Repeat::Every { until: None }
    }

    fn condition(query: &str) -> Trigger {
        Trigger::Condition {
            query: query.into(),
        }
    }

    /// A schedule fed snapshots one wall-clock second apart
    struct Run {
        schedule: Schedule,
        now: Instant,
        seq: u64,
        last: Progress,
    }

    impl Run {
        fn new() -> Self {
            Self {
                schedule: Schedule::new(),
                now: Instant::now(),
                seq: 0,
                last: at(0.0, 0),
            }
        }

        fn add(&mut self, request: NewRequest) -> RequestId {
            let made = request.made.clone();
            let id = self.schedule.next_id();
            let applied = self.schedule.apply(vec![Change::Add(id, request)], &made);
            assert_eq!(applied.ids.len(), 1);
            assert!(applied.missing.is_empty());
            applied.ids[0]
        }

        fn tick(&mut self, progress: Progress) -> Advance {
            self.tick_after(progress, Duration::from_secs(1))
        }

        fn tick_after(&mut self, mut progress: Progress, wait: Duration) -> Advance {
            self.seq += 1;
            progress.seq = self.seq;
            self.now += wait;
            self.last = progress.clone();
            self.schedule.advance(&progress, self.now)
        }

        /// The requests that fell due and wait for a scheduled turn
        fn due(&self) -> Vec<RequestId> {
            self.schedule
                .requests
                .iter()
                .filter(|r| matches!(r.state, State::Due { .. }))
                .map(|r| r.id)
                .collect()
        }

        /// Takes the batch once its gathering window has passed
        fn take(&mut self) -> Batch {
            self.now += GATHER;
            self.schedule.take_batch(self.now).expect("a batch")
        }

        /// A scheduled turn answers every due request, and its answer plays
        fn answer(&mut self) -> Vec<RequestId> {
            let batch = self.take();
            self.schedule.answered(&batch.fired, &self.last);
            self.schedule.resolved(&batch.fired);
            batch.fired.iter().map(|f| f.id).collect()
        }

        fn condition_ids(&self) -> Vec<ConditionId> {
            self.schedule
                .conditions()
                .into_iter()
                .map(|(id, _)| id)
                .collect()
        }
    }

    #[test]
    fn time_trigger_fires_from_its_baseline() {
        let mut run = Run::new();
        let id = run.add(request(
            Trigger::Time { seconds: 300.0 },
            Repeat::Once,
            &at(100.0, 0),
        ));
        run.tick(at(399.9, 0));
        assert!(run.due().is_empty());
        run.tick(at(400.0, 0));
        assert_eq!(run.due(), [id]);

        // The game clock is a sum of float steps: a hair short of the target still reaches it
        let mut run = Run::new();
        let id = run.add(request(
            Trigger::Time { seconds: 300.0 },
            Repeat::Once,
            &at(100.0, 0),
        ));
        run.tick(at(399.999_999_9, 0));
        assert_eq!(run.due(), [id]);
    }

    #[test]
    fn lap_trigger_fires_from_its_baseline() {
        let mut run = Run::new();
        let id = run.add(request(Trigger::Laps { count: 3 }, Repeat::Once, &laps(10)));
        run.tick(laps(12));
        assert!(run.due().is_empty());
        run.tick(laps(13));
        assert_eq!(run.due(), [id]);
    }

    #[test]
    fn every_rearms_from_the_firing() {
        let mut run = Run::new();
        let id = run.add(request(Trigger::Laps { count: 2 }, every(), &laps(10)));
        run.tick(laps(11));
        assert!(run.due().is_empty());
        run.tick(laps(12));
        assert_eq!(run.due(), [id]);

        // Answered a lap later: the next firing still counts from lap 12
        run.tick(laps(13));
        assert_eq!(run.answer(), [id]);
        assert!(run.due().is_empty());
        run.tick(laps(14));
        assert_eq!(run.due(), [id]);
        assert_eq!(run.schedule.len(), 1);
    }

    #[test]
    fn until_counts_from_creation_and_lets_the_last_firing_count() {
        let mut run = Run::new();
        let id = run.add(request(
            Trigger::Laps { count: 1 },
            Repeat::Every {
                until: Some(Trigger::Laps { count: 5 }),
            },
            &laps(0),
        ));
        let mut answers = 0;
        for lap in 1..=5 {
            run.tick(laps(lap));
            assert_eq!(run.due(), [id], "lap {lap}");
            run.answer();
            answers += 1;
        }
        assert_eq!(answers, 5);
        assert_eq!(run.schedule.len(), 0);
        assert!(run.schedule.requests.is_empty());
        run.tick(laps(6));
        assert!(run.due().is_empty());
    }

    #[test]
    fn condition_fires_on_the_rise_only() {
        let mut run = Run::new();
        let id = run.add(request(condition("SELECT 1"), every(), &laps(0)));
        let [cond] = run.condition_ids()[..] else {
            panic!("one condition")
        };
        run.tick(with(laps(0), cond, ConditionState::False));
        assert!(run.due().is_empty());
        run.tick(with(laps(0), cond, ConditionState::True));
        assert_eq!(run.due(), [id]);
        run.answer();

        run.tick(with(laps(0), cond, ConditionState::True));
        assert!(run.due().is_empty());
        run.tick(with(laps(0), cond, ConditionState::False));
        assert!(run.due().is_empty());
        run.tick(with(laps(0), cond, ConditionState::True));
        assert_eq!(run.due(), [id]);
    }

    #[test]
    fn condition_true_when_made_waits_for_a_rise() {
        let mut run = Run::new();
        let id = run.add(request(condition("SELECT 1"), Repeat::Once, &laps(0)));
        let [cond] = run.condition_ids()[..] else {
            panic!("one condition")
        };
        // No result yet is no change
        run.tick(laps(0));
        assert!(run.due().is_empty());
        run.tick(with(laps(0), cond, ConditionState::True));
        assert!(run.due().is_empty());
        run.tick(with(laps(0), cond, ConditionState::True));
        assert!(run.due().is_empty());
        run.tick(with(laps(0), cond, ConditionState::False));
        assert!(run.due().is_empty());
        run.tick(with(laps(0), cond, ConditionState::True));
        assert_eq!(run.due(), [id]);
    }

    #[test]
    fn epoch_change_resets_the_condition_baseline() {
        let mut run = Run::new();
        let id = run.add(request(condition("SELECT 1"), Repeat::Once, &laps(0)));
        let [cond] = run.condition_ids()[..] else {
            panic!("one condition")
        };
        run.tick(with(laps(0), cond, ConditionState::False));
        run.tick(with(
            new_epoch(laps(0), 2, &["session"]),
            cond,
            ConditionState::True,
        ));
        assert!(run.due().is_empty());
        // An epoch that changes without a reason resets it too
        let mut progress = with(laps(0), cond, ConditionState::False);
        progress.epoch = Some(2);
        run.tick(progress);
        let mut progress = with(laps(0), cond, ConditionState::True);
        progress.epoch = Some(3);
        run.tick(progress);
        assert!(run.due().is_empty());

        let mut progress = with(laps(0), cond, ConditionState::False);
        progress.epoch = Some(3);
        run.tick(progress);
        let mut progress = with(laps(0), cond, ConditionState::True);
        progress.epoch = Some(3);
        run.tick(progress);
        assert_eq!(run.due(), [id]);
    }

    #[test]
    fn session_trigger_fires_on_a_session_epoch() {
        let race = || Trigger::Session {
            kind: Some("race".into()),
        };
        let starting = |reasons: &[&str], session_type: &str| {
            let mut progress = new_epoch(laps(0), 2, reasons);
            progress.session_type = Some(session_type.into());
            progress
        };

        let mut run = Run::new();
        let id = run.add(request(race(), Repeat::Once, &laps(0)));
        run.tick(laps(0));
        run.tick(starting(&["session"], "race"));
        assert_eq!(run.due(), [id]);

        let mut run = Run::new();
        run.add(request(race(), Repeat::Once, &laps(0)));
        run.tick(starting(&["clock_rollback"], "race"));
        assert!(run.due().is_empty());
        run.tick(starting(&["session"], "qualifying"));
        assert!(run.due().is_empty());

        let mut run = Run::new();
        let id = run.add(request(
            Trigger::Session { kind: None },
            Repeat::Once,
            &laps(0),
        ));
        run.tick(starting(&["session"], "qualifying"));
        assert_eq!(run.due(), [id]);
    }

    #[test]
    fn session_trigger_sees_an_epoch_it_missed() {
        let mut run = Run::new();
        let id = run.add(request(
            Trigger::Session {
                kind: Some("race".into()),
            },
            Repeat::Once,
            &laps(0),
        ));
        let mut progress = new_epoch(laps(0), 3, &["session", "resumed"]);
        progress.session_type = Some("race".into());
        run.tick(progress);
        assert_eq!(run.due(), [id]);
    }

    #[test]
    fn weekend_change_clears() {
        let mut run = Run::new();
        run.add(request(condition("SELECT 1"), every(), &laps(0)));
        run.add(request(Trigger::Laps { count: 1 }, Repeat::Once, &laps(0)));
        assert_eq!(run.tick(laps(0)), Advance::default());

        let mut progress = new_epoch(laps(0), 2, &["cleared"]);
        progress.weekend = Some(WEEKEND + 1);
        let advance = run.tick(progress);
        assert!(advance.cleared);
        assert_eq!(run.schedule.len(), 0);
        assert!(run.schedule.conditions().is_empty());
        assert!(run.schedule.pending_text(&run.last).contains("none"));
    }

    #[test]
    fn changes_from_another_weekend_are_discarded() {
        let mut schedule = Schedule::new();
        let made = laps(0);
        let mut now = laps(0);
        now.weekend = Some(WEEKEND + 1);
        let id = schedule.next_id();
        let applied = schedule.apply(
            vec![Change::Add(
                id,
                request(Trigger::Laps { count: 1 }, Repeat::Once, &made),
            )],
            &now,
        );
        // Reported, as the pilot heard it confirmed
        assert_eq!(
            applied,
            Applied {
                dropped: vec![id],
                ..Default::default()
            }
        );
        assert_eq!(schedule.len(), 0);
    }

    #[test]
    fn adds_keep_the_ids_the_tool_gave_them() {
        let mut schedule = Schedule::new();
        let made = laps(0);
        let first = schedule.next_id();
        // A dropped turn's adds were never applied: the next turn numbers from the same id
        assert_eq!(schedule.next_id(), first);
        let second = RequestId(first.0 + 1);
        let ids = schedule.apply(
            vec![
                Change::Add(
                    first,
                    request(Trigger::Laps { count: 1 }, Repeat::Once, &made),
                ),
                Change::Add(
                    second,
                    request(Trigger::Laps { count: 2 }, Repeat::Once, &made),
                ),
            ],
            &made,
        );
        assert_eq!(ids.ids, [first, second]);
        assert_eq!(schedule.next_id(), RequestId(second.0 + 1));
        // An id already taken isn't added twice
        let ids = schedule.apply(
            vec![Change::Add(
                first,
                request(Trigger::Laps { count: 3 }, Repeat::Once, &made),
            )],
            &made,
        );
        assert_eq!(ids, Applied::default());
        assert_eq!(schedule.len(), 2);
    }

    #[test]
    fn unchecked_conditions_neither_change_nor_fail() {
        let mut run = Run::new();
        let made = laps(0);
        run.add(request(
            Trigger::Condition {
                query: "SELECT 1".into(),
            },
            Repeat::Once,
            &made,
        ));
        let cond = run.condition_ids()[0];
        run.tick(with(laps(0), cond, ConditionState::False));
        // A True left over from before, not checked for this snapshot: not a rise
        let mut stale = laps(0);
        stale.conditions.insert(cond, ConditionState::True);
        run.tick(stale);
        assert!(run.due().is_empty());
        // One failure, carried over unchecked on later snapshots, counts once
        run.tick(with(laps(0), cond, ConditionState::Failed));
        for _ in 0..CONDITION_FAILURES {
            let mut carried = laps(0);
            carried.conditions.insert(cond, ConditionState::Failed);
            assert!(run.tick(carried).dropped.is_empty());
        }
    }

    #[test]
    fn stale_time_and_laps_count_from_firing() {
        let mut run = Run::new();
        let mut timed = request(Trigger::Time { seconds: 100.0 }, Repeat::Once, &at(0.0, 0));
        timed.stale_after = Some(Trigger::Time { seconds: 30.0 });
        run.add(timed);
        run.tick(at(100.0, 0));
        let fired = run.take().fired;
        assert!(!run.schedule.is_stale(&fired, &at(129.0, 0)));
        assert!(run.schedule.is_stale(&fired, &at(130.0, 0)));

        let mut run = Run::new();
        let mut lapped = request(Trigger::Laps { count: 3 }, Repeat::Once, &laps(0));
        lapped.stale_after = Some(Trigger::Laps { count: 1 });
        run.add(lapped);
        run.tick(laps(3));
        let fired = run.take().fired;
        assert!(!run.schedule.is_stale(&fired, &laps(3)));
        assert!(run.schedule.is_stale(&fired, &laps(4)));

        // Without one, the default applies
        let mut run = Run::new();
        run.add(request(
            Trigger::Time { seconds: 100.0 },
            Repeat::Once,
            &at(0.0, 0),
        ));
        run.tick(at(100.0, 0));
        let fired = run.take().fired;
        assert!(!run.schedule.is_stale(&fired, &at(129.0, 0)));
        assert!(run.schedule.is_stale(&fired, &at(130.0, 0)));
    }

    #[test]
    fn stale_condition_is_stale_while_true() {
        let mut run = Run::new();
        let mut new = request(Trigger::Laps { count: 1 }, every(), &laps(0));
        new.stale_after = Some(condition("SELECT 1 FROM cars_now WHERE in_pits = 1"));
        let id = run.add(new);
        let [cond] = run.condition_ids()[..] else {
            panic!("one condition")
        };
        let fired = [Fired { id, at: laps(1) }];
        let check = |state| run.schedule.is_stale(&fired, &with(laps(1), cond, state));
        assert!(check(ConditionState::True));
        assert!(!check(ConditionState::False));
        assert!(!check(ConditionState::Failed));
        assert!(!run.schedule.is_stale(&fired, &laps(1)));

        // True when it fires: skipped at once, and armed again
        let advance = run.tick(with(laps(1), cond, ConditionState::True));
        assert_eq!(advance.skipped_stale, 1);
        assert!(run.due().is_empty());
        assert_eq!(run.schedule.len(), 1);
    }

    #[test]
    fn boundary_makes_waiting_answers_stale() {
        let mut run = Run::new();
        let lap = run.add(request(Trigger::Laps { count: 1 }, Repeat::Once, &laps(0)));
        let session = run.add(request(
            Trigger::Session { kind: None },
            Repeat::Once,
            &laps(0),
        ));
        run.tick(laps(1));
        let batch = run.take();
        assert_eq!(batch.fired.iter().map(|f| f.id).collect::<Vec<_>>(), [lap]);
        run.schedule.answered(&batch.fired, &run.last);
        let waiting = batch.fired;

        run.tick(new_epoch(laps(1), 2, &["session"]));
        assert_eq!(run.due(), [session]);
        assert!(run.schedule.is_stale(&waiting, &run.last));
        let fresh = run.take().fired;
        assert!(!run.schedule.is_stale(&fresh, &run.last));

        // A writer resuming isn't a boundary
        run.tick(new_epoch(laps(1), 3, &["resumed"]));
        assert!(!run.schedule.is_stale(&fresh, &run.last));
    }

    #[test]
    fn stale_due_request_is_skipped() {
        let mut run = Run::new();
        let mut once = request(Trigger::Laps { count: 1 }, Repeat::Once, &laps(0));
        once.stale_after = Some(Trigger::Laps { count: 1 });
        run.add(once);
        run.tick(laps(1));
        assert_eq!(run.due().len(), 1);
        let advance = run.tick(laps(2));
        assert_eq!(advance.skipped_stale, 1);
        assert_eq!(run.schedule.len(), 0);
        assert!(run.schedule.requests.is_empty());

        let mut run = Run::new();
        let mut repeating = request(Trigger::Laps { count: 2 }, every(), &laps(0));
        repeating.stale_after = Some(Trigger::Laps { count: 1 });
        let id = run.add(repeating);
        run.tick(laps(2));
        assert_eq!(run.due(), [id]);
        let advance = run.tick(laps(3));
        assert_eq!(advance.skipped_stale, 1);
        assert!(run.due().is_empty());
        assert_eq!(run.schedule.len(), 1);
        run.tick(laps(4));
        assert_eq!(run.due(), [id]);

        // A boundary makes a due request stale too
        run.tick(new_epoch(laps(4), 2, &["clock_rollback"]));
        assert!(run.due().is_empty());
    }

    #[test]
    fn gather_window_batches_requests() {
        let mut run = Run::new();
        let lap = run.add(request(
            Trigger::Laps { count: 1 },
            Repeat::Once,
            &at(0.0, 0),
        ));
        let timed = run.add(request(
            Trigger::Time { seconds: 10.0 },
            Repeat::Once,
            &at(0.0, 0),
        ));
        run.tick(at(5.0, 1));
        let first = run.now;
        assert!(run.schedule.take_batch(first).is_none());
        run.tick_after(at(10.0, 1), Duration::from_millis(500));
        assert!(
            run.schedule
                .take_batch(first + GATHER - Duration::from_millis(1))
                .is_none()
        );
        let batch = run.schedule.take_batch(first + GATHER).expect("a batch");
        let ids: Vec<_> = batch.fired.iter().map(|f| f.id).collect();
        assert_eq!(ids, [lap, timed]);
        let listed: Vec<_> = batch.requests.iter().map(|r| r.0).collect();
        assert_eq!(listed, [lap, timed]);
        assert!(
            batch
                .requests
                .iter()
                .all(|r| !r.1.is_empty() && !r.3.is_empty())
        );
        // No second batch while this one is out
        assert!(run.schedule.take_batch(first + GATHER * 10).is_none());
    }

    #[test]
    fn due_during_a_batch_waits_for_the_next() {
        let mut run = Run::new();
        let first = run.add(request(Trigger::Laps { count: 1 }, Repeat::Once, &laps(0)));
        let second = run.add(request(Trigger::Laps { count: 2 }, Repeat::Once, &laps(0)));
        run.tick(laps(1));
        let batch = run.take();
        assert_eq!(
            batch.fired.iter().map(|f| f.id).collect::<Vec<_>>(),
            [first]
        );
        run.tick(laps(2));
        assert_eq!(run.due(), [second]);
        assert!(run.schedule.take_batch(run.now + GATHER).is_none());

        run.schedule.answered(&batch.fired, &run.last);
        assert_eq!(run.answer(), [second]);
    }

    #[test]
    fn dropped_batch_stays_due() {
        let mut run = Run::new();
        let id = run.add(request(Trigger::Laps { count: 1 }, Repeat::Once, &laps(0)));
        run.tick(laps(1));
        run.take();
        assert!(run.due().is_empty());
        run.schedule.batch_dropped();
        assert_eq!(run.due(), [id]);
        assert_eq!(run.answer(), [id]);
    }

    #[test]
    fn failed_batch_waits_retry_after() {
        let mut run = Run::new();
        let id = run.add(request(Trigger::Laps { count: 1 }, Repeat::Once, &laps(0)));
        run.tick(laps(1));
        run.take();
        let failed = run.now;
        run.schedule.batch_failed(failed);
        assert_eq!(run.due(), [id]);
        assert!(
            run.schedule
                .take_batch(failed + RETRY_AFTER - Duration::from_millis(1))
                .is_none()
        );
        let batch = run
            .schedule
            .take_batch(failed + RETRY_AFTER)
            .expect("a batch");
        assert_eq!(batch.fired.iter().map(|f| f.id).collect::<Vec<_>>(), [id]);
    }

    #[test]
    fn replace_keeps_made_and_armed_baseline() {
        // For the next five laps, every lap
        let mut run = Run::new();
        let id = run.add(request(
            Trigger::Laps { count: 1 },
            Repeat::Every {
                until: Some(Trigger::Laps { count: 5 }),
            },
            &laps(10),
        ));
        for lap in [11, 12] {
            run.tick(laps(lap));
            assert_eq!(run.answer(), [id]);
        }

        // Two laps in: make that every two laps
        let replacement = request(
            Trigger::Laps { count: 2 },
            Repeat::Every {
                until: Some(Trigger::Laps { count: 5 }),
            },
            &laps(12),
        );
        let ids = run
            .schedule
            .apply(vec![Change::Replace(id, replacement)], &laps(12));
        assert_eq!(
            ids,
            Applied {
                ids: vec![id],
                ..Default::default()
            }
        );
        assert_eq!(run.schedule.len(), 1);

        run.tick(laps(13));
        assert!(run.due().is_empty());
        run.tick(laps(14));
        assert_eq!(run.due(), [id]);
        run.answer();
        run.tick(laps(15));
        assert!(run.due().is_empty());
        assert_eq!(run.schedule.len(), 0);

        // Replacing or cancelling an unknown id changes nothing
        let ids = run.schedule.apply(
            vec![
                Change::Replace(
                    RequestId(99),
                    request(Trigger::Laps { count: 1 }, Repeat::Once, &laps(15)),
                ),
                Change::Cancel(RequestId(98)),
            ],
            &laps(15),
        );
        assert_eq!(
            ids,
            Applied {
                missing: vec![RequestId(99), RequestId(98)],
                ..Default::default()
            }
        );
    }

    #[test]
    fn changes_to_gone_requests_are_reported() {
        // A one-shot that fired and was answered while the pilot's turn ran
        let mut run = Run::new();
        let gone = run.add(request(Trigger::Laps { count: 1 }, Repeat::Once, &laps(0)));
        let kept = run.add(request(Trigger::Laps { count: 5 }, every(), &laps(0)));
        run.tick(laps(1));
        assert_eq!(run.answer(), [gone]);
        let applied = run.schedule.apply(
            vec![
                Change::Replace(gone, request(Trigger::Laps { count: 2 }, every(), &laps(0))),
                Change::Cancel(gone),
                Change::Replace(kept, request(Trigger::Laps { count: 2 }, every(), &laps(0))),
            ],
            &run.last.clone(),
        );
        assert_eq!(
            applied,
            Applied {
                ids: vec![kept],
                missing: vec![gone, gone],
                ..Default::default()
            }
        );
        assert_eq!(run.schedule.pending_ids(), [kept]);

        // One whose answer is still on its way is there: cancelling it drops that answer
        let mut run = Run::new();
        let id = run.add(request(Trigger::Laps { count: 1 }, Repeat::Once, &laps(0)));
        run.tick(laps(1));
        let batch = run.take();
        run.schedule.answered(&batch.fired, &run.last);
        let applied = run
            .schedule
            .apply(vec![Change::Cancel(id)], &run.last.clone());
        assert_eq!(applied, Applied::default());
        assert!(run.schedule.is_stale(&batch.fired, &run.last));
    }

    #[test]
    fn session_trigger_made_before_a_start_and_applied_after_it_fires() {
        let typed = |epoch: i64, reasons: &[&str], session_type: &str| {
            let mut progress = new_epoch(laps(0), epoch, reasons);
            progress.session_type = Some(session_type.into());
            progress
        };
        let race = || Trigger::Session {
            kind: Some("race".into()),
        };
        // The pilot asks as qualifying ends, and the race loads while the turn runs
        // An add, or a replace of `replacing`
        let late =
            |run: &mut Run, replacing: Option<RequestId>, trigger: Trigger, made: &Progress| {
                let new = request(trigger, Repeat::Once, made);
                let (id, change) = match replacing {
                    Some(id) => (id, Change::Replace(id, new)),
                    None => {
                        let id = run.schedule.next_id();
                        (id, Change::Add(id, new))
                    }
                };
                let applied = run.schedule.apply(vec![change], &run.last.clone());
                assert_eq!(applied.ids, [id]);
                id
            };

        // Any session
        let mut run = Run::new();
        run.tick(laps(0));
        let made = run.last.clone();
        run.tick(new_epoch(laps(0), 2, &["session"]));
        let id = late(&mut run, None, Trigger::Session { kind: None }, &made);
        run.tick(new_epoch(laps(0), 2, &[]));
        assert_eq!(run.due(), [id]);
        // Once
        run.answer();
        run.tick(new_epoch(laps(0), 2, &[]));
        assert!(run.due().is_empty());
        assert_eq!(run.schedule.len(), 0);

        // A replace made before the start, of a request that was counting laps
        let mut run = Run::new();
        let id = run.add(request(Trigger::Laps { count: 5 }, Repeat::Once, &laps(0)));
        run.tick(laps(0));
        let made = run.last.clone();
        run.tick(new_epoch(laps(0), 2, &["session"]));
        assert_eq!(
            late(&mut run, Some(id), Trigger::Session { kind: None }, &made),
            id
        );
        run.tick(new_epoch(laps(0), 2, &[]));
        assert_eq!(run.due(), [id]);

        // Of a type known by then
        let mut run = Run::new();
        run.tick(laps(0));
        let made = run.last.clone();
        run.tick(typed(2, &["session"], "race"));
        let id = late(&mut run, None, race(), &made);
        run.tick(typed(2, &[], "race"));
        assert_eq!(run.due(), [id]);

        // Of a type not known yet: it waits for it
        let mut run = Run::new();
        run.tick(laps(0));
        let made = run.last.clone();
        run.tick(new_epoch(laps(0), 2, &["session"]));
        let id = late(&mut run, None, race(), &made);
        run.tick(new_epoch(laps(0), 2, &[]));
        assert!(run.due().is_empty());
        run.tick(typed(2, &[], "race"));
        assert_eq!(run.due(), [id]);

        // Of another type: it waits for the next race
        let mut run = Run::new();
        run.tick(laps(0));
        let made = run.last.clone();
        run.tick(typed(2, &["session"], "qualifying"));
        let id = late(&mut run, None, race(), &made);
        run.tick(typed(2, &[], "qualifying"));
        assert!(run.due().is_empty());
        run.tick(typed(3, &["session"], "race"));
        assert_eq!(run.due(), [id]);

        // Made after the start: it waits for the next one
        let mut run = Run::new();
        run.tick(laps(0));
        run.tick(new_epoch(laps(0), 2, &["session"]));
        let made = run.last.clone();
        late(&mut run, None, Trigger::Session { kind: None }, &made);
        run.tick(new_epoch(laps(0), 2, &[]));
        assert!(run.due().is_empty());
    }

    #[test]
    fn conditions_lists_every_slot() {
        let mut run = Run::new();
        let mut new = request(
            condition("SELECT 'trigger'"),
            Repeat::Every {
                until: Some(condition("SELECT 'until'")),
            },
            &laps(0),
        );
        new.stale_after = Some(condition("SELECT 'stale'"));
        let id = run.add(new.clone());
        let conditions = run.schedule.conditions();
        let queries: Vec<_> = conditions.iter().map(|(_, q)| q.as_str()).collect();
        assert_eq!(
            queries,
            ["SELECT 'trigger'", "SELECT 'until'", "SELECT 'stale'"]
        );
        let ids = run.condition_ids();
        assert!(ids[0] != ids[1] && ids[1] != ids[2] && ids[0] != ids[2]);

        // A replacement gets new ids, never ones used before
        run.schedule.apply(vec![Change::Replace(id, new)], &laps(0));
        let replaced = run.condition_ids();
        assert_eq!(replaced.len(), 3);
        assert!(replaced.iter().all(|c| !ids.contains(c)));

        run.schedule.apply(vec![Change::Cancel(id)], &laps(0));
        assert!(run.schedule.conditions().is_empty());

        // A one-shot's conditions stay until its answer is resolved
        let mut run = Run::new();
        let mut new = request(Trigger::Laps { count: 1 }, Repeat::Once, &laps(0));
        new.stale_after = Some(condition("SELECT 'stale'"));
        run.add(new);
        run.tick(laps(1));
        let batch = run.take();
        run.schedule.answered(&batch.fired, &run.last);
        assert_eq!(run.schedule.len(), 0);
        assert_eq!(run.schedule.conditions().len(), 1);
        run.schedule.resolved(&batch.fired);
        assert!(run.schedule.conditions().is_empty());
    }

    #[test]
    fn condition_failing_five_times_is_dropped() {
        let mut run = Run::new();
        let id = run.add(request(condition("SELECT nope"), Repeat::Once, &laps(0)));
        let [cond] = run.condition_ids()[..] else {
            panic!("one condition")
        };
        for _ in 0..CONDITION_FAILURES - 1 {
            assert!(
                run.tick(with(laps(0), cond, ConditionState::Failed))
                    .dropped
                    .is_empty()
            );
        }
        let advance = run.tick(with(laps(0), cond, ConditionState::Failed));
        assert_eq!(
            advance.dropped,
            [(id, "Tell the pilot the fuel left.".to_string())]
        );
        assert_eq!(run.schedule.len(), 0);
        assert!(run.schedule.conditions().is_empty());

        let mut run = Run::new();
        run.add(request(condition("SELECT 1"), Repeat::Once, &laps(0)));
        let [cond] = run.condition_ids()[..] else {
            panic!("one condition")
        };
        for _ in 0..CONDITION_FAILURES - 1 {
            run.tick(with(laps(0), cond, ConditionState::Failed));
        }
        run.tick(with(laps(0), cond, ConditionState::False));
        for _ in 0..CONDITION_FAILURES - 1 {
            assert!(
                run.tick(with(laps(0), cond, ConditionState::Failed))
                    .dropped
                    .is_empty()
            );
        }
        assert_eq!(run.schedule.len(), 1);
    }

    #[test]
    fn timed_repeat_rearms_from_its_target() {
        // Every minute for five minutes, the snapshots 1.3 s of game clock apart: each firing is a
        // little late, which mustn't push the next one back
        let mut run = Run::new();
        let id = run.add(request(
            Trigger::Time { seconds: 60.0 },
            Repeat::Every {
                until: Some(Trigger::Time { seconds: 300.0 }),
            },
            &at(0.0, 0),
        ));
        let mut answers = 0;
        for step in 1..=300 {
            run.tick(at(f64::from(step) * 1.3, 0));
            if !run.due().is_empty() {
                assert_eq!(run.answer(), [id]);
                answers += 1;
            }
            if run.schedule.requests.is_empty() {
                break;
            }
        }
        assert_eq!(answers, 5);
        assert!(run.schedule.requests.is_empty());

        // More than a period late (a pause, an outage): counted from the firing, not in a burst
        let mut run = Run::new();
        let id = run.add(request(
            Trigger::Time { seconds: 60.0 },
            every(),
            &at(0.0, 0),
        ));
        run.tick(at(10.0, 0));
        run.tick(at(200.0, 0));
        assert_eq!(run.answer(), [id]);
        run.tick(at(201.0, 0));
        assert!(run.due().is_empty());
        run.tick(at(259.0, 0));
        assert!(run.due().is_empty());
        run.tick(at(260.0, 0));
        assert_eq!(run.due(), [id]);
    }

    #[test]
    fn stale_skip_checks_the_trigger_again() {
        // Every session start, out of date once the next one starts
        let mut run = Run::new();
        let mut new = request(Trigger::Session { kind: None }, every(), &laps(0));
        new.stale_after = Some(Trigger::Session { kind: None });
        let id = run.add(new);
        run.tick(laps(0));
        run.tick(new_epoch(laps(0), 2, &["session"]));
        assert_eq!(run.due(), [id]);

        // Its scheduled turns keep failing until the next session loads
        run.take();
        run.schedule.batch_failed(run.now);
        run.tick(new_epoch(laps(0), 2, &[]));
        let advance = run.tick(new_epoch(laps(0), 3, &["session"]));
        assert_eq!(advance.skipped_stale, 1);
        assert_eq!(run.due(), [id]);
        let batch = run
            .schedule
            .take_batch(run.now + RETRY_AFTER)
            .expect("a batch");
        assert_eq!(batch.fired.len(), 1);
        assert_eq!(batch.fired[0].at.epoch, Some(3));
        assert!(!run.schedule.is_stale(&batch.fired, &run.last));
    }

    #[test]
    fn ended_repeat_waits_for_its_answer() {
        // Every two laps for three laps: the answer at lap 2 still waits when lap 3 ends it
        let mut run = Run::new();
        let id = run.add(request(
            Trigger::Laps { count: 2 },
            Repeat::Every {
                until: Some(Trigger::Laps { count: 3 }),
            },
            &laps(0),
        ));
        run.tick(laps(2));
        let batch = run.take();
        run.schedule.answered(&batch.fired, &run.last);
        run.tick(laps(3));
        assert_eq!(run.schedule.len(), 0);
        assert!(run.schedule.pending_ids().is_empty());
        assert!(run.schedule.pending_text(&run.last).contains("none"));
        assert!(!run.schedule.is_stale(&batch.fired, &run.last));
        // Nothing more happens to it
        run.tick(laps(4));
        assert!(run.due().is_empty());
        assert!(!run.schedule.is_stale(&batch.fired, &run.last));

        run.schedule.resolved(&batch.fired);
        assert!(run.schedule.requests.is_empty());
        assert!(run.schedule.is_stale(&batch.fired, &run.last));
        assert_eq!(batch.fired[0].id, id);
    }

    #[test]
    fn a_gone_request_makes_its_answer_stale() {
        // An answer waiting in the line, of a request that repeats
        let waiting = |run: &mut Run, made: NewRequest| {
            let id = run.add(made);
            run.tick(laps(1));
            let batch = run.take();
            run.schedule.answered(&batch.fired, &run.last);
            assert!(!run.schedule.is_stale(&batch.fired, &run.last));
            (id, batch.fired)
        };
        let lap = || request(Trigger::Laps { count: 1 }, every(), &laps(0));

        // Cancelled
        let mut run = Run::new();
        let (id, fired) = waiting(&mut run, lap());
        run.schedule
            .apply(vec![Change::Cancel(id)], &run.last.clone());
        assert!(run.schedule.is_stale(&fired, &run.last));

        // Cleared
        let mut run = Run::new();
        let (_, fired) = waiting(&mut run, lap());
        run.schedule.clear();
        assert!(run.schedule.is_stale(&fired, &run.last));

        // Dropped, its condition failing
        let mut run = Run::new();
        let mut failing = lap();
        failing.repeat = Repeat::Every {
            until: Some(condition("SELECT nope")),
        };
        let (_, fired) = waiting(&mut run, failing);
        let [cond] = run.condition_ids()[..] else {
            panic!("one condition")
        };
        for _ in 0..CONDITION_FAILURES {
            run.tick(with(laps(1), cond, ConditionState::Failed));
        }
        assert!(run.schedule.requests.is_empty());
        assert!(run.schedule.is_stale(&fired, &run.last));
    }

    #[test]
    fn a_firing_stale_at_once_counts_once() {
        let mut run = Run::new();
        let mut new = request(
            Trigger::Condition {
                query: "SELECT 1".into(),
            },
            every(),
            &laps(0),
        );
        new.stale_after = Some(Trigger::Condition {
            query: "SELECT 2".into(),
        });
        run.add(new);
        let ids = run.condition_ids();
        let (trigger, stale) = (ids[0], ids[1]);
        let both = |a, b| with(with(laps(0), trigger, a), stale, b);
        run.tick(both(ConditionState::False, ConditionState::True));
        let advance = run.tick(both(ConditionState::True, ConditionState::True));
        assert_eq!(advance.skipped_stale, 1);
        assert!(run.due().is_empty());
    }

    #[test]
    fn a_firing_noticed_late_counts_on_from_itself() {
        let mut run = Run::new();
        let id = run.add(request(
            Trigger::Time { seconds: 60.0 },
            every(),
            &at(0.0, 0),
        ));
        run.tick(at(55.0, 0));
        // An outage the clock jumped over: noticed 55 s late
        run.tick(at(115.0, 0));
        assert_eq!(run.due(), [id]);
        run.tick(at(118.0, 0));
        run.answer();
        run.tick(at(120.0, 0));
        assert!(run.due().is_empty());
        run.tick(at(174.9, 0));
        assert!(run.due().is_empty());
        run.tick(at(175.0, 0));
        assert_eq!(run.due(), [id]);
    }

    #[test]
    fn typed_session_trigger_skips_a_session_that_started_first() {
        let mut run = Run::new();
        run.tick(laps(0));
        // The race's epoch arrives before its type, and the pilot asks in between
        run.tick(new_epoch(laps(0), 2, &["session"]));
        let mut made = laps(0);
        made.epoch = Some(2);
        run.add(request(
            Trigger::Session {
                kind: Some("race".into()),
            },
            Repeat::Once,
            &made,
        ));
        let mut typed = new_epoch(laps(0), 2, &[]);
        typed.session_type = Some("race".into());
        run.tick(typed);
        assert!(run.due().is_empty());
    }

    #[test]
    fn a_replace_keeps_a_missed_session_start() {
        let mut run = Run::new();
        run.tick(laps(0));
        // Made before the race's start, applied after it: it would fire on the next step
        let made = laps(0);
        run.tick(new_epoch(laps(0), 2, &["session"]));
        let id = run.add(request(
            Trigger::Session { kind: None },
            Repeat::Once,
            &made,
        ));
        // Replaced from a call made after the start, before the next step
        let mut later = laps(0);
        later.epoch = Some(2);
        run.schedule.apply(
            vec![Change::Replace(
                id,
                request(Trigger::Session { kind: None }, Repeat::Once, &later),
            )],
            &later,
        );
        run.tick(new_epoch(laps(0), 2, &[]));
        assert_eq!(run.due(), [id]);
    }

    #[test]
    fn a_change_to_a_dropped_add_goes_with_it() {
        let mut schedule = Schedule::new();
        let made = laps(0);
        let mut now = laps(0);
        now.weekend = Some(WEEKEND + 1);
        let id = schedule.next_id();
        let applied = schedule.apply(
            vec![
                Change::Add(id, request(Trigger::Laps { count: 1 }, Repeat::Once, &made)),
                Change::Replace(id, request(Trigger::Laps { count: 2 }, Repeat::Once, &made)),
                Change::Cancel(id),
            ],
            &now,
        );
        assert_eq!(applied.dropped, [id]);
        assert!(applied.missing.is_empty());
        assert!(applied.ids.is_empty());
    }

    #[test]
    fn session_trigger_skips_the_session_it_was_armed_in() {
        let mut run = Run::new();
        run.tick(laps(0));
        // Armed from a snapshot of the new session, whose start a later snapshot reports again
        let mut made = laps(0);
        made.epoch = Some(2);
        run.add(request(
            Trigger::Session { kind: None },
            Repeat::Once,
            &made,
        ));
        run.tick(new_epoch(laps(0), 2, &["session"]));
        assert!(run.due().is_empty());
        let id = run.schedule.pending_ids()[0];
        run.tick(new_epoch(laps(0), 3, &["session"]));
        assert_eq!(run.due(), [id]);
    }

    #[test]
    fn typed_session_trigger_waits_for_the_type() {
        let race = || Trigger::Session {
            kind: Some("race".into()),
        };
        let typed = |epoch: i64, reasons: &[&str], session_type: &str| {
            let mut progress = new_epoch(laps(0), epoch, reasons);
            progress.session_type = Some(session_type.into());
            progress
        };

        // The epoch is written before the session's first scoring row
        let mut run = Run::new();
        let id = run.add(request(race(), every(), &laps(0)));
        let any = run.add(request(
            Trigger::Session { kind: None },
            Repeat::Once,
            &laps(0),
        ));
        run.tick(laps(0));
        run.tick(new_epoch(laps(0), 2, &["session"]));
        assert_eq!(run.due(), [any]);
        run.answer();
        run.tick(typed(2, &[], "race"));
        assert_eq!(run.due(), [id]);
        run.answer();
        // Once a session
        run.tick(typed(2, &[], "race"));
        assert!(run.due().is_empty());

        // Of another type
        let mut run = Run::new();
        run.add(request(race(), Repeat::Once, &laps(0)));
        run.tick(new_epoch(laps(0), 2, &["session"]));
        run.tick(typed(2, &[], "qualifying"));
        run.tick(typed(2, &[], "race"));
        assert!(run.due().is_empty());

        // Forgotten at the next epoch change: a restart isn't a session starting
        let mut run = Run::new();
        run.add(request(race(), Repeat::Once, &laps(0)));
        run.tick(new_epoch(laps(0), 2, &["session"]));
        run.tick(typed(3, &["clock_rollback"], "race"));
        run.tick(typed(3, &[], "race"));
        assert!(run.due().is_empty());
    }

    #[test]
    fn pending_ids_are_the_pending_requests() {
        let mut run = Run::new();
        let once = run.add(request(Trigger::Laps { count: 1 }, Repeat::Once, &laps(0)));
        let lap = run.add(request(Trigger::Laps { count: 1 }, every(), &laps(0)));
        assert_eq!(run.schedule.pending_ids(), [once, lap]);
        run.tick(laps(1));
        let batch = run.take();
        run.schedule.answered(&batch.fired, &run.last);
        // The one-shot's answer is on its way: it's no longer pending
        assert_eq!(run.schedule.pending_ids(), [lap]);
        assert_eq!(run.schedule.len(), 1);
    }

    #[test]
    fn pending_text_lists_requests_and_their_progress() {
        let mut run = Run::new();
        let id = run.add(request(
            Trigger::Laps { count: 1 },
            Repeat::Every {
                until: Some(Trigger::Laps { count: 5 }),
            },
            &laps(0),
        ));
        for lap in [1, 2] {
            run.tick(laps(lap));
            run.answer();
        }
        let text = run.schedule.pending_text(&laps(2));
        assert!(text.contains(&id.to_string()), "{text}");
        assert!(text.contains("Tell the pilot the fuel left."), "{text}");
        assert!(text.contains("2 of 5 laps"), "{text}");
        assert!(text.contains("every"), "{text}");
        assert_eq!(id.to_string(), format!("#{}", id.0));
    }
}
