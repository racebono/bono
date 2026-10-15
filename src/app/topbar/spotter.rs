//! The spotter: whether it calls at all, and in which language.

use crate::{
    app::topbar::pick,
    config::{ArrayStr, Config},
    language,
};

#[derive(Default)]
pub struct SpotterTopbar;

impl SpotterTopbar {
    pub fn update(&mut self, ui: &mut egui::Ui, config: &mut Config) {
        config.maybe_update(|raw| {
            let spotter = &mut raw.spotter;
            let mut changed = spotter.enabled.edit(*spotter.enabled(), |enabled| {
                ui.checkbox(enabled, "Enabled")
                    .on_hover_text(
                        "Calls cars alongside, flags, the car's own alerts and the weather by \
                         itself, the moment they happen",
                    )
                    .changed()
            });

            // Any language with a voice (the calls are in English without one)
            let mut options = vec![(None, "The pilot's".to_owned())];
            options.extend(
                raw.voices()
                    .keys()
                    .map(|&code| (Some(code), language_text(code))),
            );

            let spotter = &mut raw.spotter;
            ui.add_enabled_ui(*spotter.enabled(), |ui| {
                changed |= pick(
                    ui,
                    "Language",
                    *spotter.language(),
                    &mut spotter.language,
                    options,
                    |language| match language {
                        Some(code) => format!("{} (no voice)", language_text(*code)),
                        None => "The pilot's".into(),
                    },
                )
                .on_hover_text(
                    "The language of its calls: the pilot's is the last one they asked the \
                     engineer in. Calls are said in Spanish or English; other languages' voices \
                     say them in English",
                )
                .changed();
            });
            changed
        });
    }
}

fn language_text(code: ArrayStr<2>) -> String {
    language::name(code).unwrap_or_else(|| code.to_string())
}
