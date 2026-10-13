//! The weather forecast from LMU's own local web API (the one its UI uses), which has it for every
//! session of the event.

use lambiase_sim::sys::lambiase_forecast_point;
use serde_json::Value;
use std::time::Duration;

const URL: &str = "http://localhost:6397/rest/sessions/weather";

const SESSIONS: [(&str, &str); 4] = [
    ("PRACTICE", "practice"),
    ("QUALIFY", "qualifying"),
    ("WARMUP", "warmup"),
    ("RACE", "race"),
];
const NODES: [(&str, f64); 5] = [
    ("START", 0.0),
    ("NODE_25", 0.25),
    ("NODE_50", 0.5),
    ("NODE_75", 0.75),
    ("FINISH", 1.0),
];
const DIRECTIONS: [&str; 8] = ["N", "NE", "E", "SE", "S", "SW", "W", "NW"];

/// The forecast as of `session_time`, or `None` if the web API doesn't answer (in time) with 200.
pub fn fetch(session_time: f64) -> Option<Vec<lambiase_forecast_point>> {
    // It's on this PC: anything slow means it isn't answering
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_connect(Some(Duration::from_secs(1)))
        .timeout_global(Some(Duration::from_secs(3)))
        .build()
        .into();

    let body = agent
        .get(URL)
        .call()
        .ok()?
        .body_mut()
        .read_to_string()
        .ok()?;
    Some(parse(&body, session_time))
}

/// `{"RACE": {"START": {"WNV_RAIN_CHANCE": {"currentValue": 40, "stringValue": "40%"}, ...},
/// "NODE_25": ..., "FINISH": ...}, "PRACTICE": ..., "QUALIFY": ...}`.
fn parse(body: &str, session_time: f64) -> Vec<lambiase_forecast_point> {
    let mut points = Vec::new();
    let Ok(json @ Value::Object(_)) = serde_json::from_str::<Value>(body) else {
        return points;
    };

    for (key, session_type) in SESSIONS {
        let Some(session @ Value::Object(_)) = json.get(key) else {
            continue;
        };

        for (node_key, fraction) in NODES {
            let Some(node @ Value::Object(_)) = session.get(node_key) else {
                continue;
            };

            // A value's number and text, if there
            let value = |key: &str| node.get(key).filter(|value| value.is_object());
            let number = |key: &str| value(key)?.get("currentValue")?.as_f64();
            let text = |key: &str| value(key)?.get("stringValue")?.as_str();

            let mut point = lambiase_forecast_point::default();
            point.session_time = Some(session_time).into();
            point.session_type = Some(session_type.to_owned()).into();
            point.session_fraction = Some(fraction).into();
            point.sky = number("WNV_SKY").map(|sky| sky as i64).into();
            point.sky_text = text("WNV_SKY").map(str::to_owned).into();
            point.rain_chance = number("WNV_RAIN_CHANCE").map(|rain| rain / 100.0).into();
            point.temperature = number("WNV_TEMPERATURE").into();
            point.humidity = number("WNV_HUMIDITY").map(|humidity| humidity / 100.0).into();
            // In m/s (the game shows 1 as 3.6 km/h)
            point.wind_speed = number("WNV_WINDSPEED").into();
            point.wind_direction = number("WNV_WINDDIRECTION")
                .filter(|direction| (0.0..8.0).contains(direction))
                .map(|direction| DIRECTIONS[direction as usize].to_owned())
                .into();
            points.push(point);
        }
    }

    return points;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_the_web_api_forecast() {
        let body = r#"{
            "RACE": {
                "START": {
                    "WNV_SKY": {"currentValue": 2, "stringValue": "Partly Cloudy"},
                    "WNV_RAIN_CHANCE": {"currentValue": 40, "stringValue": "40%"},
                    "WNV_TEMPERATURE": {"currentValue": 21.5},
                    "WNV_HUMIDITY": {"currentValue": 60},
                    "WNV_WINDSPEED": {"currentValue": 3},
                    "WNV_WINDDIRECTION": {"currentValue": 5}
                },
                "FINISH": {"WNV_WINDDIRECTION": {"currentValue": 8}}
            },
            "PRACTICE": "not an object"
        }"#;

        let points = parse(body, 12.0);
        assert_eq!(points.len(), 2);

        let start = &points[0];
        assert_eq!(start.session_time.get(), Some(12.0));
        assert_eq!(start.session_type.as_deref(), Some("race"));
        assert_eq!(start.session_fraction.get(), Some(0.0));
        assert_eq!(start.sky.get(), Some(2));
        assert_eq!(start.sky_text.as_deref(), Some("Partly Cloudy"));
        assert_eq!(start.rain_chance.get(), Some(0.4));
        assert_eq!(start.temperature.get(), Some(21.5));
        assert_eq!(start.humidity.get(), Some(0.6));
        assert_eq!(start.wind_speed.get(), Some(3.0));
        assert_eq!(start.wind_direction.as_deref(), Some("SW"));

        let finish = &points[1];
        assert_eq!(finish.session_fraction.get(), Some(1.0));
        assert_eq!(finish.wind_direction.as_deref(), None);
        assert_eq!(finish.sky.get(), None);
    }

    #[test]
    fn ignores_what_isnt_a_forecast() {
        assert!(parse("not json", 0.0).is_empty());
        assert!(parse("[1, 2]", 0.0).is_empty());
    }
}
