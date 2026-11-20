//! iRacing's pit menu: read from its shared memory, changed with the SDK's `PitCommand` broadcast
//! (`irsdk_broadcastMsg`), which works while the pilot is in the car. iRacing never answers a
//! broadcast: what it did is read back from the telemetry.

use super::{read_once, sdk::Frame, session::SessionInfo, yaml};
use crate::sim::pit::{
    Choice, Clear, Corner, CornerMenu, Number, PitMenu, PitService, Position, Resolved, Tyre,
};
use anyhow::{Context as _, bail};
use std::collections::HashMap;

// `irsdk_PitSvFlags`
const LF_TIRE: u32 = 0x01;
const RF_TIRE: u32 = 0x02;
const LR_TIRE: u32 = 0x04;
const RR_TIRE: u32 = 0x08;
const FUEL_FILL: u32 = 0x10;
const WINDSHIELD_TEAROFF: u32 = 0x20;
const FAST_REPAIR: u32 = 0x40;
/// Indexed by `Corner as usize`
const TIRE_FLAGS: [u32; 4] = [LF_TIRE, RF_TIRE, LR_TIRE, RR_TIRE];
/// Each corner's requested pressure, kPa, by `Corner as usize`
const PRESSURES: [&str; 4] = ["PitSvLFP", "PitSvRFP", "PitSvLRP", "PitSvRRP"];

// `irsdk_PitCommandMode`
const CLEAR_ALL: u16 = 0;
const TEAROFF: u16 = 1;
const FUEL: u16 = 2;
/// LF; RF, LR and RR follow it, in `Corner` order
const LF: u16 = 3;
const CLEAR_TIRES: u16 = 7;
const FAST_REPAIR_ON: u16 = 8;
const CLEAR_TEAROFF: u16 = 9;
const CLEAR_FAST_REPAIR: u16 = 10;
const CLEAR_FUEL: u16 = 11;
const COMPOUND: u16 = 12;

/// `irsdk_BroadcastMsg`'s `irsdk_BroadcastPitCommand`
const BROADCAST_PIT_COMMAND: u16 = 9;
// `irsdk_PitSvStatus`
const PIT_SV_IN_PROGRESS: i32 = 1;
const PIT_SV_COMPLETE: i32 = 2;

/// iRacing's pit menu, for the engineer. Blocking: the `pit` tool calls it off the async workers.
pub struct IracingPit;

impl PitService for IracingPit {
    fn read(&self) -> anyhow::Result<PitMenu> {
        let (vars, buffer, info) =
            read_once().context("iRacing isn't running, or isn't in a session")?;
        let frame = Frame::new(&vars, &buffer);
        if frame.bool("IsOnTrack") != Some(true) {
            bail!("the pilot isn't in the car: iRacing only takes pit commands from the car");
        }
        let info = SessionInfo::new(&yaml::read(&info));
        Ok(parse(&frame, &info.car.tires, info.car.fuel_capacity))
    }

    fn write(&self, menu: &PitMenu, change: &Resolved) -> anyhow::Result<()> {
        for (mode, value) in commands(menu, change) {
            broadcast(mode, value)?;
        }
        Ok(())
    }
}

/// The compounds iRacing can fit, in the order of their index (`DriverTires`' `TireIndex`), which
/// the `PitCommand` for a compound takes: from 0, up to the first index the car doesn't list.
fn compounds(tires: &HashMap<i64, String>) -> Vec<&str> {
    (0..)
        .map_while(|index| tires.get(&index).map(String::as_str))
        .collect()
}

/// The menu from one frame and the session info's compounds and fuel capacity.
fn parse(frame: &Frame, tires: &HashMap<i64, String>, fuel_capacity: Option<f64>) -> PitMenu {
    let flags = frame.bits("PitSvFlags");
    let compounds = compounds(tires);
    let compound = (frame.i32("PitSvTireCompound"))
        .and_then(|index| usize::try_from(index).ok())
        .filter(|index| *index < compounds.len());
    let corners = (flags.is_some() && !compounds.is_empty()).then(|| {
        Corner::ALL.map(|corner| CornerMenu {
            fitted: flags
                .filter(|flags| flags & TIRE_FLAGS[corner as usize] != 0)
                .and(compound),
            tyres: (compounds.iter())
                .map(|compound| Tyre {
                    compound: (*compound).to_owned(),
                    used: false,
                    tread_pct: None,
                })
                .collect(),
            // Limits are iRacing's to apply: the read-back says what it set
            // A pressure of 0 means "the one set" to the command; iRacing limits the rest, and
            // the read-back says what it set
            pressure_kpa: frame.f64(PRESSURES[corner as usize]).map(|value| Number {
                value,
                min: Some(1.0),
                max: None,
                step: None,
                options: Vec::new(),
            }),
        })
    });
    PitMenu {
        position: position(frame),
        // The amount stays in the box with refuel unticked: nothing is added then
        fuel_add_l: frame.f64("PitSvFuel").map(|amount| Number {
            value: match flags {
                Some(flags) if flags & FUEL_FILL == 0 => 0.0,
                _ => amount,
            },
            min: Some(0.0),
            // Whole litres: a fractional tank's top is the whole litre under it
            max: fuel_capacity.map(f64::floor),
            // The command takes whole litres
            step: Some(1.0),
            options: Vec::new(),
        }),
        corners,
        iracing_tyres: true,
        repair: flags.map(|flags| Choice {
            current: usize::from(flags & FAST_REPAIR != 0),
            options: vec!["off".into(), "on".into()],
        }),
        tear_off: flags.map(|flags| flags & WINDSHIELD_TEAROFF != 0),
        ..PitMenu::default()
    }
}

/// Where the car is: in the pit lane, its service tells stopping from leaving. A car whose
/// service is complete counts as leaving, even still in its box: iRacing has cleared what was
/// ticked, and ticks every service again as it drives off.
fn position(frame: &Frame) -> Position {
    if frame.bool("OnPitRoad") != Some(true) {
        return Position::OnTrack;
    }
    match frame.i32("PlayerCarPitSvStatus") {
        Some(PIT_SV_IN_PROGRESS) => Position::Stopped,
        Some(PIT_SV_COMPLETE) => Position::Exiting,
        // In the box with no service running: at the session's start, the car leaves it with
        // every service ticked again; arriving for a stop, the service is about to start
        _ if frame.bool("PlayerCarInPitStall") == Some(true) => Position::Unknown,
        _ => Position::Entering,
    }
}

/// The `PitCommand` broadcasts for a change, `(mode, value)`, in the order they're sent.
fn commands(menu: &PitMenu, change: &Resolved) -> Vec<(u16, u32)> {
    let mut out = Vec::new();
    match change.clear {
        Some(Clear::All) => out.push((CLEAR_ALL, 0)),
        Some(Clear::Tyres) => out.push((CLEAR_TIRES, 0)),
        None => {}
    }

    let fitted_now = |at: usize| {
        change.clear.is_none()
            && (menu.corners.as_ref()).is_some_and(|corners| corners[at].fitted.is_some())
    };
    let pressure = |at: usize| change.pressures_kpa[at].map_or(0, |kpa| kpa.round() as u32);
    let mode = |at: usize| LF + at as u16;
    // A tyre can only be left out by clearing them all and ticking the others again
    let untick = (0..4).any(|at| change.tyres[at] == Some(None) && fitted_now(at));
    if untick {
        out.push((CLEAR_TIRES, 0));
    }
    // One compound for the stop, after any clear (which may reset it) and before the tyres
    if let Some(compound) = change.tyres.iter().find_map(|tyre| tyre.flatten()) {
        out.push((COMPOUND, compound as u32));
    }
    for at in 0..4 {
        let fit = match change.tyres[at] {
            Some(fit) => fit.is_some(),
            None if untick => fitted_now(at),
            // A pressure ticks its tyre too (checked: only for one being changed)
            None => change.pressures_kpa[at].is_some(),
        };
        if fit {
            // 0 keeps the pressure set
            out.push((mode(at), pressure(at)));
        }
    }

    // The fuel command's 0 is "the amount set": no fuel unticks refuel instead
    match change.fuel_add_l.map(|litres| litres.max(0.0) as u32) {
        Some(0) => out.push((CLEAR_FUEL, 0)),
        Some(litres) => out.push((FUEL, litres)),
        None => {}
    }
    match change.tear_off {
        Some(true) => out.push((TEAROFF, 0)),
        Some(false) => out.push((CLEAR_TEAROFF, 0)),
        None => {}
    }
    match change.repair {
        Some(0) => out.push((CLEAR_FAST_REPAIR, 0)),
        Some(_) => out.push((FAST_REPAIR_ON, 0)),
        None => {}
    }
    out
}

/// Sends one `PitCommand` to iRacing, as `irsdk_broadcastMsg` does.
fn broadcast(mode: u16, value: u32) -> anyhow::Result<()> {
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        HWND_BROADCAST, RegisterWindowMessageW, SendNotifyMessageW,
    };
    let name: Vec<u16> = "IRSDK_BROADCASTMSG\0".encode_utf16().collect();
    let message = unsafe { RegisterWindowMessageW(name.as_ptr()) };
    if message == 0 {
        bail!(
            "couldn't register iRacing's broadcast message: {}",
            std::io::Error::last_os_error()
        );
    }
    // MAKELONG(BROADCAST_PIT_COMMAND, mode)
    let wparam = (u32::from(BROADCAST_PIT_COMMAND) | (u32::from(mode) << 16)) as usize;
    let sent = unsafe { SendNotifyMessageW(HWND_BROADCAST, message, wparam, value as isize) };
    if sent == 0 {
        bail!(
            "couldn't send iRacing a pit command: {}",
            std::io::Error::last_os_error()
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sim::iracing::sdk::{Value, Vars};

    fn tires() -> HashMap<i64, String> {
        HashMap::from([(0, "Hard".to_string()), (1, "Wet".to_string())])
    }

    fn frame_menu(flags: i32, compound: i32, extra: &[(&str, Value)]) -> PitMenu {
        let mut values = vec![
            ("PitSvFlags", Value::Int(flags)),
            ("PitSvTireCompound", Value::Int(compound)),
            ("PitSvFuel", Value::Double(12.0)),
            ("PitSvLFP", Value::Double(165.0)),
            ("PitSvRFP", Value::Double(165.0)),
            ("PitSvLRP", Value::Double(160.0)),
            ("PitSvRRP", Value::Double(160.0)),
        ];
        values.extend(extra.iter().cloned());
        let (vars, data) = Vars::of(&values);
        parse(&Frame::new(&vars, &data), &tires(), Some(110.0))
    }

    #[test]
    fn parses_flags_and_compound() {
        let m = frame_menu((LF_TIRE | 0x10) as i32, 1, &[]);
        let corners = m.corners.as_ref().unwrap();
        assert_eq!(corners[0].fitted, Some(1));
        assert_eq!(corners[1].fitted, None);
        assert_eq!(corners[0].tyres[1].compound, "Wet");
        assert_eq!(corners[3].pressure_kpa.as_ref().unwrap().value, 160.0);
        let fuel = m.fuel_add_l.unwrap();
        assert_eq!((fuel.value, fuel.max), (12.0, Some(110.0)));
        assert_eq!(m.tear_off, Some(false));
        assert_eq!(m.repair.unwrap().current, 0);
        assert!(m.iracing_tyres);
        assert_eq!(m.position, Position::OnTrack);
    }

    #[test]
    fn position_in_the_pit_lane() {
        let at = |status: i32, stall: bool| {
            frame_menu(0, 0, &[
                ("OnPitRoad", Value::Bool(true)),
                ("PlayerCarPitSvStatus", Value::Int(status)),
                ("PlayerCarInPitStall", Value::Bool(stall)),
            ])
            .position
        };
        assert_eq!(at(0, false), Position::Entering);
        // In the box with no service (the session's start): leaving it ticks everything again
        assert_eq!(at(0, true), Position::Unknown);
        assert_eq!(at(PIT_SV_IN_PROGRESS, true), Position::Stopped);
        assert_eq!(at(PIT_SV_COMPLETE, false), Position::Exiting);
        // Serviced, still in the box: what's set now is cleared, as when it drives off
        assert_eq!(at(PIT_SV_COMPLETE, true), Position::Exiting);
    }

    #[test]
    fn fuel_and_pressure_limits() {
        let (vars, data) = Vars::of(&[
            ("PitSvFlags", Value::Int(0)),
            ("PitSvFuel", Value::Double(0.0)),
            ("PitSvLFP", Value::Double(165.0)),
        ]);
        let m = parse(&Frame::new(&vars, &data), &tires(), Some(101.92));
        // Whole litres: a fractional tank's top is the whole litre under it
        assert_eq!(m.fuel_add_l.unwrap().max, Some(101.0));
        assert_eq!(m.corners.unwrap()[0].pressure_kpa.as_ref().unwrap().min, Some(1.0));
    }

    #[test]
    fn compound_follows_the_untick() {
        // A clear of the tyres may reset the compound: it's sent after it
        let m = frame_menu((LF_TIRE | RF_TIRE | LR_TIRE | RR_TIRE) as i32, 0, &[]);
        let mut r = Resolved::default();
        r.tyres = [Some(Some(1)), Some(Some(0)), Some(Some(0)), Some(None)];
        let sent = commands(&m, &r);
        let clear = sent.iter().position(|c| *c == (7, 0)).unwrap();
        let compound = sent.iter().position(|c| c.0 == 12).unwrap();
        assert!(clear < compound, "{sent:?}");
    }

    #[test]
    fn leaving_one_tyre_out_clears_then_reticks() {
        let m = frame_menu((LF_TIRE | RF_TIRE | LR_TIRE | RR_TIRE) as i32, 0, &[]);
        let mut r = Resolved::default();
        r.tyres[Corner::Rr as usize] = Some(None);
        assert_eq!(commands(&m, &r), vec![(7, 0), (3, 0), (4, 0), (5, 0)]);
    }

    #[test]
    fn fit_with_compound_and_pressure() {
        let m = frame_menu(0, 0, &[]);
        let mut r = Resolved::default();
        r.tyres[0] = Some(Some(1));
        r.pressures_kpa[0] = Some(170.0);
        assert_eq!(commands(&m, &r), vec![(12, 1), (3, 170)]);
    }

    #[test]
    fn fuel_is_whole_litres() {
        let r = Resolved {
            fuel_add_l: Some(31.7),
            ..Default::default()
        };
        assert_eq!(commands(&frame_menu(0, 0, &[]), &r), vec![(2, 31)]);
    }

    #[test]
    fn clear_all_is_mode_zero_first() {
        let r = Resolved {
            clear: Some(Clear::All),
            fuel_add_l: Some(20.0),
            ..Default::default()
        };
        assert_eq!(commands(&frame_menu(0, 0, &[]), &r), vec![(0, 0), (2, 20)]);
    }

    #[test]
    fn unticked_refuel_adds_nothing() {
        // 12 L in the box, refuel unticked
        let m = frame_menu(0, 0, &[]);
        assert_eq!(m.fuel_add_l.unwrap().value, 0.0);
        let m = frame_menu(0x10, 0, &[]);
        assert_eq!(m.fuel_add_l.unwrap().value, 12.0);
    }

    #[test]
    fn no_fuel_clears_refuel() {
        // 0 L is "the amount set" to the SDK's fuel command: no fuel unticks refuel instead
        let r = Resolved {
            fuel_add_l: Some(0.0),
            ..Default::default()
        };
        assert_eq!(commands(&frame_menu(0x10, 0, &[]), &r), vec![(11, 0)]);
    }

    #[test]
    fn tear_off_and_repair() {
        let r = Resolved {
            tear_off: Some(true),
            repair: Some(0),
            ..Default::default()
        };
        assert_eq!(commands(&frame_menu(0, 0, &[]), &r), vec![(1, 0), (10, 0)]);
    }

    #[test]
    fn compounds_stop_at_a_gap() {
        let tires = HashMap::from([(0, "Hard".to_string()), (2, "Wet".to_string())]);
        assert_eq!(compounds(&tires), ["Hard"]);
    }
}
