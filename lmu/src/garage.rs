//! The player's garage setup, from LMU's own local web API (the one its UI uses): the plugin
//! interface has none. Only `condition`'s thread asks it, at the moments it names.

use bono_sim::setup::{Setting, Setup};
use serde_json::{Map, Value};

const URL: &str = "http://localhost:6397/rest/garage/getPlayerGarageData";

/// The garage setup now, or `None` if the web API didn't answer with one. It blocks for the
/// request, so it's only for `condition`'s thread.
pub fn fetch(agent: &ureq::Agent) -> Option<Setup> {
    let body = agent
        .get(URL)
        .call()
        .ok()?
        .body_mut()
        .read_to_string()
        .ok()?;
    parse(&body)
}

/// `{"VM_REAR_WING": {"value": 4, "minValue": 0, "maxValue": 9, "stringValue": "P5",
/// "lastSavedStringValue": "P1", "available": true, ...}, ..., "symmetric": true, "gearGraph": {...}}`:
/// each setting's step, its range as a count of steps, its display text and the loaded setup file's.
/// The web API doesn't document it, so every field is checked.
/// - A setting (`VM_*` for the car, `WM_*` for a wheel) is its display text: one without it makes
///   the whole answer none, to be asked again, as the settings it's missing would be recorded as
///   gone. Settings the car lacks (`available` false, or `N/A`) are left out.
/// - Its step and range are kept only as whole numbers that fit together.
/// - An answer with no setting isn't one: `symmetric` is only a flag about the others.
fn parse(body: &str) -> Option<Setup> {
    let Ok(Value::Object(json)) = serde_json::from_str::<Value>(body) else {
        return None;
    };
    let mut settings = Vec::new();
    for (name, value) in &json {
        if name.starts_with("VM_") || name.starts_with("WM_") {
            let Value::Object(setting) = value else {
                return None;
            };
            if let Some(setting) = garage_setting(name, setting)? {
                settings.push(setting);
            }
        }
    }
    if settings.is_empty() {
        return None;
    }
    if let Some(Value::Bool(on)) = json.get("symmetric") {
        settings.push(Setting {
            name: "symmetric".to_owned(),
            value: on.to_string(),
            ..Setting::default()
        });
    }
    settings.sort_by(|a, b| a.name.cmp(&b.name));
    // Only known when there's saved text to compare with
    let saved = settings.iter().any(|setting| setting.saved_value.is_some());
    let modified = settings.iter().any(|setting| {
        setting
            .saved_value
            .as_ref()
            .is_some_and(|saved| *saved != setting.value)
    });
    Some(Setup {
        modified: saved.then_some(modified),
        settings,
        ..Setup::default()
    })
}

/// The steering wheel's range in degrees, from the setup's `VM_STEER_LOCK`, for when the plugin
/// interface reports none. Its caption is "Wheel Range (Lock)" and its text is like `310 (12) deg`:
/// the wheel's range first, then what is likely the front wheels' lock (the GT3 fixture's current
/// and saved texts, `310 (12)` and `240 (9.3)`, keep the same ratio of the two). Nothing documents
/// that this range is the same quantity as the plugin interface's physical range, or its span: it's
/// taken as degrees lock to lock, as other tools take the plugin interface's. Any other text, even
/// a single number, is none, rather than a guess at which number it is.
pub fn wheel_range(setup: &Setup) -> Option<f64> {
    let setting = setup
        .settings
        .iter()
        .find(|setting| setting.name == "VM_STEER_LOCK")?;
    let [range, lock, "deg"] = *setting.value.split_whitespace().collect::<Vec<_>>() else {
        return None;
    };
    lock.strip_prefix('(')
        .and_then(|lock| lock.strip_suffix(')'))?
        .parse::<f64>()
        .ok()?;
    range
        .parse::<f64>()
        .ok()
        .filter(|range| range.is_finite() && *range > 0.0)
}

/// A setting, `Some(None)` for one the car lacks, and `None` for one that isn't a setting.
fn garage_setting(name: &str, setting: &Map<String, Value>) -> Option<Option<Setting>> {
    let text = |key| setting.get(key).and_then(Value::as_str).map(str::trim);
    if setting.get("available").and_then(Value::as_bool) == Some(false) {
        return Some(None);
    }
    let value = text("stringValue")?;
    if value == "N/A" {
        return Some(None);
    }
    let number = |key| setting.get(key).and_then(Value::as_i64);
    let step = number("value");
    // A count of steps, so the top one is one less. A range that doesn't hold its own lowest step,
    // or the step, isn't one
    let min_step = number("minValue");
    let max_step = number("maxValue").and_then(|count| count.checked_sub(1));
    let fits = max_step.is_some_and(|max| {
        max >= min_step.unwrap_or(0)
            && step.is_none_or(|step| (min_step.unwrap_or(0)..=max).contains(&step))
    });
    let (min_step, max_step) = match fits {
        true => (min_step, max_step),
        false => (None, None),
    };
    Some(Some(Setting {
        name: name.to_owned(),
        value: value.to_owned(),
        step,
        min_step,
        max_step,
        saved_value: text("lastSavedStringValue").map(str::to_owned),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A GT3 car's answer, from the GitHub project `Lilybankai/Apexandchilloverlaysystem`
    /// (`scripts/fixtures/garage-gt3.json`). It has no licence: the user accepted it for now, and it
    /// may be replaced with a capture of their own.
    const GT3: &str = include_str!("fixtures/garage-gt3.json");

    fn setting<'a>(setup: &'a Setup, name: &str) -> Option<&'a Setting> {
        setup.settings.iter().find(|setting| setting.name == name)
    }

    #[test]
    fn a_garage_setting_keeps_its_text_step_and_range() {
        let setup = parse(GT3).unwrap();
        assert_eq!(
            setting(&setup, "VM_REAR_WING"),
            Some(&Setting {
                name: "VM_REAR_WING".into(),
                value: "P5".into(),
                step: Some(4),
                min_step: Some(0),
                max_step: Some(8),
                saved_value: Some("P1".into()),
            })
        );
        assert_eq!(setting(&setup, "WM_CAMBER-W_FL").unwrap().value, "-1.5 deg");
        assert_eq!(
            setting(&setup, "VM_BRAKE_BALANCE").unwrap().value,
            "52.3:47.7"
        );
        assert_eq!(setup.source, None);
        assert_eq!(setup.file_name, None);
        assert!(setup.settings.is_sorted_by(|a, b| a.name < b.name));
    }

    #[test]
    fn settings_the_car_lacks_are_left_out() {
        let setup = parse(GT3).unwrap();
        assert!(setup.settings.iter().all(|setting| setting.value != "N/A"));
        assert_eq!(setting(&setup, "VM_ANTILOCK_BRAKES"), None);
        assert_eq!(setting(&setup, "VM_PITSTOP_1"), None);
        assert_eq!(setting(&setup, "VM_FRONT_3RD_SPRING"), None);
        // `available` false
        assert_eq!(setting(&setup, "VM_GEAR_7"), None);
        assert_eq!(setting(&setup, "VM_GEAR_FINAL").unwrap().value, "2.88:1");
        assert_eq!(
            setting(&setup, "VM_LEFT_CASTER").unwrap().value,
            "Non-adjustable"
        );
        assert_eq!(setting(&setup, "gearGraph"), None);
        assert_eq!(
            setting(&setup, "symmetric"),
            Some(&Setting {
                name: "symmetric".into(),
                value: "true".into(),
                ..Setting::default()
            })
        );
    }

    #[test]
    fn modified_compares_with_the_saved_setup() {
        assert_eq!(parse(GT3).unwrap().modified, Some(true));
        let body = r#"{
            "VM_REAR_WING": {"value": 4, "minValue": 0, "maxValue": 9, "stringValue": "P5",
                "lastSavedStringValue": "P5", "available": true},
            "VM_BRAKE_MIGRATION": {"value": 0, "maxValue": 5, "stringValue": " 0.0",
                "lastSavedStringValue": "0.0 "},
            "symmetric": false
        }"#;
        let setup = parse(body).unwrap();
        assert_eq!(setup.modified, Some(false));
        assert_eq!(setup.settings.len(), 3);
        assert_eq!(setting(&setup, "VM_BRAKE_MIGRATION").unwrap().value, "0.0");
        assert_eq!(
            setting(&setup, "VM_BRAKE_MIGRATION").unwrap().min_step,
            None
        );
    }

    #[test]
    fn malformed_answers_are_no_setup() {
        for body in [
            "",
            "not json",
            "[]",
            "{}",
            r#"{"VM_REAR_WING": {"value": "4"}}"#,
            r#"{"VM_REAR_WING": {"value": 4, "maxValue": 9}}"#,
            r#"{"VM_REAR_WING": {"value": 4, "maxValue": 9, "stringValue": "P5", "available": false}}"#,
            r#"{"VM_REAR_WING": [4, 9, "P5"], "gearGraph": {"VM_GEAR_1": 2.1}}"#,
            r#"{"symmetric": "true"}"#,
        ] {
            assert_eq!(parse(body), None, "{body}");
        }
    }

    /// One setting that isn't one makes the whole answer one to ask again: taken as it is, the
    /// settings it's missing would be recorded as gone.
    #[test]
    fn an_answer_with_a_malformed_setting_is_no_setup() {
        let mut json: Value = serde_json::from_str(GT3).unwrap();
        json["VM_REAR_WING"]
            .as_object_mut()
            .unwrap()
            .remove("stringValue");
        assert_eq!(parse(&json.to_string()), None);
        json["VM_REAR_WING"] = Value::from("P5");
        assert_eq!(parse(&json.to_string()), None);
    }

    /// `symmetric` alone isn't a setup: it's a flag about the others.
    #[test]
    fn a_setup_needs_a_garage_setting() {
        assert_eq!(parse(r#"{"symmetric": true}"#), None);
        let body = r#"{"VM_REAR_WING": {"value": 0, "maxValue": 9, "stringValue": "N/A"},
            "symmetric": true}"#;
        assert_eq!(parse(body), None);
    }

    /// The display text is what a setting is; its step and range are only kept as whole numbers
    /// that fit together.
    #[test]
    fn a_step_or_range_that_does_not_fit_is_left_out() {
        let body = r#"{
            "VM_REAR_WING": {"value": 4.0, "minValue": "0", "maxValue": 9, "stringValue": "P5"},
            "VM_FRONT_WING": {"value": 0, "minValue": 0, "maxValue": 0, "stringValue": "Fixed"},
            "VM_DIFF_PRELOAD": {"value": 14, "minValue": 20, "maxValue": 101, "stringValue": "70 Nm"},
            "VM_STEER_LOCK": {"stringValue": "310 deg"}
        }"#;
        let setup = parse(body).unwrap();
        let range = |name| {
            let s = setting(&setup, name).unwrap();
            (s.step, s.min_step, s.max_step)
        };
        // Not a whole number: no step; the range alone still fits
        assert_eq!(range("VM_REAR_WING"), (None, None, Some(8)));
        // No steps at all, or a step outside its range: no range
        assert_eq!(range("VM_FRONT_WING"), (Some(0), None, None));
        assert_eq!(range("VM_DIFF_PRELOAD"), (Some(14), None, None));
        assert_eq!(range("VM_STEER_LOCK"), (None, None, None));
        assert_eq!(setting(&setup, "VM_STEER_LOCK").unwrap().value, "310 deg");
    }

    fn steer_lock(text: &str) -> Setup {
        Setup {
            settings: vec![Setting {
                name: "VM_STEER_LOCK".into(),
                value: text.into(),
                ..Setting::default()
            }],
            ..Setup::default()
        }
    }

    #[test]
    fn the_wheel_range_is_the_steer_locks_first_number() {
        assert_eq!(wheel_range(&parse(GT3).unwrap()), Some(310.0));
        assert_eq!(wheel_range(&steer_lock("240 (9.3) deg")), Some(240.0));
        assert_eq!(
            wheel_range(&steer_lock("  540.5   (14)  deg ")),
            Some(540.5)
        );
    }

    /// Text that isn't like the known ones is no range, rather than a guess.
    #[test]
    fn an_unknown_steer_lock_text_is_no_wheel_range() {
        for text in [
            "",
            "deg",
            "310",
            "310 deg",
            "12 deg",
            "310°",
            "310 degrees",
            "Non-adjustable",
            "12 deg (310)",
            "(12) 310 deg",
            "310 (twelve) deg",
            "310 (12 deg",
            "310 12 deg",
            "0 (0) deg",
            "-310 (12) deg",
            "inf (12) deg",
            "NaN (12) deg",
        ] {
            assert_eq!(wheel_range(&steer_lock(text)), None, "{text}");
        }
        assert_eq!(wheel_range(&Setup::default()), None);
    }

    /// With no saved text to compare with, whether it's been changed isn't known.
    #[test]
    fn modified_is_unknown_without_saved_text() {
        let body = r#"{"VM_REAR_WING": {"value": 4, "maxValue": 9, "stringValue": "P5",
            "lastSavedStringValue": 1}}"#;
        let setup = parse(body).unwrap();
        assert_eq!(setup.modified, None);
        assert_eq!(setup.settings[0].saved_value, None);
    }
}
