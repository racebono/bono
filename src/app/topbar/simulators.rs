//! The simulators bono supports, as they are on this computer: whether they're installed and
//! running, and for those that need a plugin, which version is installed, with a button installing
//! the one that comes with the app.

use crate::{
    app::topbar::setup_label,
    sim::{ActiveSimulator, PluginStatus},
};
use bono_i18n::t;
use std::{
    path::PathBuf,
    time::{Duration, Instant},
};

/// How often it looks at the games' folders again (the pilot may install one, or the plugin,
/// meanwhile).
const REFRESH_EVERY: Duration = Duration::from_secs(2);

#[derive(Default)]
pub struct SimulatorsTopbar {
    /// When it was looked up, and every simulator's, in order. Looked up again once the menu opens
    found: Option<(Instant, Vec<Found>)>,
    /// Whether the menu was showing last frame
    open: bool,
    /// The last plugin installed: its simulator's name, and how it went
    install: Option<(&'static str, Result<(), String>)>,
}

/// What the disk says about a simulator: slow enough not to look every frame.
struct Found {
    install_dir: Option<PathBuf>,
    plugin: Option<PluginStatus>,
}

impl Found {
    /// Whether the game is installed, and its plugin isn't (or is outdated, or disabled).
    fn needs_setup(&self) -> bool {
        let Some(plugin) = self.install_dir.as_ref().and(self.plugin.as_ref()) else {
            return false;
        };
        !plugin.enabled
            || match (plugin.installed, plugin.bundled.flatten()) {
                (None, _) => true,
                (Some(Some(installed)), Some(latest)) => installed < latest,
                (Some(None), Some(_)) => true,
                (Some(_), None) => false,
            }
    }
}

impl SimulatorsTopbar {
    /// Whether an installed game's plugin needs installing, updating or enabling.
    pub fn needs_setup(&mut self, simulators: &mut ActiveSimulator) -> bool {
        self.refresh(simulators);
        self.found
            .as_ref()
            .is_some_and(|(_, found)| found.iter().any(Found::needs_setup))
    }

    /// Whether any of the games is installed, as last looked up.
    pub fn any_installed(&self) -> bool {
        self.found
            .as_ref()
            .is_some_and(|(_, found)| found.iter().any(|found| found.install_dir.is_some()))
    }

    /// Looks at the disk again, if it's been a while.
    fn refresh(&mut self, simulators: &mut ActiveSimulator) {
        if self
            .found
            .as_ref()
            .is_none_or(|(at, _)| at.elapsed() >= REFRESH_EVERY)
        {
            let found = simulators
                .simulators()
                .map(|sim| Found {
                    install_dir: sim.install_dir(),
                    plugin: sim.plugin(),
                })
                .collect();
            self.found = Some((Instant::now(), found));
        }
    }

    pub fn update(&mut self, ui: &mut egui::Ui, simulators: &mut ActiveSimulator) {
        if !self.open {
            self.open = true;
            self.found = None;
        }
        self.refresh(simulators);
        let Some((_, found)) = &self.found else {
            unreachable!()
        };

        let mut install = None;
        for (i, (sim, found)) in simulators.simulators().zip(found).enumerate() {
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
                    found.install_dir.is_some() && !running && plugin.bundled.is_some(),
                    egui::Button::new(setup_label(found.needs_setup(), install_label(plugin))),
                )
                .on_hover_text(t!("plugin.install_hover"))
                .on_disabled_hover_text(if found.install_dir.is_none() {
                    t!("simulators.not_installed_hover")
                } else if running {
                    t!("plugin.running_hover")
                } else {
                    t!("plugin.missing_hover")
                });
            if button.clicked() {
                install = Some(i);
            }

            match &self.install {
                Some((name, Ok(()))) if *name == sim.name() => {
                    ui.label(t!("plugin.install_done"));
                }
                Some((name, Err(e))) if *name == sim.name() => {
                    ui.colored_label(
                        ui.visuals().error_fg_color,
                        t!("plugin.install_failed", error = e),
                    );
                }
                _ => {}
            }
        }

        if let Some(sim) = install.and_then(|i| simulators.simulators().nth(i)) {
            let result = sim.install_plugin().map_err(|e| {
                log::error!("Error installing the {} plugin: {e}", sim.name());
                e.to_string()
            });
            self.install = Some((sim.name(), result));
            // Shows the version just installed
            self.found = None;
        }
    }

    /// Looks at the disk again the next time the menu opens, and forgets the last install.
    pub fn closed(&mut self) {
        self.open = false;
        self.install = None;
    }
}

/// Which version of the plugin is installed, next to the one that comes with the app.
fn plugin_status(ui: &mut egui::Ui, plugin: &PluginStatus) {
    let warn = ui.visuals().warn_fg_color;
    let latest = plugin.bundled.flatten();
    match plugin.installed {
        None => {
            ui.colored_label(warn, t!("plugin.not_installed"));
        }
        Some(None) => {
            ui.colored_label(warn, t!("plugin.unknown_version"));
        }
        Some(Some(version)) => match latest {
            Some(latest) if latest > version => {
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

    if plugin.installed.is_some() && !plugin.enabled {
        ui.colored_label(warn, t!("plugin.disabled"));
    }
}

/// Install, update (the app's is newer, or the installed one doesn't say), or reinstall.
fn install_label(plugin: &PluginStatus) -> &'static str {
    match (plugin.installed, plugin.bundled.flatten()) {
        (None, _) => t!("plugin.install"),
        (Some(Some(installed)), Some(latest)) if installed >= latest => t!("plugin.reinstall"),
        (Some(_), Some(_)) => t!("plugin.update"),
        (Some(_), None) => t!("plugin.reinstall"),
    }
}
