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

        let button = |pre: &str, val: Option<ConfigButton>| match val {
            Some(ConfigButton::Key(key)) => format!("{pre}: Key '{}'", key.name()),
            Some(ConfigButton::Joystick { guid, button }) => {
                format!("{pre}: Button {button} (Device '{}')", guid_to_str(&guid))
            }
            None => format!("{pre}: Disabled"),
        };

        let next_talk = self.talk_button.update(
            rt,
            ss,
            ui,
            button("Change talk button", talk),
            "Press a button",
        );

        let next_session = self.session_button.update(
            rt,
            ss,
            ui,
            button("Change session button", session),
            "Press a button",
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
                ui.add(egui::Slider::new(text_scale, 0.25..=4.0).text("Text scale"))
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
                                .prefix("Record other cars every ")
                                .suffix(" updates"),
                        )
                        .on_hover_text(
                            "Every update of your car is recorded, and one in this many of each \
                             other car's. Fewer keep the recording smaller, more make the \
                             spotter's calls and the engineer's answers about other cars more \
                             precise",
                        );
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
                Err(e) => error("Error", e),
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
