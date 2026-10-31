use crate::task::Task;
use bono_i18n::t;
use bono_updater::{
    release::{Candidate, Identity},
    stage::Ready,
};
use std::{
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};
use tokio::runtime::Handle;

enum Completed {
    Loaded(Option<Ready>, Option<Result<String, String>>),
    Checked(Option<Candidate>),
    Downloaded(Ready),
    InstallPrepared,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum OperationKind {
    Refresh,
    Check,
    Download,
    Install,
}

pub struct Updates {
    identity: Identity,
    supported: bool,
    root: PathBuf,
    operation: Option<Task<anyhow::Result<Completed>>>,
    candidate: Option<Candidate>,
    ready: Option<Ready>,
    error: Option<String>,
    error_kind: Option<OperationKind>,
    refresh_error: Option<String>,
    operation_kind: OperationKind,
    notice: Option<String>,
    next_check: Instant,
    next_load: Instant,
    progress: Arc<AtomicU64>,
    downloading: bool,
    quit: bool,
}

impl Updates {
    pub fn new(pref_path: &camino::Utf8Path) -> Self {
        let identity = Identity {
            channel: env!("BONO_RELEASE_CHANNEL").into(),
            tag: env!("BONO_RELEASE_TAG").into(),
        };
        let supported =
            cfg!(all(target_os = "windows", target_arch = "x86_64")) && identity.valid();
        let root = pref_path.join("updates").into_std_path_buf();
        let operation = supported.then(|| load_staged(root.clone(), identity.clone(), None));
        Self {
            identity,
            supported,
            root,
            operation,
            candidate: None,
            ready: None,
            error: None,
            error_kind: None,
            refresh_error: None,
            operation_kind: OperationKind::Refresh,
            notice: None,
            next_check: Instant::now(),
            next_load: Instant::now() + Duration::from_secs(5),
            progress: Arc::new(AtomicU64::new(0)),
            downloading: false,
            quit: false,
        }
    }

    pub fn poll(&mut self, rt: &Handle) {
        if let Some(result) = self.operation.as_mut().and_then(|task| task.update(rt)) {
            self.operation = None;
            self.downloading = false;
            let kind = self.operation_kind;
            if result.is_ok() && kind != OperationKind::Refresh && self.error_kind == Some(kind) {
                self.error = None;
                self.error_kind = None;
            }
            match result {
                Ok(Completed::Loaded(ready, message)) => {
                    self.ready = ready;
                    self.refresh_error = None;
                    match message {
                        Some(Ok(_)) => self.notice = Some(t!("updates.finished").to_owned()),
                        Some(Err(error)) => self.refresh_error = Some(error),
                        None => (),
                    }
                }
                Ok(Completed::Checked(candidate)) => self.candidate = candidate,
                Ok(Completed::Downloaded(ready)) => {
                    self.ready = Some(ready);
                }
                Ok(Completed::InstallPrepared) => self.quit = true,
                Err(error) => {
                    if kind == OperationKind::Refresh {
                        self.refresh_error = Some(format!("{error:#}"));
                    } else {
                        self.error = Some(format!("{error:#}"));
                        self.error_kind = Some(kind);
                    }
                }
            }
            // Cool down from completion, leaving room for checks even after a slow refresh.
            self.next_load = Instant::now() + Duration::from_secs(5);
        }
        if self.supported
            && !self.quit
            && self.operation.is_none()
            && Instant::now() >= self.next_load
        {
            self.operation_kind = OperationKind::Refresh;
            self.operation = Some(load_staged(
                self.root.clone(),
                self.identity.clone(),
                self.ready.clone(),
            ));
        }
        if self.supported
            && self.operation.is_none()
            && !self.quit
            && Instant::now() >= self.next_check
        {
            self.check();
        }
    }

    fn check(&mut self) {
        self.operation_kind = OperationKind::Check;
        self.next_check = Instant::now() + Duration::from_secs(6 * 60 * 60);
        let identity = self.identity.clone();
        let root = self.root.clone();
        self.operation = Some(Task::spawn(async move {
            let guard = tokio::task::spawn_blocking(move || bono_updater::lock(&root)).await??;
            let result = bono_updater::release::check(&identity).await;
            drop(guard);
            result.map(Completed::Checked)
        }));
    }

    pub fn available(&self) -> bool {
        self.candidate.is_some() || self.ready.is_some()
    }
    pub fn take_quit(&mut self) -> bool {
        std::mem::take(&mut self.quit)
    }

    pub fn ui(&mut self, ui: &mut egui::Ui) {
        ui.set_max_width(420.0);
        ui.label(format!("Bono {}", env!("BONO_VERSION")));
        ui.label(match self.identity.channel.as_str() {
            "stable" => t!("updates.stable"),
            "nightly" => t!("updates.nightly"),
            _ => t!("updates.development"),
        });
        if !self.supported {
            ui.label(t!("updates.unsupported"));
            return;
        }
        let busy = self.operation.is_some() || self.quit;
        if ui
            .add_enabled(!busy, egui::Button::new(t!("updates.check")))
            .clicked()
        {
            self.error = None;
            self.check();
        }
        if let Some(candidate) = self.candidate.clone() {
            let already_downloaded = self
                .ready
                .as_ref()
                .is_some_and(|r| !candidate.identity.newer_than(&r.identity));
            if !already_downloaded {
                if ui
                    .add_enabled(
                        !busy,
                        egui::Button::new(t!(
                            "updates.download",
                            version = &candidate.identity.tag
                        )),
                    )
                    .clicked()
                {
                    self.error = None;
                    self.operation_kind = OperationKind::Download;
                    self.downloading = true;
                    self.progress.store(0, Ordering::Relaxed);
                    let root = self.root.clone();
                    let progress = self.progress.clone();
                    self.operation = Some(Task::spawn(async move {
                        bono_updater::stage::download(root, candidate, progress)
                            .await
                            .map(Completed::Downloaded)
                    }));
                }
            }
        }
        if let Some(ready) = self.ready.clone() {
            ui.label(t!("updates.restart_notice"));
            if ui
                .add_enabled(
                    !busy,
                    egui::Button::new(t!("updates.install", version = &ready.identity.tag)),
                )
                .clicked()
            {
                self.error = None;
                self.operation_kind = OperationKind::Install;
                let root = self.root.clone();
                let installed = self.identity.clone();
                self.operation = Some(Task::spawn_blocking(move || {
                    let exe = std::env::current_exe()?;
                    bono_updater::install::prepare(
                        &root,
                        exe.parent()
                            .ok_or_else(|| anyhow::anyhow!("Missing app directory"))?,
                        ready,
                        installed,
                    )?;
                    Ok(Completed::InstallPrepared)
                }));
            }
        }
        if busy {
            ui.spinner();
            if self.downloading {
                let bytes = self.progress.load(Ordering::Relaxed);
                let total = self.candidate.as_ref().map_or(1, |c| c.archive.size).max(1);
                ui.add(
                    egui::ProgressBar::new((bytes as f64 / total as f64) as f32).show_percentage(),
                );
                ui.label(t!("updates.validating"));
            } else {
                ui.label(t!("updates.working"));
            }
            ui.ctx().request_repaint_after(Duration::from_millis(100));
        } else if !self.available() && self.error.is_none() && self.refresh_error.is_none() {
            ui.label(t!("updates.current"));
        }
        if let Some(notice) = &self.notice {
            ui.label(notice);
        }
        if let Some(error) = &self.refresh_error {
            ui.colored_label(ui.visuals().error_fg_color, error);
        }
        if let Some(error) = &self.error {
            ui.colored_label(ui.visuals().error_fg_color, error);
        }
    }
}

fn load_staged(
    root: PathBuf,
    identity: Identity,
    known: Option<Ready>,
) -> Task<anyhow::Result<Completed>> {
    Task::spawn_blocking(move || {
        let _guard = bono_updater::lock(&root)?;
        let ready = bono_updater::stage::refresh(&root, &identity, known.as_ref())?;
        let result_path = root.join("result.json");
        let message = if result_path.exists() {
            // A corrupt old result must not hide an otherwise valid offline-installable bundle.
            Some(
                bono_updater::read_json::<Result<String, String>>(&result_path).unwrap_or_else(
                    |error| Err(format!("Unable to read previous update result: {error:#}")),
                ),
            )
        } else {
            None
        };
        Ok(Completed::Loaded(ready, message))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn retries_staged_restore_after_another_instance_releases_lock() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("updates");
        let bundle = root.join("generation-saved/bundle");
        for name in [
            "bono.exe",
            "bono-updater.exe",
            "runtime.dll",
            "assets/a",
            "plugins/bono_lmu.dll",
            "phonemizer/bono-phonemizer.exe",
        ] {
            let path = bundle.join(name);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, "payload").unwrap();
        }
        let saved = Ready {
            generation: "generation-saved".into(),
            identity: Identity {
                channel: "stable".into(),
                tag: "v2.0.0".into(),
            },
        };
        bono_updater::write_json(
            &bundle.join(bono_updater::BUNDLE),
            &bono_updater::stage::manifest(&bundle, saved.identity.clone()).unwrap(),
        )
        .unwrap();
        bono_updater::write_json(&root.join("ready.json"), &saved).unwrap();
        let rt = tokio::runtime::Runtime::new().unwrap();
        let _entered = rt.enter();
        let mut updates = Updates::new(camino::Utf8Path::from_path(temp.path()).unwrap());
        updates.identity = Identity {
            channel: "stable".into(),
            tag: "v1.0.0".into(),
        };
        updates.supported = true;
        updates.root = root.clone();
        updates.operation = None;
        updates.next_load = Instant::now();
        updates.next_check = Instant::now() + Duration::from_secs(3600);
        let guard = bono_updater::lock(&root).unwrap();
        fn finish(updates: &mut Updates, rt: &Handle) {
            let deadline = Instant::now() + Duration::from_secs(5);
            while updates.operation.is_some() {
                assert!(Instant::now() < deadline);
                updates.poll(rt);
                std::thread::sleep(Duration::from_millis(1)); // Test worker only; never a UI callback.
            }
        }
        updates.poll(rt.handle());
        updates.next_load = Instant::now() - Duration::from_secs(10);
        finish(&mut updates, rt.handle());
        assert!(updates.ready.is_none());
        assert!(updates.refresh_error.is_some());
        // Local refresh cooldown must not suppress online discovery after a failed restore.
        updates.next_check = Instant::now();
        updates.poll(rt.handle());
        assert!(updates.next_check > Instant::now() + Duration::from_secs(60));
        finish(&mut updates, rt.handle());
        drop(guard);
        updates.next_load = Instant::now();
        updates.poll(rt.handle());
        finish(&mut updates, rt.handle());
        assert_eq!(updates.ready.as_ref().unwrap().generation, saved.generation);
        assert!(updates.refresh_error.is_none());
        assert!(updates.error.is_some()); // A refresh must preserve the failed check.
        // Successful operations clear only errors from the same source.
        for kind in [
            OperationKind::Check,
            OperationKind::Download,
            OperationKind::Install,
        ] {
            updates.error = Some("operation failed".into());
            updates.error_kind = Some(kind);
            updates.next_load = Instant::now();
            updates.poll(rt.handle());
            finish(&mut updates, rt.handle());
            assert_eq!(updates.error.as_deref(), Some("operation failed"));
            updates.operation_kind = OperationKind::Check;
            updates.operation = Some(Task::spawn(async { Ok(Completed::Checked(None)) }));
            finish(&mut updates, rt.handle());
            assert_eq!(updates.error.is_none(), kind == OperationKind::Check);
        }
        updates.error = None;
        updates.error_kind = None;
        fs::write(root.join("result.json"), "corrupt result").unwrap();
        updates.next_load = Instant::now();
        updates.poll(rt.handle());
        finish(&mut updates, rt.handle());
        assert!(updates.ready.is_some());
        assert!(
            updates
                .refresh_error
                .as_ref()
                .unwrap()
                .contains("previous update result")
        );
        // Another instance promotes a replacement and removes our previously cached generation.
        let replacement = Ready {
            generation: "generation-replacement".into(),
            identity: Identity {
                channel: "stable".into(),
                tag: "v3.0.0".into(),
            },
        };
        fs::rename(
            root.join(&saved.generation),
            root.join(&replacement.generation),
        )
        .unwrap();
        let replacement_bundle = root.join(&replacement.generation).join("bundle");
        bono_updater::write_json(
            &replacement_bundle.join(bono_updater::BUNDLE),
            &bono_updater::stage::manifest(&replacement_bundle, replacement.identity.clone())
                .unwrap(),
        )
        .unwrap();
        bono_updater::write_json(&root.join("ready.json"), &replacement).unwrap();
        updates.next_load = Instant::now();
        updates.poll(rt.handle());
        finish(&mut updates, rt.handle());
        assert_eq!(updates.ready.as_ref(), Some(&replacement));
        fs::remove_file(root.join("ready.json")).unwrap();
        updates.next_load = Instant::now();
        updates.poll(rt.handle());
        finish(&mut updates, rt.handle());
        assert!(updates.ready.is_none());
    }
}
