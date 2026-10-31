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

pub struct Updates {
    identity: Identity,
    supported: bool,
    root: PathBuf,
    operation: Option<Task<anyhow::Result<Completed>>>,
    candidate: Option<Candidate>,
    ready: Option<Ready>,
    error: Option<String>,
    notice: Option<String>,
    next_check: Instant,
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
        let operation = supported.then(|| {
            let root = root.clone();
            let identity = identity.clone();
            Task::spawn_blocking(move || {
                let _guard = bono_updater::lock(&root)?;
                let ready = bono_updater::stage::ready(&root, &identity)?;
                // Only remove obsolete generations once a durable ready pointer has been read successfully.
                bono_updater::stage::cleanup(&root, ready.as_ref())?;
                let result_path = root.join("result.json");
                let message = if result_path.exists() {
                    let result: Result<String, String> = bono_updater::read_json(&result_path)?;
                    Some(result)
                } else {
                    None
                };
                Ok(Completed::Loaded(ready, message))
            })
        });
        Self {
            identity,
            supported,
            root,
            operation,
            candidate: None,
            ready: None,
            error: None,
            notice: None,
            next_check: Instant::now(),
            progress: Arc::new(AtomicU64::new(0)),
            downloading: false,
            quit: false,
        }
    }

    pub fn poll(&mut self, rt: &Handle) {
        if let Some(result) = self.operation.as_mut().and_then(|task| task.update(rt)) {
            self.operation = None;
            self.downloading = false;
            match result {
                Ok(Completed::Loaded(ready, message)) => {
                    self.ready = ready;
                    match message {
                        Some(Ok(_)) => self.notice = Some(t!("updates.finished").to_owned()),
                        Some(Err(error)) => self.error = Some(error),
                        None => (),
                    }
                }
                Ok(Completed::Checked(candidate)) => self.candidate = candidate,
                Ok(Completed::Downloaded(ready)) => self.ready = Some(ready),
                Ok(Completed::InstallPrepared) => self.quit = true,
                Err(error) => self.error = Some(format!("{error:#}")),
            }
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
        } else if !self.available() && self.error.is_none() {
            ui.label(t!("updates.current"));
        }
        if let Some(notice) = &self.notice {
            ui.label(notice);
        }
        if let Some(error) = &self.error {
            ui.colored_label(ui.visuals().error_fg_color, error);
        }
    }
}
