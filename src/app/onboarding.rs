//! The setup guide: shown on the first run (and again once it gains steps), or from Settings. It
//! walks the pilot through what bono needs, a step at a time, and can be skipped at any point.

use crate::{
    app::topbar::{
        audio::{AudioTopbar, SetupItem, SetupState},
        settings,
        simulators::SimulatorsTopbar,
    },
    config::{Config, RawConfig},
    sim::ActiveSimulator,
};
use bono_i18n::t;
use egui::Widget;
use tokio::runtime::Handle;

/// The guide's version: bumped when it gains a step, so pilots who went through it see it again.
pub const VERSION: u32 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Step {
    /// The app's language, and the downloads it needs
    Welcome,
    /// The games, and their plugins
    Simulator,
}

impl Step {
    const ALL: [Step; 2] = [Step::Welcome, Step::Simulator];

    fn index(self) -> usize {
        Self::ALL.iter().position(|&step| step == self).unwrap()
    }

    fn next(self) -> Option<Step> {
        Self::ALL.get(self.index() + 1).copied()
    }

    fn previous(self) -> Option<Step> {
        Some(Self::ALL[self.index().checked_sub(1)?])
    }
}

/// What the pilot clicked at the bottom of the guide.
enum Action {
    Back,
    Next,
    /// Finished, or skipped
    Close,
}

pub struct Onboarding {
    /// The step showing: `None` when the guide isn't
    step: Option<Step>,
    /// Its own, rather than the topbar's: that one looks at the disk again every time its menu
    /// opens
    simulators: SimulatorsTopbar,
}

impl Onboarding {
    pub fn new(config: &RawConfig) -> Self {
        Self {
            step: (*config.onboarding() < VERSION).then_some(Step::Welcome),
            simulators: SimulatorsTopbar::default(),
        }
    }

    /// Shows the guide from its start.
    pub fn open(&mut self) {
        self.step = Some(Step::Welcome);
    }

    pub fn update(
        &mut self,
        rt: &Handle,
        ctx: &egui::Context,
        config: &mut Config,
        audio: &mut AudioTopbar,
        simulators: &mut ActiveSimulator,
    ) {
        let Some(step) = self.step else {
            return;
        };

        let mut action = None;
        egui::Modal::new(egui::Id::new("onboarding")).show(ctx, |ui| {
            ui.set_width(480.0);
            ui.weak(t!(
                "onboarding.step",
                step = step.index() + 1,
                steps = Step::ALL.len()
            ));

            match step {
                Step::Welcome => welcome(rt, ui, config, audio),
                Step::Simulator => self.simulator(rt, ui, config, audio, simulators),
            }

            ui.separator();
            ui.horizontal(|ui| {
                if step.next().is_some() && ui.button(t!("onboarding.skip")).clicked() {
                    action = Some(Action::Close);
                }

                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let (label, forward) = match step.next() {
                        Some(_) => (t!("onboarding.next"), Action::Next),
                        None => (t!("onboarding.finish"), Action::Close),
                    };
                    if ui.button(label).clicked() {
                        action = Some(forward);
                    }
                    if step.previous().is_some() && ui.button(t!("onboarding.back")).clicked() {
                        action = Some(Action::Back);
                    }
                });
            });
        });

        match action {
            Some(Action::Back) => self.step = step.previous(),
            Some(Action::Next) => {
                // The language is picked: what it needs can start downloading
                if step == Step::Welcome {
                    audio.setup(config);
                }
                self.step = step.next();
            }
            Some(Action::Close) => {
                config.update(|raw| raw.onboarding.set(VERSION));
                self.step = None;
                self.simulators.closed();
            }
            None => {}
        }
    }

    fn simulator(
        &mut self,
        rt: &Handle,
        ui: &mut egui::Ui,
        config: &Config,
        audio: &mut AudioTopbar,
        simulators: &mut ActiveSimulator,
    ) {
        ui.heading(t!("onboarding.simulator_title"));
        ui.label(t!("onboarding.simulator_body"));
        ui.add_space(8.0);

        egui::Frame::group(ui.style()).show(ui, |ui| {
            ui.set_width(ui.available_width());
            self.simulators.update(ui, simulators);
        });
        if !self.simulators.any_installed() {
            ui.weak(t!("onboarding.no_simulator"));
        }

        // Still going from the first step
        let items = audio.setup_items(rt, config);
        if !items.is_empty() {
            ui.add_space(8.0);
            ui.strong(t!("onboarding.downloads_title"));
            if downloads(ui, &items) {
                audio.setup(config);
            }
            ui.weak(t!("onboarding.downloads_continue"));
        }
    }
}

fn welcome(rt: &Handle, ui: &mut egui::Ui, config: &mut Config, audio: &mut AudioTopbar) {
    ui.heading(t!("onboarding.welcome_title"));
    ui.label(t!("onboarding.welcome_body"));

    ui.add_space(8.0);
    ui.strong(t!("language"));
    config.maybe_update(|raw| settings::languages(ui, raw));

    let items = audio.setup_items(rt, config);
    if !items.is_empty() {
        ui.add_space(8.0);
        ui.strong(t!("onboarding.downloads_title"));
        ui.label(t!("onboarding.downloads_body"));
        if downloads(ui, &items) {
            audio.setup(config);
        }
    }
}

/// What the guide downloads, and how far along each is. Returns whether those that failed are to
/// be tried again.
fn downloads(ui: &mut egui::Ui, items: &[(&str, SetupItem)]) -> bool {
    egui::Grid::new("onboarding-downloads")
        .num_columns(3)
        .spacing([16.0, 4.0])
        .show(ui, |ui| {
            for (what, item) in items {
                ui.label(*what);
                match item {
                    SetupItem::Loading => {
                        ui.spinner();
                        ui.weak(t!("onboarding.looking_up"));
                    }
                    SetupItem::Unavailable => {
                        ui.colored_label(ui.visuals().warn_fg_color, t!("onboarding.unavailable"));
                    }
                    SetupItem::Found { name, size, state } => {
                        ui.label(name);
                        match state {
                            SetupState::Missing => {
                                if let Some(size) = size {
                                    ui.weak(format!("{:.0} MB", *size as f64 / 1e6));
                                }
                            }
                            SetupState::Downloading(progress) => {
                                egui::ProgressBar::new(*progress as f32)
                                    .show_percentage()
                                    .desired_width(160.0)
                                    .ui(ui);
                            }
                            SetupState::Failed => {
                                ui.colored_label(
                                    ui.visuals().warn_fg_color,
                                    t!("onboarding.failed"),
                                );
                            }
                            SetupState::Ready => {
                                ui.label(format!(
                                    "{} {}",
                                    egui_phosphor::bold::CHECK,
                                    t!("onboarding.ready")
                                ));
                            }
                        }
                    }
                }
                ui.end_row();
            }
        });

    let failed = items.iter().any(|(_, item)| {
        matches!(
            item,
            SetupItem::Found {
                state: SetupState::Failed,
                ..
            }
        )
    });
    failed && ui.button(t!("onboarding.retry")).clicked()
}
