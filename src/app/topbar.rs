use crate::{
    app::{
        Subsystems,
        topbar::{
            audio::AudioTopbar, provider::ProviderTopbar, settings::SettingsTopbar,
            simulators::SimulatorsTopbar, spotter::SpotterTopbar, volume::VolumeTopbar,
        },
    },
    config::{Config, RawConfig, Setting},
    sim::{ActiveSimulator, Sim},
    synthesizer::Synthesizer,
};
use bono_i18n::t;
use camino::Utf8Path;
use egui::{
    Atoms, IntoAtoms, PopupCloseBehavior,
    menu::{MenuButton, MenuConfig},
};
use sdl3::event::Event;
use std::sync::Arc;
use tokio::runtime::Handle;

pub(super) mod audio;
mod provider;
pub(super) mod settings;
pub(super) mod simulators;
mod spotter;
mod volume;

pub struct Topbar {
    settings: SettingsTopbar,
    pub audio: AudioTopbar,
    volume: VolumeTopbar,
    provider: ProviderTopbar,
    spotter: SpotterTopbar,
    simulators: SimulatorsTopbar,
    /// "Clear data" was clicked: asking before clearing
    confirm_clear: bool,
}

impl Topbar {
    pub fn new(
        pref_path: &Arc<Utf8Path>,
        ss: &Subsystems,
        config: &RawConfig,
        client: &reqwest::Client,
    ) -> Self {
        Self {
            settings: SettingsTopbar::default(),
            volume: VolumeTopbar,
            provider: ProviderTopbar::new(client),
            spotter: SpotterTopbar,
            simulators: SimulatorsTopbar::default(),
            confirm_clear: false,
            audio: AudioTopbar::new(pref_path, ss, config, client),
        }
    }

    pub fn event(&mut self, rt: &Handle, config: &Config, event: &Event) -> bool {
        if self.audio.event(rt, config, event) {
            return true;
        }
        return false;
    }

    /// Returns whether the setup guide was asked for.
    pub fn update(
        &mut self,
        rt: &Handle,
        ss: &Subsystems,
        ui: &mut egui::Ui,
        config: &mut Config,
        synth: &mut Synthesizer,
        simulators: &mut ActiveSimulator,
    ) -> bool {
        self.audio.poll(rt, config, synth);

        // Every menu with something in it that isn't set up yet
        let settings_setup = config.get().talk_button().is_none();
        let audio_setup = self.audio.needs_setup(config);
        let provider_setup = provider::needs_setup(config);
        let simulators_setup = self.simulators.needs_setup(simulators);

        let mut guide = false;
        egui::Panel::top("tobpar").show(ui, |ui| {
            egui::MenuBar::new().ui(ui, |ui| {
                let (_, settings) = MenuButton::new(setup_label(settings_setup, t!("settings")))
                    .config(
                        MenuConfig::new().close_behavior(PopupCloseBehavior::CloseOnClickOutside),
                    )
                    .ui(ui, |ui| self.settings.update(rt, ss, ui, config));
                guide = settings.as_ref().is_some_and(|settings| settings.inner);
                if settings.is_none() {
                    self.settings.closed();
                }

                MenuButton::new(setup_label(audio_setup, t!("audio")))
                    .config(
                        MenuConfig::new().close_behavior(PopupCloseBehavior::CloseOnClickOutside),
                    )
                    .ui(ui, |ui| self.audio.update(rt, ss, ui, config, synth));

                MenuButton::new(t!("volume"))
                    .config(
                        MenuConfig::new().close_behavior(PopupCloseBehavior::CloseOnClickOutside),
                    )
                    .ui(ui, |ui| self.volume.update(ui, config));

                MenuButton::new(setup_label(provider_setup, t!("provider")))
                    .config(
                        MenuConfig::new().close_behavior(PopupCloseBehavior::CloseOnClickOutside),
                    )
                    .ui(ui, |ui| self.provider.update(rt, ui, config));

                MenuButton::new(t!("spotter"))
                    .config(
                        MenuConfig::new().close_behavior(PopupCloseBehavior::CloseOnClickOutside),
                    )
                    .ui(ui, |ui| self.spotter.update(ui, config));

                let (_, sims) = MenuButton::new(setup_label(simulators_setup, t!("simulators")))
                    .config(
                        MenuConfig::new().close_behavior(PopupCloseBehavior::CloseOnClickOutside),
                    )
                    .ui(ui, |ui| self.simulators.update(ui, simulators));
                if sims.is_none() {
                    self.simulators.closed();
                }

                let simulator = simulators.current();
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let clear = ui
                        .add_enabled(
                            simulator.is_some(),
                            egui::Button::new(t!("topbar.clear_data")),
                        )
                        .on_hover_text(t!("topbar.clear_data_hover"))
                        .on_disabled_hover_text(t!("topbar.no_simulator_hover"));

                    if clear.clicked() {
                        self.confirm_clear = true;
                    }

                    ui.label(
                        simulator.map_or(t!("topbar.no_simulator"), |simulator| simulator.name()),
                    );
                });
            });
        });

        if self.confirm_clear {
            match simulators.current_mut() {
                Some(simulator) => self.confirm_clear = confirm_clear(ui.ctx(), simulator),
                // It stopped meanwhile: nothing to clear
                None => self.confirm_clear = false,
            }
        }
        guide
    }
}

/// The warning sign's color
const SETUP_COLOR: egui::Color32 = egui::Color32::from_rgb(240, 190, 0);

/// `text`, after a yellow warning sign when what it names isn't set up yet (for a menu, something
/// in it).
fn setup_label<'a>(missing: bool, text: impl IntoAtoms<'a>) -> Atoms<'a> {
    let mut atoms = text.into_atoms();
    if missing {
        atoms.push_left(egui::RichText::new(egui_phosphor::bold::WARNING).color(SETUP_COLOR));
    }
    atoms
}

/// A sub-menu picking one of `options` (a `ComboBox` would close the menu it's in), with
/// `other` naming a value that isn't one of them (set in the config file). Returns the sub-menu's
/// button, changed if the pick did.
fn pick<T, S>(
    ui: &mut egui::Ui,
    label: &str,
    current: T,
    setting: &mut Setting<T>,
    options: impl IntoIterator<Item = (T, S)>,
    other: impl FnOnce(&T) -> String,
) -> egui::Response
where
    T: Clone + PartialEq,
    S: Into<String>,
{
    let options = options
        .into_iter()
        .map(|(value, text)| (value, text.into()))
        .collect::<Vec<(T, String)>>();
    let text = match options.iter().find(|(value, _)| *value == current) {
        Some((_, text)) => text.clone(),
        None => other(&current),
    };

    let mut response = None;
    setting.edit(current, |value| {
        let before = value.clone();
        let menu = ui.menu_button(format!("{label}: {text}"), |ui| {
            for (option, text) in options {
                ui.selectable_value(value, option, text);
            }
        });
        let changed = *value != before;
        let mut button = menu.response;
        if changed {
            button.mark_changed();
        }
        response = Some(button);
        changed
    });
    response.unwrap()
}

/// Asks before clearing the simulator's recorded data, and clears it if confirmed. Returns whether
/// the dialog stays open.
fn confirm_clear(ctx: &egui::Context, simulator: &mut Sim) -> bool {
    let name = simulator.name();
    let modal = egui::Modal::new(egui::Id::new("confirm clear")).show(ctx, |ui| {
        ui.heading(t!("clear.title"));
        ui.label(t!("clear.body", name = name));
        ui.add_space(8.0);

        ui.horizontal(|ui| {
            let clear = ui.button(t!("clear.confirm")).clicked();
            let cancel = ui.button(t!("cancel")).clicked();
            (clear, cancel)
        })
        .inner
    });

    let (clear, cancel) = modal.inner;
    if clear {
        // The simulator's plugin clears it within a few seconds
        simulator.clear();
    }

    // Escape or a click outside cancels too
    !(clear || cancel || modal.should_close())
}
