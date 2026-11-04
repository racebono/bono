//! Each car's position within its class, which LMU doesn't report: calculated from every car's
//! overall position and class in a scoring update.

use bono_sim::sys::bono_standing;

/// A car with no class or no overall position has none, and doesn't count for the others.
pub fn fill(standings: &mut [bono_standing]) {
    for i in 0..standings.len() {
        let car = &standings[i];
        let position = car.position.get().filter(|position| *position > 0);
        let class_position = position
            .zip(car.vehicle_class.as_deref())
            .map(|(mine, class)| {
                let ahead = standings.iter().filter(|other| {
                    other.vehicle_class.as_deref() == Some(class)
                        && other.position.get().is_some_and(|p| p > 0 && p < mine)
                });
                ahead.count() as i64 + 1
            });
        standings[i].class_position = class_position.into();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn car(position: Option<i64>, class: Option<&str>) -> bono_standing {
        let mut car = bono_standing::default();
        car.position = position.into();
        car.vehicle_class = class.map(str::to_owned).into();
        car
    }

    fn positions(mut cars: Vec<bono_standing>) -> Vec<Option<i64>> {
        fill(&mut cars);
        cars.iter().map(|car| car.class_position.get()).collect()
    }

    #[test]
    fn counts_only_the_cars_ahead_in_the_same_class() {
        // Not in position order, as nothing promises they are
        let cars = vec![
            car(Some(4), Some("Hyper")),
            car(Some(1), Some("Hyper")),
            car(Some(3), Some("GT3")),
            car(Some(5), Some("GT3")),
            car(Some(2), Some("Hyper")),
        ];
        assert_eq!(
            positions(cars),
            [Some(3), Some(1), Some(1), Some(2), Some(2)]
        );
    }

    #[test]
    fn leaves_out_cars_without_a_class_or_position() {
        let cars = vec![
            car(Some(1), None),
            car(Some(0), Some("GT3")),
            car(None, Some("GT3")),
            car(Some(2), Some("GT3")),
        ];
        assert_eq!(positions(cars), [None, None, None, Some(1)]);
    }

    #[test]
    fn cars_sharing_a_position_share_a_class_position() {
        let cars = vec![car(Some(2), Some("GT3")), car(Some(2), Some("GT3"))];
        assert_eq!(positions(cars), [Some(1), Some(1)]);
    }
}
