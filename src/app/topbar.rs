use tokio::{runtime::Runtime, task::coop::unconstrained};

use crate::{app::AppState, button::Button, task::JoinHandle};

impl AppState {
    pub fn update_topbar(&mut self, ui: &mut egui::Ui) {
        egui::Panel::top("tobpar").show(ui, |ui| {
            egui::MenuBar::new().ui(ui, |ui| {
                // ui.visuals_mut().button_frame = false;
                ui.menu_button(
                    "Settings",
                    |ui| {
                        if ui.button("Select talk button").clicked() {}
                    },
                );
            });
        });
    }

    fn change_button(&mut self, ui: &mut egui::Ui, text: &str) {
        if ui.button("Select talk button").clicked() {}
    }
}

struct ChangeButton {
    waiting: Option<JoinHandle<Button>>,
}

impl ChangeButton {
    fn update(&mut self, rt: &Runtime, ui: &mut egui::Ui) -> Option<Button> {
        if let Some(ref mut waiting) = self.waiting {
            if waiting.is_finished() {
                let button = rt.block_on(unconstrained(waiting));
                self.waiting = None;
                return Some(button);
            } else {
                todo!()
            }
        }

        todo!()
    }
}
