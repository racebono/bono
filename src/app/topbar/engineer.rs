//! The engineer: how it talks on the radio.

use crate::{
    app::topbar::pick,
    config::{ArrayStr, Config},
    language,
};
use bono_i18n::t;

#[derive(Default)]
pub struct EngineerTopbar;

impl EngineerTopbar {
    pub fn update(&mut self, ui: &mut egui::Ui, config: &mut Config) {
        config.maybe_update(|raw| {
            let mut spoken = vec![(None, t!("engineer.pilots_language").to_owned())];
            spoken.extend(
                raw.voices()
                    .keys()
                    .map(|&code| (Some(code), language::ui_name(code))),
            );
            let mut changed = pick(
                ui,
                t!("engineer.spoken_language"),
                *raw.spoken_language(),
                &mut raw.spoken_language,
                spoken,
                |lang| match lang {
                    Some(code) => t!("spotter.no_voice", language = language::ui_name(*code)),
                    None => t!("engineer.pilots_language").into(),
                },
            )
            .on_hover_text(t!("engineer.spoken_language_hover"))
            .changed();

            let mut languages = (0..=whisper_rs::get_lang_max_id())
                .filter_map(|id| {
                    let code = ArrayStr::from_str(whisper_rs::get_lang_str(id)?)?;
                    let localized = language::ui_name(code);
                    let name = if localized == &*code {
                        let name = whisper_rs::get_lang_str_full(id).unwrap_or(&code);
                        let mut chars = name.chars();
                        chars
                            .next()
                            .map(|first| first.to_uppercase().collect::<String>() + chars.as_str())
                            .unwrap_or_default()
                    } else {
                        localized
                    };
                    Some((Some(code), name))
                })
                .collect::<Vec<_>>();
            languages.sort_by(|a, b| a.1.cmp(&b.1));
            languages.insert(0, (None, t!("engineer.detect_language").to_owned()));
            changed |= pick(
                ui,
                t!("engineer.heard_language"),
                *raw.heard_language(),
                &mut raw.heard_language,
                languages,
                |lang| {
                    lang.map(language::ui_name)
                        .unwrap_or_else(|| t!("engineer.detect_language").into())
                },
            )
            .on_hover_text(t!("engineer.heard_language_hover"))
            .changed();
            changed
        });
        config.maybe_update(|raw| {
            raw.corner_names.edit(*raw.corner_names(), |names| {
                ui.checkbox(names, t!("engineer.corner_names"))
                    .on_hover_text(t!("engineer.corner_names_hover"))
                    .changed()
            })
        });
    }
}
