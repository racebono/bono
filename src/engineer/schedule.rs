//! The pilot's scheduled requests, and the rules for when they fire, end and go stale.
//!
//! The rules are a plain function of the schedule, the data thread's latest [`Progress`] and the
//! wall clock: nothing here waits, reads the database or talks to the model.

use super::tool::{ConditionId, ConditionState, Progress};
use std::{
    fmt,
    time::{Duration, Instant},
};

/// How long a scheduled turn waits after the first request falls due, to take in requests that
/// fire together but are noticed a few checks apart
// Used by the engineer's scheduled turns, which a later task adds.
#[allow(dead_code)]
pub const GATHER: Duration = Duration::from_secs(1);

/// Failed checks in a row that drop a condition's request
// Used by the engineer's scheduled turns, which a later task adds.
#[allow(dead_code)]
pub const CONDITION_FAILURES: u32 = 5;

/// When an answer stops being worth saying, for a request made without its own
// Used by the engineer's scheduled turns, which a later task adds.
#[allow(dead_code)]
pub const DEFAULT_STALE: Trigger = Trigger::Time { seconds: 30.0 };

/// How long a failed scheduled turn waits before its requests are tried again
// Used by the engineer's scheduled turns, which a later task adds.
#[allow(dead_code)]
pub const RETRY_AFTER: Duration = Duration::from_secs(10);

/// The most requests that may be pending, a guard against a model stuck adding them
// Used by the `schedule` tool, which a later task adds.
#[allow(dead_code)]
pub const MAX_REQUESTS: usize = 50;

/// The game clock is a sum of float differences, so a time target counts as reached this close
/// to it
const TOLERANCE_S: f64 = 1e-6;

/// The epoch reasons after which an answer from before is out of date (`resumed` isn't one: the
/// same session carries on)
const BOUNDARIES: [&str; 3] = ["session", "clock_rollback", "replay"];

/// What the model and the pilot call a request by, shown as `#3`
// Used by the engineer's scheduled turns, which a later task adds.
#[allow(dead_code)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct RequestId(pub u32);

impl fmt::Display for RequestId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "#{}", self.0)
    }
}

/// When a request fires, ends or goes stale, counted from a baseline
// Used by the engineer's scheduled turns, which a later task adds.
#[allow(dead_code)]
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
// Used by the engineer's scheduled turns, which a later task adds.
#[allow(dead_code)]
#[derive(Clone, Debug, PartialEq)]
pub enum Repeat {
    /// Answered, then removed
    Once,
    /// Answered, then armed again, until this trigger fires (counted from when the request was
    /// made)
    Every { until: Option<Trigger> },
}

/// A request as the model asks for it
// Used by the engineer's scheduled turns, which a later task adds.
#[allow(dead_code)]
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
// Used by the engineer's scheduled turns, which a later task adds.
#[allow(dead_code)]
#[derive(Clone, Debug)]
pub enum Change {
    Add(NewRequest),
    Cancel(RequestId),
    /// Keeps the old request's `made` and, while it's armed, its armed baseline
    Replace(RequestId, NewRequest),
}

/// Where a request fired, which its answer carries to be checked for staleness
// Used by the engineer's scheduled turns, which a later task adds.
#[allow(dead_code)]
#[derive(Clone, Debug)]
pub struct Fired {
    pub id: RequestId,
    pub at: Progress,
}

/// The due requests one scheduled turn answers
// Used by the engineer's scheduled turns, which a later task adds.
#[allow(dead_code)]
#[derive(Clone, Debug)]
pub struct Batch {
    /// Each request's id, instruction, when it was made and why it fired now, as text for the
    /// model
    pub requests: Vec<(RequestId, String, String, String)>,
    pub fired: Vec<Fired>,
}

/// What a step of the schedule did that the engineer tells the pilot about
// Used by the engineer's scheduled turns, which a later task adds.
#[allow(dead_code)]
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
// Owned by the engineer, which a later task makes use it.
#[allow(dead_code)]
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
    /// A weekend change seen by `apply`, for the next `advance` to report
    cleared: bool,
    /// A batch was taken and hasn't been answered, dropped or failed
    batch_out: bool,
    /// No batch is taken before this, after a failed one
    retry_at: Option<Instant>,
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
}

#[derive(Debug)]
enum State {
    /// Waiting for its trigger, counted from `since`
    Armed { since: Progress },
    /// Fired, waiting for a scheduled turn
    Due { fired: Progress, at: Instant },
    /// Fired, in the scheduled turn that's out
    InBatch { fired: Progress, at: Instant },
    /// Its last answer was written and hasn't played or been dropped yet; then it goes
    Answered { fired: Progress },
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
        match progress.conditions.get(&self.id) {
            // Not checked yet: no change
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
        !matches!(self.state, State::Answered { .. })
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
                    reached(&self.stale_after, None, fired, progress)
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
fn reached(trigger: &Trigger, slot: Option<&Slot>, base: &Progress, progress: &Progress) -> bool {
    match trigger {
        Trigger::Time { seconds } => progress.clock_s >= base.clock_s + seconds - TOLERANCE_S,
        Trigger::Laps { count } => progress.laps >= base.laps.saturating_add(u64::from(*count)),
        Trigger::Session { kind } => {
            progress.new_epochs.iter().any(|reason| reason == "session")
                && kind.as_ref().is_none_or(|kind| {
                    progress
                        .session_type
                        .as_ref()
                        .is_some_and(|now| now.eq_ignore_ascii_case(kind))
                })
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
            cleared: false,
            batch_out: false,
            retry_at: None,
        }
    }

    /// Applies a finished turn's changes, at `progress`. Adds made in another weekend than
    /// `progress`'s are dropped, as are cancels and replaces of requests that aren't there.
    /// Returns the ids added or replaced.
    pub fn apply(&mut self, changes: Vec<Change>, progress: &Progress) -> Vec<RequestId> {
        self.follow_weekend(progress.weekend);
        let mut ids = Vec::new();
        for change in changes {
            match change {
                Change::Add(new) => {
                    if new.made.weekend != progress.weekend {
                        continue;
                    }
                    let id = RequestId(self.next_id);
                    self.next_id += 1;
                    let since = new.made.clone();
                    let request = self.request(id, new, since);
                    self.requests.push(request);
                    ids.push(id);
                }
                Change::Cancel(id) => self.requests.retain(|r| r.id != id),
                Change::Replace(id, new) => {
                    let Some(index) = self.requests.iter().position(|r| r.id == id) else {
                        continue;
                    };
                    let old = self.requests.remove(index);
                    // Counting carries on from the last firing, if it has fired since it was armed
                    let since = match old.state {
                        State::Armed { since } => since,
                        State::Due { fired, .. }
                        | State::InBatch { fired, .. }
                        | State::Answered { fired } => fired,
                    };
                    let mut request = self.request(id, new, since);
                    request.made = old.made;
                    self.requests.insert(index, request);
                    ids.push(id);
                }
            }
        }
        ids
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

        let mut kept = Vec::with_capacity(self.requests.len());
        for mut request in std::mem::take(&mut self.requests) {
            // An answer on its way: nothing more happens to it
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
                reached(until, request.until_slot.as_ref(), &request.made, progress)
            });
            if let State::Armed { since } = &request.state {
                if reached(
                    &request.trigger,
                    request.trigger_slot.as_ref(),
                    since,
                    progress,
                ) {
                    // If it ends on the same step, this firing still counts
                    request.ending = until;
                    request.state = State::Due {
                        fired: progress.clone(),
                        at: now,
                    };
                } else if until {
                    continue;
                }
            } else {
                request.ending |= until;
            }

            if let State::Due { fired, .. } = &request.state
                && request.stale(fired, progress, self.boundary)
            {
                advance.skipped_stale += 1;
                if request.last_firing() {
                    continue;
                }
                let since = fired.clone();
                request.state = State::Armed { since };
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
            let State::Due { fired, at } = &request.state else {
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

    /// The scheduled turn answered `fired`: repeating requests are armed again from their firing,
    /// and the others wait for their answer to be resolved. `_progress` is when it was answered,
    /// which counting doesn't use: it carries on from the firing, so a slow turn doesn't push
    /// the next one back.
    pub fn answered(&mut self, fired: &[Fired], _progress: &Progress) {
        for request in &mut self.requests {
            let State::InBatch { fired: at, .. } = &request.state else {
                continue;
            };
            if !fired.iter().any(|f| f.id == request.id && same(&f.at, at)) {
                continue;
            }
            let at = at.clone();
            request.state = if request.last_firing() {
                State::Answered { fired: at }
            } else {
                State::Armed { since: at }
            };
        }
        // Anything else in the batch wasn't answered after all
        self.return_batch();
    }

    /// The answer to `fired` played or was dropped: the requests it was the last answer of go
    pub fn resolved(&mut self, fired: &[Fired]) {
        self.requests.retain(|request| match &request.state {
            State::Answered { fired: at } => {
                !fired.iter().any(|f| f.id == request.id && same(&f.at, at))
            }
            _ => true,
        });
    }

    /// Whether an answer to `fired` is out of date at `progress`: once the first of its requests
    /// is, or once one of them is gone
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

    /// Pending requests, for the cap
    pub fn len(&self) -> usize {
        self.requests.iter().filter(|r| r.pending()).count()
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
        }
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
            if let State::InBatch { fired, at } = &request.state {
                request.state = State::Due {
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
            let ids = self.schedule.apply(vec![Change::Add(request)], &made);
            assert_eq!(ids.len(), 1);
            ids[0]
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
        let ids = schedule.apply(
            vec![Change::Add(request(
                Trigger::Laps { count: 1 },
                Repeat::Once,
                &made,
            ))],
            &now,
        );
        assert!(ids.is_empty());
        assert_eq!(schedule.len(), 0);
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
        assert_eq!(ids, [id]);
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
        assert!(ids.is_empty());
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
