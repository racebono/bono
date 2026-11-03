//! The conversation with the engineer, and a text box to ask it questions by typing.

use crate::engineer::{Engineer, Speaker};
use bono_i18n::t;
use egui::RichText;

#[derive(Default)]
pub struct Chat {
    /// What's being typed
    draft: String,
}

impl Chat {
    /// Draws the chat in the space left: the conversation, and the text box at the bottom.
    /// Returns a question typed and sent.
    pub fn update(&mut self, ui: &mut egui::Ui, engineer: &Engineer) -> Option<String> {
        let mut sent = None;

        // Before the central panel, which takes the space left
        egui::Panel::bottom("chat input").show(ui, |ui| {
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                let send_width = 60.0;
                let input = ui.add(
                    egui::TextEdit::singleline(&mut self.draft)
                        .hint_text(t!("chat.ask_hint"))
                        .desired_width(ui.available_width() - send_width),
                );
                let entered = input.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
                let has_text = !self.draft.trim().is_empty();
                let clicked = ui
                    .add_enabled(
                        has_text,
                        egui::Button::new(t!("chat.send")).min_size([send_width, 0.0].into()),
                    )
                    .clicked();

                // The box lets go of the keyboard once a question is sent: while it has it, egui
                // takes every key, a talk or session button bound to one included
                if (entered || clicked) && has_text {
                    sent = Some(std::mem::take(&mut self.draft).trim().to_owned());
                    input.surrender_focus();
                }
            });
            ui.add_space(4.0);
        });

        egui::CentralPanel::default().show(ui, |ui| {
            egui::ScrollArea::vertical()
                .auto_shrink(false)
                .stick_to_bottom(true)
                .show(ui, |ui| {
                    if engineer.chat().is_empty() {
                        ui.weak(t!("chat.empty"));
                    }

                    for entry in engineer.chat() {
                        let (name, color) = match entry.speaker {
                            Speaker::Pilot => (t!("chat.pilot"), ui.visuals().hyperlink_color),
                            Speaker::Engineer => (t!("chat.engineer"), ui.visuals().warn_fg_color),
                            Speaker::Notice => {
                                ui.vertical_centered(|ui| {
                                    ui.label(RichText::new(&entry.text).italics().weak());
                                });
                                ui.add_space(4.0);
                                continue;
                            }
                        };

                        egui::Frame::group(ui.style()).show(ui, |ui| {
                            ui.set_width(ui.available_width());
                            ui.label(RichText::new(name).strong().color(color));
                            ui.label(&entry.text);
                            if let Some(detail) = &entry.detail {
                                ui.label(RichText::new(detail).small().weak());
                            }
                        });
                        ui.add_space(4.0);
                    }

                    if engineer.is_thinking() {
                        ui.horizontal(|ui| {
                            ui.spinner();
                            ui.weak(t!("chat.thinking"));
                        });
                    }
                });
        });

        sent
    }
}
