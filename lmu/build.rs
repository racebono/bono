//! Compiles `lmu.cpp` into the plugin DLL, against the plugin SDK headers that come with the game
//! (`Support/SharedMemoryInterface` in its install folder). Their license only allows using them to
//! develop plugins, so they're found in the install rather than kept in this repository: set
//! `LMU_DIR` to the install folder, or it's looked for in every Steam library.

use std::{
    env,
    path::{Path, PathBuf},
    process::Command,
};

/// Le Mans Ultimate's Steam app id.
const APP_ID: u32 = 2399420;

fn main() {
    println!("cargo::rerun-if-changed=lmu.cpp");
    println!("cargo::rerun-if-changed=recorder.h");
    println!("cargo::rerun-if-changed=../include");
    println!("cargo::rerun-if-env-changed=LMU_DIR");

    // LMU only runs on Windows: elsewhere the crate is empty
    if env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }

    let lmu_dir = env::var_os("LMU_DIR")
        .map(PathBuf::from)
        .or_else(find_lmu_steam_folder)
        .expect(
            "Le Mans Ultimate not found in any Steam library: set LMU_DIR to its install folder",
        );

    let sdk = lmu_dir.join("Support/SharedMemoryInterface");
    assert!(
        sdk.join("InternalsPlugin.hpp").is_file(),
        "No plugin SDK in {}: set LMU_DIR to Le Mans Ultimate's install folder",
        sdk.display()
    );

    let manifest_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
    cc::Build::new()
        .cpp(true)
        .std("c++20")
        .file(manifest_dir.join("lmu.cpp"))
        .include(manifest_dir)
        .include(manifest_dir.join("../include"))
        .include(&sdk)
        .flag_if_supported("/EHsc")
        .flag_if_supported("/utf-8")
        // Nothing in Rust refers to lmu.cpp: without this, the linker would leave it out, and with
        // it the functions LMU looks the plugin up by
        .link_lib_modifier("+whole-archive")
        .compile("lambiase_lmu_plugin");

    version_resource();
}

/// The DLL's version (this crate's), as a Windows version resource: the app reads it from the copy
/// in the game's Plugins folder to tell which one is installed, and Explorer shows it too.
fn version_resource() {
    let version = env::var("CARGO_PKG_VERSION").unwrap();
    let number = |name: &str| env::var(name).unwrap().parse::<u16>().unwrap_or(0);
    let (major, minor, patch) = (
        number("CARGO_PKG_VERSION_MAJOR"),
        number("CARGO_PKG_VERSION_MINOR"),
        number("CARGO_PKG_VERSION_PATCH"),
    );

    let rc = format!(
        r#"#include <winver.h>

VS_VERSION_INFO VERSIONINFO
FILEVERSION {major},{minor},{patch},0
PRODUCTVERSION {major},{minor},{patch},0
FILEOS VOS_NT_WINDOWS32
FILETYPE VFT_DLL
BEGIN
    BLOCK "StringFileInfo"
    BEGIN
        BLOCK "040904B0"
        BEGIN
            VALUE "FileDescription", "lambiase plugin for Le Mans Ultimate"
            VALUE "FileVersion", "{version}"
            VALUE "ProductName", "lambiase"
            VALUE "ProductVersion", "{version}"
            VALUE "OriginalFilename", "lambiase_lmu.dll"
        END
    END
    BLOCK "VarFileInfo"
    BEGIN
        VALUE "Translation", 0x409, 1200
    END
END
"#
    );

    let path = PathBuf::from(env::var_os("OUT_DIR").unwrap()).join("version.rc");
    std::fs::write(&path, rc).unwrap();
    embed_resource::compile_for_cdylib(&path, embed_resource::NONE)
        .manifest_required()
        .unwrap();
}

/// The install folder, through the library listing `appmanifest_<APP_ID>.acf`. A port of
/// my-engineer's `find_lmu_steam_folder` CMake function.
fn find_lmu_steam_folder() -> Option<PathBuf> {
    let roots = [
        registry_value(r"HKCU\Software\Valve\Steam", "SteamPath"),
        registry_value(r"HKLM\SOFTWARE\WOW6432Node\Valve\Steam", "InstallPath"),
        Some(r"C:\Program Files (x86)\Steam".to_owned()),
    ];

    // Every Steam library is listed in libraryfolders.vdf as: "path"  "D:\\SteamLibrary"
    let mut libraries = Vec::<PathBuf>::new();
    for root in roots.into_iter().flatten() {
        let Ok(vdf) =
            std::fs::read_to_string(Path::new(&root).join("steamapps/libraryfolders.vdf"))
        else {
            continue;
        };

        for library in vdf.lines().filter_map(|line| vdf_value(line, "path")) {
            let library = PathBuf::from(library.replace(r"\\", r"\"));
            if !libraries.contains(&library) {
                libraries.push(library);
            }
        }
    }

    // The library that owns the game has the manifest naming its folder under common/
    libraries.into_iter().find_map(|library| {
        let steamapps = library.join("steamapps");
        let manifest =
            std::fs::read_to_string(steamapps.join(format!("appmanifest_{APP_ID}.acf"))).ok()?;
        let installdir = manifest
            .lines()
            .find_map(|line| vdf_value(line, "installdir"))?;
        let folder = steamapps.join("common").join(installdir);
        folder.is_dir().then_some(folder)
    })
}

/// The value of a `"key"  "value"` line of a Valve KeyValues file.
fn vdf_value<'a>(line: &'a str, key: &str) -> Option<&'a str> {
    let rest = line
        .trim()
        .strip_prefix('"')?
        .strip_prefix(key)?
        .strip_prefix('"')?;
    rest.trim_start().strip_prefix('"')?.strip_suffix('"')
}

/// A string value from the registry, through `reg query`.
fn registry_value(key: &str, name: &str) -> Option<String> {
    let output = Command::new("reg")
        .args(["query", key, "/v", name])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }

    // "    SteamPath    REG_SZ    c:/program files (x86)/steam"
    let stdout = String::from_utf8_lossy(&output.stdout);
    stdout.lines().find_map(|line| {
        let (_, value) = line.trim().strip_prefix(name)?.split_once("REG_SZ")?;
        Some(value.trim().to_owned())
    })
}
