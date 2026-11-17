//! What the engineer's scheduled requests count from: the game clock and the laps the pilot
//! completes, added up across sessions, and the latest result of every condition.

use super::*;
use std::collections::BTreeSet;

/// A scheduled request's condition, as the data thread knows it
pub type ConditionId = u64;

/// The latest check of a condition
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConditionState {
    /// The statement returned at least one row
    True,
    /// The statement returned no rows
    False,
    /// The statement failed
    Failed,
}

/// What the data thread has measured, published about once a second for the engineer's schedule
// Read by the schedule, which a later task adds.
#[allow(dead_code)]
#[derive(Clone, Debug, Default)]
pub struct Progress {
    /// Goes up with every snapshot, so a reader can tell a new one from one it has seen
    pub seq: u64,
    /// The recording's current weekend
    pub weekend: Option<i64>,
    /// The recording's current epoch
    pub epoch: Option<i64>,
    /// The reasons of the epochs started since the last snapshot, oldest first
    pub new_epochs: Vec<String>,
    /// The current session's type
    pub session_type: Option<String>,
    /// Seconds of game clock, added up across sessions (see [`Counters`])
    pub clock_s: f64,
    /// Laps the pilot completed, added up across sessions (see [`Counters`])
    pub laps: u64,
    /// The latest check of every condition
    pub conditions: BTreeMap<ConditionId, ConditionState>,
    /// The conditions checked for this snapshot: the others weren't reached within the tick's
    /// budget, and their result in `conditions` is an older one
    pub checked: BTreeSet<ConditionId>,
}

/// The furthest the session clock can step back without the recorder starting a new epoch (see
/// `went_back` in `bono_sim::recorder`), in seconds
const SMALL_REWIND_S: f64 = 1.0;

/// The game clock and the laps the pilot completes, added up across sessions. Each observation
/// adds how far the session clock and the lap count moved forward since the last one, only within
/// one epoch: a new epoch, or a missing value, starts a new baseline instead of a jump, and a
/// paused or rewound clock adds nothing.
#[derive(Debug, Default)]
pub(crate) struct Counters {
    epoch: Option<i64>,
    session_time_s: Option<f64>,
    laps_completed: Option<i64>,
    clock_s: f64,
    laps: u64,
}

impl Counters {
    /// Takes the newest values of the pilot's session clock and completed laps
    pub(crate) fn observe(
        &mut self,
        epoch: Option<i64>,
        session_time_s: Option<f64>,
        laps_completed: Option<i64>,
    ) {
        let mut session_time_s = session_time_s;
        if epoch == self.epoch {
            if let (Some(last), Some(now)) = (self.session_time_s, session_time_s) {
                if now > last {
                    self.clock_s += now - last;
                } else if last - now <= SMALL_REWIND_S {
                    // The recorder keeps a step back this small in the same epoch: counting on
                    // from it would count the same stretch of clock twice
                    session_time_s = Some(last);
                }
            }
            if let (Some(last), Some(now)) = (self.laps_completed, laps_completed)
                && now > last
            {
                self.laps += (now - last) as u64;
            }
        }
        self.epoch = epoch;
        self.session_time_s = session_time_s;
        self.laps_completed = laps_completed;
    }

    /// Seconds of game clock counted so far
    pub(crate) fn clock_s(&self) -> f64 {
        self.clock_s
    }

    /// Laps completed so far
    pub(crate) fn laps(&self) -> u64 {
        self.laps
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clock_adds_forward_time_in_one_epoch() {
        let mut counters = Counters::default();
        counters.observe(Some(1), Some(10.0), None);
        counters.observe(Some(1), Some(12.5), None);
        assert_eq!(counters.clock_s(), 2.5);
    }

    #[test]
    fn paused_or_backwards_clock_adds_nothing() {
        let mut counters = Counters::default();
        counters.observe(Some(1), Some(10.0), None);
        counters.observe(Some(1), Some(10.0), None);
        counters.observe(Some(1), Some(9.0), None);
        assert_eq!(counters.clock_s(), 0.0);
    }

    #[test]
    fn new_epoch_restarts_the_baseline() {
        let mut counters = Counters::default();
        counters.observe(Some(1), Some(100.0), None);
        counters.observe(Some(1), Some(110.0), None);
        counters.observe(Some(2), Some(0.0), None);
        counters.observe(Some(2), Some(5.0), None);
        assert_eq!(counters.clock_s(), 15.0);
    }

    #[test]
    fn new_epoch_starting_higher_adds_nothing() {
        // The writer resuming: a new epoch that carries on from further along
        let mut counters = Counters::default();
        counters.observe(Some(1), Some(100.0), Some(3));
        counters.observe(Some(2), Some(400.0), Some(5));
        assert_eq!((counters.clock_s(), counters.laps()), (0.0, 0));
        counters.observe(Some(2), Some(401.0), Some(6));
        assert_eq!((counters.clock_s(), counters.laps()), (1.0, 1));
    }

    #[test]
    fn epochless_recordings_keep_counting() {
        let mut counters = Counters::default();
        counters.observe(None, Some(10.0), Some(1));
        counters.observe(None, Some(12.0), Some(2));
        assert_eq!((counters.clock_s(), counters.laps()), (2.0, 1));
    }

    #[test]
    fn small_rewind_is_not_counted_twice() {
        let mut counters = Counters::default();
        counters.observe(Some(1), Some(100.0), None);
        counters.observe(Some(1), Some(99.2), None);
        counters.observe(Some(1), Some(100.5), None);
        assert!((counters.clock_s() - 0.5).abs() < 1e-9);

        // Further back is a new baseline, as the recorder would start a new epoch there
        counters.observe(Some(1), Some(50.0), None);
        counters.observe(Some(1), Some(51.0), None);
        assert!((counters.clock_s() - 1.5).abs() < 1e-9);
    }

    #[test]
    fn laps_count_completions_across_sessions() {
        let mut counters = Counters::default();
        for laps in [3, 4, 5] {
            counters.observe(Some(1), None, Some(laps));
        }
        for laps in [0, 1] {
            counters.observe(Some(2), None, Some(laps));
        }
        assert_eq!(counters.laps(), 3);
    }

    #[test]
    fn missing_values_add_nothing() {
        let mut counters = Counters::default();
        counters.observe(Some(1), Some(10.0), Some(2));
        counters.observe(Some(1), Some(11.0), Some(3));
        counters.observe(Some(1), None, None);
        assert_eq!((counters.clock_s(), counters.laps()), (1.0, 1));

        // The values after the gap are a new baseline, not a jump from before it.
        counters.observe(Some(1), Some(20.0), Some(6));
        assert_eq!((counters.clock_s(), counters.laps()), (1.0, 1));
        counters.observe(Some(1), Some(21.5), Some(7));
        assert_eq!((counters.clock_s(), counters.laps()), (2.5, 2));
    }
}
