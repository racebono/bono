//! The volumes: of everything the app plays, and of each voice on the radio. And how much it
//! sounds like a radio, and how fast the voices talk.

use crate::{
    config::{Config, Setting},
    synthesizer::{MAX_SPEED, MIN_SPEED},
};
use bono_i18n::t;
use std::ops::RangeInclusive;

#[derive(Default)]
pub struct VolumeTopbar;

impl VolumeTopbar {
    pub fn update(&mut self, ui: &mut egui::Ui, config: &mut Config) {
        config.maybe_update(|raw| {
            let mut changed = volume(
                ui,
                *raw.master_volume(),
                &mut raw.master_volume,
                0.0..=1.0,
                t!("volume.master"),
                t!("volume.master_hover"),
            );
            ui.separator();

            // `|` rather than `||`: every slider is drawn, whichever changed
            changed |= volume(
                ui,
                *raw.engineer_volume(),
                &mut raw.engineer_volume,
                0.0..=1.0,
                t!("volume.engineer"),
                t!("volume.engineer_hover"),
            );
            changed |= volume(
                ui,
                *raw.spotter_volume(),
                &mut raw.spotter_volume,
                0.0..=1.0,
                t!("volume.spotter"),
                t!("volume.spotter_hover"),
            );
            changed |= volume(
                ui,
                *raw.beep_volume(),
                &mut raw.beep_volume,
                0.0..=1.0,
                t!("volume.beep"),
                t!("volume.beep_hover"),
            );
            ui.separator();
            changed |= volume(
                ui,
                *raw.radio_effect(),
                &mut raw.radio_effect,
                0.0..=1.0,
                t!("volume.radio_effect"),
                t!("volume.radio_effect_hover"),
            );
            changed |= volume(
                ui,
                *raw.speech_speed(),
                &mut raw.speech_speed,
                MIN_SPEED..=MAX_SPEED,
                t!("volume.speech_speed"),
                t!("volume.speech_speed_hover"),
            );
            changed
        });
    }
}

/// A percentage within `range` (0 to 1 for a volume or effect), `current` until it's set. Returns
/// whether it changed.
pub fn volume(
    ui: &mut egui::Ui,
    current: f32,
    setting: &mut Setting<f32>,
    range: RangeInclusive<f32>,
    name: &str,
    hover: &str,
) -> bool {
    setting.edit(current, |value| {
        ui.add(
            egui::Slider::new(value, range)
                .text(name)
                .custom_formatter(|value, _| format!("{:.0}%", value * 100.0))
                .custom_parser(|text| {
                    let text = text.trim().trim_end_matches('%').trim();
                    text.parse::<f64>().ok().map(|percent| percent / 100.0)
                }),
        )
        .on_hover_text(hover)
        .changed()
    })
}
