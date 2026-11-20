//! Whether each car's lap is invalidated, as LMU decides it: the SDK's own flag (`mLapInvalidated`)
//! doesn't say. Scoring has whether a lap's time counts, and telemetry the car's track-limit steps,
//! in separate updates.
//!
//! A lap is invalidated, for the rest of it, once either:
//! - its steps rise while its time isn't counting, as the scoring after the rise says. The flag
//!   turns off as the car leaves the track, and back on if LMU forgives it; scoring the cut while
//!   it's off is how LMU is seen to decide.
//!   That's observed, not documented: a lap this misses still ends without an official time, which
//!   the laps table and the spotter check at the line.
//! - it starts not counting (by its first scoring), away from the pits. LMU's guide says a late or
//!   serious offence invalidates the next lap too; an out lap doesn't count for being one.
//!
//! Only for laps whose start was seen, in sessions other than races (races give points instead),
//! while green, for cars that haven't finished.

use bono_sim::sys::{bono_session_info, bono_standing, bono_telemetry};
use std::{collections::HashMap, sync::Mutex};

/// Each car's lap, by vehicle id.
#[derive(Default)]
pub struct LapValidity(Mutex<HashMap<i64, Car>>);

#[derive(Default)]
struct Car {
    /// From the newest scoring: the laps completed, whether that lap's time counts, and whether
    /// the car's laps are judged at all
    scored_laps: Option<i64>,
    counting: Option<bool>,
    judged: bool,
    /// The first scoring of the laps completed: how the lap began
    start: Option<Start>,
    /// The telemetry lap the rest is about
    lap: Option<i64>,
    /// Its start was seen (straight on from the lap before), and it began in the pits
    seen_start: bool,
    began_in_pits: bool,
    start_judged: bool,
    steps: Option<i64>,
    /// The steps rose: judged by the scoring after, as the flag may change with them
    rose: bool,
    invalidated: bool,
}

#[derive(Clone, Copy)]
struct Start {
    laps: i64,
    counting: Option<bool>,
    in_pits: bool,
    judged: bool,
}

impl Car {
    /// Whether the lap, once its first scoring's here, began not counting away from the pits.
    fn judge_start(&mut self) {
        let Some(start) = self.start.filter(|start| Some(start.laps) == self.lap) else {
            return;
        };
        if self.start_judged {
            return;
        }
        self.start_judged = true;
        self.began_in_pits |= start.in_pits;
        self.invalidated |=
            start.judged && start.counting == Some(false) && self.seen_start && !self.began_in_pits;
    }
}

impl LapValidity {
    /// Forgets every car, for a new session.
    pub fn clear(&self) {
        self.0.lock().unwrap_or_else(|e| e.into_inner()).clear();
    }

    /// Takes each car's newest scoring.
    pub fn fill_scoring(&self, session: &bono_session_info, standings: &[bono_standing]) {
        let session_judged = session.session_type.as_deref().is_some_and(|t| t != "race")
            && session.phase.as_deref() == Some("green");
        let mut cars = self.0.lock().unwrap_or_else(|e| e.into_inner());
        for standing in standings {
            let Some(vehicle) = standing.vehicle_id.get() else {
                continue;
            };
            let car = cars.entry(vehicle).or_default();
            let laps = standing.laps_completed.get();
            car.counting = standing.lap_time_counts.get();
            car.judged = session_judged
                && standing
                    .finish_status
                    .as_deref()
                    .is_none_or(|s| s == "none");
            if laps != car.scored_laps {
                car.start = laps.map(|laps| Start {
                    laps,
                    counting: car.counting,
                    in_pits: standing.in_pits.get().unwrap_or(false),
                    judged: car.judged,
                });
            }
            car.scored_laps = laps;
            car.judge_start();
            // The lap counter and scoring tick over a moment apart: until then, scoring is the
            // last lap's
            if car.rose && car.lap.is_some() && car.scored_laps == car.lap {
                car.rose = false;
                car.invalidated |= car.judged
                    && car.counting == Some(false)
                    && car.seen_start
                    && !car.began_in_pits;
            }
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
            // Only straight on from the lap before: after laps unseen, this one's begun unseen too
            car.seen_start = car
                .lap
                .zip(lap)
                .is_some_and(|(before, now)| now == before + 1);
            car.lap = lap;
            car.began_in_pits = telemetry.in_pit_lane.get().unwrap_or(false);
            car.start_judged = false;
            car.rose = false;
            car.invalidated = false;
        }
        car.judge_start();
        let steps = telemetry.state.track_limits_steps.get();
        if let (Some(before), Some(now)) = (car.steps, steps) {
            car.rose |= now > before;
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

    /// Lap `lap` of the car, counting, seen in telemetry with 4 steps: the next lap's start is
    /// then seen.
    fn seen(validity: &LapValidity, vehicle: i64, lap: i64) {
        scoring(validity, vehicle, lap, Some(true), false);
        telemetry(validity, vehicle, lap, false, 4);
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
        seen(&validity, 1, 2);
        scoring(&validity, 1, 3, Some(true), false);
        assert_eq!(telemetry(&validity, 1, 3, false, 4), Some(false));
        scoring(&validity, 1, 3, Some(false), false);
        assert_eq!(telemetry(&validity, 1, 3, false, 4), Some(false));
        // Judged by the scoring after the rise
        assert_eq!(telemetry(&validity, 1, 3, false, 5), Some(false));
        scoring(&validity, 1, 3, Some(false), false);
        assert_eq!(telemetry(&validity, 1, 3, false, 5), Some(true));
        scoring(&validity, 1, 3, Some(true), false);
        assert_eq!(telemetry(&validity, 1, 3, false, 5), Some(true));

        scoring(&validity, 1, 4, Some(true), false);
        assert_eq!(telemetry(&validity, 1, 4, false, 5), Some(false));
    }

    #[test]
    fn points_while_counting_do_not_invalidate() {
        let validity = LapValidity::default();
        seen(&validity, 1, 2);
        scoring(&validity, 1, 3, Some(true), false);
        assert_eq!(telemetry(&validity, 1, 3, false, 4), Some(false));
        assert_eq!(telemetry(&validity, 1, 3, false, 5), Some(false));
        scoring(&validity, 1, 3, Some(true), false);
        assert_eq!(telemetry(&validity, 1, 3, false, 5), Some(false));
    }

    #[test]
    fn points_scored_as_the_flag_comes_back_do_not_invalidate() {
        let validity = LapValidity::default();
        seen(&validity, 1, 2);
        scoring(&validity, 1, 3, Some(true), false);
        telemetry(&validity, 1, 3, false, 4);
        scoring(&validity, 1, 3, Some(false), false);
        // Telemetry sees the points before scoring says the lap counts again
        assert_eq!(telemetry(&validity, 1, 3, false, 5), Some(false));
        scoring(&validity, 1, 3, Some(true), false);
        assert_eq!(telemetry(&validity, 1, 3, false, 5), Some(false));
    }

    #[test]
    fn points_scored_as_the_flag_drops_invalidate() {
        let validity = LapValidity::default();
        seen(&validity, 1, 2);
        scoring(&validity, 1, 3, Some(true), false);
        telemetry(&validity, 1, 3, false, 4);
        assert_eq!(telemetry(&validity, 1, 3, false, 5), Some(false));
        scoring(&validity, 1, 3, Some(false), false);
        assert_eq!(telemetry(&validity, 1, 3, false, 5), Some(true));
    }

    #[test]
    fn points_on_a_lap_begun_unseen_do_not_invalidate() {
        let validity = LapValidity::default();
        scoring(&validity, 1, 3, Some(false), false);
        telemetry(&validity, 1, 3, false, 4);
        telemetry(&validity, 1, 3, false, 5);
        scoring(&validity, 1, 3, Some(false), false);
        assert_eq!(telemetry(&validity, 1, 3, false, 5), Some(false));
    }

    #[test]
    fn in_lap_cut_scored_in_the_pits_is_invalid() {
        let validity = LapValidity::default();
        seen(&validity, 1, 2);
        scoring(&validity, 1, 3, Some(true), false);
        telemetry(&validity, 1, 3, false, 4);
        // Cut before the pit entry, scored in the pit lane
        scoring(&validity, 1, 3, Some(false), false);
        assert_eq!(telemetry(&validity, 1, 3, false, 4), Some(false));
        scoring(&validity, 1, 3, Some(false), true);
        telemetry(&validity, 1, 3, true, 5);
        scoring(&validity, 1, 3, Some(false), true);
        assert_eq!(telemetry(&validity, 1, 3, true, 5), Some(true));
    }

    #[test]
    fn carry_over_is_judged_by_the_laps_first_scoring() {
        // The car's telemetry is only kept now and then: its first update of lap 6 comes after
        // a moment off track, which LMU then forgives
        let validity = LapValidity::default();
        seen(&validity, 1, 5);
        scoring(&validity, 1, 6, Some(true), false);
        scoring(&validity, 1, 6, Some(false), false);
        assert_eq!(telemetry(&validity, 1, 6, false, 0), Some(false));
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
    fn in_laps_points_scored_on_the_out_lap_do_not_invalidate_it() {
        let validity = LapValidity::default();
        scoring(&validity, 1, 4, Some(false), false);
        telemetry(&validity, 1, 4, false, 0);
        // Cut before the pit entry, scored once the out lap's begun in the pits
        scoring(&validity, 1, 5, Some(false), true);
        assert_eq!(telemetry(&validity, 1, 5, true, 0), Some(false));
        assert_eq!(telemetry(&validity, 1, 5, true, 4), Some(false));
        scoring(&validity, 1, 5, Some(false), true);
        assert_eq!(telemetry(&validity, 1, 5, true, 4), Some(false));
    }

    #[test]
    fn a_lap_after_laps_unseen_is_not_carried_over() {
        let validity = LapValidity::default();
        scoring(&validity, 1, 5, Some(true), false);
        telemetry(&validity, 1, 5, false, 0);
        // No telemetry for lap 6: lap 7 is seen from its middle, off track
        scoring(&validity, 1, 7, Some(false), false);
        assert_eq!(telemetry(&validity, 1, 7, false, 0), Some(false));
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
