//! The setup guide: shown on the first run (and again once it gains steps), or from Settings. It
//! walks the pilot through what bono needs, a step at a time, and can be skipped at any point.

use crate::{
    account::Account,
    app::{
        Subsystems,
        topbar::{
            Topbar,
            audio::{AudioTopbar, SetupItem, SetupState, app_language},
            settings::{self, ChangeButton, button_name},
            setup_label,
            simulators::SimulatorsTopbar,
            volume::volume,
        },
    },
    config::{Config, RawConfig},
    engineer::{Engineer, Speaker},
    sim::ActiveSimulator,
    synthesizer::Synthesizer,
};
use bono_i18n::t;
use egui::{RichText, Widget};
use tokio::runtime::Handle;

/// The guide's version: bumped when it gains a step, so pilots who went through it see it again.
pub const VERSION: u32 = 4;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Step {
    /// The app's language, and the downloads it needs
    Welcome,
    /// The games, and their plugins
    Simulator,
    /// The pilot's bono account, which Bono Cloud needs: optional
    Account,
    /// The language model the engineer thinks with
    Provider,
    /// The microphone, and whether it hears the pilot
    Microphone,
    /// The speaker, and the engineer's voice on it
    Voice,
    /// The buttons to talk, and to start over
    TalkButton,
    /// Talking to the engineer, start to end
    RadioCheck,
}

impl Step {
    const ALL: [Step; 8] = [
        Step::Welcome,
        Step::Simulator,
        Step::Account,
        Step::Provider,
        Step::Microphone,
        Step::Voice,
        Step::TalkButton,
        Step::RadioCheck,
    ];

    /// The steps shown: the account's only where this installation has accounts.
    fn all(account: bool) -> Vec<Step> {
        (Self::ALL.into_iter())
            .filter(|&step| account || step != Step::Account)
            .collect()
    }

    fn index(self, steps: &[Step]) -> usize {
        steps.iter().position(|&step| step == self).unwrap_or(0)
    }

    fn next(self, steps: &[Step]) -> Option<Step> {
        steps.get(self.index(steps) + 1).copied()
    }

    fn previous(self, steps: &[Step]) -> Option<Step> {
        Some(steps[self.index(steps).checked_sub(1)?])
    }
}

/// What the pilot clicked at the bottom of the guide.
enum Action {
    Back,
    Next,
    /// Finished, or skipped
    Close,
}

pub struct Onboarding {
    /// The step showing: `None` when the guide isn't
    step: Option<Step>,
    /// Its own, rather than the topbar's: that one looks at the disk again every time its menu
    /// opens
    simulators: SimulatorsTopbar,
    talk_button: ChangeButton,
    session_button: ChangeButton,
    /// The radio check: how long the chat was when it started, so it shows what was said since
    radio_from: usize,
    /// The radio check: what the pilot said was being transcribed last frame
    transcribing: bool,
    /// The radio check: how many times the pilot was understood, when the talk button was last
    /// pressed
    pilot_lines: usize,
    /// The radio check: the last thing the pilot said wasn't understood
    nothing_heard: bool,
}

impl Onboarding {
    pub fn new(config: &RawConfig) -> Self {
        Self {
            step: (*config.onboarding() < VERSION).then_some(Step::Welcome),
            simulators: SimulatorsTopbar::default(),
            talk_button: ChangeButton::default(),
            session_button: ChangeButton::default(),
            radio_from: 0,
            transcribing: false,
            pilot_lines: 0,
            nothing_heard: false,
        }
    }

    /// Shows the guide from its start.
    pub fn open(&mut self) {
        self.step = Some(Step::Welcome);
    }

    pub fn update(
        &mut self,
        rt: &Handle,
        ss: &Subsystems,
        ctx: &egui::Context,
        config: &mut Config,
        topbar: &mut Topbar,
        synth: &mut Synthesizer,
        simulators: &mut ActiveSimulator,
        engineer: &Engineer,
        account: Option<&Account>,
    ) {
        // What a step started stops once it isn't showing
        if self.step != Some(Step::Microphone) {
            topbar.audio.stop_meter();
        }
        if self.step != Some(Step::TalkButton) {
            self.talk_button.cancel();
            self.session_button.cancel();
        }

        let Some(step) = self.step else {
            return;
        };
        let steps = Step::all(account.is_some());

        let mut action = None;
        egui::Modal::new(egui::Id::new("onboarding")).show(ctx, |ui| {
            ui.set_width(480.0);
            ui.weak(t!(
                "onboarding.step",
                step = step.index(&steps) + 1,
                steps = steps.len()
            ));

            // Room left for the buttons below, however small the window
            let max_height = (ctx.content_rect().height() - 160.0).max(120.0);
            egui::ScrollArea::vertical()
                .max_height(max_height)
                .show(ui, |ui| match step {
                    Step::Welcome => welcome(rt, ui, config, &mut topbar.audio),
                    Step::Simulator => {
                        self.simulator(rt, ui, config, &mut topbar.audio, simulators)
                    }
                    Step::Account => {
                        ui.heading(t!("onboarding.account_title"));
                        ui.label(t!("onboarding.account_body"));
                        ui.add_space(8.0);
                        egui::Frame::group(ui.style()).show(ui, |ui| {
                            ui.set_width(ui.available_width());
                            topbar.account.update(ui, account);
                        });
                        if let Some(account) = account {
                            ui.add_space(8.0);
                            topbar.account.sign_up(ui, account);
                        }
                        ui.add_space(8.0);
                        ui.weak(t!("onboarding.account_optional"));
                        still_downloading(rt, ui, config, &mut topbar.audio);
                    }
                    Step::Provider => {
                        ui.heading(t!("onboarding.provider_title"));
                        ui.label(t!("onboarding.provider_body"));
                        ui.add_space(8.0);
                        topbar.provider.guide(rt, ui, config);
                        still_downloading(rt, ui, config, &mut topbar.audio);
                    }
                    Step::Microphone => microphone(rt, ss, ui, config, &mut topbar.audio),
                    Step::Voice => voice(rt, ss, ui, config, &mut topbar.audio, synth),
                    Step::TalkButton => self.talk_button(rt, ss, ui, config),
                    Step::RadioCheck => {
                        self.radio_check(rt, ui, config, &mut topbar.audio, simulators, engineer)
                    }
                });

            ui.separator();
            ui.horizontal(|ui| {
                if step.next(&steps).is_some() && ui.button(t!("onboarding.skip")).clicked() {
                    action = Some(Action::Close);
                }

                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let (label, forward) = match step.next(&steps) {
                        Some(_) => (t!("onboarding.next"), Action::Next),
                        None => (t!("onboarding.finish"), Action::Close),
                    };
                    if ui.button(label).clicked() {
                        action = Some(forward);
                    }
                    if step.previous(&steps).is_some() && ui.button(t!("onboarding.back")).clicked()
                    {
                        action = Some(Action::Back);
                    }
                });
            });
        });

        match action {
            Some(Action::Back) => self.step = step.previous(&steps),
            Some(Action::Next) => {
                // The language is picked: what it needs can start downloading
                if step == Step::Welcome {
                    topbar.audio.setup(config);
                }
                self.step = step.next(&steps);
                if self.step == Some(Step::RadioCheck) {
                    self.radio_from = engineer.chat().len();
                    self.nothing_heard = false;
                }
            }
            Some(Action::Close) => {
                config.update(|raw| raw.onboarding.set(VERSION));
                self.step = None;
                self.simulators.closed();
            }
            None => {}
        }
    }

    fn simulator(
        &mut self,
        rt: &Handle,
        ui: &mut egui::Ui,
        config: &Config,
        audio: &mut AudioTopbar,
        simulators: &mut ActiveSimulator,
    ) {
        ui.heading(t!("onboarding.simulator_title"));
        ui.label(t!("onboarding.simulator_body"));
        ui.add_space(8.0);

        egui::Frame::group(ui.style()).show(ui, |ui| {
            ui.set_width(ui.available_width());
            self.simulators.update(rt, ui, simulators);
        });
        if !self.simulators.any_installed() {
            ui.weak(t!("onboarding.no_simulator"));
        }

        still_downloading(rt, ui, config, audio);
    }

    fn talk_button(&mut self, rt: &Handle, ss: &Subsystems, ui: &mut egui::Ui, config: &Config) {
        ui.heading(t!("onboarding.talk_title"));
        ui.label(t!("onboarding.talk_body"));
        ui.add_space(8.0);

        let (talk, session) = {
            let config = config.get();
            (
                config.talk_button().clone(),
                config.session_button().clone(),
            )
        };

        let label = format!(
            "{}: {}",
            t!("onboarding.talk_button"),
            button_name(talk.clone())
        );
        let pressed = self.talk_button.update(
            rt,
            ss,
            ui,
            setup_label(talk.is_none(), label),
            t!("settings.press_button"),
        );
        if let Some(button) = pressed {
            config.update(|raw| raw.talk_button.set(Some(button)));
        }

        ui.add_space(8.0);
        let label = format!(
            "{}: {}",
            t!("onboarding.session_button"),
            button_name(session)
        );
        let pressed = self
            .session_button
            .update(rt, ss, ui, label, t!("settings.press_button"));
        if let Some(button) = pressed {
            config.update(|raw| raw.session_button.set(Some(button)));
        }
        ui.weak(t!("onboarding.session_hint"));
    }

    fn radio_check(
        &mut self,
        rt: &Handle,
        ui: &mut egui::Ui,
        config: &Config,
        audio: &mut AudioTopbar,
        simulators: &ActiveSimulator,
        engineer: &Engineer,
    ) {
        ui.heading(t!("onboarding.radio_title"));
        ui.label(t!("onboarding.radio_body"));
        ui.add_space(8.0);

        let (talk, has_voice) = {
            let config = config.get();
            let lang = app_language(config.locale());
            (
                config.talk_button().clone(),
                config.voices().contains_key(&lang),
            )
        };
        let checks = [
            (t!("onboarding.talk_button"), talk.is_some()),
            (t!("audio.microphone"), audio.microphone.get().is_some()),
            (t!("audio.speech_to_text"), audio.listening(rt)),
            (t!("audio.voice"), has_voice),
            (t!("audio.speaker"), audio.speaker.get().is_some()),
        ];
        egui::Grid::new("onboarding-radio-checks")
            .num_columns(2)
            .min_col_width(0.0)
            .spacing([8.0, 4.0])
            .show(ui, |ui| {
                for (what, ready) in checks {
                    match ready {
                        true => ui.label(egui_phosphor::bold::CHECK),
                        false => ui.colored_label(
                            ui.visuals().warn_fg_color,
                            egui_phosphor::bold::WARNING,
                        ),
                    };
                    ui.label(what);
                    ui.end_row();
                }
            });
        ui.add_space(8.0);

        if checks.iter().any(|(_, ready)| !ready) {
            ui.colored_label(ui.visuals().warn_fg_color, t!("onboarding.radio_not_ready"));
            return;
        }
        ui.strong(t!("onboarding.radio_hold", button = button_name(talk)));

        let said = &engineer.chat()[self.radio_from.min(engineer.chat().len())..];
        let pilot_lines = (said.iter())
            .filter(|entry| entry.speaker == Speaker::Pilot)
            .count();
        // Something said that came out as nothing: no new line for the pilot
        let transcribing = audio.transcribing();
        if audio.recording() {
            self.pilot_lines = pilot_lines;
            self.nothing_heard = false;
        } else if self.transcribing && !transcribing {
            self.nothing_heard = pilot_lines == self.pilot_lines;
        }
        self.transcribing = transcribing;

        ui.horizontal(|ui| {
            if audio.recording() {
                ui.colored_label(
                    ui.visuals().error_fg_color,
                    format!(
                        "{} {}",
                        egui_phosphor::bold::MICROPHONE,
                        t!("onboarding.radio_listening")
                    ),
                );
            } else if transcribing {
                ui.spinner();
                ui.label(t!("onboarding.radio_understanding"));
            } else if engineer.is_thinking() {
                ui.spinner();
                ui.label(t!("onboarding.radio_thinking"));
            } else if self.nothing_heard {
                ui.colored_label(ui.visuals().warn_fg_color, t!("onboarding.radio_nothing"));
            }
        });

        for entry in said {
            let (name, color) = match entry.speaker {
                Speaker::Pilot => (t!("chat.pilot"), ui.visuals().hyperlink_color),
                Speaker::Engineer => (t!("chat.engineer"), ui.visuals().warn_fg_color),
                Speaker::Notice => {
                    ui.label(RichText::new(&entry.text).italics().weak());
                    continue;
                }
            };
            ui.horizontal_wrapped(|ui| {
                ui.label(RichText::new(name).strong().color(color));
                ui.label(&entry.text);
            });
        }

        if engineer_done(said) && !engineer.is_thinking() {
            ui.add_space(8.0);
            ui.label(format!(
                "{} {}",
                egui_phosphor::bold::CHECK,
                t!("onboarding.radio_done")
            ));
            if simulators.current().is_none() {
                ui.weak(t!("onboarding.radio_no_game"));
            }
        }
    }
}

/// Whether the engineer answered something in `said`.
fn engineer_done(said: &[crate::engineer::ChatEntry]) -> bool {
    said.iter().any(|entry| entry.speaker == Speaker::Engineer)
}

fn welcome(rt: &Handle, ui: &mut egui::Ui, config: &mut Config, audio: &mut AudioTopbar) {
    ui.heading(t!("onboarding.welcome_title"));
    ui.label(t!("onboarding.welcome_body"));

    ui.add_space(8.0);
    ui.strong(t!("language"));
    config.maybe_update(|raw| settings::languages(ui, raw));

    let items = audio.setup_items(rt, config);
    if !items.is_empty() {
        ui.add_space(8.0);
        ui.strong(t!("onboarding.downloads_title"));
        ui.label(t!("onboarding.downloads_body"));
        if downloads(ui, &items) {
            audio.setup(config);
        }
    }
}

fn microphone(
    rt: &Handle,
    ss: &Subsystems,
    ui: &mut egui::Ui,
    config: &mut Config,
    audio: &mut AudioTopbar,
) {
    ui.heading(t!("onboarding.microphone_title"));
    ui.label(t!("onboarding.microphone_body"));
    ui.add_space(8.0);

    ui.strong(t!("audio.microphone"));
    egui::Frame::group(ui.style()).show(ui, |ui| {
        ui.set_width(ui.available_width());
        audio.pick_microphone(ss, ui, config);
    });

    ui.horizontal(|ui| {
        ui.label(t!("onboarding.mic_level"));
        match audio.microphone_level() {
            Some(level) => {
                egui::ProgressBar::new(level)
                    .desired_width(ui.available_width())
                    .ui(ui);
                // It moves by itself
                ui.ctx().request_repaint();
            }
            None => {
                ui.colored_label(ui.visuals().warn_fg_color, t!("onboarding.mic_unavailable"));
            }
        }
    });

    still_downloading(rt, ui, config, audio);
}

fn voice(
    rt: &Handle,
    ss: &Subsystems,
    ui: &mut egui::Ui,
    config: &mut Config,
    audio: &mut AudioTopbar,
    synth: &mut Synthesizer,
) {
    ui.heading(t!("onboarding.voice_title"));
    ui.label(t!("onboarding.voice_body"));
    ui.add_space(8.0);

    ui.strong(t!("audio.speaker"));
    egui::Frame::group(ui.style()).show(ui, |ui| {
        ui.set_width(ui.available_width());
        audio.pick_speaker(ss, ui, config);
    });
    ui.add_space(8.0);

    let voice = {
        let config = config.get();
        let lang = app_language(config.locale());
        config.voices().get(&lang).cloned()
    };
    match &voice {
        Some(path) => {
            let name = path.file_stem().unwrap_or(path.as_str());
            ui.label(t!("onboarding.voice_name", name = name));
        }
        None => {
            ui.colored_label(ui.visuals().warn_fg_color, t!("onboarding.no_voice"));
        }
    }

    config.maybe_update(|raw| {
        // `|` rather than `||`: both sliders are drawn, whichever changed
        volume(
            ui,
            *raw.engineer_volume(),
            &mut raw.engineer_volume,
            t!("volume.engineer"),
            t!("volume.engineer_hover"),
        ) | volume(
            ui,
            *raw.radio_effect(),
            &mut raw.radio_effect,
            t!("volume.radio_effect"),
            t!("volume.radio_effect_hover"),
        )
    });

    ui.horizontal(|ui| {
        let ready = voice.is_some() && audio.speaker.get().is_some();
        let test = ui.add_enabled(
            ready && !audio.testing(),
            egui::Button::new(t!("onboarding.test_voice")),
        );
        if test.clicked() {
            audio.test_voice(rt, config, synth);
        }
        if audio.testing() {
            ui.spinner();
        }
    });

    still_downloading(rt, ui, config, audio);
}

/// The downloads started on the first step, while they're still going.
fn still_downloading(rt: &Handle, ui: &mut egui::Ui, config: &Config, audio: &mut AudioTopbar) {
    let items = audio.setup_items(rt, config);
    if !items.is_empty() {
        ui.add_space(8.0);
        ui.strong(t!("onboarding.downloads_title"));
        if downloads(ui, &items) {
            audio.setup(config);
        }
        ui.weak(t!("onboarding.downloads_continue"));
    }
}

/// What the guide downloads, and how far along each is. Returns whether missing or failed ones are to
/// be tried again.
fn downloads(ui: &mut egui::Ui, items: &[(&str, SetupItem)]) -> bool {
    egui::Grid::new("onboarding-downloads")
        .num_columns(3)
        .spacing([16.0, 4.0])
        .show(ui, |ui| {
            for (what, item) in items {
                ui.label(*what);
                match item {
                    SetupItem::Loading => {
                        ui.spinner();
                        ui.weak(t!("onboarding.looking_up"));
                    }
                    SetupItem::Unavailable => {
                        ui.colored_label(ui.visuals().warn_fg_color, t!("onboarding.unavailable"));
                    }
                    SetupItem::Found { name, size, state } => {
                        ui.label(name);
                        match state {
                            SetupState::Missing => {
                                if let Some(size) = size {
                                    ui.weak(format!("{:.0} MB", *size as f64 / 1e6));
                                }
                            }
                            SetupState::Downloading(progress) => {
                                egui::ProgressBar::new(*progress as f32)
                                    .show_percentage()
                                    .desired_width(160.0)
                                    .ui(ui);
                            }
                            SetupState::Failed => {
                                ui.colored_label(
                                    ui.visuals().warn_fg_color,
                                    t!("onboarding.failed"),
                                );
                            }
                            SetupState::Ready => {
                                ui.label(format!(
                                    "{} {}",
                                    egui_phosphor::bold::CHECK,
                                    t!("onboarding.ready")
                                ));
                            }
                        }
                    }
                }
                ui.end_row();
            }
        });

    let failed = items.iter().any(|(_, item)| {
        matches!(
            item,
            SetupItem::Unavailable
                | SetupItem::Found {
                    state: SetupState::Failed | SetupState::Missing,
                    ..
                }
        )
    });
    failed && ui.button(t!("onboarding.retry")).clicked()
}
