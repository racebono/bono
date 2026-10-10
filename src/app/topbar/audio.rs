use crate::{app::Subsystems, config::Config, task::spawn};
use egui::PopupCloseBehavior;
use tokio::runtime::Runtime;

pub struct AudioTopbar {}

impl AudioTopbar {
    pub fn update(
        &mut self,
        rt: &Runtime,
        ss: &Subsystems,
        ui: &mut egui::Ui,
        config: &mut Config,
    ) {
        todo!()
    }
}

struct DeviceSelector {
    salt: &'static str,
}

impl DeviceSelector {
    fn new() {
        let (send, recv) = tokio::sync::watch::channel(Vec::new());
        let handle = spawn();
    }

    fn update(&mut self, rt: &Runtime, ss: &Subsystems, ui: &mut egui::Ui, config: &mut Config) {
        let mut selected = 0;
        let mut combo = egui::ComboBox::from_id_salt(self.salt)
            .close_behavior(PopupCloseBehavior::CloseOnClickOutside);

        config.maybe_update(|raw| {
            combo.show_ui(ui, |ui| {
                ui.selectable_value(&mut raw.microphone, None, "System default")
                    .clicked();
            });

            todo!()
        });

        todo!()
    }
}
