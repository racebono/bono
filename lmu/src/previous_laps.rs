//! Whether each car's previous lap was invalid, which LMU doesn't report: an estimate from watching
//! its current lap's flag through the updates the recorder keeps. Opponents' are kept only every
//! Nth update, so a flag raised and cleared between two of them would be missed.

use bono_sim::sys::bono_telemetry;
use std::{
    collections::HashMap,
    sync::{Mutex, PoisonError},
};

/// How far the clock can go back before it's another session (as the recorder takes it).
const CLOCK_BACK: f64 = 1.0;

#[derive(Default)]
pub struct PreviousLaps(Mutex<HashMap<i64, Car>>);

struct Car {
    lap: i64,
    time: f64,
    /// The flag was seen on this lap
    invalid: bool,
    /// The lap was seen from its start, so `invalid` covers all of it
    whole_lap: bool,
    previous_invalid: Option<bool>,
}

impl PreviousLaps {
    /// Forgets every car, for a new event.
    pub fn clear(&self) {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clear();
    }

    /// Fills in the estimate for this update.
    pub fn fill(&self, telemetry: &mut bono_telemetry) {
        let (Some(vehicle), Some(lap)) = (telemetry.vehicle_id.get(), telemetry.lap.get()) else {
            return;
        };
        let time = telemetry.session_time.get().unwrap_or(0.0);
        let invalid = telemetry.lap_invalidated.get() == Some(true);

        let mut cars = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        let car = cars.entry(vehicle).or_insert(Car {
            lap,
            time,
            invalid,
            // Joined part way through: what happened before isn't known
            whole_lap: false,
            previous_invalid: None,
        });

        if time < car.time - CLOCK_BACK || lap < car.lap || lap > car.lap + 1 {
            // Another session or a gap: nothing about the lap before is known
            *car = Car {
                lap,
                time,
                invalid,
                whole_lap: false,
                previous_invalid: None,
            };
        } else if lap == car.lap + 1 {
            car.previous_invalid = car.whole_lap.then_some(car.invalid);
            car.lap = lap;
            // This update's flag can still be the last lap's. A real one stays up for the next
            car.invalid = false;
            car.whole_lap = true;
        } else {
            car.invalid |= invalid;
        }
        car.time = time;
        telemetry.state.previous_lap_invalidated = car.previous_invalid.into();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn estimate(laps: &PreviousLaps, lap: i64, time: f64, invalid: bool) -> Option<bool> {
        let mut telemetry = bono_telemetry::default();
        telemetry.vehicle_id = Some(1).into();
        telemetry.lap = Some(lap).into();
        telemetry.session_time = Some(time).into();
        telemetry.lap_invalidated = Some(invalid).into();
        laps.fill(&mut telemetry);
        telemetry.state.previous_lap_invalidated.get()
    }

    /// Past the lap the car was first seen on, which gets no verdict.
    fn on_lap_2() -> PreviousLaps {
        let laps = PreviousLaps::default();
        assert_eq!(estimate(&laps, 1, 10.0, false), None);
        assert_eq!(estimate(&laps, 2, 20.0, false), None);
        laps
    }

    #[test]
    fn the_first_lap_seen_gets_no_verdict() {
        let laps = PreviousLaps::default();
        assert_eq!(estimate(&laps, 1, 10.0, true), None);
        assert_eq!(estimate(&laps, 2, 20.0, false), None);
    }

    #[test]
    fn a_clean_lap_is_valid_and_an_invalidated_one_stays_invalid() {
        let laps = on_lap_2();
        assert_eq!(estimate(&laps, 2, 21.0, false), None);
        assert_eq!(estimate(&laps, 3, 30.0, false), Some(false));
        // Up at any point, even if it's cleared before the line
        assert_eq!(estimate(&laps, 3, 31.0, true), Some(false));
        assert_eq!(estimate(&laps, 3, 32.0, false), Some(false));
        assert_eq!(estimate(&laps, 4, 40.0, false), Some(true));
        // Held through the next lap, then that lap's own
        assert_eq!(estimate(&laps, 4, 41.0, false), Some(true));
        assert_eq!(estimate(&laps, 5, 50.0, false), Some(false));
    }

    #[test]
    fn a_flag_left_from_the_last_lap_does_not_count_for_the_next() {
        let laps = on_lap_2();
        assert_eq!(estimate(&laps, 2, 21.0, true), None);
        // One update of lap 3 still has lap 2's flag
        assert_eq!(estimate(&laps, 3, 30.0, true), Some(true));
        assert_eq!(estimate(&laps, 3, 31.0, false), Some(true));
        assert_eq!(estimate(&laps, 4, 40.0, false), Some(false));
    }

    #[test]
    fn new_sessions_and_gaps_forget_the_verdict() {
        // The lap number going back
        let laps = on_lap_2();
        assert_eq!(estimate(&laps, 3, 30.0, false), Some(false));
        assert_eq!(estimate(&laps, 1, 31.0, false), None);
        // The clock going back on the same lap
        let laps = on_lap_2();
        assert_eq!(estimate(&laps, 3, 30.0, false), Some(false));
        assert_eq!(estimate(&laps, 3, 5.0, false), None);
        assert_eq!(estimate(&laps, 4, 15.0, false), None);
        // Laps missed
        let laps = on_lap_2();
        assert_eq!(estimate(&laps, 5, 50.0, false), None);
        // A new event
        let laps = on_lap_2();
        assert_eq!(estimate(&laps, 3, 30.0, false), Some(false));
        laps.clear();
        assert_eq!(estimate(&laps, 3, 31.0, false), None);
    }

    #[test]
    fn cars_are_tracked_apart() {
        let laps = on_lap_2();
        let mut other = bono_telemetry::default();
        other.vehicle_id = Some(2).into();
        other.lap = Some(9).into();
        laps.fill(&mut other);
        assert_eq!(other.state.previous_lap_invalidated.get(), None);
        assert_eq!(estimate(&laps, 3, 30.0, false), Some(false));
    }
}
