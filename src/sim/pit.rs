//! The pit menu in Bono's own terms, shared by the games that let the engineer change it: what the
//! car can take for its next stop, the change the engineer asks for, and the checks that turn one
//! into something a game can write. Each game reads and writes it through its [`PitService`].

use anyhow::bail;
use serde::Deserialize;

/// A car's corner, in the order `[T; 4]` arrays here are indexed by (`corner as usize`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Corner {
    Fl,
    Fr,
    Rl,
    Rr,
}

impl Corner {
    pub const ALL: [Corner; 4] = [Corner::Fl, Corner::Fr, Corner::Rl, Corner::Rr];

    pub fn name(self) -> &'static str {
        match self {
            Corner::Fl => "FL",
            Corner::Fr => "FR",
            Corner::Rl => "RL",
            Corner::Rr => "RR",
        }
    }
}

/// Where the car is, as far as the pits go.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Position {
    OnTrack,
    /// A stop is requested (LMU)
    StopRequested,
    /// In the pit lane, not stopped yet
    Entering,
    /// Stopped in its box
    Stopped,
    /// Leaving the pits
    Exiting,
    #[default]
    Unknown,
}

/// Which stop a change made now counts for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Timing {
    ThisStop,
    NextStop,
    /// iRacing clears what's ticked when the service ends, and ticks every service again as the
    /// car leaves the pits
    Lost,
    Unknown,
}

/// Which stop a change made while the car is at `position` counts for. Seen in both games: a
/// change made entering the pit lane is still set when the service starts. Once stopped, LMU
/// keeps it for the next stop, while iRacing's crew still does it at this one (what's ticked
/// mid-service is serviced); iRacing then clears everything when the service ends and ticks every
/// service again as the car leaves the pits.
pub fn timing(position: Position, iracing: bool) -> Timing {
    match position {
        Position::OnTrack | Position::StopRequested | Position::Entering => Timing::ThisStop,
        Position::Stopped if iracing => Timing::ThisStop,
        Position::Exiting if iracing => Timing::Lost,
        Position::Stopped | Position::Exiting => Timing::NextStop,
        Position::Unknown => Timing::Unknown,
    }
}

/// A number the menu sets, with what it can take.
#[derive(Clone, Debug, PartialEq)]
pub struct Number {
    pub value: f64,
    pub min: Option<f64>,
    pub max: Option<f64>,
    /// The values it takes are `min` (or 0) plus whole steps
    pub step: Option<f64>,
    /// The values it takes, when the game lists them (LMU): a value is set to the nearest. The
    /// game's text rounds them, so they needn't be whole steps apart
    pub options: Vec<f64>,
}

/// One of a row's options, by text, as the game words them.
#[derive(Clone, Debug, PartialEq)]
pub struct Choice {
    pub current: usize,
    pub options: Vec<String>,
}

/// A tyre a corner can be fitted with.
#[derive(Clone, Debug, PartialEq)]
pub struct Tyre {
    /// As the game names it, e.g. `Medium`
    pub compound: String,
    pub used: bool,
    /// What's left of a used tyre's tread, when the game says
    pub tread_pct: Option<f64>,
}

impl Tyre {
    /// e.g. `"Medium", used, 99%`: the compound is the game's text, quoted and escaped
    pub fn describe(&self) -> String {
        let mut text = format!(
            "{:?}, {}",
            self.compound,
            if self.used { "used" } else { "new" }
        );
        if let Some(tread) = self.tread_pct {
            text.push_str(&format!(", {tread}%"));
        }
        text
    }
}

/// A corner's part of the menu.
#[derive(Clone, Debug, PartialEq)]
pub struct CornerMenu {
    /// The tyre fitted at the stop, by its index in `tyres`: `None` leaves the one on
    pub fitted: Option<usize>,
    pub tyres: Vec<Tyre>,
    pub pressure_kpa: Option<Number>,
}

/// The pit menu: each field the car has, with what it's set to and what it can take. Fields the
/// car doesn't have are `None`.
#[derive(Clone, Debug, PartialEq, Default)]
pub struct PitMenu {
    pub position: Position,
    /// Fuel added at the stop (iRacing)
    pub fuel_add_l: Option<Number>,
    /// Fuel the car leaves the stop with (LMU without virtual energy)
    pub fuel_leave_l: Option<Number>,
    /// Energy the car leaves the stop with (LMU with virtual energy)
    pub energy_leave_pct: Option<Number>,
    pub fuel_ratio: Option<Number>,
    /// Indexed by `Corner as usize`
    pub corners: Option<[CornerMenu; 4]>,
    /// iRacing: every fitted tyre takes one compound, and a pressure ticks its tyre
    pub iracing_tyres: bool,
    pub repair: Option<Choice>,
    pub replace_brakes: Option<bool>,
    pub rear_wing: Option<Choice>,
    pub grille: Option<Choice>,
    pub front_brake_duct: Option<Choice>,
    pub rear_brake_duct: Option<Choice>,
    pub tear_off: Option<bool>,
}

/// What a change clears first, before its other fields.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Clear {
    /// Every service: no tyres, no fuel added (iRacing), no repair, no brakes, no tear-off. What
    /// the car leaves with (LMU) stays as set
    All,
    Tyres,
}

/// What the engineer asks for a corner's tyre.
#[derive(Clone, Debug, PartialEq, Deserialize)]
#[serde(untagged)]
pub enum TyreRequest {
    NoChange(NoChangeTag),
    Fit(Fit),
}

/// A tyre to fit: any other field is an error, not dropped.
#[derive(Clone, Debug, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Fit {
    pub compound: String,
    #[serde(default)]
    pub used: bool,
    pub tread_pct: Option<f64>,
}

/// `"no_change"`
#[derive(Clone, Copy, Debug, PartialEq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NoChangeTag {
    NoChange,
}

/// A value per corner, each optional. Any other key is an error: `"FL"` or `"all"` mustn't read
/// as no corner at all.
#[derive(Clone, Debug, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CornerMap<T> {
    pub fl: Option<T>,
    pub fr: Option<T>,
    pub rl: Option<T>,
    pub rr: Option<T>,
}

impl<T> Default for CornerMap<T> {
    fn default() -> Self {
        Self {
            fl: None,
            fr: None,
            rl: None,
            rr: None,
        }
    }
}

impl<T> CornerMap<T> {
    /// Indexed by `Corner as usize`
    fn by_corner(&self) -> [Option<&T>; 4] {
        [
            self.fl.as_ref(),
            self.fr.as_ref(),
            self.rl.as_ref(),
            self.rr.as_ref(),
        ]
    }
}

/// The change the engineer asks for: only the fields it sets.
#[derive(Clone, Debug, PartialEq, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PitChange {
    pub clear: Option<Clear>,
    pub fuel_add_l: Option<f64>,
    pub fuel_leave_l: Option<f64>,
    pub energy_leave_pct: Option<f64>,
    pub fuel_ratio: Option<f64>,
    #[serde(default)]
    pub tyres: CornerMap<TyreRequest>,
    #[serde(default)]
    pub pressures_kpa: CornerMap<f64>,
    pub repair: Option<String>,
    pub replace_brakes: Option<bool>,
    pub rear_wing: Option<String>,
    pub grille: Option<String>,
    pub front_brake_duct: Option<String>,
    pub rear_brake_duct: Option<String>,
    pub tear_off: Option<bool>,
}

/// A checked change: numbers snapped to the menu's steps, options by index.
#[derive(Clone, Debug, PartialEq, Default)]
pub struct Resolved {
    pub clear: Option<Clear>,
    pub fuel_add_l: Option<f64>,
    pub fuel_leave_l: Option<f64>,
    pub energy_leave_pct: Option<f64>,
    pub fuel_ratio: Option<f64>,
    /// `None`: untouched; `Some(None)`: no change; `Some(Some(i))`: that corner's `tyres[i]`
    pub tyres: [Option<Option<usize>>; 4],
    pub pressures_kpa: [Option<f64>; 4],
    pub repair: Option<usize>,
    pub replace_brakes: Option<bool>,
    pub rear_wing: Option<usize>,
    pub grille: Option<usize>,
    pub front_brake_duct: Option<usize>,
    pub rear_brake_duct: Option<usize>,
    pub tear_off: Option<bool>,
}

/// Checks `change` against what the car can take, and resolves it for the game to write. Any
/// field that's wrong rejects the whole change, with an error worded for the engineer.
pub fn check(menu: &PitMenu, change: &PitChange) -> anyhow::Result<Resolved> {
    let has = fields_text(menu);
    let number = |name: &str, field: &Option<Number>, value: Option<f64>| match (field, value) {
        (_, None) => Ok(None),
        (None, Some(_)) => bail!("this car has no `{name}`; it has: {has}"),
        (Some(number), Some(value)) => snap(name, number, value).map(Some),
    };
    let choice = |name: &str, field: &Option<Choice>, text: &Option<String>| match (field, text) {
        (_, None) => Ok(None),
        (None, Some(_)) => bail!("this car has no `{name}`; it has: {has}"),
        (Some(choice), Some(text)) => match choice.options.iter().position(|o| o == text) {
            Some(at) => Ok(Some(at)),
            None => bail!(
                "`{name}` has no option {text:?}; its options are {}",
                quoted(&choice.options)
            ),
        },
    };
    let flag = |name: &str, field: &Option<bool>, value: Option<bool>| match (field, value) {
        (None, Some(_)) => bail!("this car has no `{name}`; it has: {has}"),
        (_, value) => Ok(value),
    };

    let mut resolved = Resolved {
        clear: change.clear,
        fuel_add_l: number("fuel_add_l", &menu.fuel_add_l, change.fuel_add_l)?,
        fuel_leave_l: number("fuel_leave_l", &menu.fuel_leave_l, change.fuel_leave_l)?,
        energy_leave_pct: number(
            "energy_leave_pct",
            &menu.energy_leave_pct,
            change.energy_leave_pct,
        )?,
        fuel_ratio: number("fuel_ratio", &menu.fuel_ratio, change.fuel_ratio)?,
        repair: choice("repair", &menu.repair, &change.repair)?,
        replace_brakes: flag("replace_brakes", &menu.replace_brakes, change.replace_brakes)?,
        rear_wing: choice("rear_wing", &menu.rear_wing, &change.rear_wing)?,
        grille: choice("grille", &menu.grille, &change.grille)?,
        front_brake_duct: choice(
            "front_brake_duct",
            &menu.front_brake_duct,
            &change.front_brake_duct,
        )?,
        rear_brake_duct: choice(
            "rear_brake_duct",
            &menu.rear_brake_duct,
            &change.rear_brake_duct,
        )?,
        tear_off: flag("tear_off", &menu.tear_off, change.tear_off)?,
        ..Resolved::default()
    };

    let tyres = change.tyres.by_corner();
    let pressures = change.pressures_kpa.by_corner();
    let touched = tyres.iter().any(Option::is_some) || pressures.iter().any(Option::is_some);
    let corners = match (&menu.corners, touched) {
        (_, false) if resolved == Resolved::default() => {
            bail!("the change sets nothing: give the fields to change, as PIT OPTIONS names them")
        }
        (_, false) => return Ok(resolved),
        (None, true) => bail!("this car's tyres can't be set from the pit menu"),
        (Some(corners), true) => corners,
    };
    for corner in Corner::ALL {
        let menu = &corners[corner as usize];
        let at = corner as usize;
        resolved.tyres[at] = match tyres[at] {
            None => None,
            Some(TyreRequest::NoChange(_)) => Some(None),
            Some(TyreRequest::Fit(Fit {
                compound,
                used,
                tread_pct,
            })) => Some(Some(fit(corner, menu, compound, *used, *tread_pct)?)),
        };
        if let Some(pressure) = pressures[at] {
            let Some(number) = &menu.pressure_kpa else {
                bail!("the {} pressure can't be set from the pit menu", corner.name());
            };
            resolved.pressures_kpa[at] = Some(snap(
                &format!("{} pressure", corner.name()),
                number,
                *pressure,
            )?);
        }
    }

    if menu.iracing_tyres {
        let mut compounds = Corner::ALL.into_iter().filter_map(|corner| {
            let at = resolved.tyres[corner as usize]??;
            Some(&corners[corner as usize].tyres[at].compound)
        });
        if let Some(first) = compounds.next() {
            if compounds.any(|other| other != first) {
                bail!(
                    "iRacing fits one compound for the whole stop: pick the same one for every \
                     corner"
                );
            }
            // The compound is the stop's: a corner already being changed would take it too
            let moved: Vec<_> = (Corner::ALL.into_iter())
                .filter(|corner| {
                    let at = *corner as usize;
                    resolved.tyres[at].is_none()
                        && resolved.clear.is_none()
                        && (corners[at].fitted)
                            .is_some_and(|fitted| &corners[at].tyres[fitted].compound != first)
                })
                .map(Corner::name)
                .collect();
            if !moved.is_empty() {
                bail!(
                    "iRacing fits one compound for the whole stop, and {} already has a tyre \
                     change on another one: set it to {first:?} too, or to no_change",
                    moved.join(", ")
                );
            }
        }
        // A tyre is left out by clearing all four and ticking the others again: those are part
        // of the change, so the read-back waits for them too
        let untick = (0..4).any(|at| resolved.tyres[at] == Some(None) && corners[at].fitted.is_some());
        if untick && resolved.clear.is_none() {
            for at in 0..4 {
                if resolved.tyres[at].is_none() && corners[at].fitted.is_some() {
                    resolved.tyres[at] = Some(corners[at].fitted);
                }
            }
        }
        for corner in Corner::ALL {
            let at = corner as usize;
            let fitted = match resolved.tyres[at] {
                Some(fitted) => fitted.is_some(),
                // A clear unticks it first
                None => resolved.clear.is_none() && corners[at].fitted.is_some(),
            };
            if resolved.pressures_kpa[at].is_some() && !fitted {
                bail!(
                    "in iRacing a pressure only applies to a tyre being changed: change the {} \
                     tyre too, or leave its pressure out",
                    corner.name()
                );
            }
        }
    }
    Ok(resolved)
}

/// The tyre `corner` offers that matches, by its index in the corner's `tyres`.
fn fit(
    corner: Corner,
    menu: &CornerMenu,
    compound: &str,
    used: bool,
    tread_pct: Option<f64>,
) -> anyhow::Result<usize> {
    let matches: Vec<usize> = (menu.tyres.iter().enumerate())
        .filter(|(_, tyre)| {
            tyre.compound.eq_ignore_ascii_case(compound)
                && tyre.used == used
                && tread_pct.is_none_or(|want| {
                    tyre.tread_pct.is_some_and(|tread| (tread - want).abs() <= 0.5)
                })
        })
        .map(|(at, _)| at)
        .collect();
    let offered = || {
        (menu.tyres.iter())
            .map(Tyre::describe)
            .collect::<Vec<_>>()
            .join("; ")
    };
    match matches[..] {
        [at] => Ok(at),
        // The same tyre twice: either will do
        [first, ..] if matches.iter().all(|at| menu.tyres[*at] == menu.tyres[first]) => Ok(first),
        [] => bail!(
            "{} doesn't offer {compound}, {}; it offers: {}",
            corner.name(),
            if used { "used" } else { "new" },
            offered()
        ),
        _ => bail!(
            "{} offers more than one {compound}, {}: give `tread_pct` to pick one of: {}",
            corner.name(),
            if used { "used" } else { "new" },
            offered()
        ),
    }
}

/// `value` checked against `number`'s limits and snapped to its step.
fn snap(name: &str, number: &Number, value: f64) -> anyhow::Result<f64> {
    if !value.is_finite() {
        bail!("`{name}` must be a number");
    }
    let low = number.min.unwrap_or(f64::NEG_INFINITY);
    let high = number.max.unwrap_or(f64::INFINITY);
    // Listed options are shown rounded: a value at the end as shown is that end
    let slack = match number.options.is_empty() {
        true => 0.0,
        false => number.step.map_or(0.0, |step| step / 2.0),
    };
    if value < low - slack || value > high + slack {
        match (number.min, number.max) {
            (Some(min), Some(max)) => bail!("`{name}` must be between {min} and {max}"),
            (Some(min), None) => bail!("`{name}` must be at least {min}"),
            (None, Some(max)) => bail!("`{name}` must be at most {max}"),
            (None, None) => unreachable!("an unlimited number takes any finite value"),
        }
    }
    // The nearest value the game lists, when it lists them
    if let Some(nearest) = (number.options.iter().copied())
        .min_by(|a, b| (a - value).abs().total_cmp(&(b - value).abs()))
    {
        return Ok(nearest);
    }
    let Some(step) = number.step.filter(|step| *step > 0.0) else {
        return Ok(value);
    };
    let base = number.min.unwrap_or(0.0);
    let snapped = base + ((value - base) / step).round() * step;
    // Only float noise trimmed (0.8800000000000001 is 0.88): it never moves off the grid
    let snapped = (snapped * 1e9).round() / 1e9;
    Ok(snapped.clamp(low, high))
}

/// The fields the car has, for an error naming what it can take instead.
fn fields_text(menu: &PitMenu) -> String {
    let fields = [
        ("fuel_add_l", menu.fuel_add_l.is_some()),
        ("fuel_leave_l", menu.fuel_leave_l.is_some()),
        ("energy_leave_pct", menu.energy_leave_pct.is_some()),
        ("fuel_ratio", menu.fuel_ratio.is_some()),
        ("tyres", menu.corners.is_some()),
        (
            "pressures_kpa",
            (menu.corners.iter().flatten()).any(|c| c.pressure_kpa.is_some()),
        ),
        ("repair", menu.repair.is_some()),
        ("replace_brakes", menu.replace_brakes.is_some()),
        ("rear_wing", menu.rear_wing.is_some()),
        ("grille", menu.grille.is_some()),
        ("front_brake_duct", menu.front_brake_duct.is_some()),
        ("rear_brake_duct", menu.rear_brake_duct.is_some()),
        ("tear_off", menu.tear_off.is_some()),
    ];
    let has: Vec<_> = (fields.iter())
        .filter(|(_, has)| *has)
        .map(|(name, _)| *name)
        .collect();
    match has.is_empty() {
        true => "nothing".into(),
        false => has.join(", "),
    }
}

fn quoted(options: &[String]) -> String {
    (options.iter())
        .map(|o| format!("{o:?}"))
        .collect::<Vec<_>>()
        .join(", ")
}

/// The fields of `resolved` that `after` doesn't show, worded for the engineer.
pub fn unmet(after: &PitMenu, resolved: &Resolved) -> Vec<String> {
    let mut unmet = Vec::new();
    let mut number = |name: &str, field: &Option<Number>, want: Option<f64>| {
        let Some(want) = want else { return };
        let close = field.as_ref().is_some_and(|number| {
            let tolerance = number.step.map_or(0.05, |step| step / 2.0);
            (number.value - want).abs() <= tolerance
        });
        if !close {
            unmet.push(name.to_string());
        }
    };
    number("fuel", &after.fuel_add_l, resolved.fuel_add_l);
    number("fuel", &after.fuel_leave_l, resolved.fuel_leave_l);
    number("energy", &after.energy_leave_pct, resolved.energy_leave_pct);
    number("fuel ratio", &after.fuel_ratio, resolved.fuel_ratio);
    // A clear adds no fuel and no services: what it couldn't clear is said, not taken as cleared
    if resolved.clear == Some(Clear::All) {
        if resolved.repair.is_none() && after.repair.as_ref().is_some_and(|r| r.current != 0) {
            unmet.push("repair".into());
        }
        if resolved.replace_brakes.is_none() && after.replace_brakes == Some(true) {
            unmet.push("replace brakes".into());
        }
        if resolved.tear_off.is_none() && after.tear_off == Some(true) {
            unmet.push("tear-off".into());
        }
        if resolved.fuel_add_l.is_none()
            && after.fuel_add_l.as_ref().is_some_and(|fuel| fuel.value > 0.5)
        {
            unmet.push("fuel".into());
        }
    }

    let cleared = resolved.clear.is_some();
    for corner in Corner::ALL {
        let at = corner as usize;
        let menu = after.corners.as_ref().map(|corners| &corners[at]);
        let want = match (resolved.tyres[at], cleared) {
            (Some(want), _) => Some(want),
            // Only a car with tyres to set had them cleared
            (None, true) if after.corners.is_some() => Some(None),
            (None, _) => None,
        };
        if let Some(want) = want
            && menu.is_none_or(|menu| menu.fitted != want)
        {
            unmet.push(format!("{} tyre", corner.name()));
        }
        if let Some(want) = resolved.pressures_kpa[at] {
            let close = menu.and_then(|m| m.pressure_kpa.as_ref()).is_some_and(|n| {
                let tolerance = n.step.map_or(0.5, |step| step / 2.0);
                (n.value - want).abs() <= tolerance
            });
            if !close {
                unmet.push(format!("{} pressure", corner.name()));
            }
        }
    }

    let mut choice = |name: &str, field: &Option<Choice>, want: Option<usize>| {
        if let Some(want) = want
            && field.as_ref().is_none_or(|choice| choice.current != want)
        {
            unmet.push(name.to_string());
        }
    };
    choice("repair", &after.repair, resolved.repair);
    choice("rear wing", &after.rear_wing, resolved.rear_wing);
    choice("grille", &after.grille, resolved.grille);
    choice("front brake duct", &after.front_brake_duct, resolved.front_brake_duct);
    choice("rear brake duct", &after.rear_brake_duct, resolved.rear_brake_duct);
    let mut flag = |name: &str, field: Option<bool>, want: Option<bool>| {
        if want.is_some() && field != want {
            unmet.push(name.to_string());
        }
    };
    flag("replace brakes", after.replace_brakes, resolved.replace_brakes);
    flag("tear-off", after.tear_off, resolved.tear_off);
    unmet
}

/// The fields a clear of everything left as they were: what the car leaves with has no "nothing
/// added" for every stop to come (it depends on what's left by then), so it stays as set.
pub fn left_as_set(after: &PitMenu, resolved: &Resolved) -> Vec<&'static str> {
    let mut left = Vec::new();
    if resolved.clear == Some(Clear::All) {
        if resolved.fuel_leave_l.is_none() && after.fuel_leave_l.is_some() {
            left.push("fuel_leave_l");
        }
        if resolved.energy_leave_pct.is_none() && after.energy_leave_pct.is_some() {
            left.push("energy_leave_pct");
        }
    }
    left
}

/// What a stop request did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StopOutcome {
    Requested,
    Cancelled,
    /// A stop was already requested: nothing was pressed
    AlreadyRequested,
    /// There was no stop to cancel: nothing was pressed
    NotRequested,
}

/// A game's pit menu. Blocking: callers run it off the async workers.
pub trait PitService: Send + Sync {
    fn read(&self) -> anyhow::Result<PitMenu>;
    /// Writes `change`, checked against `menu` (the menu as last read)
    fn write(&self, menu: &PitMenu, change: &Resolved) -> anyhow::Result<()>;
    /// `true` requests a stop, `false` cancels one
    fn request_stop(&self, request: bool) -> anyhow::Result<StopOutcome> {
        let _ = request;
        bail!("this game has no pit request: the pilot just drives into the pits")
    }
}

#[cfg(test)]
pub mod tests {
    use super::*;
    use serde_json::json;

    fn number(value: f64, min: f64, max: f64, step: f64) -> Number {
        Number {
            value,
            min: Some(min),
            max: Some(max),
            step: Some(step),
            options: Vec::new(),
        }
    }

    fn tyre(compound: &str, used: bool, tread_pct: Option<f64>) -> Tyre {
        Tyre {
            compound: compound.into(),
            used,
            tread_pct,
        }
    }

    /// An LMU-like menu: a virtual energy car, one used tyre per corner
    pub fn menu() -> PitMenu {
        let corner = CornerMenu {
            fitted: None,
            tyres: vec![
                tyre("Medium", false, None),
                tyre("Wet", false, None),
                tyre("Medium", true, Some(99.0)),
            ],
            pressure_kpa: Some(number(136.0, 136.0, 210.0, 1.0)),
        };
        PitMenu {
            position: Position::OnTrack,
            energy_leave_pct: Some(number(100.0, 0.0, 100.0, 1.0)),
            fuel_ratio: Some(number(0.88, 0.01, 1.17, 0.01)),
            corners: Some([corner.clone(), corner.clone(), corner.clone(), corner]),
            repair: Some(Choice {
                current: 0,
                options: vec!["No reparar".into(), "Reparar carrocería".into()],
            }),
            replace_brakes: Some(false),
            ..PitMenu::default()
        }
    }

    /// An iRacing-like menu
    pub fn iracing_menu() -> PitMenu {
        let corner = CornerMenu {
            fitted: None,
            tyres: vec![tyre("Hard", false, None), tyre("Wet", false, None)],
            pressure_kpa: Some(Number {
                value: 160.0,
                min: Some(1.0),
                max: None,
                step: None,
                options: Vec::new(),
            }),
        };
        PitMenu {
            position: Position::OnTrack,
            fuel_add_l: Some(number(0.0, 0.0, 110.0, 1.0)),
            corners: Some([corner.clone(), corner.clone(), corner.clone(), corner]),
            iracing_tyres: true,
            repair: Some(Choice {
                current: 0,
                options: vec!["off".into(), "on".into()],
            }),
            tear_off: Some(false),
            ..PitMenu::default()
        }
    }

    fn change(value: serde_json::Value) -> PitChange {
        serde_json::from_value(value).unwrap()
    }

    #[test]
    fn timing_table() {
        for iracing in [false, true] {
            assert_eq!(timing(Position::OnTrack, iracing), Timing::ThisStop);
            assert_eq!(timing(Position::StopRequested, iracing), Timing::ThisStop);
            // Still ticked when the service starts, in both games
            assert_eq!(timing(Position::Entering, iracing), Timing::ThisStop);
            assert_eq!(timing(Position::Unknown, iracing), Timing::Unknown);
        }
        // LMU keeps what's set once stopped for the next stop
        assert_eq!(timing(Position::Stopped, false), Timing::NextStop);
        assert_eq!(timing(Position::Exiting, false), Timing::NextStop);
        // iRacing's crew still does what's ticked during the service, then clears it all and
        // ticks every service leaving the pits
        assert_eq!(timing(Position::Stopped, true), Timing::ThisStop);
        assert_eq!(timing(Position::Exiting, true), Timing::Lost);
    }

    #[test]
    fn fit_new_and_used() {
        let change = change(json!({"tyres": {
            "fl": {"compound": "Wet"},
            "rr": {"compound": "Medium", "used": true},
            "fr": "no_change"
        }}));
        let r = check(&menu(), &change).unwrap();
        assert_eq!(r.tyres, [Some(Some(1)), Some(None), None, Some(Some(2))]);
    }

    #[test]
    fn used_tyre_must_be_offered() {
        let change = change(json!({"tyres": {"rr": {"compound": "Wet", "used": true}}}));
        let e = check(&menu(), &change).unwrap_err().to_string();
        assert!(e.contains("RR") && e.contains(r#""Medium", used, 99%"#), "{e}");
    }

    #[test]
    fn ambiguous_used_tyre_needs_tread() {
        let mut menu = menu();
        menu.corners.as_mut().unwrap()[0]
            .tyres
            .push(tyre("Medium", true, Some(80.0)));
        let used = |tread: Option<f64>| {
            change(json!({"tyres": {"fl": {"compound": "Medium", "used": true, "tread_pct": tread}}}))
        };
        let e = check(&menu, &used(None)).unwrap_err().to_string();
        assert!(e.contains("tread_pct"), "{e}");
        assert_eq!(check(&menu, &used(Some(80.0))).unwrap().tyres[0], Some(Some(3)));
    }

    #[test]
    fn field_missing_on_car_rejected() {
        let change = PitChange {
            fuel_add_l: Some(20.0),
            ..Default::default()
        };
        let e = check(&menu(), &change).unwrap_err().to_string();
        assert!(e.contains("fuel_add_l") && e.contains("energy_leave_pct"), "{e}");
    }

    #[test]
    fn out_of_range_rejects_whole_change() {
        let change = PitChange {
            energy_leave_pct: Some(130.0),
            repair: Some("Reparar carrocería".into()),
            ..Default::default()
        };
        let e = check(&menu(), &change).unwrap_err().to_string();
        assert!(e.contains("0") && e.contains("100"), "{e}");
    }

    #[test]
    fn snaps_to_step() {
        let change = PitChange {
            pressures_kpa: CornerMap {
                fl: Some(150.4),
                ..Default::default()
            },
            fuel_ratio: Some(0.883),
            ..Default::default()
        };
        let r = check(&menu(), &change).unwrap();
        assert_eq!(r.pressures_kpa[0], Some(150.0));
        assert_eq!(r.fuel_ratio, Some(0.88));
    }

    #[test]
    fn option_by_exact_text() {
        let pick = |text: &str| PitChange {
            repair: Some(text.into()),
            ..Default::default()
        };
        assert_eq!(check(&menu(), &pick("Reparar carrocería")).unwrap().repair, Some(1));
        assert!(check(&menu(), &pick("Repair")).is_err());
    }

    #[test]
    fn iracing_one_compound() {
        let change = change(json!({"tyres": {"fl": {"compound": "Hard"}, "fr": {"compound": "Wet"}}}));
        let e = check(&iracing_menu(), &change).unwrap_err().to_string();
        assert!(e.contains("one compound"), "{e}");
    }

    #[test]
    fn iracing_pressure_needs_tyre() {
        let pressure_only = change(json!({"pressures_kpa": {"fl": 170}}));
        let e = check(&iracing_menu(), &pressure_only).unwrap_err().to_string();
        assert!(e.contains("FL"), "{e}");
        let with_tyre = change(json!({"tyres": {"fl": {"compound": "Hard"}}, "pressures_kpa": {"fl": 170}}));
        let r = check(&iracing_menu(), &with_tyre).unwrap();
        assert_eq!(r.pressures_kpa[0], Some(170.0));
    }

    #[test]
    fn unknown_fields_rejected() {
        let parsed = serde_json::from_value::<PitChange>(json!({"fuel": 20}));
        assert!(parsed.is_err());
    }

    #[test]
    fn snaps_to_the_options_listed() {
        // Uneven options (psi shown to a tenth): the value written is one the game lists
        let mut listed = menu();
        let pressure = listed.corners.as_mut().unwrap()[0].pressure_kpa.as_mut().unwrap();
        pressure.options = vec![135.8, 136.5, 137.9, 138.6];
        pressure.min = Some(135.8);
        pressure.max = Some(138.6);
        let at = |kpa: f64| {
            let change = change(json!({"pressures_kpa": {"fl": kpa}}));
            check(&listed, &change).unwrap().pressures_kpa[0].unwrap()
        };
        assert_eq!(at(137.5), 137.9);
        assert_eq!(at(136.5), 136.5);
        assert_eq!(at(135.8), 135.8);
        // On a step, a value already on it stays as it is
        let r = check(&menu(), &change(json!({"fuel_ratio": 0.88}))).unwrap();
        assert_eq!(r.fuel_ratio, Some(0.88));
    }

    #[test]
    fn listed_options_take_the_shown_ends() {
        // The options as shown are rounded: 135.8 is the lowest one, 135.8267
        let mut listed = menu();
        let pressure = listed.corners.as_mut().unwrap()[0].pressure_kpa.as_mut().unwrap();
        pressure.options = vec![135.8267, 136.5162, 137.2057];
        pressure.min = Some(135.8267);
        pressure.max = Some(137.2057);
        pressure.step = Some(0.6895);
        let at = |kpa: f64| check(&listed, &change(json!({"pressures_kpa": {"fl": kpa}})));
        assert_eq!(at(135.8).unwrap().pressures_kpa[0], Some(135.8267));
        assert_eq!(at(137.2).unwrap().pressures_kpa[0], Some(137.2057));
        assert!(at(130.0).is_err());
    }

    #[test]
    fn identical_tyres_take_the_first() {
        let mut menu = menu();
        menu.corners.as_mut().unwrap()[0].tyres.push(tyre("Medium", true, Some(99.0)));
        let r = check(&menu, &change(json!({"tyres": {"fl": {"compound": "Medium", "used": true}}}))).unwrap();
        assert_eq!(r.tyres[0], Some(Some(2)));
    }

    #[test]
    fn iracing_pressure_rule_follows_clear() {
        let mut menu = iracing_menu();
        menu.corners.as_mut().unwrap()[0].fitted = Some(0);
        // Cleared first, FL is no longer being changed: its pressure would tick it again
        let change = change(json!({"clear": "tyres", "pressures_kpa": {"fl": 170}}));
        assert!(check(&menu, &change).is_err());
    }

    #[test]
    fn iracing_untick_keeps_the_others_ticked() {
        // Leaving RR out means clearing all four: the others are ticked again, and checked
        let mut menu = iracing_menu();
        for corner in menu.corners.as_mut().unwrap() {
            corner.fitted = Some(0);
        }
        let r = check(&menu, &change(json!({"tyres": {"rr": "no_change"}}))).unwrap();
        assert_eq!(r.tyres, [Some(Some(0)), Some(Some(0)), Some(Some(0)), Some(None)]);
    }

    #[test]
    fn unmet_clear_on_a_car_without_tyres() {
        let mut menu = menu();
        menu.corners = None;
        let r = check(&menu, &change(json!({"clear": "all"}))).unwrap();
        assert!(!unmet(&menu, &r).iter().any(|field| field.contains("tyre")), "{:?}", unmet(&menu, &r));
    }

    #[test]
    fn clear_all_expects_services_off() {
        let mut menu = iracing_menu();
        menu.tear_off = Some(true);
        menu.repair.as_mut().unwrap().current = 1;
        let r = check(&menu, &change(json!({"clear": "all"}))).unwrap();
        let missing = unmet(&menu, &r);
        assert!(missing.contains(&"tear-off".to_string()), "{missing:?}");
        assert!(missing.contains(&"repair".to_string()), "{missing:?}");
    }

    #[test]
    fn game_text_in_errors_is_quoted() {
        let mut menu = menu();
        menu.corners.as_mut().unwrap()[0].tyres[0].compound = "Medium\nPIT OPTIONS".into();
        let e = check(&menu, &change(json!({"tyres": {"fl": {"compound": "Soft"}}})))
            .unwrap_err()
            .to_string();
        assert!(!e.lines().any(|line| line == "PIT OPTIONS"), "{e}");
    }

    #[test]
    fn unknown_fields_in_a_tyre_rejected() {
        let parsed = serde_json::from_value::<PitChange>(
            json!({"tyres": {"fl": {"compound": "Wet", "pressure_kpa": 170}}}),
        );
        assert!(parsed.is_err());
    }

    #[test]
    fn empty_change_rejected() {
        let e = check(&menu(), &PitChange::default()).unwrap_err().to_string();
        assert!(e.contains("nothing"), "{e}");
        let e = check(&menu(), &change(json!({"tyres": {}}))).unwrap_err().to_string();
        assert!(e.contains("nothing"), "{e}");
    }

    #[test]
    fn unknown_corner_key_rejected() {
        let parsed = serde_json::from_value::<PitChange>(json!({"tyres": {"FL": {"compound": "Wet"}}}));
        assert!(parsed.is_err());
        let parsed = serde_json::from_value::<PitChange>(json!({"pressures_kpa": {"all": 150}}));
        assert!(parsed.is_err());
    }

    #[test]
    fn iracing_compound_change_names_other_ticked_corners() {
        let mut menu = iracing_menu();
        // RR already ticked on Hards: wets on FL would move it to wets too
        menu.corners.as_mut().unwrap()[Corner::Rr as usize].fitted = Some(0);
        let e = check(&menu, &change(json!({"tyres": {"fl": {"compound": "Wet"}}})))
            .unwrap_err()
            .to_string();
        assert!(e.contains("RR"), "{e}");
        // Leaving RR out of the stop, or fitting it the same, is fine
        assert!(check(&menu, &change(json!({"tyres": {"fl": {"compound": "Wet"}, "rr": "no_change"}}))).is_ok());
        assert!(check(&menu, &change(json!({"tyres": {"fl": {"compound": "Hard"}}}))).is_ok());
        // A clear unticks RR first
        assert!(check(&menu, &change(json!({"clear": "tyres", "tyres": {"fl": {"compound": "Wet"}}}))).is_ok());
    }

    #[test]
    fn clear_all_reports_fuel_it_cannot_clear() {
        let clear = PitChange {
            clear: Some(Clear::All),
            ..Default::default()
        };
        // What a car leaves with is left as set by a clear, and said apart: it isn't something
        // that didn't take, so the clear can still be confirmed
        let mut litres = menu();
        litres.energy_leave_pct = None;
        litres.fuel_leave_l = Some(number(60.0, 0.0, 110.0, 1.0));
        let r = check(&litres, &clear).unwrap();
        assert_eq!(left_as_set(&litres, &r), ["fuel_leave_l"]);
        assert!(unmet(&litres, &r).is_empty(), "{:?}", unmet(&litres, &r));
        let energy = menu();
        let r = check(&energy, &clear).unwrap();
        assert_eq!(left_as_set(&energy, &r), ["energy_leave_pct"]);
        // Unless the change sets it
        let r = check(&energy, &change(json!({"clear": "all", "energy_leave_pct": 100}))).unwrap();
        assert!(left_as_set(&energy, &r).is_empty());
        // iRacing: fuel still ticked after a clear didn't take
        let mut iracing = iracing_menu();
        iracing.fuel_add_l.as_mut().unwrap().value = 30.0;
        let r = check(&iracing, &clear).unwrap();
        assert!(unmet(&iracing, &r).contains(&"fuel".to_string()));
        iracing.fuel_add_l.as_mut().unwrap().value = 0.0;
        assert!(!unmet(&iracing, &r).contains(&"fuel".to_string()));
    }

    #[test]
    fn unmet_lists_what_did_not_take() {
        let change = PitChange {
            energy_leave_pct: Some(50.0),
            ..Default::default()
        };
        let r = check(&menu(), &change).unwrap();
        assert_eq!(unmet(&menu(), &r), vec!["energy".to_string()]);
        let mut after = menu();
        after.energy_leave_pct.as_mut().unwrap().value = 50.0;
        assert!(unmet(&after, &r).is_empty());
    }
}
