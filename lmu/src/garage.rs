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
/// The web API doesn't document it, so every field is checked, and a setting without its step, range
/// and text is left out. Settings the car lacks read `N/A`, and are left out too. An answer with no
/// settings isn't an answer.
fn parse(body: &str) -> Option<Setup> {
    let Ok(Value::Object(json)) = serde_json::from_str::<Value>(body) else {
        return None;
    };
    let mut settings = json
        .iter()
        .filter_map(|(name, value)| match value {
            Value::Object(setting) => garage_setting(name, setting),
            // A flag with no step or text of its own
            Value::Bool(on) if name == "symmetric" => Some(Setting {
                name: name.clone(),
                value: on.to_string(),
                ..Setting::default()
            }),
            _ => None,
        })
        .collect::<Vec<_>>();
    if settings.is_empty() {
        return None;
    }
    settings.sort_by(|a, b| a.name.cmp(&b.name));
    let modified = settings.iter().any(|setting| {
        setting
            .saved_value
            .as_ref()
            .is_some_and(|saved| *saved != setting.value)
    });
    Some(Setup {
        modified: Some(modified),
        settings,
        ..Setup::default()
    })
}

fn garage_setting(name: &str, setting: &Map<String, Value>) -> Option<Setting> {
    let text = |key| setting.get(key).and_then(Value::as_str).map(str::trim);
    if setting.get("available").and_then(Value::as_bool) == Some(false) {
        return None;
    }
    let value = text("stringValue").filter(|value| *value != "N/A")?;
    let step = setting.get("value").and_then(Value::as_i64)?;
    // A count of steps, so the top one is one less
    let max_step = setting
        .get("maxValue")
        .and_then(Value::as_i64)?
        .checked_sub(1)?;
    Some(Setting {
        name: name.to_owned(),
        value: value.to_owned(),
        step: Some(step),
        min_step: setting.get("minValue").and_then(Value::as_i64),
        max_step: Some(max_step),
        saved_value: text("lastSavedStringValue").map(str::to_owned),
    })
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
            r#"{"VM_REAR_WING": {"value": 4.5, "maxValue": 9, "stringValue": "P5"}}"#,
            r#"{"VM_REAR_WING": {"value": 4, "maxValue": 9, "stringValue": "P5", "available": false}}"#,
            r#"{"VM_REAR_WING": [4, 9, "P5"], "gearGraph": {"VM_GEAR_1": 2.1}}"#,
            r#"{"symmetric": "true"}"#,
        ] {
            assert_eq!(parse(body), None, "{body}");
        }

        // Fields of the wrong type are left out, and so is a setting without the ones it needs
        let body = r#"{
            "VM_REAR_WING": {"value": 4, "minValue": "0", "maxValue": 9, "stringValue": "P5",
                "lastSavedStringValue": 1, "available": "yes"},
            "VM_FRONT_WING": {"value": 2, "maxValue": 5}
        }"#;
        let setup = parse(body).unwrap();
        assert_eq!(
            setup.settings,
            [Setting {
                name: "VM_REAR_WING".into(),
                value: "P5".into(),
                step: Some(4),
                min_step: None,
                max_step: Some(8),
                saved_value: None,
            }]
        );
        assert_eq!(setup.modified, Some(false));
    }
}
