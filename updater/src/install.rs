//! Runs on workers or in the standalone helper process, never on the application's UI thread.
use crate::{
    BUNDLE, HELPER, no_links, read_json, safe_path,
    stage::{self, Bundle, Ready},
    write_json,
};
use anyhow::{Context, ensure};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::{Path, PathBuf},
    process::Command,
    time::{Duration, Instant},
};

pub const RECOVERY: &str = "Bono Update Recovery.exe";
const POINTER: &str = "BONO-UPDATE-RECOVERY.txt";

#[derive(Debug, Serialize, Deserialize)]
pub struct Operation {
    pub root: PathBuf,
    pub destination: PathBuf,
    pub ready: Ready,
    pub installed: crate::release::Identity,
}
#[derive(Debug, Serialize, Deserialize)]
struct Journal {
    phase: String,
    files: BTreeMap<String, Option<String>>,
}

pub fn command(executable: &Path) -> Command {
    let mut cmd = Command::new(executable);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x08000000); // CREATE_NO_WINDOW
    }
    cmd
}

fn validate_operation(directory: &Path, op: &Operation) -> anyhow::Result<PathBuf> {
    no_links(directory)?;
    no_links(&op.root)?;
    no_links(&op.destination)?;
    ensure!(
        op.root.is_absolute() && op.destination.is_absolute(),
        "Nonabsolute update path"
    );
    ensure!(
        fs::canonicalize(directory)? == fs::canonicalize(op.root.join("recovery"))?,
        "Recovery directory mismatch"
    );
    let dest = fs::canonicalize(&op.destination)?;
    let root = fs::canonicalize(&op.root)?;
    ensure!(
        !dest.starts_with(&root) && !root.starts_with(&dest) && dest.parent().is_some(),
        "Overlapping update paths"
    );
    ensure!(
        op.ready.identity.newer_than(&op.installed),
        "Update is not newer on this channel"
    );
    Ok(stage::generation_path(&op.root, &op.ready)?.join("bundle"))
}

fn owned(bundle: &Bundle) -> anyhow::Result<BTreeSet<String>> {
    let mut files = BTreeSet::new();
    let mut aliases = BTreeSet::new();
    for name in bundle
        .files
        .keys()
        .chain(std::iter::once(&BUNDLE.to_owned()))
    {
        safe_path(name)?;
        ensure!(
            aliases.insert(name.to_ascii_lowercase()),
            "Conflicting manifest path"
        );
        files.insert(name.clone());
    }
    Ok(files)
}

/// Validate everything that can fail before the app closes, including writing beside the executable.
fn preflight(directory: &Path, op: &Operation) -> anyhow::Result<()> {
    no_links(&op.destination.join(POINTER))?;
    let source = validate_operation(directory, op)?;
    stage::validate_bundle(&source, &op.ready.identity)?;
    let current: Bundle = read_json(&op.destination.join(BUNDLE))?;
    ensure!(
        current.identity == op.installed,
        "Installed release changed; restart Bono"
    );
    let new: Bundle = read_json(&source.join(BUNDLE))?;
    let previous = owned(&current)?;
    for name in owned(&new)? {
        let target = op.destination.join(&name);
        no_links(&target)?;
        ensure!(
            !target.exists() || previous.contains(&name),
            "Update conflicts with unrelated file: {name}"
        );
    }
    for name in previous {
        no_links(&op.destination.join(name))?;
    }
    // Creating and removing a temporary file checks directory permissions without touching app files.
    let probe = tempfile::NamedTempFile::new_in(&op.destination)
        .context("Installation directory is not writable")?;
    probe.as_file().sync_all()?;
    drop(probe);
    Ok(())
}

pub fn prepare(
    root: &Path,
    destination: &Path,
    ready: Ready,
    installed: crate::release::Identity,
) -> anyhow::Result<()> {
    let guard = crate::lock(root)?;
    let directory = root.join("recovery");
    no_links(&directory)?;
    fs::create_dir_all(&directory)?;
    if directory.join("journal.json").exists() {
        let journal: Journal = read_json(&directory.join("journal.json"))?;
        ensure!(
            journal.phase == "committed" || journal.phase == "rolled_back",
            "An interrupted installation needs recovery: {}",
            directory.join(RECOVERY).display()
        );
    }
    // Fixed recovery directory is private to this updater and has been checked for links.
    for entry in fs::read_dir(&directory)? {
        let path = entry?.path();
        no_links(&path)?;
        if path.is_dir() {
            fs::remove_dir_all(path)?;
        } else {
            fs::remove_file(path)?;
        }
    }
    let op = Operation {
        root: fs::canonicalize(root)?,
        destination: fs::canonicalize(destination)?,
        ready,
        installed,
    };
    preflight(&directory, &op)?;
    let current: Bundle = read_json(&op.destination.join(BUNDLE))?;
    ensure!(
        stage::hash(&op.destination.join(HELPER))?
            == *current
                .files
                .get(HELPER)
                .context("Missing installed helper digest")?,
        "Installed helper checksum mismatch"
    );
    fs::copy(op.destination.join(HELPER), directory.join(RECOVERY))?;
    // A copied MSVC executable must also work on machines that only have the app-local CRT.
    // Keep the helper's runtime files outside the installation so replacement cannot lock them.
    for name in [
        "vcruntime140.dll",
        "vcruntime140_1.dll",
        "msvcp140.dll",
        "msvcp140_1.dll",
        "msvcp140_2.dll",
    ] {
        if let Some(digest) = current.files.get(name) {
            let source = op.destination.join(name);
            no_links(&source)?;
            ensure!(
                stage::hash(&source)? == *digest,
                "Installed helper runtime checksum mismatch: {name}"
            );
            fs::copy(source, directory.join(name))?;
            fs::OpenOptions::new()
                .write(true)
                .open(directory.join(name))?
                .sync_all()?;
        }
    }
    fs::OpenOptions::new()
        .write(true)
        .open(directory.join(RECOVERY))?
        .sync_all()?;
    write_json(&directory.join("operation.json"), &op)?;
    write_pointer(&op.destination, &directory, root)?;
    // Reserve the handoff before releasing the lock: other app instances must not replace
    // this operation or clean up its staged generation before the helper takes ownership.
    write_json(&directory.join("pending.json"), &true)?;
    drop(guard);
    let result = wait_for_helper(&directory);
    if result.is_err() {
        if let Ok(_guard) = crate::lock_raw(root) {
            let _ = fs::remove_file(directory.join("pending.json"));
        }
    }
    result
}

fn write_pointer(destination: &Path, directory: &Path, root: &Path) -> anyhow::Result<()> {
    use std::io::Write;
    let pointer = destination.join(POINTER);
    no_links(&pointer)?;
    // Replace the directory entry atomically instead of opening/truncating it. Even if the
    // entry is swapped for a link after validation, persist does not write through that link.
    let mut temporary = tempfile::NamedTempFile::new_in(destination)?;
    temporary.write_all(format!(
            "If Bono cannot start after an interrupted update, run:\r\n{}\r\n\r\nThis restores the previous installation and restarts Bono. Errors are saved to {}.\r\n",
            directory.join(RECOVERY).display(),
            root.join("result.json").display()
        ).as_bytes())?;
    temporary.flush()?;
    temporary.as_file().sync_all()?;
    temporary.persist(pointer).map_err(|e| e.error)?;
    Ok(())
}

fn wait_for_helper(directory: &Path) -> anyhow::Result<()> {
    let mut child = command(&directory.join(RECOVERY))
        .args(["--install", &std::process::id().to_string()])
        .spawn()?;
    let start = Instant::now();
    loop {
        if directory.join("ack.json").exists() {
            let result: Result<(), String> = read_json(&directory.join("ack.json"))?;
            return result.map_err(anyhow::Error::msg);
        }
        if child.try_wait()?.is_some() {
            anyhow::bail!(
                "Update helper exited before becoming ready; see {}",
                directory.parent().unwrap().join("result.json").display()
            );
        }
        if start.elapsed() > Duration::from_secs(180) {
            let _ = child.kill();
            let _ = child.wait();
            anyhow::bail!("Update helper did not become ready");
        }
        std::thread::sleep(Duration::from_millis(25));
    }
}

fn backup(directory: &Path, op: &Operation, new: &Bundle) -> anyhow::Result<Journal> {
    let old: Bundle = read_json(&op.destination.join(BUNDLE))?;
    let names = owned(&old)?
        .union(&owned(new)?)
        .cloned()
        .collect::<BTreeSet<_>>();
    let mut files = BTreeMap::new();
    for name in names {
        let target = op.destination.join(&name);
        no_links(&target)?;
        if target.exists() {
            ensure!(target.is_file(), "Target is not a file: {name}");
            let path = directory.join("backup").join(&name);
            no_links(&path)?;
            fs::create_dir_all(path.parent().unwrap())?;
            fs::copy(&target, &path)?;
            fs::OpenOptions::new().write(true).open(&path)?.sync_all()?;
            files.insert(name, Some(stage::hash(&path)?));
        } else {
            files.insert(name, None);
        }
    }
    let journal = Journal {
        phase: "applying".into(),
        files,
    };
    write_json(&directory.join("journal.json"), &journal)?;
    Ok(journal)
}

fn rollback(directory: &Path, op: &Operation, journal: &mut Journal) -> anyhow::Result<()> {
    // Validate every backup before touching any destination file.
    for (name, digest) in &journal.files {
        safe_path(name)?;
        no_links(&op.destination.join(name))?;
        if let Some(digest) = digest {
            let path = directory.join("backup").join(name);
            no_links(&path)?;
            ensure!(
                stage::hash(&path)? == *digest,
                "Recovery backup is damaged: {name}"
            );
        }
    }
    for (name, digest) in &journal.files {
        let target = op.destination.join(name);
        if digest.is_some() {
            // A second running Bono may still hold an unchanged executable open. Do not rewrite
            // files already equal to their backup, including after a partially completed rollback.
            if target.is_file() && stage::hash(&target).ok().as_ref() == digest.as_ref() {
                continue;
            }
            fs::create_dir_all(target.parent().unwrap())?;
            fs::copy(directory.join("backup").join(name), &target)?;
            fs::OpenOptions::new()
                .write(true)
                .open(target)?
                .sync_all()?;
        } else if target.exists() {
            fs::remove_file(target)?;
        }
    }
    journal.phase = "rolled_back".into();
    write_json(&directory.join("journal.json"), journal)
}

#[cfg(test)]
fn apply(directory: &Path, op: &Operation) -> anyhow::Result<()> {
    apply_tracking(directory, op, &mut false)
}

fn apply_tracking(
    directory: &Path,
    op: &Operation,
    mutation_started: &mut bool,
) -> anyhow::Result<()> {
    preflight(directory, op)?;
    let source = validate_operation(directory, op)?;
    let bundle = stage::validate_bundle(&source, &op.ready.identity)?;
    let mut journal = backup(directory, op, &bundle)?;
    *mutation_started = true;
    let result = (|| -> anyhow::Result<()> {
        let new = owned(&bundle)?;
        for name in journal.files.keys() {
            let target = op.destination.join(name);
            no_links(&target)?;
            if new.contains(name) {
                fs::create_dir_all(target.parent().unwrap())?;
                fs::copy(source.join(name), &target)?;
                fs::OpenOptions::new()
                    .write(true)
                    .open(&target)?
                    .sync_all()?;
            } else if target.exists() {
                fs::remove_file(target)?;
            }
        }
        stage::validate_bundle(&op.destination, &op.ready.identity)?;
        journal.phase = "committed".into();
        write_json(&directory.join("journal.json"), &journal)?;
        Ok(())
    })();
    if let Err(error) = result {
        rollback(directory, op, &mut journal)
            .context(format!("Install failed: {error:#}; rollback also failed"))?;
        return Err(error);
    }
    if let Ok(ready) = read_json::<Ready>(&op.root.join("ready.json")) {
        if ready.generation == op.ready.generation {
            let _ = fs::remove_file(op.root.join("ready.json"));
        }
    }
    let _ = stage::cleanup(&op.root, None);
    Ok(())
}

#[cfg(windows)]
struct Parent(windows_sys::Win32::Foundation::HANDLE);
#[cfg(windows)]
impl Parent {
    fn open(pid: u32) -> anyhow::Result<Self> {
        use windows_sys::Win32::System::Threading::{OpenProcess, PROCESS_SYNCHRONIZE};
        let handle = unsafe { OpenProcess(PROCESS_SYNCHRONIZE, 0, pid) };
        ensure!(
            !handle.is_null(),
            "Cannot open Bono process: {}",
            std::io::Error::last_os_error()
        );
        Ok(Self(handle))
    }
    fn wait(&self) -> anyhow::Result<()> {
        use windows_sys::Win32::{
            Foundation::WAIT_OBJECT_0, System::Threading::WaitForSingleObject,
        };
        ensure!(
            unsafe { WaitForSingleObject(self.0, 120_000) } == WAIT_OBJECT_0,
            "Bono did not exit within 120 seconds; no files were changed"
        );
        Ok(())
    }
}
#[cfg(windows)]
impl Drop for Parent {
    fn drop(&mut self) {
        unsafe {
            windows_sys::Win32::Foundation::CloseHandle(self.0);
        }
    }
}

/// Default invocation recovers an interrupted operation, even when bono.exe is missing.
pub fn run_helper(directory: &Path, parent: Option<u32>) -> anyhow::Result<()> {
    let op: Operation = read_json(&directory.join("operation.json"))?;
    validate_operation(directory, &op)?;
    // Publish the result under the lock, then release it before the restarted app loads its cache.
    let guard = crate::lock_raw(&op.root)?;
    let mut parent_exited = false;
    let mut mutation_started = false;
    let result = (|| -> anyhow::Result<()> {
        if let Some(pid) = parent {
            #[cfg(windows)]
            let process = Parent::open(pid)?;
            #[cfg(not(windows))]
            {
                let _ = pid;
                anyhow::bail!("Installation is supported only on Windows");
            }
            preflight(directory, &op)?;
            write_json(&directory.join("ack.json"), &Result::<(), String>::Ok(()))?;
            #[cfg(windows)]
            process.wait()?;
            parent_exited = true;
            apply_tracking(directory, &op, &mut mutation_started)?;
        } else if directory.join("journal.json").exists() {
            let mut journal: Journal = read_json(&directory.join("journal.json"))?;
            if journal.phase == "applying" {
                rollback(directory, &op, &mut journal)?;
            } else {
                ensure!(
                    journal.phase == "rolled_back" || journal.phase == "committed",
                    "Unknown recovery state"
                );
            }
        } else {
            anyhow::bail!("There is no interrupted installation to recover");
        }
        Ok(())
    })();
    if let Err(ref error) = result {
        if parent.is_some() && !directory.join("ack.json").exists() {
            let _ = write_json(
                &directory.join("ack.json"),
                &Result::<(), String>::Err(format!("{error:#}")),
            );
        }
    }
    write_json(
        &op.root.join("result.json"),
        &result
            .as_ref()
            .map(|_| "Update operation completed")
            .map_err(|e| format!("{e:#}")),
    )?;
    // Publish the result before spawning Bono. A failed install that rolled back is also safe
    // to restart; a timeout or failed rollback must not launch another app instance.
    let restart = result.is_ok()
        // No destination mutations happen before the backup journal is published. Once the
        // parent has exited, preflight/backup failures without a journal can restart the old app.
        || (parent_exited && (!mutation_started
            || read_json::<Journal>(&directory.join("journal.json"))
                .is_ok_and(|journal| journal.phase == "rolled_back")));
    let _ = fs::remove_file(directory.join("pending.json"));
    drop(guard);
    if restart {
        let exe = op.destination.join("bono.exe");
        no_links(&exe)?;
        if let Err(error) = command(&exe).current_dir(&op.destination).spawn() {
            let message = format!(
                "Update operation finished, but restarting Bono failed: {error}. Start Bono manually."
            );
            write_json(
                &op.root.join("result.json"),
                &Result::<String, String>::Err(message.clone()),
            )?;
            anyhow::bail!(message);
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::release::Identity;
    fn fixture() -> (tempfile::TempDir, Operation, PathBuf) {
        let temp = tempfile::tempdir().unwrap();
        let dest = temp.path().join("app");
        let root = temp.path().join("cache");
        let directory = root.join("recovery");
        fs::create_dir_all(&dest).unwrap();
        fs::create_dir_all(&directory).unwrap();
        let old = Identity {
            channel: "stable".into(),
            tag: "v1.0.0".into(),
        };
        let new = Identity {
            channel: "stable".into(),
            tag: "v2.0.0".into(),
        };
        let ready = Ready {
            generation: "generation-test".into(),
            identity: new.clone(),
        };
        let source = root.join(&ready.generation).join("bundle");
        fs::create_dir_all(&source).unwrap();
        for (dir, id, text) in [(&dest, old.clone(), "old"), (&source, new, "new")] {
            for name in [
                "bono.exe",
                HELPER,
                "runtime.dll",
                "assets/a",
                "plugins/bono_lmu.dll",
                "phonemizer/bono-phonemizer.exe",
            ] {
                let path = dir.join(name);
                fs::create_dir_all(path.parent().unwrap()).unwrap();
                fs::write(path, text).unwrap();
            }
            write_json(&dir.join(BUNDLE), &stage::manifest(dir, id).unwrap()).unwrap();
        }
        fs::write(dest.join("my-notes.txt"), "keep").unwrap();
        let op = Operation {
            root: fs::canonicalize(root).unwrap(),
            destination: fs::canonicalize(dest).unwrap(),
            ready,
            installed: old,
        };
        (temp, op, directory)
    }
    #[test]
    fn installs_full_bundle_and_preserves_unrelated_files() {
        let (_temp, op, dir) = fixture();
        apply(&dir, &op).unwrap();
        assert_eq!(
            fs::read_to_string(op.destination.join("bono.exe")).unwrap(),
            "new"
        );
        assert_eq!(
            fs::read_to_string(op.destination.join("my-notes.txt")).unwrap(),
            "keep"
        );
        assert_eq!(
            read_json::<Journal>(&dir.join("journal.json"))
                .unwrap()
                .phase,
            "committed"
        );
    }
    #[test]
    fn recovers_when_app_executable_was_removed() {
        let (_temp, op, dir) = fixture();
        let bundle: Bundle = read_json(
            &op.root
                .join(&op.ready.generation)
                .join("bundle")
                .join(BUNDLE),
        )
        .unwrap();
        let mut journal = backup(&dir, &op, &bundle).unwrap();
        fs::remove_file(op.destination.join("bono.exe")).unwrap();
        fs::write(op.destination.join("runtime.dll"), "half-installed").unwrap();
        rollback(&dir, &op, &mut journal).unwrap();
        assert_eq!(
            fs::read_to_string(op.destination.join("bono.exe")).unwrap(),
            "old"
        );
        assert_eq!(
            fs::read_to_string(op.destination.join("runtime.dll")).unwrap(),
            "old"
        );
    }
    #[test]
    fn removes_obsolete_owned_files_and_rollback_removes_new_files() {
        let (_temp, op, dir) = fixture();
        fs::write(op.destination.join("obsolete.dll"), "old").unwrap();
        write_json(
            &op.destination.join(BUNDLE),
            &stage::manifest(&op.destination, op.installed.clone()).unwrap(),
        )
        .unwrap();
        let source = op.root.join(&op.ready.generation).join("bundle");
        fs::write(source.join("introduced.dll"), "new").unwrap();
        write_json(
            &source.join(BUNDLE),
            &stage::manifest(&source, op.ready.identity.clone()).unwrap(),
        )
        .unwrap();
        let bundle: Bundle = read_json(&source.join(BUNDLE)).unwrap();
        let mut journal = backup(&dir, &op, &bundle).unwrap();
        fs::write(op.destination.join("introduced.dll"), "new").unwrap();
        fs::remove_file(op.destination.join("obsolete.dll")).unwrap();
        rollback(&dir, &op, &mut journal).unwrap();
        assert!(!op.destination.join("introduced.dll").exists());
        assert!(op.destination.join("obsolete.dll").exists());
        apply(&dir, &op).unwrap();
        assert!(!op.destination.join("obsolete.dll").exists());
        assert!(op.destination.join("introduced.dll").exists());
    }
    #[test]
    fn pending_handoff_blocks_all_app_operations() {
        let (_temp, op, dir) = fixture();
        write_json(&dir.join("pending.json"), &true).unwrap();
        assert!(crate::lock(&op.root).is_err());
        assert!(crate::lock_raw(&op.root).is_ok());
    }

    #[test]
    fn recovery_pointer_replacement_does_not_truncate_linked_file() {
        let (temp, op, dir) = fixture();
        let unrelated = temp.path().join("unrelated.txt");
        fs::write(&unrelated, "preserve").unwrap();
        fs::hard_link(&unrelated, op.destination.join(POINTER)).unwrap();
        write_pointer(&op.destination, &dir, &op.root).unwrap();
        assert_eq!(fs::read_to_string(&unrelated).unwrap(), "preserve");
        assert!(
            fs::read_to_string(op.destination.join(POINTER))
                .unwrap()
                .contains(RECOVERY)
        );
    }

    #[cfg(unix)]
    #[test]
    fn recovery_pointer_rejects_symlinks() {
        let (temp, op, dir) = fixture();
        let unrelated = temp.path().join("unrelated.txt");
        fs::write(&unrelated, "preserve").unwrap();
        std::os::unix::fs::symlink(&unrelated, op.destination.join(POINTER)).unwrap();
        assert!(write_pointer(&op.destination, &dir, &op.root).is_err());
        assert_eq!(fs::read_to_string(unrelated).unwrap(), "preserve");
    }
    #[cfg(windows)]
    #[test]
    fn parent_wait_times_out_without_mutating_files() {
        use windows_sys::Win32::{
            Foundation::WAIT_TIMEOUT, System::Threading::WaitForSingleObject,
        };
        let parent = Parent::open(std::process::id()).unwrap();
        assert_eq!(unsafe { WaitForSingleObject(parent.0, 0) }, WAIT_TIMEOUT);
    }
    #[test]
    fn refuses_unrelated_collision_and_damaged_backup() {
        let (_temp, op, dir) = fixture();
        let source = op.root.join(&op.ready.generation).join("bundle");
        fs::write(source.join("my-notes.txt"), "overwrite").unwrap();
        write_json(
            &source.join(BUNDLE),
            &stage::manifest(&source, op.ready.identity.clone()).unwrap(),
        )
        .unwrap();
        assert!(preflight(&dir, &op).is_err());
        let bundle = read_json(&source.join(BUNDLE)).unwrap();
        let mut journal = backup(&dir, &op, &bundle).unwrap();
        fs::write(dir.join("backup/bono.exe"), "bad").unwrap();
        assert!(rollback(&dir, &op, &mut journal).is_err());
        assert_eq!(
            fs::read_to_string(op.destination.join("bono.exe")).unwrap(),
            "old"
        );
    }
}
