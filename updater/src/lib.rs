//! Blocking filesystem functions in this crate must be called on a worker, never the UI thread.
pub mod install;
pub mod release;
pub mod stage;

use anyhow::{Context, ensure};
use serde::{Serialize, de::DeserializeOwned};
use std::{
    fs::{self, File, OpenOptions},
    io::Write,
    path::{Component, Path},
};

pub const BUNDLE: &str = "bono-bundle.json";
pub const ARCHIVE: &str = "bono-windows-x86_64.zip";
pub const HELPER: &str = "bono-updater.exe";

pub fn read_json<T: DeserializeOwned>(path: &Path) -> anyhow::Result<T> {
    ensure!(
        fs::metadata(path)?.len() <= 8 * 1024 * 1024,
        "Manifest is too large"
    );
    Ok(serde_json::from_reader(File::open(path)?)?)
}

/// Replace a file through a fully flushed temporary file on the same filesystem.
pub fn write_json(path: &Path, value: &impl Serialize) -> anyhow::Result<()> {
    let mut temp = tempfile::NamedTempFile::new_in(path.parent().context("Missing parent")?)?;
    serde_json::to_writer(&mut temp, value)?;
    temp.flush()?;
    temp.as_file().sync_all()?;
    temp.persist(path).map_err(|e| e.error)?;
    Ok(())
}

pub fn lock(root: &Path) -> anyhow::Result<File> {
    let file = lock_raw(root)?;
    no_links(&root.join("recovery/pending.json"))?;
    ensure!(
        !root.join("recovery/pending.json").exists(),
        "An installation is being prepared or needs recovery; run the helper named in BONO-UPDATE-RECOVERY.txt"
    );
    Ok(file)
}

pub(crate) fn lock_raw(root: &Path) -> anyhow::Result<File> {
    fs::create_dir_all(root)?;
    no_links(root)?;
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(root.join("update.lock"))?;
    file.try_lock()
        .context("Another Bono instance is updating")?;
    Ok(file)
}

/// A portable relative Windows path. Reject aliases, device names and ADS even on Unix tests.
pub fn safe_path(name: &str) -> anyhow::Result<()> {
    ensure!(
        name.is_ascii() && !name.bytes().any(|b| b < 32 || b"<>\"|?*".contains(&b)),
        "Unsupported bundle path: {name}"
    );
    ensure!(
        !name.is_empty() && !name.contains(['\\', ':', '\0']),
        "Unsafe bundle path: {name}"
    );
    ensure!(
        Path::new(name)
            .components()
            .all(|p| matches!(p, Component::Normal(_))),
        "Unsafe bundle path: {name}"
    );
    for part in name.split('/') {
        ensure!(
            !part.is_empty() && !part.ends_with(['.', ' ']) && part != "." && part != "..",
            "Unsafe bundle path: {name}"
        );
        let stem = part.split('.').next().unwrap().to_ascii_uppercase();
        ensure!(
            !matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL")
                && !(stem.len() == 4
                    && (stem.starts_with("COM") || stem.starts_with("LPT"))
                    && stem.as_bytes()[3].is_ascii_digit()),
            "Device path: {name}"
        );
    }
    Ok(())
}

/// Reject symlinks and Windows junctions in every existing component before filesystem mutations.
pub fn no_links(path: &Path) -> anyhow::Result<()> {
    for ancestor in path.ancestors() {
        match fs::symlink_metadata(ancestor) {
            Ok(meta) => {
                ensure!(
                    !meta.file_type().is_symlink(),
                    "Linked update path: {}",
                    ancestor.display()
                );
                #[cfg(windows)]
                {
                    use std::os::windows::fs::MetadataExt;
                    ensure!(
                        meta.file_attributes() & 0x400 == 0,
                        "Reparse point: {}",
                        ancestor.display()
                    );
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => (),
            Err(e) => return Err(e.into()),
        }
    }
    Ok(())
}
