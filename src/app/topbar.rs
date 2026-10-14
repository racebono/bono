use std::sync::Arc;

use crate::{
    app::{
        Subsystems,
        topbar::{audio::AudioTopbar, settings::SettingsTopbar, volume::VolumeTopbar},
    },
    config::{Config, RawConfig},
    sim::Sim,
    synthesizer::Synthesizer,
};
use camino::Utf8Path;
use egui::{
    PopupCloseBehavior,
    menu::{MenuButton, MenuConfig},
};
use sdl3::event::Event;
use tokio::runtime::Handle;

mod audio;
mod settings;
mod volume;

pub struct Topbar {
    settings: SettingsTopbar,
    pub audio: AudioTopbar,
    volume: VolumeTopbar,
    /// "Clear data" was clicked: asking before clearing
    confirm_clear: bool,
}

impl Topbar {
    pub fn new(
        pref_path: &Arc<Utf8Path>,
        ss: &Subsystems,
        config: &RawConfig,
        client: &reqwest::Client,
    ) -> Self {
        Self {
            settings: SettingsTopbar::default(),
            volume: VolumeTopbar,
            confirm_clear: false,
            audio: AudioTopbar::new(pref_path, ss, config, client),
        }
    }

    pub fn event(&mut self, rt: &Handle, config: &Config, event: &Event) -> bool {
        if self.audio.event(rt, config, event) {
            return true;
        }

        return false;
    }

    pub fn update(
        &mut self,
        rt: &Handle,
        ss: &Subsystems,
        ui: &mut egui::Ui,
        config: &mut Config,
        synth: &mut Synthesizer,
        simulator: Option<&mut Sim>,
    ) {
        egui::Panel::top("tobpar").show(ui, |ui| {
            egui::MenuBar::new().ui(ui, |ui| {
                MenuButton::new("Settings")
                    .config(
                        MenuConfig::new().close_behavior(PopupCloseBehavior::CloseOnClickOutside),
                    )
                    .ui(ui, |ui| self.settings.update(rt, ss, ui, config));

                MenuButton::new("Audio")
                    .config(
                        MenuConfig::new().close_behavior(PopupCloseBehavior::CloseOnClickOutside),
                    )
                    .ui(ui, |ui| self.audio.update(rt, ss, ui, config, synth));

                MenuButton::new("Volume")
                    .config(
                        MenuConfig::new().close_behavior(PopupCloseBehavior::CloseOnClickOutside),
                    )
                    .ui(ui, |ui| self.volume.update(ui, config));

                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let clear = ui
                        .add_enabled(simulator.is_some(), egui::Button::new("Clear data"))
                        .on_hover_text("Delete everything recorded from the simulator so far")
                        .on_disabled_hover_text("No simulator is running");

                    if clear.clicked() {
                        self.confirm_clear = true;
                    }

                    ui.label(
                        simulator
                            .as_deref()
                            .map_or("No simulator running", |simulator| simulator.name()),
                    );
                });
            });
        });

        if self.confirm_clear {
            match simulator {
                Some(simulator) => self.confirm_clear = confirm_clear(ui.ctx(), simulator),
                // It stopped meanwhile: nothing to clear
                None => self.confirm_clear = false,
            }
        }
    }
}

/// Asks before clearing the simulator's recorded data, and clears it if confirmed. Returns whether
/// the dialog stays open.
fn confirm_clear(ctx: &egui::Context, simulator: &mut Sim) -> bool {
    let name = simulator.name();
    let modal = egui::Modal::new(egui::Id::new("confirm clear")).show(ctx, |ui| {
        ui.heading("Clear recorded data?");
        ui.label(format!(
            "Everything recorded from {name} so far will be deleted. This can't be undone."
        ));
        ui.add_space(8.0);

        ui.horizontal(|ui| {
            let clear = ui.button("Clear").clicked();
            let cancel = ui.button("Cancel").clicked();
            (clear, cancel)
        })
        .inner
    });

    let (clear, cancel) = modal.inner;
    if clear {
        // The simulator's plugin clears it within a few seconds
        simulator.clear();
    }

    // Escape or a click outside cancels too
    !(clear || cancel || modal.should_close())
}
