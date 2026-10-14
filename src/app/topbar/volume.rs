//! The volumes: of everything the app plays, and of each voice on the radio.

use crate::config::Config;

#[derive(Default)]
pub struct VolumeTopbar;

impl VolumeTopbar {
    pub fn update(&mut self, ui: &mut egui::Ui, config: &mut Config) {
        config.maybe_update(|raw| {
            let mut changed = volume(
                ui,
                &mut raw.master_volume,
                "Master",
                "Everything the app plays",
            );
            ui.separator();

            // `|` rather than `||`: every slider is drawn, whichever changed
            changed |= volume(
                ui,
                &mut raw.engineer_volume,
                "Engineer",
                "The engineer's answers on the radio",
            );
            changed |= volume(
                ui,
                &mut raw.spotter_volume,
                "Spotter",
                "The automatic spotter's calls (the spotter isn't built yet)",
            );
            changed |= volume(
                ui,
                &mut raw.beep_volume,
                "Radio beep",
                "The roger beep that ends each transmission, from the next one on",
            );
            changed
        });
    }
}

/// A volume from 0 to 100%. Returns whether it changed.
fn volume(ui: &mut egui::Ui, value: &mut f32, name: &str, hover: &str) -> bool {
    ui.add(
        egui::Slider::new(value, 0.0..=1.0)
            .text(name)
            .custom_formatter(|value, _| format!("{:.0}%", value * 100.0))
            .custom_parser(|text| {
                let text = text.trim().trim_end_matches('%').trim();
                text.parse::<f64>().ok().map(|percent| percent / 100.0)
            }),
    )
    .on_hover_text(hover)
    .changed()
}
