//! The event's sessions and rules from LMU's local web API: the sessions it schedules, and the
//! settings they run under. The API is undocumented, so only settings whose meaning and unit Studio
//! 397's `Settings.JSON` comments give, or every tool agrees on, are read; the rest are left out.

use bono_sim::{
    sql::Event,
    sys::{bono_event_rule, bono_event_session},
};
use serde_json::Value;
use std::time::Duration;

const INFO_URL: &str = "http://localhost:6397/rest/sessions/GetSessionsInfoForEvent";
const SETTINGS_URL: &str = "http://localhost:6397/rest/sessions";

const SESSIONS: [(&str, &str); 4] = [
    ("PRACTICE", "practice"),
    ("QUALIFYING", "qualifying"),
    ("WARMUP", "warmup"),
    ("RACE", "race"),
];

/// The event as of `session_time`, or `None` if either request doesn't get an answer (in time).
pub fn fetch(session_time: f64) -> Option<Event> {
    // About 3 s per request
    let agent = crate::local_agent(Duration::from_millis(750));
    let get = |url: &str| -> Option<String> {
        agent.get(url).call().ok()?.body_mut().read_to_string().ok()
    };

    let info = get(INFO_URL)?;
    let settings = get(SETTINGS_URL)?;
    parse(&info, &settings, session_time)
}

/// `info` is `{"scheduledSessions": [{"lengthTime": 60, "name": "QUALIFYING", ...}, ...]}`, with
/// lengths in minutes. `settings` is `{"SESSSET_Fuel_Usage": {"currentValue": 2, "stringValue": "X2",
/// ...}, ...}`, where `stringValue` may be in the game's language: it's only ever kept as display
/// text.
fn parse(info: &str, settings: &str, session_time: f64) -> Option<Event> {
    let info = serde_json::from_str::<Value>(info).ok()?;
    let scheduled = info.get("scheduledSessions")?.as_array()?;
    let settings @ Value::Object(_) = serde_json::from_str::<Value>(settings).ok()? else {
        return None;
    };

    // A setting's number and text, if there
    let setting = |key: &str| {
        settings
            .get(format!("SESSSET_{key}"))
            .filter(|setting| setting.is_object())
    };
    let number = |key: &str| setting(key)?.get("currentValue")?.as_f64();
    let text = |key: &str| {
        let text = setting(key)?.get("stringValue")?.as_str()?;
        (!text.is_empty()).then(|| text.to_owned())
    };

    // All sources agree only on 1 and 2; the others conflict, so the race's length isn't known then
    let race_finish = match number("Finish_Criteria") {
        Some(1.0) => "laps",
        Some(2.0) => "time",
        _ => "unknown",
    };

    let mut sessions: Vec<bono_event_session> = Vec::new();
    for (order, entry) in scheduled.iter().enumerate() {
        let name = entry.get("name").and_then(Value::as_str);
        let Some((name, session_type)) = SESSIONS.into_iter().find(|(key, _)| Some(*key) == name)
        else {
            continue;
        };

        // Counted from 1 within its type, as `lmu.cpp` numbers the session; it gives warm-up none
        let earlier = sessions
            .iter()
            .filter(|session| session.session_type.as_deref() == Some(session_type))
            .count();
        let session_number = (session_type != "warmup").then_some(earlier as i64 + 1);

        // Practice, qualifying and warm-up run on time. 0 minutes means the default
        let finish = if session_type == "race" {
            race_finish
        } else {
            "time"
        };
        let minutes = entry
            .get("lengthTime")
            .and_then(Value::as_f64)
            .filter(|minutes| *minutes > 0.0);
        let laps = number("Race_Laps").filter(|laps| *laps > 0.0);

        let (start_key, rubber_key) = match (session_type, session_number) {
            ("practice", Some(1)) => (
                Some("practice1_starting_time"),
                Some("pract1_realroad_init"),
            ),
            ("qualifying", n) => (
                Some("qualify_starting_time"),
                (n == Some(1)).then_some("qual1_realroad_init"),
            ),
            ("warmup", _) => (Some("warmup_starting_time"), None),
            ("race", n) => (
                Some("race_starting_time"),
                (n == Some(1)).then_some("race_realroad_init"),
            ),
            _ => (None, None),
        };
        // Minutes after midnight; -1 is the track's default, and -2 and -3 random
        let start = start_key
            .and_then(number)
            .filter(|minutes| (0.0..=1439.0).contains(minutes));

        let mut session = bono_event_session::default();
        session.session_time = Some(session_time).into();
        session.event_order = Some(order as i64).into();
        session.session_type = Some(session_type.to_owned()).into();
        session.session_number = session_number.into();
        session.name = Some(name.to_owned()).into();
        session.length_time = minutes
            .filter(|_| finish == "time")
            .map(|minutes| minutes * 60.0)
            .into();
        session.length_laps = laps
            .filter(|_| finish == "laps")
            .map(|laps| laps as i64)
            .into();
        session.finish = Some(finish.to_owned()).into();
        session.start_time_of_day = start.map(|minutes| minutes * 60.0).into();
        session.track_rubber = rubber_key.and_then(text).into();
        sessions.push(session);
    }

    // 1 is the real rate
    let multiplier = |key: &str| {
        let value = number(key)?;
        if value == 0.0 {
            Some("off".to_owned())
        } else if [1.0, 2.0, 3.0].contains(&value) {
            Some(format!("×{value}"))
        } else {
            None
        }
    };
    let rules = [
        ("fuel_use_multiplier", None, multiplier("Fuel_Usage")),
        ("tire_wear_multiplier", None, multiplier("Tire_Wear")),
        (
            "damage_multiplier",
            None,
            number("Damage_Multi").map(|percent| format!("{percent}%")),
        ),
        ("mechanical_failures", None, text("Mech_Failures")),
        (
            "tires_in_garage",
            None,
            number("tires_available_in_garage").map(|tires| format!("{tires} tyres")),
        ),
        ("tire_warmers", None, text("tire_warmers")),
        ("parc_ferme", None, text("parc_ferme")),
        ("formation", None, text("formation")),
        ("flag_rules", None, text("flag_rules")),
        ("cut_rules", None, text("cut_rules")),
        (
            "cuts_allowed",
            None,
            number("cuts_allowed").map(|cuts| cuts.to_string()),
        ),
        (
            "safety_car_threshold",
            None,
            number("safetycar_thresh").map(|threshold| threshold.to_string()),
        ),
        ("time_scale", None, text("race_timescale")),
        ("weather_mode", None, text("weather")),
        ("private_qualifying", None, text("private_qual")),
        (
            "rubber_rate",
            Some("practice"),
            text("realroad_timescale_practice"),
        ),
        (
            "rubber_rate",
            Some("qualifying"),
            text("realroad_timescale_qualify"),
        ),
        ("rubber_rate", Some("race"), text("realroad_timescale_race")),
    ];
    let rules = rules
        .into_iter()
        .filter_map(|(name, session_type, value)| {
            let mut rule = bono_event_rule::default();
            rule.session_time = Some(session_time).into();
            rule.name = Some(name.to_owned()).into();
            rule.session_type = session_type.map(str::to_owned).into();
            rule.value = Some(value?).into();
            Some(rule)
        })
        .collect();

    Some(Event { sessions, rules })
}

#[cfg(test)]
mod tests {
    use super::*;

    const INFO_OFFLINE: &str = include_str!("fixtures/sessions_info_offline.json");
    const INFO_ONLINE: &str = include_str!("fixtures/sessions_info_online.json");
    const OFFLINE: &str = include_str!("fixtures/sessions_offline.json");
    const ONLINE: &str = include_str!("fixtures/sessions_online.json");

    fn rule<'a>(event: &'a Event, name: &str) -> Option<&'a str> {
        let rule = event
            .rules
            .iter()
            .find(|rule| rule.name.as_deref() == Some(name))?;
        rule.value.as_deref()
    }

    #[test]
    fn parses_the_offline_event() {
        let event = parse(INFO_OFFLINE, OFFLINE, 12.0).unwrap();
        assert_eq!(event.sessions.len(), 3);

        let [practice, qualifying, race] = &event.sessions[..] else {
            unreachable!()
        };
        assert_eq!(practice.session_time.get(), Some(12.0));
        assert_eq!(practice.event_order.get(), Some(0));
        assert_eq!(practice.session_type.as_deref(), Some("practice"));
        assert_eq!(practice.session_number.get(), Some(1));
        assert_eq!(practice.name.as_deref(), Some("PRACTICE"));
        assert_eq!(practice.length_time.get(), Some(21600.0));
        assert_eq!(practice.finish.as_deref(), Some("time"));
        assert_eq!(practice.start_time_of_day.get(), Some(53100.0));
        assert_eq!(practice.track_rubber.as_deref(), Some("Preset: Medium"));

        assert_eq!(qualifying.session_type.as_deref(), Some("qualifying"));
        assert_eq!(qualifying.session_number.get(), Some(1));
        assert_eq!(qualifying.length_time.get(), Some(3600.0));

        assert_eq!(race.event_order.get(), Some(2));
        assert_eq!(race.session_type.as_deref(), Some("race"));
        assert_eq!(race.session_number.get(), Some(1));
        assert_eq!(race.length_time.get(), Some(4320.0));
        assert_eq!(race.length_laps.get(), None);
        assert_eq!(race.finish.as_deref(), Some("time"));
        assert_eq!(race.start_time_of_day.get(), None);

        assert_eq!(rule(&event, "fuel_use_multiplier"), Some("×2"));
        assert_eq!(rule(&event, "tire_wear_multiplier"), Some("×2"));
        assert_eq!(rule(&event, "tires_in_garage"), Some("12 tyres"));
        assert_eq!(rule(&event, "damage_multiplier"), Some("100%"));
        assert_eq!(rule(&event, "safety_car_threshold"), Some("100"));
        let rubber = event
            .rules
            .iter()
            .find(|rule| rule.session_type.as_deref() == Some("qualifying"))
            .unwrap();
        assert_eq!(rubber.name.as_deref(), Some("rubber_rate"));
        assert_eq!(rubber.value.as_deref(), Some("Normal"));
    }

    #[test]
    fn parses_the_online_event() {
        let event = parse(INFO_ONLINE, ONLINE, 0.0).unwrap();
        let lengths: Vec<_> = event
            .sessions
            .iter()
            .map(|session| session.length_time.get())
            .collect();
        assert_eq!(lengths, [Some(1200.0), Some(600.0), Some(1200.0)]);
        assert_eq!(event.sessions[2].start_time_of_day.get(), Some(43200.0));
        assert_eq!(rule(&event, "fuel_use_multiplier"), Some("×1"));
    }

    #[test]
    fn share_of_track_finish_leaves_the_race_length_unknown() {
        let mut settings: Value = serde_json::from_str(OFFLINE).unwrap();
        settings["SESSSET_Finish_Criteria"]["currentValue"] = 5.into();

        let event = parse(INFO_OFFLINE, &settings.to_string(), 0.0).unwrap();
        let race = &event.sessions[2];
        assert_eq!(race.finish.as_deref(), Some("unknown"));
        assert_eq!(race.length_time.get(), None);
        assert_eq!(race.length_laps.get(), None);
    }

    #[test]
    fn warmup_has_no_number() {
        let info = r#"{"scheduledSessions": [
            {"lengthTime": 60, "name": "QUALIFYING"},
            {"lengthTime": 10, "name": "WARMUP"},
            {"lengthTime": 72, "name": "RACE"}
        ]}"#;

        let event = parse(info, OFFLINE, 0.0).unwrap();
        let warmup = &event.sessions[1];
        assert_eq!(warmup.session_type.as_deref(), Some("warmup"));
        assert_eq!(warmup.session_number.get(), None);
        assert_eq!(warmup.length_time.get(), Some(600.0));
        assert_eq!(event.sessions[2].session_number.get(), Some(1));
    }

    #[test]
    fn nothing_without_both_answers() {
        assert!(parse(INFO_OFFLINE, "not json", 0.0).is_none());
        assert!(parse("{}", OFFLINE, 0.0).is_none());
    }
}
