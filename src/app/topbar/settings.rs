use crate::{
    app::{Subsystems, topbar::setup_label},
    button::ConfigButton,
    config::{Config, RawConfig},
    error, guid_to_str,
    task::LocalTask,
};
use bono_i18n::{locales, t};
use egui::IntoAtoms;
use std::num::NonZeroU32;
use tokio::runtime::Handle;

#[derive(Default)]
pub struct SettingsTopbar {
    talk_button: ChangeButton,
    session_button: ChangeButton,
}

impl SettingsTopbar {
    /// The menu isn't showing: a button still waiting for a press stops waiting, or the next key
    /// pressed anywhere (typing in the chat, say) would be bound once the menu opens again.
    pub fn closed(&mut self) {
        self.talk_button.cancel();
        self.session_button.cancel();
    }

    /// Returns whether the setup guide was asked for.
    pub fn update(
        &mut self,
        rt: &Handle,
        ss: &Subsystems,
        ui: &mut egui::Ui,
        config: &mut Config,
    ) -> bool {
        let (talk, session) = {
            let config = config.get();
            (
                config.talk_button().clone(),
                config.session_button().clone(),
            )
        };

        let button = |pre: &str, val: Option<ConfigButton>| format!("{pre}: {}", button_name(val));

        let next_talk = self.talk_button.update(
            rt,
            ss,
            ui,
            setup_label(
                talk.is_none(),
                button(t!("settings.change_talk"), talk.clone()),
            ),
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
            let name = locales!()
                .into_iter()
                .find(|locale| picked(&current, locale))
                .map_or(current.as_str(), |locale| t!("language_name": locale));

            ui.menu_button(format!("{}: {name}", t!("language")), |ui| {
                languages(ui, raw)
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

        ui.separator();
        let guide = ui.button(t!("settings.setup_guide")).clicked();
        if guide {
            ui.close();
        }

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
        guide
    }
}

/// A button as the pilot reads it: the key, or the joystick's button.
pub fn button_name(button: Option<ConfigButton>) -> String {
    match button {
        Some(ConfigButton::Key(key)) => t!("settings.key", key = key.name()),
        Some(ConfigButton::Joystick { guid, button }) => t!(
            "settings.joystick_button",
            button = button,
            device = guid_to_str(&guid)
        ),
        None => t!("settings.disabled").to_owned(),
    }
}

/// Whether `locale` is the one picked, `current`: `en-US` is English's when there's only `en`.
fn picked(current: &str, locale: &str) -> bool {
    current == locale
        || (!locales!().contains(&current) && current.split('-').next() == Some(locale))
}

/// Every language the app is in, to pick one. Returns whether the pick changed.
pub fn languages(ui: &mut egui::Ui, raw: &mut RawConfig) -> bool {
    let current = raw.locale().clone();
    let mut changed = false;
    for locale in locales!() {
        let option = ui.selectable_label(picked(&current, locale), t!("language_name": locale));
        if option.clicked() && !picked(&current, locale) {
            changed |= raw.set_locale(locale);
        }
    }
    changed
}

/// A button that, clicked, waits for the next button pressed (a key, or a joystick's) to bind it.
#[derive(Default)]
pub struct ChangeButton {
    waiting: Option<LocalTask<anyhow::Result<ConfigButton>>>,
}

impl ChangeButton {
    /// Stops waiting for a press.
    pub fn cancel(&mut self) {
        self.waiting = None;
    }

    /// `pending` until clicked, then `working` until a button is pressed (clicking it again
    /// stops waiting). Returns the button pressed.
    pub fn update<'a>(
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
