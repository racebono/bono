//! The pilot's Pit Request key in LMU, which the engineer presses to request a stop: LMU has no
//! other way for a program to request one. Keyboard keys only reach LMU while its window is in
//! front, so a press brings it to the front and puts the window that was there back.
//!
//! The key comes from `UserData\player\keyboard.json`, which LMU writes (undocumented; its observed
//! shape is `{"Input": {"<control>": <DirectInput scan code>, …}, …}`, one key per control and no
//! entry for a control without one). Bono binds F11 there for a pilot without one, while LMU is
//! closed.

use crate::sim::{Game, PitKeyStatus};
use anyhow::{Context as _, bail};
use serde_json::Value;
use std::{
    path::PathBuf,
    time::{Duration, Instant},
};

/// F11's DirectInput scan code: LMU takes it (it ignores F13).
pub const F11: u16 = 0x57;
const PIT_REQUEST: &str = "Pit Request";
/// How long the key is held: a frame or two of the game's
const HOLD: Duration = Duration::from_millis(250);
/// How long a press waits for LMU's window to come to the front
const FOCUS_WAIT: Duration = Duration::from_millis(500);
const FOCUS_POLL: Duration = Duration::from_millis(10);
const EXECUTABLE: &str = "Le Mans Ultimate.exe";

fn input(json: &Value) -> anyhow::Result<&serde_json::Map<String, Value>> {
    let input = (json.get("Input").and_then(Value::as_object))
        .context("LMU's keyboard.json has no \"Input\" object")?;
    if input.values().any(|key| key.as_i64().is_none()) {
        bail!("LMU's keyboard.json has a key that isn't a scan code");
    }
    Ok(input)
}

/// Pit Request's scan code in keyboard.json's text: `None` when it has no key.
pub fn pit_request_key(keyboard_json: &str) -> anyhow::Result<Option<u16>> {
    let json: Value = serde_json::from_str(keyboard_json).context("LMU's keyboard.json isn't JSON")?;
    let key = input(&json)?.get(PIT_REQUEST).and_then(Value::as_i64);
    match key {
        // Unbound: only an absent control is (what 0 or less means to LMU isn't known)
        None => Ok(None),
        // A keyboard scan code (extended keys over 0x80)
        Some(key @ 1..=0xFF) => Ok(Some(key as u16)),
        // Bound to something that isn't a key there is to press: never taken as unbound, so it's
        // never bound over
        Some(key) => bail!("LMU's Pit Request is bound to {key}, which isn't a keyboard key"),
    }
}

/// The file with Pit Request bound to F11, every other key and value kept: `None` if another
/// control has F11.
pub fn bind_f11(keyboard_json: &str) -> anyhow::Result<Option<String>> {
    let mut json: Value =
        serde_json::from_str(keyboard_json).context("LMU's keyboard.json isn't JSON")?;
    if (input(&json)?.iter()).any(|(control, key)| control != PIT_REQUEST && key.as_i64() == Some(F11.into())) {
        return Ok(None);
    }
    let input = (json.get_mut("Input").and_then(Value::as_object_mut))
        .context("LMU's keyboard.json has no \"Input\" object")?;
    input.insert(PIT_REQUEST.into(), F11.into());
    Ok(Some(serde_json::to_string_pretty(&json)?))
}

fn keyboard_json(game: &Game) -> Option<PathBuf> {
    Some(game.install_dir()?.join("UserData").join("player").join("keyboard.json"))
}

/// Pit Request's key, read from the game's files. Blocks: it may look through Steam's libraries.
pub fn read_key(game: &Game) -> anyhow::Result<Option<u16>> {
    let path = keyboard_json(game).context("Le Mans Ultimate isn't installed")?;
    let text = std::fs::read_to_string(&path)
        .with_context(|| format!("couldn't read {}", path.display()))?;
    pit_request_key(&text)
}

/// Looks at Pit Request's key, and binds F11 to it if the pilot lets the engineer request stops,
/// it has none, and LMU is closed (it may write the file back as it had it when it exits). `None`
/// if the game isn't installed. Blocks: it reads and writes the game's files.
pub fn upkeep(game: &Game, requests_on: bool) -> Option<PitKeyStatus> {
    if !requests_on {
        return Some(PitKeyStatus::Off);
    }
    let path = keyboard_json(game)?;
    let status = (|| {
        let text = std::fs::read_to_string(&path)?;
        if let Some(key) = pit_request_key(&text)? {
            return Ok(PitKeyStatus::Key(key));
        }
        if game.is_running() {
            return Ok(PitKeyStatus::Unbound);
        }
        let Some(bound) = bind_f11(&text)? else {
            return Ok(PitKeyStatus::F11Taken);
        };
        // A temporary file moved over it: LMU never reads half a file of the pilot's bindings
        let mut temp = path.clone().into_os_string();
        temp.push(".tmp");
        // On disk before it replaces the pilot's bindings, not only after a crash
        let mut file = std::fs::File::create(&temp)?;
        std::io::Write::write_all(&mut file, bound.as_bytes())?;
        file.sync_all()?;
        drop(file);
        std::fs::rename(&temp, &path)?;
        log::info!("Bound F11 to LMU's Pit Request, for the engineer's stop requests");
        anyhow::Ok(PitKeyStatus::Key(F11))
    })();
    Some(status.unwrap_or_else(|e| {
        log::warn!("Error looking at LMU's Pit Request key in {}: {e:#}", path.display());
        PitKeyStatus::Unreadable
    }))
}

/// Brings LMU's window to the front, holds `scan` (a DirectInput scan code) for a moment, and
/// puts the window that was in front back. Blocks for about a quarter of a second.
pub fn press(scan: u16) -> anyhow::Result<()> {
    use windows_sys::Win32::UI::{
        Input::KeyboardAndMouse::{
            INPUT, INPUT_0, INPUT_KEYBOARD, KEYBDINPUT, KEYEVENTF_EXTENDEDKEY, KEYEVENTF_KEYUP,
            KEYEVENTF_SCANCODE, SendInput, VK_MENU,
        },
        WindowsAndMessaging::{GetForegroundWindow, SetForegroundWindow},
    };

    let key = |vk: u16, scan: u16, flags: u32| INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: vk,
                wScan: scan,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: 0,
            },
        },
    };
    let send = |inputs: &[INPUT]| {
        let sent = unsafe {
            SendInput(inputs.len() as u32, inputs.as_ptr(), size_of::<INPUT>() as i32)
        };
        match sent as usize == inputs.len() {
            true => Ok(()),
            false => Err(std::io::Error::last_os_error()),
        }
    };

    let window = lmu_window().context("couldn't find LMU's window")?;
    // The window to put back: none while Windows is mid-switch, so it's waited for
    let start = Instant::now();
    let previous = loop {
        let previous = unsafe { GetForegroundWindow() };
        if !previous.is_null() {
            break previous;
        }
        if start.elapsed() >= FOCUS_WAIT {
            bail!("no window is in front to come back to: try again in a moment");
        }
        std::thread::sleep(FOCUS_POLL);
    };
    /// Puts the window that was in front back, however the press ends (even when LMU only came
    /// to the front after the wait gave up)
    struct Restore(windows_sys::Win32::Foundation::HWND);
    impl Drop for Restore {
        fn drop(&mut self) {
            if !self.0.is_null() && unsafe { SetForegroundWindow(self.0) } == 0 {
                log::warn!("Couldn't put the window that was in front back after pressing LMU's Pit Request");
            }
        }
    }
    let _restore = (previous != window).then(|| Restore(previous));
    if previous != window && unsafe { SetForegroundWindow(window) } == 0 {
        // Windows lets a program that just had input bring a window to the front: an Alt tap
        send(&[key(VK_MENU, 0, 0), key(VK_MENU, 0, KEYEVENTF_KEYUP)])
            .context("couldn't bring LMU to the front")?;
        unsafe { SetForegroundWindow(window) };
    }
    // Windows switches the foreground on its own time: there's no foreground window meanwhile
    let start = Instant::now();
    while unsafe { GetForegroundWindow() } != window {
        if start.elapsed() >= FOCUS_WAIT {
            bail!("couldn't bring LMU's window to the front");
        }
        std::thread::sleep(FOCUS_POLL);
    }

    // DirectInput's codes over 0x80 are the extended keys' (0xE0-prefixed) codes
    let (code, extended) = match scan & 0x80 {
        0 => (scan, 0),
        _ => (scan & 0x7F, KEYEVENTF_EXTENDEDKEY),
    };
    let flags = KEYEVENTF_SCANCODE | extended;
    // The key goes to whichever window is in front: not one that took LMU's place meanwhile
    if unsafe { GetForegroundWindow() } != window {
        bail!("LMU's window lost the front before the key was pressed");
    }
    send(&[key(0, code, flags)]).context("couldn't press the Pit Request key")?;
    std::thread::sleep(HOLD);
    // Released before the window that was in front comes back (`_restore`, dropped after)
    send(&[key(0, code, flags | KEYEVENTF_KEYUP)]).context("couldn't release the Pit Request key")?;
    Ok(())
}

/// LMU's main window: a visible one without an owner, of a `Le Mans Ultimate.exe` process.
fn lmu_window() -> Option<windows_sys::Win32::Foundation::HWND> {
    use windows_sys::Win32::{
        Foundation::{HWND, LPARAM},
        UI::WindowsAndMessaging::{
            EnumWindows, GW_OWNER, GetWindow, GetWindowThreadProcessId, IsWindowVisible,
        },
    };

    struct Search {
        pids: Vec<u32>,
        found: Option<HWND>,
    }
    unsafe extern "system" fn each(window: HWND, search: LPARAM) -> i32 {
        // The `Search` `lmu_window` passes, alive for the whole of `EnumWindows`
        let search = unsafe { &mut *(search as *mut Search) };
        let mut pid = 0;
        unsafe { GetWindowThreadProcessId(window, &mut pid) };
        let main = unsafe { IsWindowVisible(window) != 0 && GetWindow(window, GW_OWNER).is_null() };
        if main && search.pids.contains(&pid) {
            search.found = Some(window);
            return 0;
        }
        1
    }

    let pids: Vec<u32> = (crate::process::win32::snapshot().ok()?.into_iter())
        .filter(|(_, name)| name.to_string_lossy().eq_ignore_ascii_case(EXECUTABLE))
        .map(|(pid, _)| pid.as_u32())
        .collect();
    if pids.is_empty() {
        return None;
    }
    let mut search = Search { pids, found: None };
    // Its answer is only whether it went through every window: `found` says what it found
    unsafe { EnumWindows(Some(each), &mut search as *mut Search as LPARAM) };
    search.found
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sim::key_name;

    #[test]
    fn reads_bound_key() {
        assert_eq!(pit_request_key(r#"{"Input":{"Pit Request":87}}"#).unwrap(), Some(0x57));
    }

    #[test]
    fn only_keyboard_scan_codes() {
        // Bound, but not to a key there is to press: never taken as unbound (and rebound)
        assert!(pit_request_key(r#"{"Input":{"Pit Request":300}}"#).is_err());
        // Only an absent control is unbound: what 0 means to LMU isn't known
        assert!(pit_request_key(r#"{"Input":{"Pit Request":0}}"#).is_err());
    }

    #[test]
    fn absent_is_unbound() {
        assert_eq!(pit_request_key(r#"{"Input":{"Brake":44}}"#).unwrap(), None);
    }

    #[test]
    fn wrong_shape_is_error() {
        assert!(pit_request_key(r#"{"Input":[1]}"#).is_err());
        assert!(pit_request_key(r#"{"Input":{"Brake":"x"}}"#).is_err());
        assert!(pit_request_key("not json").is_err());
        assert!(bind_f11(r#"{"Device":"k"}"#).is_err());
    }

    #[test]
    fn binds_f11_keeping_the_rest() {
        let out = bind_f11(r#"{"Device":"k","Input":{"Brake":44},"Type":1}"#)
            .unwrap()
            .unwrap();
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["Input"]["Pit Request"], 87);
        assert_eq!(v["Input"]["Brake"], 44);
        assert_eq!(v["Type"], 1);
        assert_eq!(v["Device"], "k");
    }

    #[test]
    fn f11_taken_binds_nothing() {
        assert_eq!(bind_f11(r#"{"Input":{"Pause":87}}"#).unwrap(), None);
    }

    #[test]
    fn the_pilots_own_file_reads() {
        let path = format!("{}/src/sim/lmu/fixtures/keyboard.json", env!("CARGO_MANIFEST_DIR"));
        let text = std::fs::read_to_string(path).unwrap();
        assert_eq!(pit_request_key(&text).unwrap(), Some(F11));
    }

    #[test]
    fn key_names() {
        assert_eq!(key_name(F11), "F11");
        assert_eq!(key_name(0x3B), "F1");
        assert_eq!(key_name(0x1E), "key 0x1E");
    }
}
