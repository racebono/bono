//! LMU's pit menu, through its local web API (the one its UI uses; undocumented, so every answer
//! is checked for the shape it's expected to have). The menu is an array of rows, each with its
//! options' text in the game's language and the option set now (`currentSetting`); tyre options
//! also carry their compound (`type`, in English) and whether they're used (`isUsed`).

use crate::sim::pit::{Choice, Clear, Corner, CornerMenu, Number, PitMenu, Position, Resolved, Tyre};
use anyhow::{Context as _, bail};
use serde_json::Value;

const TYRE_ROWS: [&str; 4] = ["FL TIRE:", "FR TIRE:", "RL TIRE:", "RR TIRE:"];
const PRESSURE_ROWS: [&str; 4] = ["FL PRESS:", "FR PRESS:", "RL PRESS:", "RR PRESS:"];
const ENERGY: &str = "VIRTUAL ENERGY:";
const FUEL: &str = "FUEL:";
const FUEL_RATIO: &str = "FUEL RATIO:";
const DAMAGE: &str = "DAMAGE:";
const REPLACE_BRAKES: &str = "REPLACE BRAKES:";
const REAR_WING: &str = "R WING:";
const GRILLE: &str = "GRILLE:";
const FRONT_BRAKE_DUCT: &str = "F BRAKE DUCT:";
const REAR_BRAKE_DUCT: &str = "R BRAKE DUCT:";

/// A row of the menu as read.
struct Row<'a> {
    current: usize,
    options: &'a [Value],
}

/// The row named `name`, if the menu has it. Err if it's there but not shaped like a row.
fn row<'a>(rows: &'a [Value], name: &str) -> anyhow::Result<Option<Row<'a>>> {
    let Some(row) = rows.iter().find(|row| row.get("name").and_then(Value::as_str) == Some(name))
    else {
        return Ok(None);
    };
    let options = (row.get("settings").and_then(Value::as_array))
        .with_context(|| format!("the pit menu's {name} row has no options"))?;
    let current = (row.get("currentSetting").and_then(Value::as_u64))
        .and_then(|current| usize::try_from(current).ok())
        .filter(|current| *current < options.len())
        .with_context(|| format!("the pit menu's {name} row has no valid current option"))?;
    Ok(Some(Row { current, options }))
}

fn text(option: &Value) -> Option<&str> {
    option.get("text").and_then(Value::as_str)
}

fn texts(row: &Row, name: &str) -> anyhow::Result<Vec<String>> {
    (row.options.iter())
        .map(|option| text(option).map(str::to_owned))
        .collect::<Option<_>>()
        .with_context(|| format!("an option of the pit menu's {name} row has no text"))
}

/// The number an option's text starts with: `100% 16 Vueltas` is 100, `137 (+1)` is 137, `0,88`
/// is 0.88.
fn leading_number(text: &str) -> Option<f64> {
    let end = (text.char_indices())
        .find(|(_, c)| !(c.is_ascii_digit() || *c == '.' || *c == ','))
        .map_or(text.len(), |(at, _)| at);
    text[..end].replace(',', ".").parse().ok()
}

/// What's left of a used tyre's tread: the number before the text's last `%`.
fn tread(text: &str) -> Option<f64> {
    let before = text.trim_end().strip_suffix('%')?;
    let start = (before.char_indices().rev())
        .find(|(_, c)| !(c.is_ascii_digit() || *c == '.' || *c == ','))
        .map_or(0, |(at, c)| at + c.len_utf8());
    before[start..].replace(',', ".").parse().ok()
}

/// The numbers a row's options stand for, in order.
fn numbers(row: &Row, name: &str) -> anyhow::Result<Vec<f64>> {
    (row.options.iter())
        .map(|option| text(option).and_then(leading_number))
        .collect::<Option<_>>()
        .with_context(|| format!("an option of the pit menu's {name} row isn't a number"))
}

fn number(row: &Row, name: &str) -> anyhow::Result<Number> {
    let numbers = numbers(row, name)?;
    let step = match numbers[..] {
        [first, second, ..] => Some(second - first).filter(|step| *step > 0.0),
        _ => None,
    };
    Ok(Number {
        value: numbers[row.current],
        min: numbers.first().copied(),
        max: numbers.last().copied(),
        step,
    })
}

/// A tyre row's tyres (every option but "no change"), each with its option's index in the row.
/// Options without a compound and a used flag (LMU has shown an `INVALID` one) are left out.
fn tyres(row: &Row) -> Vec<(usize, Tyre)> {
    (row.options.iter().enumerate().skip(1))
        .filter_map(|(at, option)| {
            let compound = option.get("type").and_then(Value::as_str)?;
            let used = option.get("isUsed").and_then(Value::as_bool)?;
            let tread_pct = used.then(|| text(option).and_then(tread)).flatten();
            Some((
                at,
                Tyre {
                    compound: compound.to_owned(),
                    used,
                    tread_pct,
                },
            ))
        })
        .collect()
}

fn choice(row: &Row, name: &str) -> anyhow::Result<Choice> {
    Ok(Choice {
        current: row.current,
        options: texts(row, name)?,
    })
}

/// Where a car is, from its `pitState` in `/rest/watch/standings`.
pub fn position(pit_state: &str) -> Position {
    match pit_state {
        "NONE" => Position::OnTrack,
        "REQUEST" => Position::StopRequested,
        "ENTERING" => Position::Entering,
        "STOPPED" => Position::Stopped,
        "EXITING" => Position::Exiting,
        _ => Position::Unknown,
    }
}

/// The player's car in the standings, if it's there.
pub fn player(standings: &Value) -> anyhow::Result<Option<&Value>> {
    let cars = standings
        .as_array()
        .context("LMU's standings aren't a list of cars")?;
    Ok(cars
        .iter()
        .find(|car| car.get("player").and_then(Value::as_bool) == Some(true)))
}

/// The menu in Bono's terms, from the raw `receivePitMenu` array and the `standings` array.
pub fn parse(rows: &Value, standings: &Value) -> anyhow::Result<PitMenu> {
    let rows = rows.as_array().context("LMU's pit menu isn't a list of rows")?;
    let player = player(standings)?;
    let mut menu = PitMenu {
        position: (player.and_then(|car| car.get("pitState")).and_then(Value::as_str))
            .map_or(Position::Unknown, position),
        ..PitMenu::default()
    };

    if let Some(row) = row(rows, ENERGY)? {
        menu.energy_leave_pct = Some(number(&row, ENERGY)?);
        menu.level_now = (player.and_then(|car| car.get("veFraction")).and_then(Value::as_f64))
            .map(|fraction| fraction * 100.0);
    }
    if let Some(row) = row(rows, FUEL)? {
        menu.fuel_leave_l = Some(number(&row, FUEL)?);
    }
    if let Some(row) = row(rows, FUEL_RATIO)? {
        menu.fuel_ratio = Some(number(&row, FUEL_RATIO)?);
    }

    let mut corners = Vec::new();
    for corner in Corner::ALL {
        let name = TYRE_ROWS[corner as usize];
        let Some(tyre_row) = row(rows, name)? else {
            break;
        };
        let tyres = tyres(&tyre_row);
        let fitted = tyres.iter().position(|(at, _)| *at == tyre_row.current);
        let pressure_name = PRESSURE_ROWS[corner as usize];
        let pressure_kpa = match row(rows, pressure_name)? {
            Some(row) => Some(number(&row, pressure_name)?),
            None => None,
        };
        corners.push(CornerMenu {
            fitted,
            tyres: tyres.into_iter().map(|(_, tyre)| tyre).collect(),
            pressure_kpa,
        });
    }
    // All four, or none: a car with only some corner rows isn't one this understands
    menu.corners = corners.try_into().ok();

    if let Some(row) = row(rows, DAMAGE)? {
        menu.repair = Some(choice(&row, DAMAGE)?);
    }
    if let Some(row) = row(rows, REPLACE_BRAKES)? {
        if row.options.len() != 2 {
            bail!("the pit menu's {REPLACE_BRAKES} row doesn't have two options");
        }
        menu.replace_brakes = Some(row.current != 0);
    }
    for (name, field) in [
        (REAR_WING, &mut menu.rear_wing),
        (GRILLE, &mut menu.grille),
        (FRONT_BRAKE_DUCT, &mut menu.front_brake_duct),
        (REAR_BRAKE_DUCT, &mut menu.rear_brake_duct),
    ] {
        if let Some(row) = row(rows, name)? {
            *field = Some(choice(&row, name)?);
        }
    }
    Ok(menu)
}

/// `rows` (as read) with `currentSetting` changed for `change`, every other byte kept: LMU takes
/// the whole array back. Each row it touches must still offer what `menu` said, or the menu
/// changed since it was read and nothing is written.
pub fn apply(rows: &Value, menu: &PitMenu, change: &Resolved) -> anyhow::Result<Value> {
    let mut out = rows.clone();
    let read = rows.as_array().context("LMU's pit menu isn't a list of rows")?;
    let changed = || anyhow::anyhow!("the pit menu changed since it was read: read it again");
    let mut set: Vec<(&str, usize)> = Vec::new();

    let number_at = |name: &'static str, want: f64| -> anyhow::Result<(&'static str, usize)> {
        let row = row(read, name)?.ok_or_else(changed)?;
        let numbers = numbers(&row, name)?;
        let tolerance = match numbers[..] {
            [first, second, ..] => (second - first).abs() / 2.0,
            _ => 0.0,
        }
        .max(1e-6);
        let at = (numbers.iter())
            .position(|number| (number - want).abs() <= tolerance)
            .ok_or_else(changed)?;
        Ok((name, at))
    };

    // A clear first, so the fields after it can set what it cleared
    if let Some(clear) = change.clear {
        for name in TYRE_ROWS {
            if row(read, name)?.is_some() {
                set.push((name, 0));
            }
        }
        if clear == Clear::All {
            for name in [DAMAGE, REPLACE_BRAKES] {
                if row(read, name)?.is_some() {
                    set.push((name, 0));
                }
            }
            if let (Some(_), Some(level)) = (&menu.energy_leave_pct, menu.level_now)
                && let Some(row) = row(read, ENERGY)?
            {
                let numbers = numbers(&row, ENERGY)?;
                if let Some(at) = numbers.iter().rposition(|number| *number <= level) {
                    set.push((ENERGY, at));
                }
            }
        }
    }

    if let Some(want) = change.energy_leave_pct {
        set.push(number_at(ENERGY, want)?);
    }
    if let Some(want) = change.fuel_leave_l {
        set.push(number_at(FUEL, want)?);
    }
    if let Some(want) = change.fuel_ratio {
        set.push(number_at(FUEL_RATIO, want)?);
    }

    let corners = menu.corners.as_ref();
    for corner in Corner::ALL {
        let at = corner as usize;
        if let Some(fit) = change.tyres[at] {
            let name = TYRE_ROWS[at];
            let row = row(read, name)?.ok_or_else(changed)?;
            let now = tyres(&row);
            let expected = &corners.ok_or_else(changed)?[at].tyres;
            if now.len() != expected.len() || now.iter().zip(expected).any(|((_, a), b)| a != b) {
                return Err(changed());
            }
            set.push((name, fit.map_or(0, |fit| now[fit].0)));
        }
        if let Some(want) = change.pressures_kpa[at] {
            set.push(number_at(PRESSURE_ROWS[at], want)?);
        }
    }

    for (name, field, want) in [
        (DAMAGE, &menu.repair, change.repair),
        (REAR_WING, &menu.rear_wing, change.rear_wing),
        (GRILLE, &menu.grille, change.grille),
        (FRONT_BRAKE_DUCT, &menu.front_brake_duct, change.front_brake_duct),
        (REAR_BRAKE_DUCT, &menu.rear_brake_duct, change.rear_brake_duct),
    ] {
        let Some(want) = want else { continue };
        let row = row(read, name)?.ok_or_else(changed)?;
        if Some(&texts(&row, name)?) != field.as_ref().map(|choice| &choice.options) {
            return Err(changed());
        }
        set.push((name, want));
    }
    if let Some(want) = change.replace_brakes {
        if row(read, REPLACE_BRAKES)?.is_none() {
            return Err(changed());
        }
        set.push((REPLACE_BRAKES, usize::from(want)));
    }

    let rows = out.as_array_mut().context("LMU's pit menu isn't a list of rows")?;
    for (name, at) in set {
        let row = (rows.iter_mut())
            .find(|row| row.get("name").and_then(Value::as_str) == Some(name))
            .ok_or_else(changed)?;
        row["currentSetting"] = at.into();
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sim::pit::{CornerMap, PitChange, check};

    fn fixture(name: &str) -> Value {
        let path = format!("{}/src/sim/lmu/fixtures/{name}", env!("CARGO_MANIFEST_DIR"));
        serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
    }

    fn menu_rows() -> Value {
        fixture("pit_menu_used_es.json")
    }

    fn parsed() -> PitMenu {
        parse(&menu_rows(), &fixture("standings.json")).unwrap()
    }

    fn current(rows: &Value, name: &str) -> u64 {
        let row = rows
            .as_array()
            .unwrap()
            .iter()
            .find(|row| row["name"] == name)
            .unwrap();
        row["currentSetting"].as_u64().unwrap()
    }

    #[test]
    fn parses_spanish_used_menu() {
        let m = parsed();
        let energy = m.energy_leave_pct.as_ref().unwrap();
        assert_eq!((energy.value, energy.min, energy.max, energy.step), (100.0, Some(0.0), Some(100.0), Some(1.0)));
        assert!(m.fuel_leave_l.is_none() && m.fuel_add_l.is_none());
        assert_eq!(m.fuel_ratio.as_ref().unwrap().value, 0.88);
        let rr = &m.corners.as_ref().unwrap()[Corner::Rr as usize];
        assert_eq!(rr.fitted, None);
        assert!(rr.tyres.contains(&Tyre {
            compound: "Medium".into(),
            used: true,
            tread_pct: Some(98.0)
        }));
        assert_eq!(rr.pressure_kpa.as_ref().unwrap().value, 136.0);
        assert_eq!(m.repair.as_ref().unwrap().options, ["N/D"]);
        assert_eq!(m.rear_wing.as_ref().unwrap().options[5], "11.3 grados");
        assert_eq!(m.replace_brakes, Some(false));
        assert_eq!(m.position, Position::OnTrack);
        let level = m.level_now.unwrap();
        assert!((level - 88.488).abs() < 0.01, "{level}");
    }

    #[test]
    fn apply_sets_only_touched_rows_and_keeps_the_array() {
        let rows = menu_rows();
        let m = parsed();
        let change = PitChange {
            tyres: CornerMap {
                fl: Some(serde_json::from_value(serde_json::json!({"compound": "Wet"})).unwrap()),
                ..Default::default()
            },
            ..Default::default()
        };
        let out = apply(&rows, &m, &check(&m, &change).unwrap()).unwrap();
        assert_eq!(current(&out, "FL TIRE:"), 2);
        let (before, after) = (rows.as_array().unwrap(), out.as_array().unwrap());
        assert_eq!(before.len(), after.len());
        for (before, after) in before.iter().zip(after) {
            assert_eq!(before["name"], after["name"]);
            if after["name"] != "FL TIRE:" {
                assert_eq!(before, after);
            }
        }
    }

    #[test]
    fn apply_keeps_replace_brakes_as_read() {
        let mut rows = menu_rows();
        let brakes = (rows.as_array_mut().unwrap().iter_mut())
            .find(|row| row["name"] == REPLACE_BRAKES)
            .unwrap();
        brakes["currentSetting"] = 1.into();
        let m = parse(&rows, &fixture("standings.json")).unwrap();
        let change = PitChange {
            energy_leave_pct: Some(50.0),
            ..Default::default()
        };
        let out = apply(&rows, &m, &check(&m, &change).unwrap()).unwrap();
        assert_eq!(current(&out, REPLACE_BRAKES), 1);
        assert_eq!(current(&out, ENERGY), 50);
    }

    #[test]
    fn apply_refuses_when_menu_moved() {
        let m = parsed();
        let change = PitChange {
            tyres: CornerMap {
                fl: Some(
                    serde_json::from_value(serde_json::json!({"compound": "Medium", "used": true}))
                        .unwrap(),
                ),
                ..Default::default()
            },
            ..Default::default()
        };
        let resolved = check(&m, &change).unwrap();
        let mut rows = menu_rows();
        let fl = (rows.as_array_mut().unwrap().iter_mut())
            .find(|row| row["name"] == "FL TIRE:")
            .unwrap();
        fl["settings"].as_array_mut().unwrap().pop();
        let e = apply(&rows, &m, &resolved).unwrap_err().to_string();
        assert!(e.contains("changed"), "{e}");
    }

    #[test]
    fn clear_all_spares_aero() {
        let rows = menu_rows();
        let mut m = parsed();
        let change = PitChange {
            clear: Some(Clear::All),
            ..Default::default()
        };
        let resolved = check(&m, &change).unwrap();
        let out = apply(&rows, &m, &resolved).unwrap();
        for name in TYRE_ROWS.into_iter().chain([DAMAGE, REPLACE_BRAKES]) {
            assert_eq!(current(&out, name), 0, "{name}");
        }
        for name in [REAR_WING, GRILLE, FRONT_BRAKE_DUCT, REAR_BRAKE_DUCT, FUEL_RATIO] {
            assert_eq!(current(&out, name), current(&rows, name), "{name}");
        }
        // Nothing added: the highest energy not above the 88.5% the car has
        assert_eq!(current(&out, ENERGY), 88);
        // Without a level, the energy is left alone
        m.level_now = None;
        let out = apply(&rows, &m, &resolved).unwrap();
        assert_eq!(current(&out, ENERGY), current(&rows, ENERGY));
    }

    #[test]
    fn clear_tyres_only_touches_tyres() {
        let rows = menu_rows();
        let m = parsed();
        let change = PitChange {
            clear: Some(Clear::Tyres),
            ..Default::default()
        };
        let out = apply(&rows, &m, &check(&m, &change).unwrap()).unwrap();
        assert_eq!(current(&out, ENERGY), current(&rows, ENERGY));
    }

    #[test]
    fn numbers_from_option_text() {
        assert_eq!(leading_number("100% 16 Vueltas"), Some(100.0));
        assert_eq!(leading_number("137 (+1)"), Some(137.0));
        assert_eq!(leading_number("0,88"), Some(0.88));
        assert_eq!(leading_number("Abierto"), None);
        assert_eq!(tread("Usado Medio  99%"), Some(99.0));
        assert_eq!(tread("Nuevo Medio "), None);
    }

    #[test]
    fn unknown_pit_state_is_unknown() {
        assert_eq!(position("SOMETHING"), Position::Unknown);
        assert_eq!(position("STOPPED"), Position::Stopped);
    }

    #[test]
    fn wrong_shape_is_an_error() {
        let rows = serde_json::json!([{"name": "VIRTUAL ENERGY:", "currentSetting": 3, "settings": [{"text": "0%"}]}]);
        assert!(parse(&rows, &serde_json::json!([])).is_err());
        assert!(parse(&serde_json::json!({}), &serde_json::json!([])).is_err());
    }
}
