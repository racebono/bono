//! The engineer: how it talks on the radio.

use crate::config::Config;
use bono_i18n::t;

#[derive(Default)]
pub struct EngineerTopbar;

impl EngineerTopbar {
    pub fn update(&mut self, ui: &mut egui::Ui, config: &mut Config) {
        config.maybe_update(|raw| {
            raw.corner_names.edit(*raw.corner_names(), |names| {
                ui.checkbox(names, t!("engineer.corner_names"))
                    .on_hover_text(t!("engineer.corner_names_hover"))
                    .changed()
            })
        });
    }
}
