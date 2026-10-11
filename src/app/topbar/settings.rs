use crate::{app::Subsystems, button::Button, config::Config, error, guid_to_str, task::Task};
use egui::IntoAtoms;
use tokio::runtime::Runtime;

#[derive(Default)]
pub struct SettingsTopbar {
    talk_button: ChangeButton,
    session_button: ChangeButton,
}

impl SettingsTopbar {
    pub fn update(
        &mut self,
        rt: &Runtime,
        ss: &Subsystems,
        ui: &mut egui::Ui,
        config: &mut Config,
    ) {
        let (talk, session) = {
            let config = config.get();
            (config.talk_button.clone(), config.session_button.clone())
        };

        let button = |pre: &str, val: Option<Button>| match val {
            Some(Button::Key(key)) => format!("{pre}: Key '{}'", key.name()),
            Some(Button::Joystick { guid, button }) => {
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
            ui.add(egui::Slider::new(&mut raw.text_scale, 0.25..=4.0).text("Text scale"))
                .changed()
        });

        if next_talk.is_some() || next_session.is_some() {
            config.update(|raw| {
                if let Some(talk) = next_talk {
                    raw.talk_button = Some(talk);
                }
                if let Some(session) = next_session {
                    raw.session_button = Some(session);
                }
            });
        }
    }
}

#[derive(Default)]
struct ChangeButton {
    waiting: Option<Task<anyhow::Result<Button>>>,
}

impl ChangeButton {
    fn update<'a>(
        &mut self,
        rt: &Runtime,
        ss: &Subsystems,
        ui: &mut egui::Ui,
        pending: impl IntoAtoms<'a>,
        working: impl IntoAtoms<'a>,
    ) -> Option<Button> {
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
            self.waiting = Some(Task::spawn(ss.next_button()));
        }

        return None;
    }
}
