//! What Steam knows about the games it installs.

use std::path::PathBuf;

/// The folder the app is installed in, looked for in every Steam library.
pub fn app_install_dir(app_id: u32) -> Option<PathBuf> {
    let steam = steamlocate::SteamDir::locate().ok()?;
    let (app, library) = steam.find_app(app_id).ok()??;
    Some(library.resolve_app_dir(&app))
}

/// Whether Steam reports the app's process alive: its `Running` value, which Steam keeps in the
/// registry.
#[cfg(windows)]
pub fn is_app_running(app_id: u32) -> bool {
    use winreg::{RegKey, enums::HKEY_CURRENT_USER};

    RegKey::predef(HKEY_CURRENT_USER)
        .open_subkey(format!(r"Software\Valve\Steam\Apps\{app_id}"))
        .and_then(|key| key.get_value::<u32, _>("Running"))
        .is_ok_and(|running| running != 0)
}

/// Not known outside Windows: assumed running, so callers go by what the game leaves on disk.
#[cfg(not(windows))]
pub fn is_app_running(_app_id: u32) -> bool {
    true
}
