//! What the session info says (see `yaml`), read into what the recording needs: the track, the
//! event's sessions, the cars and their drivers, and the player's car. Keys and their units are
//! the ones the session info writes, e.g. `TrackLength: 4.0569 km`.

use super::yaml::{Node, number};
use std::collections::HashMap;

#[derive(Debug, Default, Clone)]
pub struct SessionInfo {
    /// The track's id, e.g. `roadatlanta full` (`TrackName`), which track data knows it by
    pub track: Option<String>,
    /// Metres
    pub track_length: Option<f64>,
    /// What tells one event from another: the track, the session and the sub-session ids
    pub event: (Option<i64>, Option<i64>, Option<i64>),
    /// m/s
    pub pit_speed_limit: Option<f64>,
    pub incident_limit: Option<f64>,
    pub fixed_setup: Option<bool>,
    pub starters: Option<i64>,
    /// Seconds since midnight, when the event starts
    pub start_time_of_day: Option<f64>,
    /// A replay of a session, rather than one being driven
    pub replay: bool,
    /// The event's sessions, by `SessionNum`
    pub sessions: Vec<Session>,
    /// The player's car (`DriverCarIdx`)
    pub player: Option<i64>,
    /// Every car's entry, by car index
    pub drivers: HashMap<i64, Driver>,
    pub car: PlayerCar,
    /// Where each timing sector starts, as a fraction of the lap
    pub sectors: Vec<f64>,
    /// Each car's place in qualifying, from 1: the grid a race starts from
    pub qualifying: HashMap<i64, i64>,
    /// The player's garage setup; `None` without a `CarSetup` section (the user can turn it off)
    pub setup: Option<PlayerSetup>,
}

/// The player's garage setup, from `CarSetup`: each setting by its path through the section
/// (`Chassis/LeftFront/Camber`), with the game's display text (`-2.9 deg`). The names differ from
/// car to car. Settings changed from the cockpit aren't sent again, so those are as of when it was.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct PlayerSetup {
    /// `CarSetup:UpdateCount`, which goes up each time the game sends the setup
    pub update_count: Option<i64>,
    /// The setup file it was loaded from (`DriverSetupName`)
    pub file_name: Option<String>,
    /// Whether it was changed since it was loaded (`DriverSetupIsModified`)
    pub modified: Option<bool>,
    /// Path and trimmed text, each path once, in the order the game writes them. Readouts (see
    /// `READOUTS`) are left out
    pub settings: Vec<(String, String)>,
}

/// Entries of `CarSetup` that are readings after a run, not settings: they move after every run,
/// and the telemetry has them live. A key that is one of these, or starts with `LastTemps`.
const READOUTS: [&str; 4] = [
    "LastHotPressure",
    "TreadRemaining",
    "CornerWeight",
    "AeroCalculator",
];

impl PlayerSetup {
    fn new(doc: &Node) -> Option<Self> {
        let section = doc.get("CarSetup")?;
        let Node::Map(entries) = section else {
            return None;
        };
        let drivers = doc.get("DriverInfo");
        let mut setup = PlayerSetup {
            update_count: section.int("UpdateCount"),
            file_name: drivers
                .and_then(|d| d.text("DriverSetupName"))
                .map(str::to_owned),
            modified: drivers
                .and_then(|d| d.int("DriverSetupIsModified"))
                .map(|v| v != 0),
            settings: Vec::new(),
        };
        for (key, node) in entries.iter().filter(|(key, _)| key != "UpdateCount") {
            setup.add(key.clone(), key, node);
        }
        Some(setup)
    }

    /// Adds the settings at `path`, whose last key is `key`.
    fn add(&mut self, path: String, key: &str, node: &Node) {
        if READOUTS.contains(&key) || key.starts_with("LastTemps") {
            return;
        }
        match node {
            Node::Scalar(value) => {
                let value = value.trim();
                if !value.is_empty() && !self.settings.iter().any(|(name, _)| *name == path) {
                    self.settings.push((path, value.to_owned()));
                }
            }
            Node::Map(entries) => {
                for (key, node) in entries {
                    self.add(format!("{path}/{key}"), key, node);
                }
            }
            Node::List(_) => {}
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Session {
    pub num: i64,
    /// test, practice, qualifying, warmup or race (`session.session_type`)
    pub kind: Option<&'static str>,
    /// 1-based within its kind
    pub number: i64,
    /// Each car's result so far, by car index
    pub results: HashMap<i64, Result>,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct Result {
    pub incidents: Option<i64>,
    pub reason_out: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Driver {
    pub name: Option<String>,
    pub team: Option<String>,
    pub car_number: Option<String>,
    pub car: Option<String>,
    pub car_id: Option<i64>,
    pub class: Option<String>,
    pub class_id: Option<i64>,
    /// Seconds: the game's estimate of the class's lap
    pub class_lap_time: Option<f64>,
    pub ai: bool,
    pub pace_car: bool,
    pub spectator: bool,
    /// The team's and the current driver's incidents, when the game shares them (-1 when not)
    pub team_incidents: Option<i64>,
}

/// What the session info says about the player's car only.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct PlayerCar {
    /// Litres
    pub fuel_capacity: Option<f64>,
    pub red_line: Option<f64>,
    pub forward_gears: Option<i64>,
    /// The car's compounds, by the index the telemetry gives
    pub tires: HashMap<i64, String>,
}

impl SessionInfo {
    pub fn new(doc: &Node) -> Self {
        let weekend = doc.get("WeekendInfo");
        let options = weekend.and_then(|w| w.get("WeekendOptions"));
        let w = |key: &str| weekend.and_then(|w| w.text(key));
        let o = |key: &str| options.and_then(|o| o.text(key));

        let mut info = SessionInfo {
            track: w("TrackName").map(str::to_owned),
            track_length: weekend
                .and_then(|w| w.text("TrackLength"))
                .and_then(length)
                .filter(|length| *length > 0.0),
            event: (
                weekend.and_then(|w| w.int("TrackID")),
                weekend.and_then(|w| w.int("SessionID")),
                weekend.and_then(|w| w.int("SubSessionID")),
            ),
            pit_speed_limit: w("TrackPitSpeedLimit").and_then(speed),
            incident_limit: o("IncidentLimit").and_then(number),
            fixed_setup: options.and_then(|o| o.int("IsFixedSetup")).map(|v| v != 0),
            starters: options
                .and_then(|o| o.int("NumStarters"))
                .filter(|n| *n > 0),
            start_time_of_day: o("TimeOfDay").and_then(time_of_day),
            replay: w("SimMode") == Some("replay"),
            setup: PlayerSetup::new(doc),
            ..Default::default()
        };

        let mut seen = HashMap::<&'static str, i64>::new();
        for session in doc
            .at(&["SessionInfo", "Sessions"])
            .map_or(&[][..], Node::items)
        {
            let Some(num) = session.int("SessionNum") else {
                continue;
            };
            let kind = session.text("SessionType").and_then(session_kind);
            let number = kind.map_or(1, |kind| {
                let count = seen.entry(kind).or_default();
                *count += 1;
                *count
            });
            let results = session
                .get("ResultsPositions")
                .map_or(&[][..], Node::items)
                .iter()
                .filter_map(|result| {
                    Some((
                        result.int("CarIdx")?,
                        Result {
                            incidents: result.int("Incidents"),
                            reason_out: result.text("ReasonOutStr").map(str::to_owned),
                        },
                    ))
                })
                .collect();
            info.sessions.push(Session {
                num,
                kind,
                number,
                results,
            });
        }

        let drivers = doc.get("DriverInfo");
        info.player = drivers.and_then(|d| d.int("DriverCarIdx"));
        for driver in drivers
            .and_then(|d| d.get("Drivers"))
            .map_or(&[][..], Node::items)
        {
            let Some(idx) = driver.int("CarIdx") else {
                continue;
            };
            let text = |key: &str| driver.text(key).map(str::to_owned);
            let flag = |key: &str| driver.int(key).is_some_and(|v| v != 0);
            info.drivers.insert(
                idx,
                Driver {
                    name: text("UserName"),
                    team: text("TeamName"),
                    car_number: text("CarNumber"),
                    car: text("CarScreenName"),
                    car_id: driver.int("CarID"),
                    class: text("CarClassShortName"),
                    class_id: driver.int("CarClassID"),
                    class_lap_time: driver.number("CarClassEstLapTime").filter(|t| *t > 0.0),
                    ai: flag("CarIsAI"),
                    pace_car: flag("CarIsPaceCar"),
                    spectator: flag("IsSpectator"),
                    team_incidents: driver.int("TeamIncidentCount").filter(|n| *n >= 0),
                },
            );
        }
        let d = |key: &str| drivers.and_then(|d| d.number(key));
        info.car = PlayerCar {
            fuel_capacity: d("DriverCarFuelMaxLtr")
                .map(|max| max * d("DriverCarMaxFuelPct").unwrap_or(1.0))
                .filter(|l| *l > 0.0),
            red_line: d("DriverCarRedLine").filter(|rpm| *rpm > 0.0),
            forward_gears: drivers
                .and_then(|d| d.int("DriverCarGearNumForward"))
                .filter(|n| *n > 0),
            tires: drivers
                .and_then(|d| d.get("DriverTires"))
                .map_or(&[][..], Node::items)
                .iter()
                .filter_map(|tire| {
                    Some((
                        tire.int("TireIndex")?,
                        tire.text("TireCompoundType")?.to_owned(),
                    ))
                })
                .collect(),
        };

        let mut sectors: Vec<f64> = doc
            .at(&["SplitTimeInfo", "Sectors"])
            .map_or(&[][..], Node::items)
            .iter()
            .filter_map(|sector| sector.number("SectorStartPct"))
            .filter(|start| (0.0..1.0).contains(start))
            .collect();
        sectors.sort_by(f64::total_cmp);
        sectors.dedup();
        info.sectors = sectors;

        // A class without a name (AI ones have none) goes by its car's, when every car in it is the
        // same model
        let mut models = HashMap::<i64, Option<&str>>::new();
        for driver in info
            .drivers
            .values()
            .filter(|d| !d.pace_car && !d.spectator)
        {
            if let Some(id) = driver.class_id {
                let car = driver.car.as_deref();
                models
                    .entry(id)
                    .and_modify(|model| *model = model.filter(|m| Some(*m) == car))
                    .or_insert(car);
            }
        }
        let models: HashMap<i64, String> = models
            .into_iter()
            .filter_map(|(id, model)| Some((id, model?.to_owned())))
            .collect();
        for driver in info.drivers.values_mut() {
            if driver.class.is_none()
                && let Some(model) = driver.class_id.and_then(|id| models.get(&id))
            {
                driver.class = Some(model.clone());
            }
        }

        // Ranked by the positions given, so it doesn't matter where they count from
        let mut qualifying: Vec<(i64, i64)> = doc
            .at(&["QualifyResultsInfo", "Results"])
            .map_or(&[][..], Node::items)
            .iter()
            .filter_map(|result| Some((result.int("Position")?, result.int("CarIdx")?)))
            .collect();
        qualifying.sort();
        info.qualifying = qualifying
            .into_iter()
            .enumerate()
            .map(|(rank, (_, car))| (car, rank as i64 + 1))
            .collect();

        info
    }

    pub fn session(&self, num: i64) -> Option<&Session> {
        self.sessions.iter().find(|session| session.num == num)
    }

    /// The grid a race starts from, by qualifying: each car's place overall and in its class, from
    /// 1, among the cars there are (a qualifier who's left, or watches, leaves no gap).
    pub fn grid(&self) -> HashMap<i64, (i64, i64)> {
        let mut grid: Vec<_> = self
            .cars()
            .filter_map(|(car, driver)| Some((*self.qualifying.get(&car)?, car, driver.class_id)))
            .collect();
        grid.sort();
        let mut in_class = HashMap::<Option<i64>, i64>::new();
        grid.into_iter()
            .enumerate()
            .map(|(place, (_, car, class))| {
                let in_class = in_class.entry(class).or_default();
                *in_class += 1;
                (car, (place as i64 + 1, *in_class))
            })
            .collect()
    }

    /// The cars racing (not the pace car or spectators), by car index.
    pub fn cars(&self) -> impl Iterator<Item = (i64, &Driver)> {
        self.drivers
            .iter()
            .filter(|(_, driver)| !driver.pace_car && !driver.spectator)
            .map(|(idx, driver)| (*idx, driver))
    }
}

/// The recording's session type for iRacing's (`Offline Testing`, `Lone Qualify`, `Race`...).
fn session_kind(kind: &str) -> Option<&'static str> {
    let kind = kind.to_ascii_lowercase();
    Some(if kind.contains("test") {
        "test"
    } else if kind.contains("practice") {
        "practice"
    } else if kind.contains("qualif") || kind.contains("time trial") {
        "qualifying"
    } else if kind.contains("warmup") || kind.contains("warm up") {
        "warmup"
    } else if kind.contains("race") || kind.contains("heat") || kind.contains("feature") {
        "race"
    } else {
        return None;
    })
}

/// Metres, from `4.0569 km` (or `m`).
fn length(text: &str) -> Option<f64> {
    let value = number(text)?;
    match text.split_whitespace().nth(1) {
        Some("km") => Some(value * 1000.0),
        Some("m") | None => Some(value),
        Some("mi") => Some(value * 1609.344),
        _ => None,
    }
}

/// m/s, from `72.42 kph` (or `mph`, `m/s`).
fn speed(text: &str) -> Option<f64> {
    let value = number(text)?;
    let speed = match text.split_whitespace().nth(1) {
        Some("kph" | "km/h") => value / 3.6,
        Some("mph") => value * 0.44704,
        Some("m/s") | None => value,
        _ => return None,
    };
    (speed > 0.0).then_some(speed)
}

/// Seconds since midnight, from `3:05 pm` (or `15:05`).
fn time_of_day(text: &str) -> Option<f64> {
    let mut words = text.split_whitespace();
    let (hours, minutes) = words.next()?.split_once(':')?;
    let (mut hours, minutes): (u32, u32) = (hours.parse().ok()?, minutes.parse().ok()?);
    match words.next().map(str::to_ascii_lowercase).as_deref() {
        Some("am") if hours == 12 => hours = 0,
        Some("pm") if hours < 12 => hours += 12,
        Some("am" | "pm") | None => {}
        _ => return None,
    }
    (hours < 24 && minutes < 60).then(|| f64::from(hours * 3600 + minutes * 60))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sim::iracing::yaml::parse;

    #[test]
    fn reads_units_and_times_of_day() {
        assert!((length("4.0569 km").unwrap() - 4056.9).abs() < 1e-9);
        assert_eq!(length("812 m"), Some(812.0));
        assert!((speed("72.42 kph").unwrap() - 20.1167).abs() < 1e-3);
        assert_eq!(time_of_day("3:05 pm"), Some(54300.0));
        assert_eq!(time_of_day("12:30 am"), Some(1800.0));
        assert_eq!(time_of_day("9:00 am"), Some(32400.0));
        assert_eq!(session_kind("Lone Qualify"), Some("qualifying"));
        assert_eq!(session_kind("Offline Testing"), Some("test"));
        assert_eq!(session_kind("Open Practice"), Some("practice"));
        assert_eq!(session_kind("Race"), Some("race"));
    }

    #[test]
    fn reads_the_event_its_sessions_and_cars() {
        let doc = parse(
            "WeekendInfo:
 TrackName: roadatlanta full
 TrackID: 127
 TrackLength: 4.0569 km
 TrackPitSpeedLimit: 72.00 kph
 SessionID: 5
 SubSessionID: 6
 SimMode: full
 WeekendOptions:
  NumStarters: 20
  TimeOfDay: 3:05 pm
  IsFixedSetup: 1
  IncidentLimit: 17
SessionInfo:
 Sessions:
 - SessionNum: 0
   SessionType: Practice
   ResultsPositions:
   - Position: 1
     CarIdx: 3
     Incidents: 4
     ReasonOutStr: Running
 - SessionNum: 1
   SessionType: Practice
 - SessionNum: 2
   SessionType: Race
QualifyResultsInfo:
 Results:
 - Position: 1
   CarIdx: 3
 - Position: 0
   CarIdx: 7
DriverInfo:
 DriverCarIdx: 3
 DriverCarFuelMaxLtr: 100.000
 DriverCarMaxFuelPct: 0.500
 DriverCarRedLine: 7500.000
 DriverCarGearNumForward: 6
 DriverTires:
 - TireIndex: 0
   TireCompoundType: \"Hard\"
 Drivers:
 - CarIdx: 3
   UserName: Ana Silva
   CarNumber: \"07\"
   CarClassShortName: GR86
   CarClassEstLapTime: 91.2545
   CarIsPaceCar: 0
   IsSpectator: 0
   TeamIncidentCount: -1
 - CarIdx: 64
   UserName: Pace Car
   CarIsPaceCar: 1
SplitTimeInfo:
 Sectors:
 - SectorNum: 0
   SectorStartPct: 0.000000
 - SectorNum: 1
   SectorStartPct: 0.442307
",
        );
        let info = SessionInfo::new(&doc);
        assert_eq!(info.track.as_deref(), Some("roadatlanta full"));
        assert!((info.track_length.unwrap() - 4056.9).abs() < 1e-9);
        assert_eq!(info.event, (Some(127), Some(5), Some(6)));
        assert!((info.pit_speed_limit.unwrap() - 20.0).abs() < 1e-9);
        assert_eq!(info.incident_limit, Some(17.0));
        assert_eq!(info.starters, Some(20));
        assert_eq!(info.fixed_setup, Some(true));
        assert!(!info.replay);
        let kinds: Vec<_> = info.sessions.iter().map(|s| (s.kind, s.number)).collect();
        assert_eq!(
            kinds,
            [
                (Some("practice"), 1),
                (Some("practice"), 2),
                (Some("race"), 1)
            ]
        );
        assert_eq!(info.sessions[0].results[&3].incidents, Some(4));
        assert_eq!(info.qualifying[&7], 1);
        assert_eq!(info.qualifying[&3], 2);
        assert_eq!(info.player, Some(3));
        assert_eq!(info.car.fuel_capacity, Some(50.0));
        assert_eq!(info.car.tires[&0], "Hard");
        assert_eq!(info.cars().count(), 1);
        let driver = &info.drivers[&3];
        assert_eq!(driver.car_number.as_deref(), Some("07"));
        assert_eq!(driver.team_incidents, None);
        assert_eq!(info.sectors, [0.0, 0.442307]);
    }

    /// AI classes have no names: one goes by its car's, when it's the only model in it, and the
    /// grid's places count within each class too.
    #[test]
    fn nameless_classes_go_by_their_car() {
        let doc = parse(
            "QualifyResultsInfo:
 Results:
 - Position: 0
   CarIdx: 2
 - Position: 1
   CarIdx: 5
 - Position: 2
   CarIdx: 0
 - Position: 3
   CarIdx: 1
 - Position: 4
   CarIdx: 3
DriverInfo:
 Drivers:
 - CarIdx: 0
   CarScreenName: FIA F4
   CarClassID: 3188
   CarClassShortName: 
 - CarIdx: 1
   CarScreenName: FIA F4
   CarClassID: 3188
   CarClassShortName: 
 - CarIdx: 2
   CarScreenName: Cadillac V-Series.R
   CarClassID: 4024
   CarClassShortName: 
 - CarIdx: 3
   CarScreenName: Porsche 963
   CarClassID: 4024
   CarClassShortName: 
 - CarIdx: 4
   CarScreenName: Toyota GR86
   CarClassID: 77
   CarClassShortName: GR86
 - CarIdx: 64
   CarScreenName: safety pcsedan
   CarClassID: 3188
   CarIsPaceCar: 1
",
        );
        let info = SessionInfo::new(&doc);
        let class = |car: i64| info.drivers[&car].class.as_deref();
        assert_eq!(class(0), Some("FIA F4"));
        assert_eq!(class(1), Some("FIA F4"));
        // Two models: no name to go by
        assert_eq!(class(2), None);
        assert_eq!(class(3), None);
        assert_eq!(class(4), Some("GR86"));
        // The grid, overall and in the class, among the cars there are: the one who's left (5,
        // qualified second) leaves no gap
        assert_eq!([0, 1, 2, 3].map(|car| info.qualifying[&car]), [3, 4, 1, 5]);
        let grid = info.grid();
        assert_eq!(
            [0, 1, 2, 3].map(|car| grid[&car]),
            [(2, 1), (3, 2), (1, 1), (4, 2)]
        );
        assert!(!grid.contains_key(&5));
    }

    const CADILLAC: &str = include_str!("fixtures/cadillac_setup.yaml");
    const IR04: &str = include_str!("fixtures/ir04_setup.yaml");

    fn setup_of(text: &str) -> PlayerSetup {
        SessionInfo::new(&parse(text)).setup.unwrap()
    }

    fn names(setup: &PlayerSetup) -> Vec<&str> {
        setup
            .settings
            .iter()
            .map(|(name, _)| name.as_str())
            .collect()
    }

    #[test]
    fn the_car_setup_is_flattened_into_paths() {
        let setup = setup_of(&format!(
            "DriverInfo:\n DriverCarIdx: 0\n DriverSetupName: test.sto\n DriverSetupIsModified: 1\n{CADILLAC}"
        ));
        assert_eq!(setup.file_name.as_deref(), Some("test.sto"));
        assert_eq!(setup.modified, Some(true));
        assert_eq!(setup.update_count, Some(3));
        let value = |name: &str| {
            setup
                .settings
                .iter()
                .find(|(n, _)| n == name)
                .map(|(_, v)| v.as_str())
        };
        assert_eq!(value("Chassis/LeftFront/Camber"), Some("-2.9 deg"));
        assert_eq!(
            value("BrakesDriveUnit/BrakeSpec/BrakePressureBias"),
            Some("49.25%")
        );
        // Written with two spaces before it
        assert_eq!(value("Chassis/Front/ArbSize"), Some("Soft"));
        assert_eq!(value("Chassis/LeftFront/TorsionBarOD"), Some("14.76 mm"));
        // In the order the game writes them
        assert_eq!(names(&setup)[0], "TiresAero/TireType/TireType");
    }

    #[test]
    fn readouts_are_left_out() {
        for fixture in [CADILLAC, IR04] {
            for name in names(&setup_of(fixture)) {
                let last = name.rsplit('/').next().unwrap();
                assert!(
                    ![
                        "LastHotPressure",
                        "TreadRemaining",
                        "CornerWeight",
                        "UpdateCount"
                    ]
                    .contains(&last)
                        && !last.starts_with("LastTemps")
                        && !name.split('/').any(|segment| segment == "AeroCalculator"),
                    "{name}"
                );
            }
        }
    }

    #[test]
    fn both_fixtures_keep_their_own_names() {
        let (cadillac, ir04) = (setup_of(CADILLAC), setup_of(IR04));
        assert!(!ir04.settings.is_empty());
        assert_ne!(names(&cadillac), names(&ir04));
        assert!(names(&ir04).contains(&"TiresAero/LeftFront/ColdPressure"));
        assert!(names(&cadillac).contains(&"TiresAero/LeftFront/StartingPressure"));
        // Neither has a setup file name or modified flag without the driver info
        assert_eq!((ir04.file_name, ir04.modified), (None, None));
    }

    /// The recording keeps each name once: a key the game writes twice keeps its first value.
    #[test]
    fn a_path_written_twice_keeps_its_first_value() {
        let setup = setup_of("CarSetup:\n Chassis:\n  Front:\n   ToeIn: 1 mm\n   ToeIn: 2 mm\n");
        assert_eq!(
            setup.settings,
            [("Chassis/Front/ToeIn".to_owned(), "1 mm".to_owned())]
        );
    }

    #[test]
    fn no_car_setup_section_is_no_setup() {
        let doc =
            parse("WeekendInfo:\n TrackName: roadatlanta full\nDriverInfo:\n DriverCarIdx: 3\n");
        assert_eq!(SessionInfo::new(&doc).setup, None);
    }
}
