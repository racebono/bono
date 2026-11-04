//! LMU's track limits as the shared incident columns. LMU has no collision penalties, so its track
//! limits play the part of other simulators' incident points: both count offences towards a
//! penalty. The raw steps stay in their own columns, so this mapping can be checked or undone.
//!
//! Telemetry has a car's steps and scoring has how many make a point, in separate updates.

use bono_sim::sys::{bono_session_info, bono_telemetry};
use std::sync::atomic::{AtomicI64, Ordering};

/// The session's steps per point, 0 until it's known.
#[derive(Default)]
pub struct TrackLimits(AtomicI64);

impl TrackLimits {
    /// Forgets the rule, for a new event.
    pub fn clear(&self) {
        self.0.store(0, Ordering::Relaxed);
    }

    /// Takes the session's rule, and fills in how many points get a penalty.
    pub fn fill_session(&self, session: &mut bono_session_info) {
        let per_point = session.track_limits_steps_per_point.get().unwrap_or(0);
        self.0.store(per_point, Ordering::Relaxed);
        session.max_incident_points = session
            .track_limits_steps_per_penalty
            .get()
            .filter(|per_penalty| *per_penalty > 0)
            .and_then(|per_penalty| points(per_penalty, per_point))
            .into();
    }

    /// Fills in the car's points. Steps needn't make whole points, so they can be fractions.
    pub fn fill_telemetry(&self, telemetry: &mut bono_telemetry) {
        let per_point = self.0.load(Ordering::Relaxed);
        telemetry.state.incident_points = telemetry
            .state
            .track_limits_steps
            .get()
            .and_then(|steps| points(steps, per_point))
            .into();
    }
}

/// `None` when the session has no steps per point.
fn points(steps: i64, per_point: i64) -> Option<f64> {
    (per_point > 0).then(|| steps as f64 / per_point as f64)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session(per_point: Option<i64>, per_penalty: Option<i64>) -> bono_session_info {
        let mut session = bono_session_info::default();
        session.track_limits_steps_per_point = per_point.into();
        session.track_limits_steps_per_penalty = per_penalty.into();
        session
    }

    fn incident_points(limits: &TrackLimits, steps: Option<i64>) -> Option<f64> {
        let mut telemetry = bono_telemetry::default();
        telemetry.state.track_limits_steps = steps.into();
        limits.fill_telemetry(&mut telemetry);
        telemetry.state.incident_points.get()
    }

    #[test]
    fn steps_become_points_keeping_fractions() {
        let limits = TrackLimits::default();
        let mut rules = session(Some(4), Some(20));
        limits.fill_session(&mut rules);
        assert_eq!(rules.max_incident_points.get(), Some(5.0));
        assert_eq!(incident_points(&limits, Some(0)), Some(0.0));
        assert_eq!(incident_points(&limits, Some(3)), Some(0.75));
        assert_eq!(incident_points(&limits, Some(8)), Some(2.0));
        assert_eq!(incident_points(&limits, None), None);
    }

    #[test]
    fn not_known_until_the_session_says() {
        let limits = TrackLimits::default();
        assert_eq!(incident_points(&limits, Some(3)), None);

        limits.fill_session(&mut session(Some(4), Some(20)));
        assert_eq!(incident_points(&limits, Some(3)), Some(0.75));
        limits.clear();
        assert_eq!(incident_points(&limits, Some(3)), None);
    }

    #[test]
    fn a_session_without_the_rule_has_no_points_or_limit() {
        for (per_point, per_penalty) in [
            (None, None),
            (Some(0), Some(20)),
            (None, Some(20)),
            (Some(4), Some(0)),
            (Some(4), None),
        ] {
            let limits = TrackLimits::default();
            let mut rules = session(per_point, per_penalty);
            limits.fill_session(&mut rules);
            assert_eq!(rules.max_incident_points.get(), None);
        }
        let limits = TrackLimits::default();
        limits.fill_session(&mut session(Some(0), Some(20)));
        assert_eq!(incident_points(&limits, Some(3)), None);
    }
}
