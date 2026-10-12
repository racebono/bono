use crate::{
    app::{
        Subsystems,
        topbar::{audio::AudioTopbar, settings::SettingsTopbar},
    },
    config::{Config, RawConfig},
};
use egui::{
    PopupCloseBehavior,
    menu::{MenuButton, MenuConfig},
};
use tokio::runtime::Runtime;

mod audio;
mod settings;

pub struct Topbar {
    settings: SettingsTopbar,
    pub audio: AudioTopbar,
}

impl Topbar {
    pub fn new(ss: &Subsystems, config: &RawConfig) -> Self {
        Self {
            settings: SettingsTopbar::default(),
            audio: AudioTopbar::new(ss, config),
        }
    }

    pub fn update(
        &mut self,
        rt: &Runtime,
        ss: &Subsystems,
        ui: &mut egui::Ui,
        config: &mut Config,
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
                    .ui(ui, |ui| self.audio.update(ss, ui, config));
            });
        });
    }
}
