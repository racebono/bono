//! The simulators bono supports, as they are on this computer: whether they're installed and
//! running, and for those that need a plugin, which version is installed, with a button installing
//! the one that comes with the app.

use crate::{
    app::topbar::setup_label,
    sim::{ActiveSimulator, PluginStatus},
    task::MaybeDone,
};
use bono_i18n::t;
use std::{
    path::PathBuf,
    time::{Duration, Instant},
};
use tokio::runtime::Handle;

/// How often it looks at the games' folders again (the pilot may install one, or the plugin,
/// meanwhile).
const REFRESH_EVERY: Duration = Duration::from_secs(2);

#[derive(Default)]
pub struct SimulatorsTopbar {
    /// Every simulator's last known status, retained while a lookup runs.
    found: Vec<Found>,
    /// When the last lookup started; `None` requests a fresh lookup.
    looked: Option<Instant>,
    /// Whether the menu was showing last frame
    open: bool,
    /// The last plugin install: its simulator's name, progress, and result.
    install: Option<(&'static str, MaybeDone<anyhow::Result<()>>)>,
}

/// What the disk says about a simulator: slow enough not to look every frame.
#[derive(Default)]
struct Found {
    install_dir: Option<PathBuf>,
    plugin: Option<PluginStatus>,
    lookup: Option<MaybeDone<PluginStatus>>,
}

impl Found {
    /// Whether the game is installed, and its plugin isn't (or is outdated, disabled, or not the
    /// code its version should be).
    fn needs_setup(&self) -> bool {
        let Some(plugin) = self.install_dir.as_ref().and(self.plugin.as_ref()) else {
            return false;
        };
        !plugin.enabled
            || plugin.unexpected_code()
            || match (&plugin.installed, plugin.bundled_version()) {
                (None, _) => true,
                (Some(Some(installed)), Some(latest)) => latest.replaces(installed),
                (Some(None), Some(_)) => true,
                (Some(_), None) => false,
            }
    }
}

impl SimulatorsTopbar {
    /// Whether an installed game's plugin needs installing, updating or enabling.
    pub fn needs_setup(&mut self, rt: &Handle, simulators: &mut ActiveSimulator) -> bool {
        self.refresh(rt, simulators);
        self.found.iter().any(Found::needs_setup)
    }

    /// Whether any of the games is installed, as last looked up.
    pub fn any_installed(&self) -> bool {
        self.found.iter().any(|found| found.install_dir.is_some())
    }

    /// Looks at the disk again, if it's been a while.
    fn refresh(&mut self, rt: &Handle, simulators: &mut ActiveSimulator) {
        if let Some((_, task)) = &mut self.install
            && matches!(task, MaybeDone::Task(_))
            && task.update(rt).is_some()
        {
            // Discard lookups started before the install completed.
            for found in &mut self.found {
                found.lookup = None;
            }
            self.looked = None;
        }
        if self.looked.is_none_or(|at| at.elapsed() >= REFRESH_EVERY) {
            self.found
                .resize_with(simulators.simulators().count(), Found::default);
            for (sim, found) in simulators.simulators().zip(&mut self.found) {
                found.install_dir = sim.install_dir();
                if found.lookup.is_none() {
                    found.lookup = sim.plugin();
                }
            }
            self.looked = Some(Instant::now());
        }
        for found in &mut self.found {
            if found
                .lookup
                .as_mut()
                .is_some_and(|lookup| lookup.update(rt).is_some())
                && let Some(MaybeDone::Done(plugin)) = found.lookup.take()
            {
                found.plugin = Some(plugin);
            }
        }
    }

    pub fn update(&mut self, rt: &Handle, ui: &mut egui::Ui, simulators: &mut ActiveSimulator) {
        if !self.open {
            self.open = true;
            self.looked = None;
        }
        self.refresh(rt, simulators);
        let installing = matches!(self.install, Some((_, MaybeDone::Task(_))));

        let mut install = None;
        for (i, (sim, found)) in simulators.simulators().zip(&self.found).enumerate() {
            if i > 0 {
                ui.separator();
            }

            let running = sim.is_running();
            ui.horizontal(|ui| {
                ui.strong(sim.name());
                match &found.install_dir {
                    _ if running => {
                        ui.colored_label(ui.visuals().selection.bg_fill, t!("simulators.running"));
                    }
                    Some(dir) => {
                        ui.label(t!("simulators.installed")).on_hover_text(t!(
                            "simulators.install_dir_hover",
                            path = dir.display()
                        ));
                    }
                    None => {
                        ui.weak(t!("simulators.not_installed"))
                            .on_hover_text(t!("simulators.not_installed_hover"));
                    }
                }
            });

            let Some(plugin) = &found.plugin else {
                continue;
            };
            plugin_status(ui, plugin);

            let button = ui
                .add_enabled(
                    found.install_dir.is_some()
                        && !running
                        && !installing
                        && plugin.bundled.is_some(),
                    egui::Button::new(setup_label(found.needs_setup(), install_label(plugin))),
                )
                .on_hover_text(t!("plugin.install_hover"))
                .on_disabled_hover_text(if found.install_dir.is_none() {
                    t!("simulators.not_installed_hover")
                } else if running {
                    t!("plugin.running_hover")
                } else if installing {
                    t!("plugin.installing")
                } else {
                    t!("plugin.missing_hover")
                });
            if button.clicked() {
                install = Some(i);
            }

            match &self.install {
                Some((name, MaybeDone::Task(_))) if *name == sim.name() => {
                    ui.horizontal(|ui| {
                        ui.spinner();
                        ui.label(t!("plugin.installing"));
                    });
                }
                Some((name, MaybeDone::Done(Ok(())))) if *name == sim.name() => {
                    ui.label(t!("plugin.install_done"));
                }
                Some((name, MaybeDone::Done(Err(e)))) if *name == sim.name() => {
                    ui.colored_label(
                        ui.visuals().error_fg_color,
                        t!("plugin.install_failed", error = e),
                    );
                }
                _ => {}
            }
        }

        if let Some(sim) = install.and_then(|i| simulators.simulators().nth(i)) {
            self.install = Some((sim.name(), sim.install_plugin()));
        }
    }

    /// Looks again next time the menu opens, and forgets completed installs. Keeps a running
    /// install alive so closing the menu doesn't cancel it halfway through.
    pub fn closed(&mut self) {
        self.open = false;
        if matches!(self.install, Some((_, MaybeDone::Done(_)))) {
            self.install = None;
        }
    }
}

/// Which version of the plugin is installed, next to the one that comes with the app.
fn plugin_status(ui: &mut egui::Ui, plugin: &PluginStatus) {
    let warn = ui.visuals().warn_fg_color;
    let latest = plugin.bundled_version();
    match &plugin.installed {
        None => {
            ui.colored_label(warn, t!("plugin.not_installed"));
        }
        Some(None) => {
            ui.colored_label(warn, t!("plugin.unknown_version"));
        }
        Some(Some(version)) => match latest {
            Some(latest) if latest.replaces(version) => {
                ui.colored_label(
                    warn,
                    t!("plugin.outdated", version = version, latest = latest),
                );
            }
            Some(latest) if latest == version => {
                ui.label(t!("plugin.up_to_date", version = version));
            }
            _ => {
                ui.label(t!("plugin.installed", version = version));
            }
        },
    }

    if plugin.unexpected_code() {
        ui.colored_label(warn, t!("plugin.unexpected_code"))
            .on_hover_text(t!("plugin.unexpected_code_hover"));
    }
    if plugin.installed.is_some() && !plugin.enabled {
        ui.colored_label(warn, t!("plugin.disabled"));
    }
}

/// Install, update (the app's is newer or another build, or the installed one doesn't say), or
/// reinstall.
fn install_label(plugin: &PluginStatus) -> &'static str {
    match (&plugin.installed, plugin.bundled_version()) {
        (None, _) => t!("plugin.install"),
        (Some(Some(installed)), Some(latest)) if !latest.replaces(installed) => {
            t!("plugin.reinstall")
        }
        (Some(_), Some(_)) => t!("plugin.update"),
        (Some(_), None) => t!("plugin.reinstall"),
    }
}
