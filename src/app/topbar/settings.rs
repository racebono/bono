use crate::{
    app::Subsystems, button::ConfigButton, config::Config, error, guid_to_str, task::LocalTask,
};
use egui::IntoAtoms;
use lambiase_i18n::{locales, t};
use std::num::NonZeroU32;
use tokio::runtime::Handle;

#[derive(Default)]
pub struct SettingsTopbar {
    talk_button: ChangeButton,
    session_button: ChangeButton,
}

impl SettingsTopbar {
    pub fn update(&mut self, rt: &Handle, ss: &Subsystems, ui: &mut egui::Ui, config: &mut Config) {
        let (talk, session) = {
            let config = config.get();
            (
                config.talk_button().clone(),
                config.session_button().clone(),
            )
        };

        let button = |pre: &str, val: Option<ConfigButton>| {
            let value = match val {
                Some(ConfigButton::Key(key)) => t!("settings.key", key = key.name()),
                Some(ConfigButton::Joystick { guid, button }) => t!(
                    "settings.joystick_button",
                    button = button,
                    device = guid_to_str(&guid)
                ),
                None => t!("settings.disabled").to_owned(),
            };
            format!("{pre}: {value}")
        };

        let next_talk = self.talk_button.update(
            rt,
            ss,
            ui,
            button(t!("settings.change_talk"), talk),
            t!("settings.press_button"),
        );

        let next_session = self.session_button.update(
            rt,
            ss,
            ui,
            button(t!("settings.change_session"), session),
            t!("settings.press_button"),
        );

        ui.separator();
        config.maybe_update(|raw| {
            let current = raw.locale().clone();
            let locales = locales!();
            // `en-US` is English's when there's only `en`
            let picked = |locale: &str| {
                current == locale
                    || (!locales.contains(&current.as_str())
                        && current.split('-').next() == Some(locale))
            };
            let name = locales
                .iter()
                .find(|locale| picked(locale))
                .map_or(current.as_str(), |locale| t!("language_name": locale));

            ui.menu_button(format!("{}: {name}", t!("language")), |ui| {
                let mut changed = false;
                for locale in locales {
                    let option = ui.selectable_label(picked(locale), t!("language_name": locale));
                    if option.clicked() && !picked(locale) {
                        changed |= raw.set_locale(locale);
                    }
                }
                changed
            })
            .inner
            .unwrap_or(false)
        });

        config.maybe_update(|raw| {
            raw.text_scale.edit(*raw.text_scale(), |text_scale| {
                ui.add(egui::Slider::new(text_scale, 0.25..=4.0).text(t!("settings.text_scale")))
                    .changed()
            })
        });

        ui.separator();
        config.maybe_update(|raw| {
            raw.opponent_frequency
                .edit(*raw.opponent_frequency(), |frequency| {
                    let mut updates = frequency.get();
                    let field = ui
                        .add(
                            egui::DragValue::new(&mut updates)
                                .range(1..=60)
                                .prefix(t!("settings.opponents_prefix"))
                                .suffix(t!("settings.opponents_suffix")),
                        )
                        .on_hover_text(t!("settings.opponents_hover"));
                    let changed = field.changed() && updates != frequency.get();
                    if let Some(updates) = NonZeroU32::new(updates) {
                        *frequency = updates;
                    }
                    changed
                })
        });

        if next_talk.is_some() || next_session.is_some() {
            config.update(|raw| {
                if let Some(talk) = next_talk {
                    raw.talk_button.set(Some(talk));
                }
                if let Some(session) = next_session {
                    raw.session_button.set(Some(session));
                }
            });
        }
    }
}

#[derive(Default)]
struct ChangeButton {
    waiting: Option<LocalTask<anyhow::Result<ConfigButton>>>,
}

impl ChangeButton {
    fn update<'a>(
        &mut self,
        rt: &Handle,
        ss: &Subsystems,
        ui: &mut egui::Ui,
        pending: impl IntoAtoms<'a>,
        working: impl IntoAtoms<'a>,
    ) -> Option<ConfigButton> {
        if let Some(ref mut waiting) = self.waiting {
            if ui.button(working).clicked() {
                self.waiting = None;
                return None;
            }

            let result = waiting.update(rt)?;
            self.waiting = None;
            match result {
                Ok(button) => return Some(button),
                Err(e) => error(t!("error.title"), e),
            }
        }

        if ui.button(pending).clicked() {
            let ss = ss.clone();
            self.waiting = Some(LocalTask::spawn_local(async move {
                Ok(ss.next_button().await?.to_config(&ss.joystick)?)
            }));
        }

        return None;
    }
}
