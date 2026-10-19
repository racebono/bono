//! The spotter: whether it calls at all, and in which language.

use crate::{app::topbar::pick, config::Config, language};
use bono_i18n::t;

#[derive(Default)]
pub struct SpotterTopbar;

impl SpotterTopbar {
    pub fn update(&mut self, ui: &mut egui::Ui, config: &mut Config) {
        config.maybe_update(|raw| {
            let spotter = &mut raw.spotter;
            let mut changed = spotter.enabled.edit(*spotter.enabled(), |enabled| {
                ui.checkbox(enabled, t!("spotter.enabled"))
                    .on_hover_text(t!("spotter.enabled_hover"))
                    .changed()
            });

            // Any language with a voice (the calls are in English without one)
            let mut options = vec![(None, t!("spotter.pilots_language").to_owned())];
            options.extend(
                raw.voices()
                    .keys()
                    .map(|&code| (Some(code), language::ui_name(code))),
            );

            let spotter = &mut raw.spotter;
            ui.add_enabled_ui(*spotter.enabled(), |ui| {
                changed |= pick(
                    ui,
                    t!("language"),
                    *spotter.language(),
                    &mut spotter.language,
                    options,
                    |language| match language {
                        Some(code) => {
                            t!("spotter.no_voice", language = language::ui_name(*code))
                        }
                        None => t!("spotter.pilots_language").into(),
                    },
                )
                .on_hover_text(t!("spotter.language_hover"))
                .changed();
            });
            changed
        });
    }
}
