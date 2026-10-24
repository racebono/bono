//! The plugin (`lmu/`) in the game: the DLL goes in the game's `Plugins` folder, and the game
//! loads it if `UserData\player\CustomPluginVariables.JSON` enables it. The DLL carries its version
//! in a version resource (see `lmu/build.rs`), which is how the installed one is told apart from the
//! one that comes with the app, and its SHA-256 is how a copy with the same version but other code
//! (a local build, or a damaged or modified file) is told apart.

use super::write_replacing;
use crate::{
    sim::{PluginStatus, Version},
    task::{JoinHandle, spawn_blocking},
};
use anyhow::Context;
use futures::join;
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use tokio::try_join;

/// The DLL's name, in the app's `plugins` folder and the game's `Plugins` folder.
const DLL: &str = "bono_lmu.dll";
/// The game's settings for every plugin, by DLL name, in its install folder.
const PLUGIN_SETTINGS: &[&str] = &["UserData", "player", "CustomPluginVariables.JSON"];
/// The setting the game loads a plugin by (the space is the game's).
const ENABLED: &str = " Enabled";

/// The plugin as installed in `install_dir` (the game's folder, if it's installed), and the copy
/// that comes with the app.
pub async fn status(install_dir: Option<PathBuf>) -> PluginStatus {
    let bundled_dll = bundled_dll().await;
    let installed_dll = install_dir
        .as_deref()
        .map(installed_dll)
        .filter(|dll| dll.is_file());

    let installed = installed_dll.as_deref().map(version);
    let bundled = bundled_dll.as_deref().map(version);

    let enabled = async {
        if let Some(ref dir) = install_dir {
            return read_settings(&settings_path(dir))
                .await
                .ok()
                .and_then(|settings| settings.get(DLL).cloned())
                .and_then(|plugin| plugin.get(ENABLED).and_then(Value::as_i64))
                .is_some_and(|enabled| enabled != 0);
        }
        return false;
    };

    let same_as_bundled = async {
        match (installed_dll, bundled_dll) {
            (Some(installed), Some(bundled)) => match try_join!(digest(installed), digest(bundled))
            {
                Ok((installed, bundled)) => Some(installed == bundled),
                Err(e) => {
                    log::warn!("Couldn't hash the Le Mans Ultimate plugin: {e}");
                    None
                }
            },
            _ => None,
        }
    };

    let (enabled, same_as_bundled) = join!(enabled, same_as_bundled);
    PluginStatus {
        installed,
        enabled,
        bundled,
        same_as_bundled,
    }
}

/// Copies the app's DLL into the game's `Plugins` folder, replacing the one there, and enables it.
/// Fails while the game runs, since it keeps the DLL it loaded open.
pub async fn install(install_dir: &Path) -> anyhow::Result<()> {
    let Some(bundled) = bundled_dll().await else {
        anyhow::bail!("{DLL} isn't next to the app");
    };
    install_from(&bundled, install_dir).await
}

async fn install_from(bundled: &Path, install_dir: &Path) -> anyhow::Result<()> {
    // Validate existing settings before replacing the DLL or changing any files.
    let path = settings_path(install_dir);
    let mut settings = read_settings(&path).await?;
    let dll = installed_dll(install_dir);
    if let Some(plugins) = dll.parent() {
        tokio::fs::create_dir_all(plugins).await?;
    }
    write_replacing(&dll, &tokio::fs::read(bundled).await?).await?;

    // Keeps whatever else is in it: every other plugin's settings, and ours
    let plugin = settings
        .entry(DLL)
        .or_insert_with(|| Value::Object(Map::new()));
    if !plugin.is_object() {
        *plugin = Value::Object(Map::new());
    }
    plugin
        .as_object_mut()
        .unwrap()
        .insert(ENABLED.into(), 1.into());

    if let Some(player) = path.parent() {
        tokio::fs::create_dir_all(player).await?;
    }
    let mut json = serde_json::to_vec_pretty(&settings)?;
    json.push(b'\n');
    write_replacing(&path, &json).await?;
    Ok(())
}

fn installed_dll(install_dir: &Path) -> PathBuf {
    install_dir.join("Plugins").join(DLL)
}

fn settings_path(install_dir: &Path) -> PathBuf {
    PLUGIN_SETTINGS
        .iter()
        .fold(install_dir.to_owned(), |path, part| path.join(part))
}

/// Empty only if it's missing (the game makes it on its first run).
async fn read_settings(path: &Path) -> anyhow::Result<Map<String, Value>> {
    let bytes = match tokio::fs::read(path).await {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Map::new()),
        Err(error) => {
            return Err(error).with_context(|| format!("Could not read {}", path.display()));
        }
    };
    serde_json::from_slice(&bytes)
        .with_context(|| format!("Could not parse {} as a JSON object", path.display()))
}

/// The DLL that comes with the app: in `plugins` next to it once packaged (`cargo xtask dist`), or
/// right next to it when built with the workspace (`cargo build --workspace`).
async fn bundled_dll() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let dir = exe.parent()?;

    // In order: the packaged one first
    for dll in [dir.join("plugins").join(DLL), dir.join(DLL)] {
        if tokio::fs::metadata(&dll)
            .await
            .is_ok_and(|metadata| metadata.is_file())
        {
            return Some(dll);
        }
    }
    None
}

/// The SHA-256 of the DLL's contents.
fn digest(dll: PathBuf) -> JoinHandle<std::io::Result<[u8; 32]>> {
    spawn_blocking(move || {
        let mut hasher = Sha256::new();
        std::io::copy(&mut std::fs::File::open(dll)?, &mut hasher)?;
        Ok(hasher.finalize().into())
    })
}

/// The file version in the DLL's version resource, if it has one.
fn version(dll: &Path) -> Option<Version> {
    use std::{ffi::c_void, os::windows::ffi::OsStrExt, ptr};
    use windows_sys::Win32::Storage::FileSystem::{
        GetFileVersionInfoSizeW, GetFileVersionInfoW, VS_FIXEDFILEINFO, VerQueryValueW,
    };

    let wide = |s: &std::ffi::OsStr| s.encode_wide().chain([0]).collect::<Vec<u16>>();
    let path = wide(dll.as_os_str());

    let size = unsafe { GetFileVersionInfoSizeW(path.as_ptr(), ptr::null_mut()) };
    if size == 0 {
        return None;
    }
    let mut data = vec![0u8; size as usize];
    if unsafe { GetFileVersionInfoW(path.as_ptr(), 0, size, data.as_mut_ptr().cast()) } == 0 {
        return None;
    }

    // "\" is the fixed part, which has the numeric version
    let root = wide("\\".as_ref());
    let mut info = ptr::null_mut::<c_void>();
    let mut len = 0u32;
    let found = unsafe { VerQueryValueW(data.as_ptr().cast(), root.as_ptr(), &mut info, &mut len) };
    if found == 0 || info.is_null() || (len as usize) < size_of::<VS_FIXEDFILEINFO>() {
        return None;
    }

    // Points into `data`, which outlives it
    let info = unsafe { &*info.cast::<VS_FIXEDFILEINFO>() };

    // The build is only in the version string ("0.1.0+abc1234"), in the language `lmu/build.rs`
    // writes it in. Older DLLs and release builds have none
    let name = wide("\\StringFileInfo\\040904B0\\ProductVersion".as_ref());
    let mut text = ptr::null_mut::<c_void>();
    let mut chars = 0u32;
    let found =
        unsafe { VerQueryValueW(data.as_ptr().cast(), name.as_ptr(), &mut text, &mut chars) };
    let build = (found != 0 && !text.is_null())
        .then(|| {
            // `chars` counts the terminating NUL
            let text = unsafe { std::slice::from_raw_parts(text.cast::<u16>(), chars as usize) };
            let text = String::from_utf16_lossy(text);
            let (_, build) = text.trim_end_matches('\0').split_once('+')?;
            Some(build.to_owned())
        })
        .flatten();

    Some(Version {
        major: (info.dwFileVersionMS >> 16) as u16,
        minor: info.dwFileVersionMS as u16,
        patch: (info.dwFileVersionLS >> 16) as u16,
        build,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[tokio::test]
    async fn installs_and_enables_keeping_other_plugins_settings() {
        let game = tempfile::tempdir().unwrap();
        let settings = settings_path(game.path());
        std::fs::create_dir_all(settings.parent().unwrap()).unwrap();
        std::fs::write(
            &settings,
            json!({
                "Other.dll": { " Enabled": 1, "Mode": 3 },
                DLL: { " Enabled": 0, "Kept": 2 },
            })
            .to_string(),
        )
        .unwrap();

        let before = status(Some(game.path().to_owned())).await;
        assert!(before.installed.is_none());
        assert!(!before.enabled);

        // A DLL without a version resource
        let bundled = game.path().join("bundled.dll");
        std::fs::write(&bundled, b"not really a dll").unwrap();
        install_from(&bundled, game.path()).await.unwrap();

        assert_eq!(
            std::fs::read(installed_dll(game.path())).unwrap(),
            b"not really a dll"
        );
        assert_eq!(
            Value::Object(read_settings(&settings).await.unwrap()),
            json!({
                "Other.dll": { " Enabled": 1, "Mode": 3 },
                DLL: { " Enabled": 1, "Kept": 2 },
            })
        );

        let after = status(Some(game.path().to_owned())).await;
        assert_eq!(after.installed, Some(None));
        assert!(after.enabled);
        // No bundled copy next to the test binary to compare it with
        assert_eq!(after.same_as_bundled, None);
    }

    #[tokio::test]
    async fn digest_tells_copies_apart_by_contents() {
        let dir = tempfile::tempdir().unwrap();
        let [a, b, c] = ["a.dll", "b.dll", "c.dll"].map(|name| dir.path().join(name));
        tokio::fs::write(&a, b"plugin").await.unwrap();
        tokio::fs::write(&b, b"plugin").await.unwrap();
        tokio::fs::write(&c, b"plugin, modified").await.unwrap();

        assert_eq!(digest(a.clone()).await.unwrap(), digest(b).await.unwrap());
        assert_ne!(digest(a).await.unwrap(), digest(c).await.unwrap());
        assert!(digest(dir.path().join("missing.dll")).await.is_err());
    }

    #[tokio::test]
    async fn installs_into_a_game_that_never_ran() {
        let game = tempfile::tempdir().unwrap();
        let bundled = game.path().join("bundled.dll");
        std::fs::write(&bundled, b"dll").unwrap();
        install_from(&bundled, game.path()).await.unwrap();

        assert_eq!(
            Value::Object(read_settings(&settings_path(game.path())).await.unwrap()),
            json!({ DLL: { " Enabled": 1 } })
        );
    }

    #[tokio::test]
    async fn rejects_invalid_settings_without_changing_files() {
        for contents in [
            "\u{feff}{\"Other.dll\":{\" Enabled\":1}}",
            "{ /* comment */ \"Other.dll\": {} }",
            "{\"Other.dll\":",
            "",
            "[]",
            "null",
        ] {
            let game = tempfile::tempdir().unwrap();
            let settings = settings_path(game.path());
            std::fs::create_dir_all(settings.parent().unwrap()).unwrap();
            std::fs::write(&settings, contents).unwrap();
            let dll = installed_dll(game.path());
            std::fs::create_dir_all(dll.parent().unwrap()).unwrap();
            std::fs::write(&dll, b"original dll").unwrap();
            let bundled = game.path().join("bundled.dll");
            std::fs::write(&bundled, b"new dll").unwrap();

            let error = install_from(&bundled, game.path()).await.unwrap_err();
            assert!(error.to_string().contains("CustomPluginVariables.JSON"));
            assert_eq!(std::fs::read(&settings).unwrap(), contents.as_bytes());
            assert_eq!(std::fs::read(&dll).unwrap(), b"original dll");
            assert!(!status(Some(game.path().to_owned())).await.enabled);
        }
    }

    #[tokio::test]
    async fn rejects_settings_read_errors_before_installing() {
        let game = tempfile::tempdir().unwrap();
        let settings = settings_path(game.path());
        // A directory at the file path reliably causes a read error on Windows.
        std::fs::create_dir_all(&settings).unwrap();
        let bundled = game.path().join("bundled.dll");
        std::fs::write(&bundled, b"dll").unwrap();

        assert!(install_from(&bundled, game.path()).await.is_err());
        assert!(settings.is_dir());
        assert!(!installed_dll(game.path()).exists());
    }
}
