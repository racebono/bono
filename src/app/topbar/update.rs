use crate::{
    Download,
    app::{Subsystems, topbar::size_text},
    task::Task,
    update::{self, Channel, Release, Staging},
};
use bono_i18n::t;
use egui::IntoAtoms;
use std::sync::Arc;
use tokio::runtime::Handle;

/// The color of the sign on the menu once a new version is ready to install
const READY_COLOR: egui::Color32 = egui::Color32::from_rgb(80, 200, 120);

enum State {
    Unsupported,
    Checking(Task<anyhow::Result<(Option<Release>, bool)>>),
    CheckFailed(String),
    UpToDate,
    Available(Release),
    Downloading(Release, Download),
    Unpacking(Release, Task<anyhow::Result<()>>),
    Ready(Release),
    Installing(Release, Task<anyhow::Result<()>>),
    /// The app is closing for the installer
    Restarting,
}

/// The "Update" menu: this version, and a newer one to download and install.
pub struct UpdateTopbar {
    client: reqwest::Client,
    staging: Option<Arc<Staging>>,
    state: State,
    /// What last went wrong unpacking or installing (a failed download says so itself)
    error: Option<String>,
    /// The check at startup offers a new version in a popup
    startup: bool,
    popup: bool,
}

impl UpdateTopbar {
    pub fn new(client: &reqwest::Client) -> Self {
        let staging = update::supported()
            .then(|| Staging::new())
            .and_then(|staging| {
                staging
                    .inspect_err(|e| log::warn!("Updates are unavailable: {e:#}"))
                    .ok()
            })
            .map(Arc::new);
        let mut this = Self {
            client: client.clone(),
            staging,
            state: State::Unsupported,
            error: None,
            startup: true,
            popup: false,
        };
        this.check();
        this
    }

    fn check(&mut self) {
        let Some(staging) = self.staging.clone() else {
            return;
        };
        let client = self.client.clone();
        self.error = None;
        self.state = State::Checking(Task::spawn(async move {
            let latest = update::check(&client).await?;
            let tidy = latest.clone();
            let ready = tokio::task::spawn_blocking(move || staging.tidy(tidy.as_ref())).await?;
            Ok((latest, ready))
        }));
    }

    fn download(&mut self) {
        let (Some(staging), State::Available(release)) = (&self.staging, &self.state) else {
            return;
        };
        self.error = None;
        let download = staging.download(&self.client, release);
        self.state = State::Downloading(release.clone(), download);
    }

    /// A new version is downloaded, ready to install.
    pub fn ready(&self) -> bool {
        matches!(self.state, State::Ready(_))
    }

    /// The menu's label: with a sign when a new version is ready to install.
    pub fn label(&self) -> egui::Atoms<'static> {
        let mut atoms = t!("update").into_atoms();
        if self.ready() {
            atoms.push_left(
                egui::RichText::new(egui_phosphor::bold::ARROW_CIRCLE_UP).color(READY_COLOR),
            );
        }
        atoms
    }

    /// Follows what's going on, even while the menu is closed.
    pub fn poll(&mut self, rt: &Handle, ss: &Subsystems) {
        let state = std::mem::replace(&mut self.state, State::Unsupported);
        self.state = match state {
            State::Checking(mut task) => match task.update(rt) {
                None => State::Checking(task),
                Some(result) => {
                    let startup = std::mem::replace(&mut self.startup, false);
                    match result {
                        Ok((None, _)) => State::UpToDate,
                        Ok((Some(release), true)) => State::Ready(release),
                        Ok((Some(release), false)) => {
                            self.popup = startup;
                            State::Available(release)
                        }
                        Err(e) => {
                            log::warn!("Checking for updates: {e:#}");
                            State::CheckFailed(format!("{e:#}"))
                        }
                    }
                }
            },
            State::Downloading(release, download) if download.finished() => {
                let staging = self.staging.clone().unwrap();
                let unpack = release.clone();
                State::Unpacking(
                    release,
                    Task::spawn_blocking(move || staging.unpack(&unpack)),
                )
            }
            // The download said what went wrong
            State::Downloading(release, download) if download.failed() => State::Available(release),
            State::Unpacking(release, mut task) => match task.update(rt) {
                None => State::Unpacking(release, task),
                Some(Ok(())) => State::Ready(release),
                Some(Err(e)) => {
                    log::error!("Unpacking the update: {e:#}");
                    self.error = Some(format!("{e:#}"));
                    State::Available(release)
                }
            },
            State::Installing(release, mut task) => match task.update(rt) {
                None => State::Installing(release, task),
                Some(Ok(())) => {
                    // The installer goes on once the app has closed
                    if let Err(e) = ss
                        .event
                        .push_event(sdl3::event::Event::Quit { timestamp: 0 })
                    {
                        log::error!("Unable to close for the update: {e}");
                    }
                    State::Restarting
                }
                Some(Err(e)) => {
                    log::error!("Installing the update: {e:#}");
                    self.error = Some(format!("{e:#}"));
                    State::Ready(release)
                }
            },
            state => state,
        };
    }

    pub fn update(&mut self, ui: &mut egui::Ui) {
        ui.set_max_width(320.0);
        let channel = match Channel::current() {
            Channel::Stable => t!("update.stable"),
            Channel::Nightly => t!("update.nightly"),
        };
        ui.label(t!(
            "update.current",
            version = env!("BONO_VERSION"),
            channel = channel
        ));
        ui.separator();

        enum Action {
            Check,
            Download,
            Cancel,
            Install,
        }
        let mut action = None;
        match &self.state {
            State::Unsupported => {
                ui.label(t!("update.unsupported"));
            }
            State::Checking(_) => {
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label(t!("update.checking"));
                });
            }
            State::CheckFailed(error) => {
                ui.colored_label(
                    ui.visuals().error_fg_color,
                    t!("update.check_failed", error = error),
                );
            }
            State::UpToDate => {
                ui.label(t!("update.up_to_date"));
            }
            State::Available(release) => {
                ui.label(t!("update.available", version = &release.name));
                let label = t!("update.download", size = size_text(release.size));
                if ui.button(label).clicked() {
                    action = Some(Action::Download);
                }
            }
            State::Downloading(release, download) => {
                ui.label(t!("update.downloading", version = &release.name));
                ui.add(egui::ProgressBar::new(download.progress() as f32).show_percentage());
                if ui.button(t!("cancel")).clicked() {
                    action = Some(Action::Cancel);
                }
            }
            State::Unpacking(release, _) => {
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label(t!("update.unpacking", version = &release.name));
                });
            }
            State::Ready(release) => {
                ui.label(t!("update.ready", version = &release.name));
                ui.label(t!("update.install_note"));
                if ui.button(t!("update.install")).clicked() {
                    action = Some(Action::Install);
                }
            }
            State::Installing(..) | State::Restarting => {
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label(t!("update.installing"));
                });
            }
        }
        if let Some(error) = &self.error {
            ui.colored_label(ui.visuals().error_fg_color, error);
        }
        if matches!(
            self.state,
            State::CheckFailed(_) | State::UpToDate | State::Available(_) | State::Ready(_)
        ) {
            ui.separator();
            if ui.button(t!("update.check")).clicked() {
                action = Some(Action::Check);
            }
        }

        match action {
            Some(Action::Check) => self.check(),
            Some(Action::Download) => self.download(),
            Some(Action::Cancel) => {
                // Dropping the download stops it and removes what it got
                if let State::Downloading(release, _) = &self.state {
                    self.state = State::Available(release.clone());
                }
            }
            Some(Action::Install) => {
                if let (Some(staging), State::Ready(release)) = (&self.staging, &self.state) {
                    let (staging, release) = (staging.clone(), release.clone());
                    self.error = None;
                    let install = release.clone();
                    self.state = State::Installing(
                        release,
                        Task::spawn(
                            async move { staging.install(&install, std::process::id()).await },
                        ),
                    );
                }
            }
            None => {}
        }
    }

    /// At startup, offers a new version to download, once nothing else is in the way (`hold`).
    pub fn popup(&mut self, ctx: &egui::Context, hold: bool) {
        let State::Available(release) = &self.state else {
            self.popup = false;
            return;
        };
        if !self.popup || hold {
            return;
        }
        let modal = egui::Modal::new(egui::Id::new("update available")).show(ctx, |ui| {
            ui.set_max_width(360.0);
            ui.heading(t!("update.popup_title"));
            ui.label(t!(
                "update.popup_body",
                version = &release.name,
                current = env!("BONO_VERSION")
            ));
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                let label = t!("update.download", size = size_text(release.size));
                let download = ui.button(label).clicked();
                let later = ui.button(t!("update.later")).clicked();
                (download, later)
            })
            .inner
        });
        let (download, later) = modal.inner;
        if download {
            self.download();
        }
        // Escape or a click outside puts it off too
        if download || later || modal.should_close() {
            self.popup = false;
        }
    }
}
