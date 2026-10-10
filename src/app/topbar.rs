use crate::{
    app::{Subsystems, topbar::settings::SettingsTopbar},
    config::Config,
};
use egui::{
    PopupCloseBehavior,
    menu::{MenuButton, MenuConfig},
};
use tokio::runtime::Runtime;

mod audio;
mod settings;

#[derive(Default)]
pub(super) struct Topbar {
    settings: SettingsTopbar,
}

impl Topbar {
    pub fn update(
        &mut self,
        rt: &Runtime,
        ss: &Subsystems,
        ui: &mut egui::Ui,
        config: &mut Config,
    ) {
        egui::Panel::top("tobpar").show(ui, |ui| {
            egui::MenuBar::new().ui(ui, |ui| {
                // ui.visuals_mut().button_frame = false;
                MenuButton::new("Settings")
                    .config(
                        MenuConfig::new().close_behavior(PopupCloseBehavior::CloseOnClickOutside),
                    )
                    .ui(ui, |ui| self.settings.update(rt, ss, ui, config));
            });
        });
    }
}
