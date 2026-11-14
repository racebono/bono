//! Requests for later: the pilot asks the engineer to say something, or to answer a question,
//! when its moment comes (some session time from now, a lap, or something the telemetry shows),
//! and keeps driving.
//!
//! The requests are kept here, apart from the model's conversation, which can start over without
//! losing them. They belong to the recording followed and to the game session they were set in,
//! and end with either. Watching them never asks the model anything: the session clock and the
//! player's laps are compared here, and conditions are the model's read-only queries, run by the
//! thread keeping the derived tables ([`Watcher`]) about once a second while there's something
//! to watch, each bounded in time. Only once a request comes due is the model asked, for an
//! assessment; a reminder is said as it was written.
//!
//! A request registered during a turn of the conversation counts only once that turn ends with
//! an answer, which is when the pilot hears it confirmed; one cancelled in it likewise. A turn
//! that fails or is cut off leaves them as they were.
//!
//! A request is done once it's been delivered: spoken to the end, or written in the chat when
//! there's no voice to speak it with. A delivery cut off is tried again a few times, re-saying an
//! answer that's still recent rather than asking the model again, and the pilot is told when it
//! can't be delivered at all.

use super::tool::{MAX_WATCHED, Reading, Verdict, Watcher};
use crate::{config::ArrayStr, provider::ToolHandler, task::Task};
use anyhow::{Context as _, bail};
use serde::Deserialize;
use std::{
    fmt::Write as _,
    sync::{Arc, Mutex, MutexGuard, PoisonError},
    time::{Duration, Instant},
};
use tokio::runtime::Handle;

/// A request's number, as the pilot and the model know it.
pub type Id = u32;

/// The most requests there can be at once, of which at most [`MAX_WATCHED`] watch a condition.
const MAX_REQUESTS: usize = 8;
const MAX_SQL_CHARS: usize = 2000;
const MAX_TEXT_CHARS: usize = 300;
/// How often the telemetry is read while there's something to watch
const POLL: Duration = Duration::from_secs(1);
/// How many readings in a row a condition must hold on, unless the model says otherwise
const DEFAULT_CONFIRM: u32 = 2;
const MAX_CONFIRM: u32 = 10;
/// A condition failing this many readings in a row stops being watched
const MAX_FAILURES: u32 = 5;
/// How many times a delivery is tried
const MAX_ATTEMPTS: u32 = 3;
/// How long after a delivery is cut off it's tried again
const RETRY_AFTER: Duration = Duration::from_secs(5);
/// An assessment cut off this soon after it was written is said again as it was; an older one is
/// asked for again, the telemetry having moved on
const REUSE_FOR: Duration = Duration::from_secs(30);
/// Due this long without being delivered (the radio never free), it's dropped: it's old news
const DUE_FOR: Duration = Duration::from_secs(300);
/// The same request asked for again this soon is the one already registered: a turn tried again
const DUPLICATE_WITHIN: Duration = Duration::from_secs(120);
const MAX_SECONDS: f64 = 24.0 * 3600.0;
const MAX_LAPS: i64 = 1000;

/// What a request does when it's due.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    /// Its message is said as it was written
    Reminder,
    /// Its message is a question the model answers then
    Assessment,
}

impl Kind {
    fn name(self) -> &'static str {
        match self {
            Kind::Reminder => "reminder",
            Kind::Assessment => "assessment",
        }
    }
}

/// When a request comes due.
#[derive(Debug, Clone, PartialEq)]
enum Trigger {
    /// The session clock reaching this
    Time(f64),
    /// The player starting this lap
    Lap(i64),
    /// The player having completed this many laps
    Laps(i64),
    /// The query holding on this many readings in a row, each with new telemetry
    Condition { sql: String, confirm: u32 },
}

impl Trigger {
    fn describe(&self) -> String {
        match self {
            Trigger::Time(at) => format!("at session time {}", clock(*at)),
            Trigger::Lap(lap) => format!("when the pilot starts lap {lap}"),
            Trigger::Laps(laps) => format!("once the pilot has completed {laps} laps"),
            Trigger::Condition { sql, confirm } => {
                format!("when this holds on {confirm} readings in a row: {sql}")
            }
        }
    }
}

/// What made a request due, as it was then.
#[derive(Debug, Clone, Default, PartialEq)]
struct Observation {
    session_time: Option<f64>,
    lap: Option<i64>,
    /// What the condition said it saw
    detail: Option<String>,
}

struct Request {
    id: Id,
    kind: Kind,
    message: String,
    summary: String,
    trigger: Trigger,
    /// The language it's said in: the one the turn that set it answered in
    lang: ArrayStr<2>,
    /// The session it belongs to (see [`Reading::session`])
    session: (i64, String),
    /// The session clock and the player's lap when it was set
    set_at: Option<f64>,
    set_lap: Option<i64>,
    /// The trigger as asked for (`after_laps 2`): the same request asked for again soon after
    /// is told apart by it, its trigger worked out again having moved on
    asked: String,
    created: Instant,
    /// The turn that registered it, until that turn's answered: until then it doesn't count
    provisional: Option<u64>,
    /// The turn cancelling it, until that turn's answered
    cancelling: Option<u64>,
    state: State,
}

impl Request {
    fn active(&self) -> bool {
        self.provisional.is_none() && self.cancelling.is_none()
    }

    fn line(&self, out: &mut String) {
        let _ = write!(
            out,
            "#{} {} {:?}: due {}",
            self.id,
            self.kind.name(),
            self.summary,
            self.trigger.describe()
        );
        if self.kind == Kind::Reminder {
            let _ = write!(out, ", saying {:?}", self.message);
        } else {
            let _ = write!(out, ", asking {:?}", self.message);
        }
        let _ = match &self.state {
            State::Waiting { .. } => write!(out, "; waiting"),
            State::Due(_) => write!(out, "; due, waiting for the radio"),
        };
        if self.provisional.is_some() {
            out.push_str(" (registered in this answer)");
        }
        if self.cancelling.is_some() {
            out.push_str(" (being cancelled in this answer)");
        }
        out.push('\n');
    }
}

enum State {
    Waiting {
        /// Readings in a row its condition has held on
        streak: u32,
        /// The newest telemetry row of the last reading counted
        last_row: Option<i64>,
        /// Readings in a row its condition failed on
        failures: u32,
    },
    Due(Due),
}

struct Due {
    since: Instant,
    observed: Observation,
    attempts: u32,
    /// Not tried again before this, after it was cut off
    not_before: Option<Instant>,
    /// An assessment's answer, when it was cut off, and when it was written
    said: Option<(String, Instant)>,
    /// Being delivered now
    delivering: bool,
}

/// What came of something: the pilot's told in the chat.
#[derive(Debug, Clone, PartialEq)]
pub enum Event {
    /// The turn that registered it was answered
    Registered {
        id: Id,
        summary: String,
    },
    /// The turn that cancelled it was answered
    Cancelled {
        id: Id,
        summary: String,
    },
    /// The turn that registered it wasn't answered: it was never registered
    Dropped {
        id: Id,
        summary: String,
    },
    /// The turn that cancelled it wasn't answered: it's still there
    NotCancelled {
        id: Id,
        summary: String,
    },
    Ended {
        id: Id,
        summary: String,
        why: Ended,
    },
    /// Its condition kept failing
    WatchFailed {
        id: Id,
        summary: String,
        error: String,
    },
    /// It was due for too long without the radio being free
    Overdue {
        id: Id,
        summary: String,
    },
    /// Every delivery was cut off, or failed
    Undelivered {
        id: Id,
        summary: String,
    },
}

/// Why a request ended without being delivered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ended {
    /// Another game session started
    Session,
    /// The recording stopped being followed: the game's stopped, or another's running
    Recording,
}

/// What to do with a request that's due.
#[derive(Debug, Clone, PartialEq)]
pub enum Deliver {
    /// Say this as it is
    Say(String),
    /// Ask the model this, and say its answer
    Ask(String),
}

/// A request to deliver now: see [`Schedule::next_due`].
#[derive(Debug, Clone, PartialEq)]
pub struct Delivery {
    pub id: Id,
    pub summary: String,
    pub lang: ArrayStr<2>,
    pub what: Deliver,
}

#[derive(Default)]
struct Book {
    requests: Vec<Request>,
    last_id: Id,
    /// The turn of the conversation going on, if it may register requests, and the language it
    /// answers in
    turn: Option<(u64, ArrayStr<2>)>,
    /// The session clock and the player's lap as last read
    clock: Option<(f64, Option<i64>)>,
}

/// The requests for later of one recording. Cheap to clone: the clones are the same requests.
///
/// Its lock is only held for a few comparisons at a time, never while waiting for anything.
#[derive(Clone, Default)]
pub struct Schedule(Arc<Mutex<Book>>);

impl Schedule {
    fn lock(&self) -> MutexGuard<'_, Book> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// A turn of the conversation starts: what it registers or cancels counts once it ends with an
    /// answer. A reminder it registers is said in `lang`.
    pub fn begin_turn(&self, turn: u64, lang: ArrayStr<2>) {
        self.lock().turn = Some((turn, lang));
    }

    /// The turn `turn` ended: `answered` (the pilot heard what it said), what it registered and
    /// cancelled counts; otherwise it's as if it never happened.
    pub fn end_turn(&self, turn: u64, answered: bool) -> Vec<Event> {
        let mut book = self.lock();
        if book.turn.is_some_and(|(current, _)| current == turn) {
            book.turn = None;
        }
        let mut events = Vec::new();
        book.requests.retain_mut(|r| {
            if r.cancelling == Some(turn) {
                r.cancelling = None;
                if answered {
                    events.push(Event::Cancelled {
                        id: r.id,
                        summary: r.summary.clone(),
                    });
                    return false;
                }
                events.push(Event::NotCancelled {
                    id: r.id,
                    summary: r.summary.clone(),
                });
                match &mut r.state {
                    // Not watched meanwhile: what it had seen in a row may not have held since
                    State::Waiting {
                        streak, last_row, ..
                    } => {
                        *streak = 0;
                        *last_row = None;
                    }
                    // Kept for this turn after its last try: it's not tried again
                    State::Due(due) if !due.delivering && due.attempts >= MAX_ATTEMPTS => {
                        events.push(Event::Undelivered {
                            id: r.id,
                            summary: r.summary.clone(),
                        });
                        return false;
                    }
                    State::Due(_) => {}
                }
            }
            if r.provisional == Some(turn) {
                if !answered {
                    log::info!("Request for later #{} dropped with its turn", r.id);
                    events.push(Event::Dropped {
                        id: r.id,
                        summary: r.summary.clone(),
                    });
                    return false;
                }
                r.provisional = None;
                events.push(Event::Registered {
                    id: r.id,
                    summary: r.summary.clone(),
                });
            }
            true
        });
        events
    }

    /// Ends every request: `why`.
    pub fn clear(&self, why: Ended) -> Vec<Event> {
        let mut book = self.lock();
        book.turn = None;
        std::mem::take(&mut book.requests)
            .into_iter()
            .filter(|r| r.provisional.is_none())
            .map(|r| Event::Ended {
                id: r.id,
                summary: r.summary,
                why,
            })
            .collect()
    }

    /// The request that's been due longest and can be delivered now, marked as being delivered:
    /// it's done once [`delivered`](Self::delivered), or tried again later once
    /// [`interrupted`](Self::interrupted).
    pub fn next_due(&self, now: Instant) -> Option<Delivery> {
        let mut book = self.lock();
        let clock = book.clock;
        let request = book
            .requests
            .iter_mut()
            .filter(|r| r.active())
            .filter(|r| match &r.state {
                State::Due(due) => {
                    !due.delivering && due.not_before.is_none_or(|retry| now >= retry)
                }
                State::Waiting { .. } => false,
            })
            .min_by_key(|r| match &r.state {
                State::Due(due) => due.since,
                State::Waiting { .. } => now,
            })?;
        let State::Due(due) = &request.state else {
            return None;
        };
        let what = match (&due.said, request.kind) {
            (Some((said, at)), _) if now.duration_since(*at) < REUSE_FOR => {
                Deliver::Say(said.clone())
            }
            (_, Kind::Reminder) => Deliver::Say(request.message.clone()),
            (_, Kind::Assessment) => Deliver::Ask(prompt(request, due, clock)),
        };
        if let State::Due(due) = &mut request.state {
            due.delivering = true;
        }
        Some(Delivery {
            id: request.id,
            summary: request.summary.clone(),
            lang: request.lang,
            what,
        })
    }

    /// The request `id` was delivered: it's done.
    pub fn delivered(&self, id: Id) {
        self.lock().requests.retain(|r| r.id != id);
        log::info!("Request for later #{id} delivered");
    }

    /// The delivery of `id` was cut off, or failed, with `said` written for it (and when) if it
    /// was: it's tried again shortly, unless it's been tried enough.
    pub fn interrupted(
        &self,
        id: Id,
        said: Option<(String, Instant)>,
        now: Instant,
    ) -> Option<Event> {
        let mut book = self.lock();
        // Cancelled meanwhile
        let index = book.requests.iter().position(|r| r.id == id)?;
        let cancelling = book.requests[index].cancelling.is_some();
        let State::Due(due) = &mut book.requests[index].state else {
            return None;
        };
        due.delivering = false;
        due.attempts += 1;
        if let Some(said) = said.filter(|(said, _)| !said.trim().is_empty()) {
            due.said = Some(said);
        }
        // One being cancelled is the turn cancelling it's to settle
        if due.attempts < MAX_ATTEMPTS || cancelling {
            due.not_before = Some(now + RETRY_AFTER);
            log::info!(
                "Request for later #{id} cut off: tried again shortly ({} of {MAX_ATTEMPTS})",
                due.attempts
            );
            return None;
        }
        let request = book.requests.remove(index);
        Some(Event::Undelivered {
            id,
            summary: request.summary,
        })
    }

    /// How many requests there are, of any kind.
    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.lock().requests.len()
    }

    /// A confirmed reminder of the current session, due now.
    #[cfg(test)]
    pub fn due_reminder(&self, message: &str) -> Id {
        let mut book = self.lock();
        book.last_id += 1;
        let id = book.last_id;
        book.requests.push(Request {
            id,
            kind: Kind::Reminder,
            message: message.into(),
            summary: format!("summary {id}"),
            trigger: Trigger::Time(0.0),
            lang: crate::arrstr!("en"),
            session: (1, String::new()),
            set_at: None,
            set_lap: None,
            asked: String::new(),
            created: Instant::now(),
            provisional: None,
            cancelling: None,
            state: State::Due(Due {
                since: Instant::now(),
                observed: Observation::default(),
                attempts: 0,
                not_before: None,
                said: None,
                delivering: false,
            }),
        });
        id
    }
}

impl Book {
    /// What the tool may register into: the turn going on, and its language.
    fn turn(&self) -> anyhow::Result<(u64, ArrayStr<2>)> {
        self.turn
            .context("there's no turn to register it in: nothing was registered")
    }

    /// The same request already there for `session`, if any: asked for the same way recently, or
    /// due at the same moment. One of a session that's over is as good as gone.
    fn duplicate(
        &self,
        session: &(i64, String),
        kind: Kind,
        message: &str,
        asked: &str,
        trigger: Option<&Trigger>,
        now: Instant,
    ) -> Option<&Request> {
        let same = |a: &str, b: &str| a.trim().eq_ignore_ascii_case(b.trim());
        self.requests.iter().find(|r| {
            r.cancelling.is_none()
                && r.session == *session
                && r.kind == kind
                && same(&r.message, message)
                && ((r.asked == asked
                    && now.duration_since(r.created) < DUPLICATE_WITHIN
                    // Not yet said: once it has been, asking again is asking for another
                    && match &r.state {
                        State::Waiting { .. } => true,
                        State::Due(due) => due.attempts == 0,
                    })
                    || trigger.is_some_and(|trigger| same_trigger(trigger, &r.trigger)))
        })
    }

    /// Whether there's room for another request, watching a condition or not.
    fn room(&self, condition: bool) -> anyhow::Result<()> {
        let kept = || self.requests.iter().filter(|r| r.cancelling.is_none());
        if kept().count() >= MAX_REQUESTS {
            bail!(
                "there are already {MAX_REQUESTS} requests for later, the most there can be: one must be cancelled first"
            );
        }
        let watching = kept()
            .filter(|r| matches!(r.trigger, Trigger::Condition { .. }))
            .count();
        if condition && watching >= MAX_WATCHED {
            bail!(
                "{MAX_WATCHED} conditions are already being watched, the most there can be: one must be cancelled first, or use a lap or a time"
            );
        }
        Ok(())
    }

    /// The conditions to read, if there's anything to read at all: a request that's due still
    /// ends with its session. With the last request registered, which the reading is for.
    fn watched(&self) -> Option<(Vec<Id>, Vec<String>, Id)> {
        self.requests.iter().find(|r| r.provisional.is_none())?;
        let (ids, conditions) = self
            .requests
            .iter()
            .filter(|r| r.active() && matches!(r.state, State::Waiting { .. }))
            .filter_map(|r| match &r.trigger {
                Trigger::Condition { sql, .. } => Some((r.id, sql.clone())),
                _ => None,
            })
            .take(MAX_WATCHED)
            .unzip();
        Some((ids, conditions, self.last_id))
    }

    /// Goes by a reading, of the conditions of `ids` (in order), asked for when `upto` was the
    /// last request registered: requests of another session end, and the ones whose moment it is
    /// are due. A request registered since may be of a newer session than the reading.
    fn observe(&mut self, ids: &[Id], upto: Id, reading: &Reading, now: Instant) -> Vec<Event> {
        let mut events = Vec::new();
        // Still reading: its session may not be the one going on
        if reading.catching_up {
            return events;
        }
        let Some(session) = &reading.session else {
            return events;
        };
        self.requests.retain(|r| {
            if &r.session == session || r.id > upto {
                return true;
            }
            // One being registered too: its answer may confirm it, but it's gone
            events.push(Event::Ended {
                id: r.id,
                summary: r.summary.clone(),
                why: Ended::Session,
            });
            false
        });
        // Paused or stopped: the session is known, but no moment has come
        if reading.stale {
            return events;
        }
        if let Some(time) = reading.session_time {
            self.clock = Some((time, reading.lap));
        }

        let mut failed = Vec::new();
        for r in self
            .requests
            .iter_mut()
            .filter(|r| r.active() && r.id <= upto)
        {
            let State::Waiting {
                streak,
                last_row,
                failures,
            } = &mut r.state
            else {
                continue;
            };
            let fired = match &r.trigger {
                Trigger::Time(at) => reading
                    .session_time
                    .is_some_and(|t| t >= *at)
                    .then_some(None),
                Trigger::Lap(lap) => reading.lap.is_some_and(|l| l >= *lap).then_some(None),
                Trigger::Laps(laps) => reading
                    .laps_completed
                    .is_some_and(|l| l >= *laps)
                    .then_some(None),
                Trigger::Condition { confirm, .. } => {
                    // Registered since the reading was asked for
                    let Some(verdict) = ids
                        .iter()
                        .position(|id| *id == r.id)
                        .and_then(|i| reading.conditions.get(i))
                    else {
                        continue;
                    };
                    match verdict {
                        // Found among tables with rows missing: it may be for want of them
                        // If they stay that way, it can't be watched
                        Ok(Some(Verdict { met: true, .. })) if reading.incomplete => {
                            *streak = 0;
                            *failures += 1;
                            if *failures >= MAX_FAILURES {
                                failed.push((
                                    r.id,
                                    "the current-state tables it reads kept missing rows".into(),
                                ));
                            }
                            None
                        }
                        Ok(Some(Verdict { met: true, detail })) => {
                            *failures = 0;
                            // Only new telemetry is more evidence
                            if reading.latest_telemetry_id.is_some()
                                && reading.latest_telemetry_id != *last_row
                            {
                                *streak += 1;
                                *last_row = reading.latest_telemetry_id;
                            }
                            (*streak >= *confirm).then(|| detail.clone())
                        }
                        // Not met, or it can't tell: what it had seen doesn't add up any more
                        Ok(_) => {
                            *streak = 0;
                            *failures = 0;
                            None
                        }
                        Err(e) => {
                            *streak = 0;
                            *failures += 1;
                            if *failures >= MAX_FAILURES {
                                failed.push((r.id, e.clone()));
                            }
                            None
                        }
                    }
                }
            };
            if let Some(detail) = fired {
                log::info!("Request for later #{} is due", r.id);
                r.state = State::Due(Due {
                    since: now,
                    observed: Observation {
                        session_time: reading.session_time,
                        lap: reading.lap,
                        detail,
                    },
                    attempts: 0,
                    not_before: None,
                    said: None,
                    delivering: false,
                });
            }
        }
        for (id, error) in failed {
            if let Some(index) = self.requests.iter().position(|r| r.id == id) {
                let request = self.requests.remove(index);
                events.push(Event::WatchFailed {
                    id,
                    summary: request.summary,
                    error,
                });
            }
        }
        events
    }

    /// Drops what's been due too long to still be worth saying.
    fn overdue(&mut self, now: Instant) -> Vec<Event> {
        let mut events = Vec::new();
        self.requests.retain(|r| match &r.state {
            // One being cancelled is the turn cancelling it's to settle
            _ if r.cancelling.is_some() => true,
            State::Due(due) if !due.delivering && now.duration_since(due.since) > DUE_FOR => {
                events.push(Event::Overdue {
                    id: r.id,
                    summary: r.summary.clone(),
                });
                false
            }
            _ => true,
        });
        events
    }

    fn list(&self) -> String {
        let mut out = String::from("Requests for later");
        if let Some((time, lap)) = self.clock {
            let _ = write!(out, " (session time now {}", clock(time));
            if let Some(lap) = lap {
                let _ = write!(out, ", the pilot on lap {lap}");
            }
            out.push(')');
        }
        out.push_str(":\n");
        if self.requests.is_empty() {
            out.push_str("none\n");
        }
        for request in &self.requests {
            request.line(&mut out);
        }
        out
    }

    /// Marks `id`, or all, as cancelled by the turn `turn`. A request registered in that same turn
    /// is simply gone.
    fn cancel(&mut self, turn: u64, id: Option<Id>, all: bool) -> anyhow::Result<String> {
        let chosen = |r: &Request| all || Some(r.id) == id;
        if !self
            .requests
            .iter()
            .any(|r| chosen(r) && r.cancelling.is_none())
        {
            let mut known = self.list();
            if id.is_none() && !all {
                bail!("`cancel` needs an `id`, or `all`. {known}");
            }
            known.insert_str(0, "there's no such request. ");
            bail!(known);
        }
        let mut out = String::new();
        self.requests.retain_mut(|r| {
            if !chosen(r) || r.cancelling.is_some() {
                return true;
            }
            let _ = writeln!(out, "Cancelling #{} {:?}", r.id, r.summary);
            if r.provisional == Some(turn) {
                return false;
            }
            r.cancelling = Some(turn);
            true
        });
        out.push_str("It's done once this answer ends: tell the pilot, in plain words.");
        Ok(out)
    }
}

/// Whether two triggers are the same moment.
fn same_trigger(a: &Trigger, b: &Trigger) -> bool {
    match (a, b) {
        (Trigger::Time(a), Trigger::Time(b)) => (a - b).abs() < 1.0,
        (a, b) => a == b,
    }
}

/// What the model's asked when an assessment is due.
fn prompt(r: &Request, due: &Due, now: Option<(f64, Option<i64>)>) -> String {
    let mut out = format!("[Scheduled request #{}, set by the pilot", r.id);
    if let Some(time) = r.set_at {
        let _ = write!(out, " at session time {}", clock(time));
    }
    if let Some(lap) = r.set_lap {
        let _ = write!(out, " on lap {lap}");
    }
    let _ = write!(
        out,
        ": {:?}]\nIt's due {}.",
        r.message,
        r.trigger.describe()
    );
    if let Trigger::Condition { .. } = r.trigger {
        let observed = &due.observed;
        out.push_str(" It was seen holding");
        if let Some(time) = observed.session_time {
            let _ = write!(out, " at session time {}", clock(time));
            if let Some((now, _)) = now.filter(|(now, _)| *now > time) {
                let _ = write!(out, " ({:.0} s ago)", now - time);
            }
        }
        if let Some(lap) = observed.lap {
            let _ = write!(out, " on lap {lap}");
        }
        if let Some(detail) = &observed.detail {
            let _ = write!(out, ", showing {detail}");
        }
        out.push('.');
    }
    out.push_str(
        " Answer it now on the radio, unprompted: say first what it's about, then answer from the \
         telemetry as it is now, briefly. It was set earlier: if what made it due has changed \
         since, say what happened and what's true now. Don't schedule it again.",
    );
    out
}

/// A session time, as `1:02:03` or `2:03`.
fn clock(seconds: f64) -> String {
    let seconds = seconds.max(0.0).round() as u64;
    let (h, m, s) = (seconds / 3600, seconds / 60 % 60, seconds % 60);
    match h {
        0 => format!("{m}:{s:02}"),
        h => format!("{h}:{m:02}:{s:02}"),
    }
}

/// Watches the requests of one recording, from the UI thread: the telemetry's read on the thread
/// keeping the derived tables, and this only looks whether it's been read.
pub struct Scheduler {
    schedule: Schedule,
    watcher: Watcher,
    /// The reading on its way, whose conditions it reads, and the last request registered then
    reading: Option<(Vec<Id>, Id, Task<anyhow::Result<Reading>>)>,
    /// When the last reading was asked for
    last: Option<Instant>,
}

impl Scheduler {
    pub fn new(watcher: Watcher) -> Self {
        Self {
            schedule: Schedule::default(),
            watcher,
            reading: None,
            last: None,
        }
    }

    #[inline]
    pub fn schedule(&self) -> &Schedule {
        &self.schedule
    }

    /// The tool the model keeps them with.
    pub fn tool(&self) -> ScheduleTool {
        ScheduleTool {
            schedule: self.schedule.clone(),
            watcher: self.watcher.clone(),
        }
    }

    /// Called every frame: the telemetry's read about once a second while there's something to
    /// watch, and what comes of it is said.
    pub fn update(&mut self, rt: &Handle) -> Vec<Event> {
        let now = Instant::now();
        let mut events = Vec::new();
        if let Some((_, _, task)) = &mut self.reading
            && let Some(result) = task.update(rt)
            && let Some((ids, upto, _)) = self.reading.take()
        {
            match result {
                Ok(reading) => events = self.schedule.lock().observe(&ids, upto, &reading, now),
                Err(e) => log::debug!("Requests for later couldn't read the telemetry: {e:#}"),
            }
        }
        let watched = self.schedule.lock().watched();
        if self.reading.is_none()
            && let Some((ids, conditions, upto)) = watched
            && self
                .last
                .is_none_or(|last| now.duration_since(last) >= POLL)
        {
            self.last = Some(now);
            let task = Task::spawn(self.watcher.watch(conditions));
            self.reading = Some((ids, upto, task));
        }
        events.extend(self.schedule.lock().overdue(now));
        events
    }
}

/// The tool the model registers, lists and cancels requests for later with.
#[derive(Clone)]
pub struct ScheduleTool {
    schedule: Schedule,
    watcher: Watcher,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Command {
    Add,
    List,
    Cancel,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ScheduleInput {
    command: Command,
    #[serde(default)]
    kind: Option<Kind>,
    #[serde(default)]
    message: Option<String>,
    #[serde(default)]
    summary: Option<String>,
    #[serde(default)]
    after_seconds: Option<f64>,
    #[serde(default)]
    at_lap: Option<i64>,
    #[serde(default)]
    after_laps: Option<i64>,
    #[serde(default)]
    when_sql: Option<String>,
    #[serde(default)]
    confirm_readings: Option<u32>,
    #[serde(default)]
    id: Option<Id>,
    #[serde(default)]
    all: Option<bool>,
}

/// A trigger as the model asked for it.
enum Asked {
    AfterSeconds(f64),
    AtLap(i64),
    AfterLaps(i64),
    When { sql: String, confirm: u32 },
}

impl Asked {
    fn new(input: &ScheduleInput) -> anyhow::Result<Self> {
        let given = [
            input.after_seconds.is_some(),
            input.at_lap.is_some(),
            input.after_laps.is_some(),
            input.when_sql.is_some(),
        ];
        if given.iter().filter(|given| **given).count() != 1 {
            bail!("give exactly one of `after_seconds`, `at_lap`, `after_laps` or `when_sql`");
        }
        if input.confirm_readings.is_some() && input.when_sql.is_none() {
            bail!("`confirm_readings` only goes with `when_sql`");
        }
        Ok(if let Some(seconds) = input.after_seconds {
            if !(seconds > 0.0 && seconds <= MAX_SECONDS) {
                bail!("`after_seconds` must be more than 0 and at most a day");
            }
            Asked::AfterSeconds(seconds)
        } else if let Some(lap) = input.at_lap {
            if !(1..=MAX_LAPS * 10).contains(&lap) {
                bail!("`at_lap` must be a lap number");
            }
            Asked::AtLap(lap)
        } else if let Some(laps) = input.after_laps {
            if !(1..=MAX_LAPS).contains(&laps) {
                bail!("`after_laps` must be at least 1");
            }
            Asked::AfterLaps(laps)
        } else {
            let sql = input.when_sql.as_deref().unwrap_or_default().trim();
            if sql.is_empty() || sql.len() > MAX_SQL_CHARS {
                bail!("`when_sql` must be one query of at most {MAX_SQL_CHARS} characters");
            }
            let confirm = input.confirm_readings.unwrap_or(DEFAULT_CONFIRM);
            if !(1..=MAX_CONFIRM).contains(&confirm) {
                bail!("`confirm_readings` must be from 1 to {MAX_CONFIRM}");
            }
            Asked::When {
                sql: sql.to_owned(),
                confirm,
            }
        })
    }

    fn key(&self) -> String {
        match self {
            Asked::AfterSeconds(seconds) => format!("after_seconds {seconds}"),
            Asked::AtLap(lap) => format!("at_lap {lap}"),
            Asked::AfterLaps(laps) => format!("after_laps {laps}"),
            Asked::When { sql, confirm } => format!("when_sql {confirm} {sql}"),
        }
    }
}

/// A text the model gave, on one line and not too long.
fn text(name: &str, text: Option<String>) -> anyhow::Result<String> {
    let text = text
        .map(|text| text.split_whitespace().collect::<Vec<_>>().join(" "))
        .unwrap_or_default();
    if text.is_empty() {
        bail!("`add` needs a `{name}`");
    }
    if text.chars().count() > MAX_TEXT_CHARS {
        bail!("`{name}` must be at most {MAX_TEXT_CHARS} characters");
    }
    Ok(text)
}

impl ScheduleTool {
    pub const NAME: &'static str = "schedule";

    pub fn description() -> String {
        "Requests for later: what the pilot wants said or answered once a moment comes, kept while \
         you talk about other things (the instructions on requests for later say how). `command`: \
         add (registers one), list, or cancel (by `id`, or `all`). Answers are plain text for you, \
         not to read out."
            .into()
    }

    pub fn input_schema() -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "command": {"type": "string", "enum": ["add", "list", "cancel"]},
                "kind": {
                    "type": "string",
                    "enum": ["reminder", "assessment"],
                    "description": "add: reminder (`message` said as it is) or assessment (`message` is a question you answer when it's due)."
                },
                "message": {
                    "type": "string",
                    "description": "add: a reminder's words, in the language you answer in; an assessment's question, in English."
                },
                "summary": {
                    "type": "string",
                    "description": "add: a few words naming it, in the language you answer in."
                },
                "after_seconds": {"type": "number", "description": "add: due after this much session time."},
                "at_lap": {"type": "integer", "description": "add: due when the pilot starts this lap."},
                "after_laps": {"type": "integer", "description": "add: due once this many more laps are completed."},
                "when_sql": {
                    "type": "string",
                    "description": "add: due when this SELECT's first column is 1 (0: not yet, NULL: can't tell); an optional second column says what was seen."
                },
                "confirm_readings": {
                    "type": "integer",
                    "description": "add, with when_sql: readings in a row it must hold on (default 2)."
                },
                "id": {"type": "integer", "description": "cancel: the request's number."},
                "all": {"type": "boolean", "description": "cancel: every request."}
            },
            "required": ["command"]
        })
    }

    async fn run(self, input: ScheduleInput) -> anyhow::Result<String> {
        match input.command {
            Command::List => Ok(self.schedule.lock().list()),
            Command::Cancel => {
                // Its turn, as for `add`: an answer cut off before this is read stops at the
                // yield (the next turn only begins once it's been told to stop), and one cut off
                // after is told so below
                let (turn, _) = self.schedule.lock().turn()?;
                tokio::task::yield_now().await;
                let mut book = self.schedule.lock();
                if book.turn()?.0 != turn {
                    bail!("this answer was cut off: nothing was cancelled");
                }
                book.cancel(turn, input.id, input.all.unwrap_or(false))
            }
            Command::Add => self.add(input).await,
        }
    }

    async fn add(self, input: ScheduleInput) -> anyhow::Result<String> {
        // Its turn: an answer cut off before this is read stops at the yield (the next turn only
        // begins once it's been told to stop), and one cut off after is told so before anything's
        // registered
        let (turn, lang) = self.schedule.lock().turn()?;
        tokio::task::yield_now().await;
        let kind = input
            .kind
            .context("`add` needs a `kind`: reminder or assessment")?;
        let message = text("message", input.message.clone())?;
        let summary = text("summary", input.summary.clone())?;
        let asked = Asked::new(&input)?;
        let key = asked.key();

        let conditions = match &asked {
            Asked::When { sql, .. } => vec![sql.clone()],
            _ => Vec::new(),
        };
        let reading = self
            .watcher
            .watch(conditions)
            .await
            .context("the telemetry can't be read now: nothing was registered")?;
        let session = reading
            .session
            .clone()
            .context("the telemetry has no session yet: nothing was registered")?;
        if reading.catching_up {
            bail!(
                "the telemetry is still being read, so where the pilot is now isn't known: \
                 nothing was registered. Try again in a moment"
            );
        }
        // Asked again: once is enough, whatever's true now, paused too. A lap or a condition is the
        // same one however long ago it was asked for (a time or a count of laps is from now: below).
        let same = match &asked {
            Asked::AtLap(lap) => Some(Trigger::Lap(*lap)),
            Asked::When { sql, confirm } => Some(Trigger::Condition {
                sql: sql.clone(),
                confirm: *confirm,
            }),
            Asked::AfterSeconds(_) | Asked::AfterLaps(_) => None,
        };
        if let Some(r) = self.schedule.lock().duplicate(
            &session,
            kind,
            &message,
            &key,
            same.as_ref(),
            Instant::now(),
        ) {
            return Ok(already(r));
        }
        // Paused, or between sessions: its clock and laps may be of a session that's over
        if reading.stale {
            bail!(
                "the telemetry isn't moving (the game's paused, or between sessions), so where \
                 the pilot is isn't known: nothing was registered. Tell the pilot it can be set \
                 once they're driving"
            );
        }
        let mut note = String::new();
        let trigger = match asked {
            Asked::AfterSeconds(seconds) => Trigger::Time(
                reading
                    .session_time
                    .context("the session clock isn't known yet: nothing was registered")?
                    + seconds,
            ),
            Asked::AtLap(lap) => {
                let now = reading
                    .lap
                    .context("the pilot's lap isn't known yet: nothing was registered")?;
                if now >= lap {
                    return Ok(format!(
                        "Not registered: the pilot is already on lap {now}. Answer now, or ask \
                         which lap they mean."
                    ));
                }
                Trigger::Lap(lap)
            }
            Asked::AfterLaps(laps) => Trigger::Laps(
                reading
                    .laps_completed
                    .context("the pilot's laps aren't known yet: nothing was registered")?
                    + laps,
            ),
            Asked::When { sql, confirm } => {
                match reading.conditions.first() {
                    Some(Err(e)) => {
                        bail!("the condition can't be watched: {e}. Nothing was registered")
                    }
                    // Unless the tables it read have rows missing: then it can't tell
                    Some(Ok(Some(Verdict { met: true, detail }))) if !reading.incomplete => {
                        let seen = detail
                            .as_ref()
                            .map(|detail| format!(", showing {detail}"))
                            .unwrap_or_default();
                        return Ok(format!(
                            "Not registered: the condition already holds now{seen}. Tell the \
                             pilot now instead."
                        ));
                    }
                    Some(Ok(None)) | None => note.push_str(
                        " It can't tell now (NULL, or no rows): it's watched until it can, and \
                         isn't due until it holds.",
                    ),
                    Some(Ok(Some(_))) => {}
                }
                Trigger::Condition { sql, confirm }
            }
        };

        let mut book = self.schedule.lock();
        // Asked again, perhaps while this call was reading: once is enough
        if let Some(r) = book.duplicate(
            &session,
            kind,
            &message,
            &key,
            Some(&trigger),
            Instant::now(),
        ) {
            return Ok(already(r));
        }
        if book.turn()?.0 != turn {
            bail!("this answer was cut off: nothing was registered");
        }
        book.room(matches!(trigger, Trigger::Condition { .. }))?;
        book.last_id += 1;
        let id = book.last_id;
        let due = trigger.describe();
        book.requests.push(Request {
            id,
            kind,
            message,
            summary: summary.clone(),
            trigger,
            lang,
            session,
            set_at: reading.session_time,
            set_lap: reading.lap,
            asked: key,
            created: Instant::now(),
            provisional: Some(turn),
            cancelling: None,
            state: State::Waiting {
                streak: 0,
                last_row: None,
                failures: 0,
            },
        });
        log::info!(
            "Request for later #{id} ({}) registered: due {due}",
            kind.name()
        );
        Ok(format!(
            "Registered #{id} ({}) {summary:?}: due {due}.{note} Confirm it to the pilot now, in a \
             few words: what you'll do, and when.",
            kind.name()
        ))
    }
}

/// What the model's told of a request asked for again.
fn already(r: &Request) -> String {
    let now = match r.state {
        State::Due(_) => " It's due now: it's said as soon as the radio's free.",
        State::Waiting { .. } => "",
    };
    format!(
        "Already registered as #{} {:?}, due {}: nothing new was added.{now}",
        r.id,
        r.summary,
        r.trigger.describe()
    )
}

impl ToolHandler for ScheduleTool {
    type Input = ScheduleInput;

    fn handle(
        &self,
        input: ScheduleInput,
    ) -> impl Future<Output = anyhow::Result<String>> + Send + 'static {
        self.clone().run(input)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::arrstr;

    fn reading(
        time: f64,
        lap: i64,
        row: i64,
        conditions: Vec<Result<Option<Verdict>, String>>,
    ) -> Reading {
        Reading {
            session: Some((1, "Test Ring|race|1".into())),
            session_time: Some(time),
            lap: Some(lap),
            laps_completed: Some(lap - 1),
            latest_telemetry_id: Some(row),
            stale: false,
            catching_up: false,
            incomplete: false,
            conditions,
        }
    }

    fn met(detail: &str) -> Result<Option<Verdict>, String> {
        Ok(Some(Verdict {
            met: true,
            detail: Some(detail.into()),
        }))
    }

    fn not_met() -> Result<Option<Verdict>, String> {
        Ok(Some(Verdict {
            met: false,
            detail: None,
        }))
    }

    /// A request registered by `turn` straight into the book.
    fn add(schedule: &Schedule, turn: u64, kind: Kind, trigger: Trigger) -> Id {
        let mut book = schedule.lock();
        book.last_id += 1;
        let id = book.last_id;
        book.requests.push(Request {
            id,
            kind,
            message: format!("message {id}"),
            summary: format!("summary {id}"),
            asked: format!("{trigger:?}"),
            trigger,
            lang: arrstr!("en"),
            session: (1, "Test Ring|race|1".into()),
            set_at: Some(10.0),
            set_lap: Some(2),
            created: Instant::now(),
            provisional: Some(turn),
            cancelling: None,
            state: State::Waiting {
                streak: 0,
                last_row: None,
                failures: 0,
            },
        });
        id
    }

    fn observe(schedule: &Schedule, ids: &[Id], reading: &Reading) -> Vec<Event> {
        schedule
            .lock()
            .observe(ids, Id::MAX, reading, Instant::now())
    }

    #[test]
    fn a_request_counts_only_once_its_turn_is_answered() {
        let schedule = Schedule::default();
        schedule.begin_turn(1, arrstr!("en"));
        let id = add(&schedule, 1, Kind::Reminder, Trigger::Time(20.0));
        // Not watched, nor due, while its turn is going on
        assert_eq!(schedule.lock().watched(), None);
        observe(&schedule, &[], &reading(30.0, 3, 1, vec![]));
        assert_eq!(schedule.next_due(Instant::now()), None);

        assert_eq!(
            schedule.end_turn(1, true),
            [Event::Registered {
                id,
                summary: "summary 1".into()
            }]
        );
        observe(&schedule, &[], &reading(30.0, 3, 2, vec![]));
        assert!(schedule.next_due(Instant::now()).is_some());

        // A turn cut off takes what it registered with it, and the pilot's told
        schedule.begin_turn(2, arrstr!("en"));
        let dropped = add(&schedule, 2, Kind::Reminder, Trigger::Time(40.0));
        assert_eq!(
            schedule.end_turn(2, false),
            [Event::Dropped {
                id: dropped,
                summary: "summary 2".into()
            }]
        );
        assert_eq!(schedule.len(), 1);
    }

    #[test]
    fn a_cancellation_counts_only_once_its_turn_is_answered() {
        let schedule = Schedule::default();
        let id = add(&schedule, 1, Kind::Reminder, Trigger::Lap(5));
        schedule.end_turn(1, true);

        for answered in [false, true] {
            schedule.begin_turn(2, arrstr!("en"));
            let said = schedule.lock().cancel(2, Some(id), false).unwrap();
            assert!(said.contains("#1"), "{said}");
            let events = schedule.end_turn(2, answered);
            assert_eq!(schedule.len(), usize::from(!answered));
            // Either way, the pilot's told
            let summary = "summary 1".into();
            match answered {
                true => assert_eq!(events, [Event::Cancelled { id, summary }]),
                false => assert_eq!(events, [Event::NotCancelled { id, summary }]),
            }
        }

        // Nothing there to cancel
        schedule.begin_turn(3, arrstr!("en"));
        assert!(schedule.lock().cancel(3, Some(9), false).is_err());
        let all = schedule.lock().cancel(3, None, true).unwrap_err();
        assert!(format!("{all}").contains("no such request"), "{all}");
    }

    #[test]
    fn one_being_cancelled_is_left_for_its_turn_to_settle() {
        let schedule = Schedule::default();
        let id = schedule.due_reminder("Reminder: box.");
        schedule.begin_turn(2, arrstr!("en"));
        schedule.lock().cancel(2, Some(id), false).unwrap();
        let late = Instant::now() + DUE_FOR * 2;
        assert_eq!(schedule.lock().overdue(late), []);
        assert_eq!(
            schedule.end_turn(2, true),
            [Event::Cancelled {
                id,
                summary: format!("summary {id}")
            }]
        );
    }

    #[test]
    fn one_tried_enough_whose_cancellation_fails_isnt_tried_again() {
        let schedule = Schedule::default();
        let id = schedule.due_reminder("Reminder: box.");
        let now = Instant::now();
        for _ in 1..MAX_ATTEMPTS {
            schedule.next_due(now + RETRY_AFTER * 10).unwrap();
            assert_eq!(schedule.interrupted(id, None, now), None);
        }
        schedule.next_due(now + RETRY_AFTER * 10).unwrap();
        schedule.begin_turn(2, arrstr!("en"));
        schedule.lock().cancel(2, Some(id), false).unwrap();
        // Its last try cut off while it's being cancelled: the cancelling turn settles it
        assert_eq!(schedule.interrupted(id, None, now), None);
        let summary = format!("summary {id}");
        assert_eq!(
            schedule.end_turn(2, false),
            [
                Event::NotCancelled {
                    id,
                    summary: summary.clone()
                },
                Event::Undelivered { id, summary }
            ]
        );
        assert_eq!(schedule.len(), 0);
    }

    #[test]
    fn a_condition_unwatched_while_a_cancellation_was_pending_starts_over() {
        let schedule = Schedule::default();
        let condition = Trigger::Condition {
            sql: "SELECT 1".into(),
            confirm: 2,
        };
        let id = add(&schedule, 1, Kind::Reminder, condition);
        schedule.end_turn(1, true);
        observe(&schedule, &[id], &reading(10.0, 2, 1, vec![met("yes")]));

        schedule.begin_turn(2, arrstr!("en"));
        schedule.lock().cancel(2, Some(id), false).unwrap();
        // What it said meanwhile isn't known
        assert_eq!(schedule.lock().watched().unwrap().0, Vec::<Id>::new());
        schedule.end_turn(2, false);

        observe(&schedule, &[id], &reading(12.0, 2, 3, vec![met("yes")]));
        assert_eq!(schedule.next_due(Instant::now()), None);
        observe(&schedule, &[id], &reading(13.0, 2, 4, vec![met("yes")]));
        assert_eq!(schedule.next_due(Instant::now()).unwrap().id, id);
    }

    #[test]
    fn times_and_laps_come_due_and_wait_while_the_telemetry_is_stale() {
        let schedule = Schedule::default();
        let time = add(&schedule, 1, Kind::Reminder, Trigger::Time(100.0));
        let lap = add(&schedule, 1, Kind::Reminder, Trigger::Lap(4));
        let laps = add(&schedule, 1, Kind::Reminder, Trigger::Laps(5));
        schedule.end_turn(1, true);

        let mut stale = reading(500.0, 10, 1, vec![]);
        stale.stale = true;
        observe(&schedule, &[], &stale);
        assert_eq!(schedule.next_due(Instant::now()), None);

        observe(&schedule, &[], &reading(99.0, 3, 2, vec![]));
        assert_eq!(schedule.next_due(Instant::now()), None);

        observe(&schedule, &[], &reading(100.0, 4, 3, vec![]));
        let first = schedule.next_due(Instant::now()).unwrap();
        let second = schedule.next_due(Instant::now()).unwrap();
        assert_eq!(
            [first.id, second.id]
                .into_iter()
                .collect::<std::collections::BTreeSet<_>>(),
            [time, lap].into()
        );
        assert_eq!(first.what, Deliver::Say(format!("message {}", first.id)));
        // Being delivered: not handed out twice
        assert_eq!(schedule.next_due(Instant::now()), None);

        // Lap 6 started: 5 completed
        observe(&schedule, &[], &reading(300.0, 6, 4, vec![]));
        assert_eq!(schedule.next_due(Instant::now()).unwrap().id, laps);

        // Paused in the next session: what's left of this one still ends with it
        let mut next = reading(1.0, 1, 0, vec![]);
        next.stale = true;
        next.session = Some((900, "Test Ring|race|2".into()));
        observe(&schedule, &[], &next);
        assert_eq!(schedule.len(), 0);
    }

    #[test]
    fn a_condition_needs_new_evidence_in_a_row_and_is_never_proven_by_missing_data() {
        let schedule = Schedule::default();
        let id = add(
            &schedule,
            1,
            Kind::Assessment,
            Trigger::Condition {
                sql: "SELECT 1".into(),
                confirm: 2,
            },
        );
        schedule.end_turn(1, true);
        let (ids, conditions, _) = schedule.lock().watched().unwrap();
        assert_eq!((ids.as_slice(), conditions.len()), (&[id][..], 1));

        let due = |schedule: &Schedule| schedule.next_due(Instant::now());
        observe(&schedule, &ids, &reading(10.0, 2, 1, vec![met("0.9 s")]));
        // The same rows read again aren't more evidence
        observe(&schedule, &ids, &reading(10.0, 2, 1, vec![met("0.9 s")]));
        assert_eq!(due(&schedule), None);
        // Nor is not knowing: it starts over
        observe(&schedule, &ids, &reading(11.0, 2, 2, vec![Ok(None)]));
        observe(&schedule, &ids, &reading(12.0, 2, 3, vec![met("0.9 s")]));
        assert_eq!(due(&schedule), None);
        observe(&schedule, &ids, &reading(13.0, 2, 4, vec![not_met()]));
        observe(&schedule, &ids, &reading(14.0, 2, 5, vec![met("0.8 s")]));
        assert_eq!(due(&schedule), None);
        // Nor is what's found with rows missing
        let mut incomplete = reading(14.5, 2, 6, vec![met("0.1 s")]);
        incomplete.incomplete = true;
        observe(&schedule, &ids, &incomplete);
        assert_eq!(due(&schedule), None);
        observe(&schedule, &ids, &reading(14.8, 2, 7, vec![met("0.8 s")]));
        assert_eq!(due(&schedule), None);
        observe(&schedule, &ids, &reading(15.0, 2, 8, vec![met("0.7 s")]));
        // Later, when the radio is free
        schedule.lock().clock = Some((40.0, Some(2)));
        let Some(Delivery {
            what: Deliver::Ask(prompt),
            ..
        }) = due(&schedule)
        else {
            panic!("an assessment is asked");
        };
        assert!(prompt.starts_with("[Scheduled request #1"), "{prompt}");
        assert!(prompt.contains("message 1"), "{prompt}");
        assert!(
            prompt.contains("at session time 0:15 (25 s ago)"),
            "{prompt}"
        );
        assert!(prompt.contains("showing 0.7 s"), "{prompt}");
    }

    #[test]
    fn a_condition_that_keeps_failing_stops_being_watched() {
        let schedule = Schedule::default();
        let condition = Trigger::Condition {
            sql: "SELECT nope".into(),
            confirm: 1,
        };
        let id = add(&schedule, 1, Kind::Assessment, condition);
        schedule.end_turn(1, true);
        let mut events = Vec::new();
        for row in 0..MAX_FAILURES {
            events = observe(
                &schedule,
                &[id],
                &reading(10.0, 2, row.into(), vec![Err("no such column".into())]),
            );
        }
        assert_eq!(
            events,
            [Event::WatchFailed {
                id,
                summary: "summary 1".into(),
                error: "no such column".into()
            }]
        );
        assert_eq!(schedule.len(), 0);
    }

    #[test]
    fn another_session_ends_the_requests_of_the_last() {
        let schedule = Schedule::default();
        let id = add(&schedule, 1, Kind::Reminder, Trigger::Lap(10));
        schedule.end_turn(1, true);
        let mut next = reading(5.0, 1, 1, vec![]);
        next.session = Some((900, "Test Ring|race|2".into()));
        assert_eq!(
            observe(&schedule, &[], &next),
            [Event::Ended {
                id,
                summary: "summary 1".into(),
                why: Ended::Session
            }]
        );
        assert_eq!(schedule.len(), 0);
    }

    #[test]
    fn a_due_request_still_ends_with_its_session_and_a_reading_older_than_a_request_doesnt() {
        let schedule = Schedule::default();
        let due = add(&schedule, 1, Kind::Reminder, Trigger::Lap(2));
        schedule.end_turn(1, true);
        observe(&schedule, &[], &reading(5.0, 2, 1, vec![]));
        // Due, and still read for its session
        let (ids, _, upto) = schedule.lock().watched().unwrap();
        assert_eq!((ids.len(), upto), (0, due));

        // Registered in the next session while that reading was on its way
        schedule.begin_turn(2, arrstr!("en"));
        let newer = add(&schedule, 2, Kind::Reminder, Trigger::Lap(9));
        schedule.lock().requests[1].session = (900, "Test Ring|race|2".into());
        let mut next = reading(6.0, 2, 2, vec![]);
        next.session = Some((1, "Test Ring|race|1".into()));
        next.lap = Some(9);
        assert_eq!(
            schedule.lock().observe(&ids, upto, &next, Instant::now()),
            []
        );
        assert_eq!(schedule.len(), 2);
        schedule.end_turn(2, true);

        next.session = Some((900, "Test Ring|race|2".into()));
        assert_eq!(
            schedule.lock().observe(&[], newer, &next, Instant::now()),
            [Event::Ended {
                id: due,
                summary: "summary 1".into(),
                why: Ended::Session
            }]
        );
        assert_eq!(schedule.len(), 1);
    }

    #[test]
    fn a_cut_off_delivery_is_tried_again_and_then_reported() {
        let schedule = Schedule::default();
        let id = add(&schedule, 1, Kind::Assessment, Trigger::Time(1.0));
        schedule.end_turn(1, true);
        observe(&schedule, &[], &reading(2.0, 1, 1, vec![]));
        let start = Instant::now();

        assert!(matches!(
            schedule.next_due(start).unwrap().what,
            Deliver::Ask(_)
        ));
        // Its answer was written, then cut off: said again as it was, once it's time to retry
        assert_eq!(
            schedule.interrupted(id, Some(("We can finish.".into(), start)), start),
            None
        );
        assert_eq!(schedule.next_due(start), None);
        let retry = start + RETRY_AFTER;
        assert_eq!(
            schedule.next_due(retry).unwrap().what,
            Deliver::Say("We can finish.".into())
        );
        // Too old to say again: asked again
        assert_eq!(schedule.interrupted(id, None, retry), None);
        let later = start + REUSE_FOR + RETRY_AFTER * 2;
        assert!(matches!(
            schedule.next_due(later).unwrap().what,
            Deliver::Ask(_)
        ));
        assert_eq!(
            schedule.interrupted(id, None, later),
            Some(Event::Undelivered {
                id,
                summary: "summary 1".into()
            })
        );
        assert_eq!(schedule.len(), 0);
    }

    #[test]
    fn delivered_and_overdue_requests_are_done() {
        let schedule = Schedule::default();
        let first = add(&schedule, 1, Kind::Reminder, Trigger::Time(1.0));
        let second = add(&schedule, 1, Kind::Reminder, Trigger::Time(1.0));
        schedule.end_turn(1, true);
        observe(&schedule, &[], &reading(2.0, 1, 1, vec![]));
        let delivery = schedule.next_due(Instant::now()).unwrap();
        assert_eq!(delivery.id, first);
        schedule.delivered(first);
        assert_eq!(
            schedule
                .lock()
                .overdue(Instant::now() + DUE_FOR + Duration::from_secs(1)),
            [Event::Overdue {
                id: second,
                summary: "summary 2".into()
            }]
        );
        assert_eq!(schedule.len(), 0);
    }

    #[test]
    fn clearing_reports_what_was_confirmed() {
        let schedule = Schedule::default();
        let id = add(&schedule, 1, Kind::Reminder, Trigger::Time(1.0));
        schedule.end_turn(1, true);
        add(&schedule, 2, Kind::Reminder, Trigger::Time(2.0));
        assert_eq!(
            schedule.clear(Ended::Recording),
            [Event::Ended {
                id,
                summary: "summary 1".into(),
                why: Ended::Recording
            }]
        );
        assert_eq!(schedule.len(), 0);
    }

    #[test]
    fn limits_and_duplicates_are_kept() {
        let schedule = Schedule::default();
        for lap in 0..MAX_REQUESTS {
            add(&schedule, 1, Kind::Reminder, Trigger::Lap(lap as i64 + 10));
        }
        let book = schedule.lock();
        assert!(book.room(false).is_err());
        let now = Instant::now();
        let session = (1, "Test Ring|race|1".to_owned());
        assert!(
            book.duplicate(
                &session,
                Kind::Reminder,
                " MESSAGE 3",
                "nothing alike",
                Some(&Trigger::Lap(12)),
                now
            )
            .is_some()
        );
        assert!(
            book.duplicate(
                &session,
                Kind::Reminder,
                "message 3",
                "nothing alike",
                Some(&Trigger::Lap(13)),
                now
            )
            .is_none()
        );
        assert!(
            book.duplicate(
                &session,
                Kind::Assessment,
                "message 3",
                "Lap(12)",
                None,
                now
            )
            .is_none()
        );
        assert!(
            book.duplicate(&session, Kind::Reminder, "message 3", "Lap(12)", None, now)
                .is_some()
        );
        assert!(
            book.duplicate(
                &session,
                Kind::Reminder,
                "message 3",
                "Lap(12)",
                None,
                now + DUPLICATE_WITHIN
            )
            .is_none()
        );
        // One of a session that's over is no duplicate
        let over = (2, "Test Ring|race|2".to_owned());
        assert!(
            book.duplicate(&over, Kind::Reminder, "message 3", "Lap(12)", None, now)
                .is_none()
        );
    }

    #[test]
    fn the_tool_refuses_without_a_turn() {
        let schedule = Schedule::default();
        assert!(schedule.lock().turn().is_err());
        schedule.begin_turn(4, arrstr!("es"));
        assert_eq!(schedule.lock().turn().unwrap(), (4, arrstr!("es")));
        schedule.end_turn(4, true);
        assert!(schedule.lock().turn().is_err());
    }

    #[test]
    fn triggers_are_asked_for_one_at_a_time() {
        let input =
            |json: serde_json::Value| -> ScheduleInput { serde_json::from_value(json).unwrap() };
        for bad in [
            serde_json::json!({"command": "add"}),
            serde_json::json!({"command": "add", "at_lap": 3, "after_laps": 2}),
            serde_json::json!({"command": "add", "after_seconds": -1.0}),
            serde_json::json!({"command": "add", "after_laps": 0}),
            serde_json::json!({"command": "add", "at_lap": 3, "confirm_readings": 2}),
            serde_json::json!({"command": "add", "when_sql": "SELECT 1", "confirm_readings": 0}),
        ] {
            assert!(Asked::new(&input(bad.clone())).is_err(), "{bad}");
        }
        let when = Asked::new(&input(
            serde_json::json!({"command": "add", "when_sql": " SELECT 1 "}),
        ))
        .unwrap();
        assert_eq!(when.key(), "when_sql 2 SELECT 1");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn the_tool_registers_only_what_it_can_watch_and_only_once() {
        let (_dir, engineer) = super::super::tool::tests::race();
        // Caught up: a reading while the tables are still being read registers nothing
        engineer.sync().await.unwrap();
        let scheduler = Scheduler::new(engineer.watcher());
        let schedule = scheduler.schedule().clone();
        let tool = scheduler.tool();
        let run = |json: serde_json::Value| tool.handle(serde_json::from_value(json).unwrap());
        let add = |when: serde_json::Value| {
            let mut json = serde_json::json!({
                "command": "add",
                "kind": "assessment",
                "message": "Is there a third car?",
                "summary": "third car",
            });
            json.as_object_mut()
                .unwrap()
                .extend(when.as_object().unwrap().clone());
            run(json)
        };

        // With no turn going on, nothing's registered
        let early = add(serde_json::json!({"after_laps": 1})).await.unwrap_err();
        assert!(format!("{early}").contains("no turn"), "{early}");
        schedule.begin_turn(1, arrstr!("en"));

        let now =
            add(serde_json::json!({"when_sql": "SELECT COUNT(*) = 2, COUNT(*) FROM cars_now"}))
                .await
                .unwrap();
        assert!(now.starts_with("Not registered"), "{now}");
        assert!(now.contains("showing 2"), "{now}");
        let broken = add(serde_json::json!({"when_sql": "SELECT nope FROM cars_now"}))
            .await
            .unwrap_err();
        assert!(format!("{broken}").contains("can't be watched"), "{broken}");
        let passed = add(serde_json::json!({"at_lap": 1})).await.unwrap();
        assert!(passed.starts_with("Not registered"), "{passed}");
        assert_eq!(schedule.len(), 0);

        let watched = serde_json::json!({"when_sql": "SELECT COUNT(*) > 2 FROM cars_now"});
        let registered = add(watched.clone()).await.unwrap();
        assert!(registered.starts_with("Registered #1"), "{registered}");
        // Asked again (a call repeated, or a turn tried again): once is enough
        let again = add(watched).await.unwrap();
        assert!(again.starts_with("Already registered as #1"), "{again}");
        let laps = add(serde_json::json!({"after_laps": 2})).await.unwrap();
        assert!(laps.starts_with("Registered #2"), "{laps}");

        let list = run(serde_json::json!({"command": "list"})).await.unwrap();
        assert!(
            list.contains("#1 assessment") && list.contains("#2 assessment"),
            "{list}"
        );
        assert!(list.contains("registered in this answer"), "{list}");

        assert_eq!(schedule.end_turn(1, true).len(), 2);
        let late = add(serde_json::json!({"after_seconds": 30}))
            .await
            .unwrap_err();
        assert!(format!("{late}").contains("no turn"), "{late}");
        assert_eq!(schedule.len(), 2);
    }

    #[test]
    fn clocks_read_like_the_session_timer() {
        assert_eq!(clock(5.4), "0:05");
        assert_eq!(clock(605.0), "10:05");
        assert_eq!(clock(3725.0), "1:02:05");
    }
}
