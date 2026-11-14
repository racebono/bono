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
    /// Each car's place in qualifying, from 1
    pub qualifying: HashMap<i64, i64>,
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
}
