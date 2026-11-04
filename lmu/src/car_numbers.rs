//! Race numbers from LMU's own local web API (the one its UI uses): the plugin interface doesn't
//! have them. They're fetched on the recorder's writer thread and only read when scoring arrives,
//! so the game's threads never wait on the web API.

use bono_sim::sys::bono_standing;
use serde_json::Value;
use std::{
    collections::HashMap,
    sync::{Mutex, PoisonError},
    time::Duration,
};

const URL: &str = "http://localhost:6397/rest/watch/standings";

/// Each slot's car and its number, as of the last answer.
#[derive(Default)]
pub struct CarNumbers(Mutex<HashMap<i64, Car>>);

#[derive(Debug, PartialEq)]
struct Car {
    vehicle: String,
    number: String,
}

impl CarNumbers {
    /// Fetches them again, keeping what's known if the web API doesn't answer (in time) with cars.
    pub fn refresh(&self) {
        // It's on this PC: anything slow means it isn't answering
        let agent: ureq::Agent = ureq::Agent::config_builder()
            .timeout_connect(Some(Duration::from_secs(1)))
            .timeout_global(Some(Duration::from_secs(3)))
            .build()
            .into();
        let Some(body) = agent
            .get(URL)
            .call()
            .ok()
            .and_then(|mut response| response.body_mut().read_to_string().ok())
        else {
            return;
        };
        self.update(&body);
    }

    /// Forgets every car, for a new event.
    pub fn clear(&self) {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clear();
    }

    fn update(&self, body: &str) {
        let cars = parse(body);
        if !cars.is_empty() {
            *self.0.lock().unwrap_or_else(PoisonError::into_inner) = cars;
        }
    }

    /// Fills in the numbers. A slot's number only counts while the same car has it: another car can
    /// take over a slot before the next refresh.
    pub fn fill(&self, standings: &mut [bono_standing]) {
        let cars = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        for standing in standings {
            let number = standing.vehicle_id.get().and_then(|slot| cars.get(&slot));
            standing.car_number = number
                .filter(|car| standing.vehicle.as_deref() == Some(car.vehicle.as_str()))
                .map(|car| car.number.clone())
                .into();
        }
    }
}

/// `[{"slotID": 1, "carNumber": "007", "vehicleName": "...", ...}, ...]`.
fn parse(body: &str) -> HashMap<i64, Car> {
    let Ok(Value::Array(cars)) = serde_json::from_str::<Value>(body) else {
        return HashMap::new();
    };
    cars.iter()
        .filter_map(|car| {
            let slot = car.get("slotID")?.as_i64()?;
            let number = car.get("carNumber")?.as_str().filter(|n| !n.is_empty())?;
            let vehicle = car.get("vehicleName")?.as_str()?;
            Some((
                slot,
                Car {
                    vehicle: vehicle.to_owned(),
                    number: number.to_owned(),
                },
            ))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const BODY: &str = r#"[
        {"slotID": 1, "carNumber": "007", "vehicleName": "Aston #007"},
        {"slotID": 2, "carNumber": "", "vehicleName": "No number"},
        {"slotID": 3, "vehicleName": "Missing number"}
    ]"#;

    fn standing(slot: i64, vehicle: &str) -> bono_standing {
        let mut standing = bono_standing::default();
        standing.vehicle_id = Some(slot).into();
        standing.vehicle = Some(vehicle.to_owned()).into();
        standing
    }

    #[test]
    fn numbers_stay_text_and_only_count_for_the_same_car() {
        let numbers = CarNumbers::default();
        numbers.update(BODY);
        let mut standings = [
            standing(1, "Aston #007"),
            standing(1, "Another car in the slot"),
            standing(2, "No number"),
            standing(9, "Unknown slot"),
        ];
        numbers.fill(&mut standings);
        assert_eq!(standings[0].car_number.as_deref(), Some("007"));
        for standing in &standings[1..] {
            assert_eq!(standing.car_number.as_deref(), None);
        }
    }

    #[test]
    fn keeps_what_is_known_when_the_answer_has_no_cars() {
        let numbers = CarNumbers::default();
        numbers.update(BODY);
        for body in ["", "[]", "not json", r#"{"status": "unavailable"}"#] {
            numbers.update(body);
        }
        let mut standings = [standing(1, "Aston #007")];
        numbers.fill(&mut standings);
        assert_eq!(standings[0].car_number.as_deref(), Some("007"));
    }
}
