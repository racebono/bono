use std::sync::Arc;

use crate::{
    app::{
        Subsystems,
        topbar::{audio::AudioTopbar, settings::SettingsTopbar},
    },
    config::{Config, RawConfig},
    sim::Simulator,
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

pub struct Topbar {
    settings: SettingsTopbar,
    pub audio: AudioTopbar,
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
        simulator: Option<&mut dyn Simulator>,
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

                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let clear = ui
                        .add_enabled(simulator.is_some(), egui::Button::new("Clear data"))
                        .on_hover_text("Delete everything recorded from the simulator so far")
                        .on_disabled_hover_text("No simulator is running");

                    match simulator {
                        Some(simulator) => {
                            ui.label(simulator.name());
                            // The simulator's plugin clears it within a few seconds
                            if clear.clicked() {
                                simulator.clear();
                            }
                        }
                        None => {
                            ui.label("No simulator running");
                        }
                    }
                });
            });
        });
    }
}
