//! The spotter: whether it calls at all (and in qualifying), in which language, whether it says
//! lap times in qualifying (even when otherwise quiet in it), and whether it calls race starts.

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
                changed |= spotter
                    .qualifying
                    .edit(*spotter.qualifying(), |qualifying| {
                        ui.checkbox(qualifying, t!("spotter.qualifying"))
                            .on_hover_text(t!("spotter.qualifying_hover"))
                            .changed()
                    });
                changed |= spotter
                    .race_starts
                    .edit(*spotter.race_starts(), |race_starts| {
                        ui.checkbox(race_starts, t!("spotter.race_starts"))
                            .on_hover_text(t!("spotter.race_starts_hover"))
                            .changed()
                    });
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

                // Said even when it's otherwise quiet in qualifying
                changed |=
                    spotter
                        .qualifying_lap_times
                        .edit(*spotter.qualifying_lap_times(), |enabled| {
                            ui.checkbox(enabled, t!("spotter.qualifying_lap_times"))
                                .on_hover_text(t!("spotter.qualifying_lap_times_hover"))
                                .changed()
                        });
            });
            changed
        });
    }
}
