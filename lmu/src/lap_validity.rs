//! Whether each car's lap is invalidated, as LMU decides it: the SDK's own flag (`mLapInvalidated`)
//! doesn't say. Scoring has whether a lap's time counts, and telemetry the car's track-limit steps,
//! in separate updates.
//!
//! A lap is invalidated, for the rest of it, once either:
//! - its steps rise while its time isn't counting. The flag turns off as the car leaves the track,
//!   and back on if LMU forgives it; scoring the cut while it's off is how LMU is seen to decide.
//!   That's observed, not documented: a lap this misses still ends without an official time, which
//!   the laps table and the spotter check at the line.
//! - it starts not counting, away from the pits. LMU's guide says a late or serious offence
//!   invalidates the next lap too; an out lap doesn't count for being one.
//!
//! Only in sessions other than races (races give points instead), while green, for cars that
//! haven't finished.

use bono_sim::sys::{bono_session_info, bono_standing, bono_telemetry};
use std::{collections::HashMap, sync::Mutex};

/// Each car's lap, by vehicle id.
#[derive(Default)]
pub struct LapValidity(Mutex<HashMap<i64, Car>>);

#[derive(Default)]
struct Car {
    /// From the newest scoring: the laps completed, whether that lap's time counts, whether the
    /// car's in the pits, and whether its laps are judged at all
    scored_laps: Option<i64>,
    counting: Option<bool>,
    in_pits: bool,
    judged: bool,
    /// The telemetry lap the rest is about
    lap: Option<i64>,
    /// Its start was seen (not the lap the car was first seen on), in the pit lane or not
    seen_start: bool,
    began_in_pit_lane: bool,
    start_judged: bool,
    steps: Option<i64>,
    invalidated: bool,
}

impl LapValidity {
    /// Forgets every car, for a new session.
    pub fn clear(&self) {
        self.0.lock().unwrap_or_else(|e| e.into_inner()).clear();
    }

    /// Takes each car's newest scoring.
    pub fn fill_scoring(&self, session: &bono_session_info, standings: &[bono_standing]) {
        let judged = session.session_type.as_deref().is_some_and(|t| t != "race")
            && session.phase.as_deref() == Some("green");
        let mut cars = self.0.lock().unwrap_or_else(|e| e.into_inner());
        for standing in standings {
            let Some(vehicle) = standing.vehicle_id.get() else {
                continue;
            };
            let car = cars.entry(vehicle).or_default();
            car.scored_laps = standing.laps_completed.get();
            car.counting = standing.lap_time_counts.get();
            car.in_pits = standing.in_pits.get().unwrap_or(false);
            car.judged = judged
                && standing
                    .finish_status
                    .as_deref()
                    .is_none_or(|s| s == "none");
        }
    }

    /// Fills in whether the car's lap is invalidated.
    pub fn fill_telemetry(&self, telemetry: &mut bono_telemetry) {
        let Some(vehicle) = telemetry.vehicle_id.get() else {
            return;
        };
        let mut cars = self.0.lock().unwrap_or_else(|e| e.into_inner());
        let car = cars.entry(vehicle).or_default();
        let lap = telemetry.lap.get();
        if car.lap != lap {
            car.seen_start = car.lap.is_some();
            car.lap = lap;
            car.began_in_pit_lane = telemetry.in_pit_lane.get().unwrap_or(false);
            car.start_judged = false;
            car.invalidated = false;
        }
        // The lap counter and scoring tick over a moment apart: until then, scoring is the last lap's
        let this_lap = lap.is_some() && car.scored_laps == lap;
        let not_counting = this_lap && car.judged && car.counting == Some(false);
        if this_lap && !car.start_judged {
            car.start_judged = true;
            car.invalidated |=
                not_counting && car.seen_start && !car.began_in_pit_lane && !car.in_pits;
        }
        let steps = telemetry.state.track_limits_steps.get();
        if let (Some(before), Some(now)) = (car.steps, steps) {
            car.invalidated |= not_counting && now > before;
        }
        if steps.is_some() {
            car.steps = steps;
        }
        telemetry.lap_invalidated = Some(car.invalidated).into();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session(session_type: &str, phase: &str) -> bono_session_info {
        let mut session = bono_session_info::default();
        session.session_type = Some(session_type.to_owned()).into();
        session.phase = Some(phase.to_owned()).into();
        session
    }

    fn standing(
        vehicle: i64,
        laps: i64,
        counting: Option<bool>,
        in_pits: bool,
        finish_status: &str,
    ) -> bono_standing {
        let mut standing = bono_standing::default();
        standing.vehicle_id = Some(vehicle).into();
        standing.laps_completed = Some(laps).into();
        standing.lap_time_counts = counting.into();
        standing.in_pits = Some(in_pits).into();
        standing.finish_status = Some(finish_status.to_owned()).into();
        standing
    }

    /// A practice session's scoring, green, with the car still running.
    fn scoring(
        validity: &LapValidity,
        vehicle: i64,
        laps: i64,
        counting: Option<bool>,
        in_pits: bool,
    ) {
        validity.fill_scoring(
            &session("practice", "green"),
            &[standing(vehicle, laps, counting, in_pits, "none")],
        );
    }

    /// The car's `lap_invalidated` for a telemetry update.
    fn telemetry(
        validity: &LapValidity,
        vehicle: i64,
        lap: i64,
        in_pit_lane: bool,
        steps: i64,
    ) -> Option<bool> {
        let mut telemetry = bono_telemetry::default();
        telemetry.vehicle_id = Some(vehicle).into();
        telemetry.lap = Some(lap).into();
        telemetry.in_pit_lane = Some(in_pit_lane).into();
        telemetry.state.track_limits_steps = Some(steps).into();
        validity.fill_telemetry(&mut telemetry);
        telemetry.lap_invalidated.get()
    }

    #[test]
    fn forgiven_off_is_not_invalid() {
        let validity = LapValidity::default();
        for counting in [true, false, true] {
            scoring(&validity, 1, 3, Some(counting), false);
            assert_eq!(telemetry(&validity, 1, 3, false, 4), Some(false));
        }
    }

    #[test]
    fn points_while_not_counting_invalidate_the_rest_of_the_lap() {
        let validity = LapValidity::default();
        scoring(&validity, 1, 3, Some(true), false);
        assert_eq!(telemetry(&validity, 1, 3, false, 4), Some(false));
        scoring(&validity, 1, 3, Some(false), false);
        assert_eq!(telemetry(&validity, 1, 3, false, 4), Some(false));
        assert_eq!(telemetry(&validity, 1, 3, false, 5), Some(true));
        scoring(&validity, 1, 3, Some(true), false);
        assert_eq!(telemetry(&validity, 1, 3, false, 5), Some(true));

        scoring(&validity, 1, 4, Some(true), false);
        assert_eq!(telemetry(&validity, 1, 4, false, 5), Some(false));
    }

    #[test]
    fn points_while_counting_do_not_invalidate() {
        let validity = LapValidity::default();
        scoring(&validity, 1, 3, Some(true), false);
        assert_eq!(telemetry(&validity, 1, 3, false, 4), Some(false));
        assert_eq!(telemetry(&validity, 1, 3, false, 5), Some(false));
    }

    #[test]
    fn carried_over_lap_is_invalid_from_its_start() {
        let validity = LapValidity::default();
        scoring(&validity, 1, 5, Some(true), false);
        assert_eq!(telemetry(&validity, 1, 5, false, 0), Some(false));
        scoring(&validity, 1, 6, Some(false), false);
        assert_eq!(telemetry(&validity, 1, 6, false, 0), Some(true));
        assert_eq!(telemetry(&validity, 1, 6, false, 4), Some(true));
    }

    #[test]
    fn out_lap_is_not_carried_over() {
        let validity = LapValidity::default();
        scoring(&validity, 1, 4, Some(true), false);
        telemetry(&validity, 1, 4, false, 0);
        // The lap starts in the pits, its time not counting
        scoring(&validity, 1, 5, Some(false), true);
        assert_eq!(telemetry(&validity, 1, 5, true, 0), Some(false));
        scoring(&validity, 1, 5, Some(false), false);
        assert_eq!(telemetry(&validity, 1, 5, false, 0), Some(false));
    }

    #[test]
    fn previous_laps_scoring_does_not_judge_the_new_lap() {
        let validity = LapValidity::default();
        scoring(&validity, 1, 5, Some(false), false);
        assert_eq!(telemetry(&validity, 1, 5, false, 0), Some(false));
        // The lap counter ticks over before scoring does
        assert_eq!(telemetry(&validity, 1, 6, false, 0), Some(false));
        scoring(&validity, 1, 6, Some(true), false);
        assert_eq!(telemetry(&validity, 1, 6, false, 0), Some(false));
        assert_eq!(telemetry(&validity, 1, 6, false, 0), Some(false));
    }

    /// A car on lap 5, then on lap 6 with `standing` and `session`'s scoring: lap 6's
    /// `lap_invalidated` at its start, and after its points rise.
    fn next_lap(
        validity: &LapValidity,
        session: &bono_session_info,
        standing: bono_standing,
    ) -> [Option<bool>; 2] {
        let vehicle = standing.vehicle_id.get().unwrap();
        scoring(validity, vehicle, 5, Some(true), false);
        telemetry(validity, vehicle, 5, false, 0);
        validity.fill_scoring(session, &[standing]);
        [
            telemetry(validity, vehicle, 6, false, 0),
            telemetry(validity, vehicle, 6, false, 4),
        ]
    }

    #[test]
    fn laps_after_the_finish_are_not_judged() {
        let finished = standing(1, 6, Some(false), false, "finished");
        let over = standing(1, 6, Some(false), false, "none");
        assert_eq!(
            next_lap(
                &LapValidity::default(),
                &session("qualifying", "green"),
                finished
            ),
            [Some(false); 2]
        );
        assert_eq!(
            next_lap(
                &LapValidity::default(),
                &session("qualifying", "over"),
                over
            ),
            [Some(false); 2]
        );
    }

    #[test]
    fn races_are_never_invalidated() {
        let racing = standing(1, 6, Some(false), false, "none");
        assert_eq!(
            next_lap(&LapValidity::default(), &session("race", "green"), racing),
            [Some(false); 2]
        );
    }

    #[test]
    fn cars_are_judged_separately() {
        let validity = LapValidity::default();
        scoring(&validity, 1, 5, Some(true), false);
        scoring(&validity, 2, 2, Some(true), false);
        telemetry(&validity, 1, 5, false, 0);
        telemetry(&validity, 2, 2, false, 0);
        validity.fill_scoring(
            &session("practice", "green"),
            &[
                standing(1, 6, Some(false), false, "none"),
                standing(2, 3, Some(true), false, "none"),
            ],
        );
        assert_eq!(telemetry(&validity, 1, 6, false, 0), Some(true));
        assert_eq!(telemetry(&validity, 2, 3, false, 0), Some(false));
    }

    #[test]
    fn a_lap_begun_before_the_car_was_first_seen_is_not_carried_over() {
        let validity = LapValidity::default();
        scoring(&validity, 1, 6, Some(false), false);
        assert_eq!(telemetry(&validity, 1, 6, false, 0), Some(false));
    }

    #[test]
    fn clear_forgets_every_car() {
        let validity = LapValidity::default();
        scoring(&validity, 1, 5, Some(true), false);
        telemetry(&validity, 1, 5, false, 0);
        scoring(&validity, 1, 6, Some(false), false);
        assert_eq!(telemetry(&validity, 1, 6, false, 0), Some(true));
        validity.clear();
        scoring(&validity, 1, 6, Some(true), false);
        assert_eq!(telemetry(&validity, 1, 6, false, 0), Some(false));
    }
}
