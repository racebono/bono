//! Exercise the real helper process without closing the test runner or replacing live app files.
#![cfg(windows)]
use bono_updater::{
    BUNDLE, HELPER,
    install::{self, Operation},
    release::Identity,
    stage::{self, Ready},
    write_json,
};
use std::{
    fs,
    process::Child,
    time::{Duration, Instant},
};

struct Worker(Child);
impl Drop for Worker {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn fixture() -> (tempfile::TempDir, Operation, std::path::PathBuf) {
    let temp = tempfile::tempdir().unwrap();
    let destination = temp.path().join("app");
    let root = temp.path().join("cache");
    let directory = root.join("recovery");
    let source = root.join("generation-test/bundle");
    fs::create_dir_all(&directory).unwrap();
    let installed = Identity {
        channel: "stable".into(),
        tag: "v1.0.0".into(),
    };
    let target = Identity {
        channel: "stable".into(),
        tag: "v2.0.0".into(),
    };
    for (dir, identity, content) in [
        (&destination, installed.clone(), "old"),
        (&source, target.clone(), "new"),
    ] {
        for name in [
            "bono.exe",
            "runtime.dll",
            "assets/a",
            "plugins/bono_lmu.dll",
            "phonemizer/bono-phonemizer.exe",
        ] {
            let path = dir.join(name);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, content).unwrap();
        }
        fs::copy(env!("CARGO_BIN_EXE_bono-updater"), dir.join(HELPER)).unwrap();
        write_json(&dir.join(BUNDLE), &stage::manifest(dir, identity).unwrap()).unwrap();
    }
    fs::copy(
        env!("CARGO_BIN_EXE_bono-updater"),
        directory.join(install::RECOVERY),
    )
    .unwrap();
    let op = Operation {
        root: fs::canonicalize(&root).unwrap(),
        destination: fs::canonicalize(&destination).unwrap(),
        ready: Ready {
            generation: "generation-test".into(),
            identity: target,
        },
        installed,
    };
    write_json(&directory.join("operation.json"), &op).unwrap();
    write_json(&directory.join("pending.json"), &true).unwrap();
    (temp, op, directory)
}

fn restarted_fixture() -> bool {
    if let Some(path) = std::env::var_os("BONO_UPDATER_RESTART_MARKER") {
        fs::write(path, "restarted").unwrap();
        return true;
    }
    false
}

#[test]
fn real_helper_acknowledges_then_waits_for_parent_exit() {
    if restarted_fixture() {
        return;
    }
    let (_temp, op, directory) = fixture();
    let destination = &op.destination;
    let root = &op.root;
    let mut child = Worker(
        install::command(&directory.join(install::RECOVERY))
            .args(["--install", &std::process::id().to_string()])
            .spawn()
            .unwrap(),
    );
    let start = Instant::now();
    while !directory.join("ack.json").exists() {
        assert!(child.0.try_wait().unwrap().is_none(), "Helper exited early");
        assert!(
            start.elapsed() < Duration::from_secs(30),
            "Helper did not acknowledge"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    let ack: Result<(), String> = bono_updater::read_json(&directory.join("ack.json")).unwrap();
    ack.unwrap();
    assert!(child.0.try_wait().unwrap().is_none());
    assert_eq!(
        fs::read_to_string(destination.join("bono.exe")).unwrap(),
        "old"
    );
    assert!(!directory.join("journal.json").exists());
    assert!(bono_updater::lock(&root).is_err());
    drop(child);
}

#[test]
#[ignore = "subprocess fixture invoked by the post-exit installation test"]
fn subprocess_parent_fixture() {
    if std::env::var_os("BONO_UPDATER_TEST_PARENT").is_some() {
        use std::io::Read;
        let _ = std::io::stdin().read_exact(&mut [0u8]);
    }
}

#[test]
fn real_helper_installs_and_restarts_after_parent_exit() {
    if restarted_fixture() {
        return;
    }
    post_exit_install(false);
}

#[test]
fn restarts_original_app_after_pre_backup_failure() {
    if restarted_fixture() {
        return;
    }
    post_exit_install(true);
}

fn post_exit_install(fail_before_backup: bool) {
    let (temp, op, directory) = fixture();
    // The test harness acts as the new Bono: on restart its tests simply write a marker and exit.
    let source = op.root.join(&op.ready.generation).join("bundle");
    fs::copy(std::env::current_exe().unwrap(), source.join("bono.exe")).unwrap();
    write_json(
        &source.join(BUNDLE),
        &stage::manifest(&source, op.ready.identity.clone()).unwrap(),
    )
    .unwrap();
    fs::copy(
        std::env::current_exe().unwrap(),
        op.destination.join("bono.exe"),
    )
    .unwrap();
    write_json(
        &op.destination.join(BUNDLE),
        &stage::manifest(&op.destination, op.installed.clone()).unwrap(),
    )
    .unwrap();
    write_json(&op.root.join("ready.json"), &op.ready).unwrap();
    let mut parent = Worker(
        std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--ignored", "--exact", "subprocess_parent_fixture"])
            .env("BONO_UPDATER_TEST_PARENT", "1")
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::null())
            .spawn()
            .unwrap(),
    );
    let marker = temp.path().join("restarted.txt");
    let mut helper = Worker(
        install::command(&directory.join(install::RECOVERY))
            .args(["--install", &parent.0.id().to_string()])
            .env("BONO_UPDATER_RESTART_MARKER", &marker)
            .stdout(std::process::Stdio::null())
            .spawn()
            .unwrap(),
    );
    let start = Instant::now();
    while !directory.join("ack.json").exists() {
        assert!(
            helper.0.try_wait().unwrap().is_none(),
            "Helper exited before acknowledgement"
        );
        assert!(start.elapsed() < Duration::from_secs(30));
        std::thread::sleep(Duration::from_millis(20));
    }
    let ack: Result<(), String> = bono_updater::read_json(&directory.join("ack.json")).unwrap();
    ack.unwrap();
    assert_eq!(
        fs::read_to_string(op.destination.join("runtime.dll")).unwrap(),
        "old"
    );
    if fail_before_backup {
        fs::remove_file(source.join("assets/a")).unwrap();
    }
    drop(parent.0.stdin.take()); // Let the original process exit normally.
    while helper.0.try_wait().unwrap().is_none() {
        assert!(start.elapsed() < Duration::from_secs(30));
        std::thread::sleep(Duration::from_millis(20));
    }
    let result: Result<String, String> =
        bono_updater::read_json(&op.root.join("result.json")).unwrap();
    assert_eq!(result.is_err(), fail_before_backup);
    assert_eq!(helper.0.wait().unwrap().success(), !fail_before_backup);
    while !marker.exists()
        || fs::OpenOptions::new()
            .write(true)
            .open(op.destination.join("bono.exe"))
            .is_err()
    {
        assert!(
            start.elapsed() < Duration::from_secs(30),
            "Updated app did not restart and exit"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    stage::validate_bundle(
        &op.destination,
        if fail_before_backup {
            &op.installed
        } else {
            &op.ready.identity
        },
    )
    .unwrap();
    assert_eq!(
        fs::read_to_string(op.destination.join("runtime.dll")).unwrap(),
        if fail_before_backup { "old" } else { "new" }
    );
    assert_eq!(op.root.join("ready.json").exists(), fail_before_backup);
    if fail_before_backup {
        assert!(!directory.join("journal.json").exists());
    }
    assert!(!directory.join("pending.json").exists());
    assert!(bono_updater::lock(&op.root).is_ok());
}
