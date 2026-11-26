//! The setup guide: shown on the first run (and again once it gains steps), or from Settings. It
//! walks the pilot through what bono needs, a step at a time, and can be skipped at any point.

use crate::{
    account::Account,
    app::{
        Subsystems,
        topbar::{
            Topbar,
            audio::{
                AudioTopbar, SetupItem, SetupState, app_language,
                engine::{self, Engine},
                voice::{RECOMMENDED, recommended},
            },
            settings::{self, ChangeButton, button_name},
            setup_label,
            simulators::SimulatorsTopbar,
            volume::{MAX_VOLUME, volume},
        },
    },
    arrstr,
    config::{ArrayStr, Config, RawConfig},
    engineer::{Engineer, Speaker},
    language,
    sim::ActiveSimulator,
    synthesizer::{Synthesizer, VoiceLoad},
};
use bono_i18n::t;
use egui::{RichText, Widget};
use std::collections::BTreeSet;
use tokio::runtime::Handle;

/// The guide's version: bumped when it gains a step, so pilots who went through it see it again.
/// 5: it asks which languages the pilot speaks, which decides how they're heard.
pub const VERSION: u32 = 5;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Step {
    /// The app's language, the languages the pilot speaks, and the downloads they need
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
    /// The radio check: the talk button was held, and what was said hasn't been heard yet
    listened: bool,
    /// The radio check: how many times the pilot was understood, when the talk button was last
    /// pressed
    pilot_lines: usize,
    /// The radio check: the last thing the pilot said wasn't understood
    nothing_heard: bool,
    /// The languages ticked in "I speak": `None` until a box is changed, so they follow the
    /// settings (and the app's language)
    ticked: Option<BTreeSet<ArrayStr<2>>>,
    /// What "I speak" is searched for
    search: String,
}

impl Onboarding {
    pub fn new(config: &RawConfig) -> Self {
        Self {
            step: (*config.onboarding() < VERSION).then_some(Step::Welcome),
            simulators: SimulatorsTopbar::default(),
            talk_button: ChangeButton::default(),
            session_button: ChangeButton::default(),
            radio_from: 0,
            listened: false,
            pilot_lines: 0,
            nothing_heard: false,
            ticked: None,
            search: String::new(),
        }
    }

    /// Shows the guide from its start.
    pub fn open(&mut self) {
        self.step = Some(Step::Welcome);
        self.ticked = None;
        self.search.clear();
    }

    /// The languages ticked in "I speak".
    fn ticked(&self, config: &RawConfig) -> BTreeSet<ArrayStr<2>> {
        self.ticked.clone().unwrap_or_else(|| {
            let voices = config.voices().keys().copied().collect::<Vec<_>>();
            ticked_at_first(*config.heard_language(), &voices, app_language(config.locale()))
        })
    }

    pub fn is_open(&self) -> bool {
        self.step.is_some()
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
                    Step::Welcome => self.welcome(rt, ui, config, &mut topbar.audio, synth),
                    Step::Simulator => {
                        self.simulator(rt, ui, config, &mut topbar.audio, synth, simulators)
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
                        still_downloading(rt, ui, config, &mut topbar.audio, synth);
                    }
                    Step::Provider => {
                        ui.heading(t!("onboarding.provider_title"));
                        ui.label(t!("onboarding.provider_body"));
                        ui.add_space(8.0);
                        topbar.provider.guide(rt, ui, config);
                        still_downloading(rt, ui, config, &mut topbar.audio, synth);
                    }
                    Step::Microphone => microphone(rt, ss, ui, config, &mut topbar.audio, synth),
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
                    // Who's heard needs a language
                    let ready = step != Step::Welcome || !self.ticked(&config.get()).is_empty();
                    if ui.add_enabled(ready, egui::Button::new(label)).clicked() {
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
                // The languages are picked: what they need can start downloading
                if step == Step::Welcome {
                    let ticked = self.ticked(&config.get());
                    let (answer, engine) = plan(&config.get(), &topbar.audio, synth, &ticked);
                    config.update(|raw| {
                        // The same language, as it was set (Nynorsk stays Nynorsk)
                        let heard = match (answer.heard, *raw.heard_language()) {
                            (Some(new), Some(old)) if language::voice_code(old) == new => Some(old),
                            (new, _) => new,
                        };
                        raw.heard_language.set(heard);
                        if engine.is_some() {
                            raw.whisper_model.set(None);
                        }
                        for &lang in &answer.remove {
                            // Freed on a worker: it waits for the voice to unload
                            if let Some(voice) = raw.remove_language(lang, synth) {
                                rt.spawn_blocking(move || drop(voice));
                            }
                        }
                    });
                    topbar.audio.setup(&answer.get, engine);
                }
                self.step = step.next(&steps);
                if self.step == Some(Step::RadioCheck) {
                    self.radio_from = engineer.chat().len();
                    self.nothing_heard = false;
                    self.listened = false;
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

    fn welcome(
        &mut self,
        rt: &Handle,
        ui: &mut egui::Ui,
        config: &mut Config,
        audio: &mut AudioTopbar,
        synth: &Synthesizer,
    ) {
        ui.heading(t!("onboarding.welcome_title"));
        ui.label(t!("onboarding.welcome_body"));

        ui.add_space(8.0);
        ui.strong(t!("onboarding.app_language"));
        config.maybe_update(|raw| settings::languages(ui, raw));

        ui.add_space(8.0);
        ui.strong(t!("onboarding.i_speak"));
        let mut ticked = self.ticked(&config.get());
        let (heard, voices) = {
            let raw = config.get();
            (*raw.heard_language(), raw.voices().keys().copied().collect::<Vec<_>>())
        };
        if languages_spoken(ui, &mut self.search, heard, &voices, &mut ticked) {
            self.ticked = Some(ticked.clone());
        }

        let (answer, engine) = plan(&config.get(), audio, synth, &ticked);
        let items = audio.setup_items(rt, config, synth, &answer.get, engine);
        if !items.is_empty() {
            ui.add_space(8.0);
            ui.strong(t!("onboarding.downloads_title"));
            ui.label(t!("onboarding.downloads_body"));
            // They start with Next
            downloads(ui, &items, false);
        }
    }

    fn simulator(
        &mut self,
        rt: &Handle,
        ui: &mut egui::Ui,
        config: &Config,
        audio: &mut AudioTopbar,
        synth: &Synthesizer,
        simulators: &mut ActiveSimulator,
    ) {
        ui.heading(t!("onboarding.simulator_title"));
        ui.label(t!("onboarding.simulator_body"));
        ui.add_space(8.0);

        egui::Frame::group(ui.style()).show(ui, |ui| {
            ui.set_width(ui.available_width());
            self.simulators.update(rt, ui, simulators);
        });
        // Games found by their process alone (iRacing) aren't looked for on the disk
        let running = simulators.simulators().any(|sim| sim.is_running());
        if !self.simulators.any_installed() && !running {
            ui.weak(t!("onboarding.no_simulator"));
        }

        still_downloading(rt, ui, config, audio, synth);
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
            let lang = audio.guide_language(&config);
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
        // Something said that came out as nothing: no new line for the pilot. Silence is heard
        // before a frame may see it transcribing, so it's whether the button was held
        let transcribing = audio.transcribing();
        if audio.recording() {
            self.pilot_lines = pilot_lines;
            self.nothing_heard = false;
            self.listened = true;
        } else if self.listened && !transcribing {
            self.listened = false;
            self.nothing_heard = pilot_lines == self.pilot_lines;
        }

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

fn microphone(
    rt: &Handle,
    ss: &Subsystems,
    ui: &mut egui::Ui,
    config: &mut Config,
    audio: &mut AudioTopbar,
    synth: &Synthesizer,
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

    still_downloading(rt, ui, config, audio, synth);
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
        let lang = audio.guide_language(&config);
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
            0.0..=MAX_VOLUME,
            t!("volume.engineer"),
            t!("volume.engineer_hover"),
        ) | volume(
            ui,
            *raw.radio_effect(),
            &mut raw.radio_effect,
            0.0..=1.0,
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

    still_downloading(rt, ui, config, audio, synth);
}

/// The downloads started on the first step, while they're still going.
fn still_downloading(
    rt: &Handle,
    ui: &mut egui::Ui,
    config: &Config,
    audio: &mut AudioTopbar,
    synth: &Synthesizer,
) {
    let (langs, engine) = {
        let raw = config.get();
        let mut langs = audio.voices.asked();
        // A voice that was set up, and then didn't load
        for (&lang, _) in raw.voices() {
            if synth.load(lang) == Some(VoiceLoad::Failed) && !langs.contains(&lang) {
                langs.push(lang);
            }
        }
        let automatic = raw.whisper_model().is_none();
        // With what Retry asks for, failed ones too
        let wanted = || audio.whisper.wanted(&raw, &langs).0;
        let engine = automatic.then(|| audio.whisper.setting_up().unwrap_or_else(wanted));
        (langs, engine)
    };
    let items = audio.setup_items(rt, config, synth, &langs, engine);
    if !items.is_empty() {
        ui.add_space(8.0);
        ui.strong(t!("onboarding.downloads_title"));
        if downloads(ui, &items, true) {
            audio.setup(&langs, engine);
        }
        ui.weak(t!("onboarding.downloads_continue"));
    }
}

/// What the guide downloads, and how far along each is, with what's left in all. Returns whether
/// missing or failed ones are to be tried again (`retry`: there's a button to).
fn downloads(ui: &mut egui::Ui, items: &[(&str, SetupItem)], retry: bool) -> bool {
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
                            SetupState::Loading => {
                                ui.horizontal(|ui| {
                                    ui.spinner();
                                    ui.weak(t!("onboarding.loading"));
                                });
                            }
                            SetupState::LoadFailed => {
                                ui.colored_label(
                                    ui.visuals().warn_fg_color,
                                    t!("onboarding.load_failed"),
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

            let left = (items.iter())
                .filter_map(|(_, item)| match item {
                    SetupItem::Found {
                        size: Some(size),
                        state: SetupState::Missing | SetupState::Downloading(_) | SetupState::Failed,
                        ..
                    } => Some(*size),
                    _ => None,
                })
                .collect::<Vec<_>>();
            if left.len() > 1 {
                ui.strong(t!("onboarding.total"));
                ui.label("");
                ui.weak(format!("{:.0} MB", left.iter().sum::<u64>() as f64 / 1e6));
                ui.end_row();
            }
        });

    let failed = items.iter().any(|(_, item)| {
        matches!(
            item,
            SetupItem::Unavailable
                | SetupItem::Found {
                    state: SetupState::Failed | SetupState::LoadFailed | SetupState::Missing,
                    ..
                }
        )
    });
    retry && failed && ui.button(t!("onboarding.retry")).clicked()
}

/// "I speak": a search field over the languages there's a voice for (and the Heard language, if
/// there's none for it), with those `ticked` first. Returns whether a box changed.
fn languages_spoken(
    ui: &mut egui::Ui,
    search: &mut String,
    heard: Option<ArrayStr<2>>,
    voices: &[ArrayStr<2>],
    ticked: &mut BTreeSet<ArrayStr<2>>,
) -> bool {
    let mut langs = (RECOMMENDED.iter())
        .filter_map(|voice| Some((ArrayStr::from_str(voice.lang)?, voice.native.to_owned())))
        // Those Parakeet or Whisper can hear (Whisper has no Kurdish)
        .filter(|&(lang, _)| {
            engine::parakeet_knows(lang) || whisper_rs::get_lang_id(&lang).is_some()
        })
        .collect::<Vec<_>>();
    let others = heard.map(language::voice_code).into_iter().chain(voices.iter().copied());
    for lang in others.chain(ticked.iter().copied()) {
        if !langs.iter().any(|(other, _)| *other == lang) {
            langs.push((lang, language::ui_name(lang)));
        }
    }

    let hint = t!("onboarding.search_languages", count = langs.len());
    egui::TextEdit::singleline(search)
        .hint_text(hint)
        .desired_width(f32::INFINITY)
        .ui(ui);
    let search = search.trim().to_lowercase();
    // A native name the app's font can't draw (Arabic's, Bengali's…) is shown in the app's language
    let font = egui::TextStyle::Body.resolve(ui.style());
    let drawn = |native: &str| ui.ctx().fonts_mut(|fonts| fonts.has_glyphs(&font, native));
    let mut rows = (langs.into_iter())
        .map(|(lang, native)| {
            let name = language::ui_name(lang);
            let native = if drawn(&native) { native } else { name.clone() };
            (lang, native, name)
        })
        .filter(|(lang, native, name)| {
            ticked.contains(lang)
                || [native, name].iter().any(|name| name.to_lowercase().contains(&search))
        })
        .collect::<Vec<_>>();
    rows.sort_by_cached_key(|(lang, _, name)| (!ticked.contains(lang), name.clone()));

    let mut changed = false;
    // Always as tall, so the guide doesn't move as the search narrows the list
    egui::ScrollArea::vertical()
        .id_salt("onboarding-languages")
        .max_height(160.0)
        .min_scrolled_height(160.0)
        .auto_shrink([false, false])
        .show(ui, |ui| {
            for (lang, native, name) in rows {
                ui.horizontal(|ui| {
                    let mut on = ticked.contains(&lang);
                    if ui.checkbox(&mut on, &native).changed() {
                        changed = true;
                        match on {
                            true => ticked.insert(lang),
                            false => ticked.remove(&lang),
                        };
                    }
                    if name != native {
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            ui.weak(name);
                        });
                    }
                });
            }
        });
    changed
}

/// What Next on the first step does with the languages `ticked`: the answer, and the engine
/// Automatic will want then, if speech to text becomes (or stays) automatic.
fn plan(
    config: &RawConfig,
    audio: &AudioTopbar,
    synth: &Synthesizer,
    ticked: &BTreeSet<ArrayStr<2>>,
) -> (Answer, Option<Engine>) {
    let voices = config.voices().keys().copied().collect::<Vec<_>>();
    let usable = audio.usable_voices(config, synth);
    let answer = answer(ticked, &voices, &usable, config.engineer_reply_language());
    let whisper = &audio.whisper;
    let picked = match config.whisper_model() {
        None => Picked::None,
        Some(path) if *path == whisper.turbo() => Picked::Turbo,
        Some(path) if path == whisper.parakeet_path() => Picked::Parakeet,
        // As whisper.cpp names its English-only models (`ggml-small.en-q5_1.bin`)
        Some(path) => Picked::Other {
            english: path.file_name().is_some_and(|name| name.contains(".en")),
        },
    };
    if !becomes_automatic(picked, !whisper.missing(config, &[]), ticked) {
        return (answer, None);
    }
    // The voices once it's done, as Automatic will count them: those being set up too (only
    // those with a recommended voice are)
    let mut after = (voices.iter().copied())
        .filter(|lang| !answer.remove.contains(lang))
        .collect::<Vec<_>>();
    for &lang in &answer.get {
        if !after.contains(&lang) && recommended(&lang).is_some() {
            after.push(lang);
        }
    }
    let reply = config.spoken_language().filter(|lang| after.contains(lang));
    let app = app_language(config.locale());
    let engine = engine::wanted(answer.heard, &after, reply, app).0;
    (answer, Some(engine))
}

/// The languages ticked in "I speak" when the guide opens: `voices` are those with a voice, and `app`
/// is the app's.
fn ticked_at_first(
    heard: Option<ArrayStr<2>>,
    voices: &[ArrayStr<2>],
    app: ArrayStr<2>,
) -> BTreeSet<ArrayStr<2>> {
    match heard {
        Some(heard) => BTreeSet::from([language::voice_code(heard)]),
        None if voices.is_empty() => BTreeSet::from([app]),
        None => voices.iter().copied().collect(),
    }
}

/// What "I speak" changes.
#[derive(Debug, PartialEq, Eq)]
struct Answer {
    /// The Heard language: `None` for automatic
    heard: Option<ArrayStr<2>>,
    /// The languages to get a voice for
    get: Vec<ArrayStr<2>>,
    /// The languages whose voice is taken out of the settings (its files stay)
    remove: Vec<ArrayStr<2>>,
}

/// `voices`: the languages with a voice, and `usable` those whose voice is on disk and loads (the
/// others are set up again). `reply`: the fixed reply language, if it has a voice.
fn answer(
    ticked: &BTreeSet<ArrayStr<2>>,
    voices: &[ArrayStr<2>],
    usable: &[ArrayStr<2>],
    reply: Option<ArrayStr<2>>,
) -> Answer {
    let heard = match ticked.len() {
        1 => ticked.first().copied(),
        _ => None,
    };
    // The engineer only speaks the reply language then: its voice is all it needs
    if let Some(reply) = reply {
        return Answer {
            heard,
            get: (!usable.contains(&reply)).then_some(reply).into_iter().collect(),
            remove: Vec::new(),
        };
    }
    let get = ticked.iter().filter(|lang| !usable.contains(lang));
    // With one language heard, the other voices don't count
    let remove = (voices.iter()).filter(|lang| heard.is_none() && !ticked.contains(*lang));
    Answer {
        heard,
        get: get.copied().collect(),
        remove: remove.copied().collect(),
    }
}

/// The speech to text model picked.
#[derive(Debug, Clone, Copy)]
enum Picked {
    None,
    /// The one the guide always picked
    Turbo,
    Parakeet,
    Other {
        english: bool,
    },
}

/// Whether the guide makes speech to text automatic. `usable`: the model picked is on disk, and
/// didn't fail to load.
fn becomes_automatic(picked: Picked, usable: bool, ticked: &BTreeSet<ArrayStr<2>>) -> bool {
    !usable
        || match picked {
            Picked::None | Picked::Turbo => true,
            Picked::Parakeet => !ticked.iter().all(|&lang| engine::parakeet_knows(lang)),
            Picked::Other { english } => english && ticked.iter().any(|&lang| lang != arrstr!("en")),
        }
}

#[cfg(test)]
mod tests {
    use super::*;

    const EN: ArrayStr<2> = arrstr!("en");
    const ES: ArrayStr<2> = arrstr!("es");
    const FR: ArrayStr<2> = arrstr!("fr");
    const CA: ArrayStr<2> = arrstr!("ca");
    const EU: ArrayStr<2> = arrstr!("eu");

    #[test]
    fn starts_from_what_the_pilot_has() {
        let first = |heard, voices: &[_], app| {
            ticked_at_first(heard, voices, app).into_iter().collect::<Vec<_>>()
        };
        assert_eq!(first(Some(ES), &[CA, ES], EN), [ES]);
        assert_eq!(first(None, &[CA, ES], EN), [CA, ES]);
        assert_eq!(first(None, &[], FR), [FR]);
        assert_eq!(first(Some(arrstr!("nn")), &[], EN), [arrstr!("no")]);
    }

    fn answered(ticked: &[ArrayStr<2>], voices: &[ArrayStr<2>], reply: Option<ArrayStr<2>>) -> Answer {
        answer(&ticked.iter().copied().collect(), voices, voices, reply)
    }

    #[test]
    fn a_broken_reply_voice_is_set_up_again_and_kept() {
        let answer = answer(&BTreeSet::from([EN, FR]), &[ES], &[], Some(ES));
        assert_eq!((answer.get, answer.remove), (vec![ES], vec![]));
    }

    #[test]
    fn a_broken_voice_is_set_up_again() {
        // Spanish's voice is set, but its files are gone (or it doesn't load)
        let answer = answer(&BTreeSet::from([ES, CA]), &[ES, EU], &[EU], None);
        assert_eq!(answer.get, [CA, ES]);
        assert_eq!(answer.remove, [EU]);
    }

    #[test]
    fn one_language_is_heard_alone() {
        let expected = Answer {
            heard: Some(FR),
            get: vec![FR],
            remove: vec![],
        };
        assert_eq!(answered(&[FR], &[ES], None), expected);
        // It has a voice already
        assert_eq!(answered(&[ES], &[CA, ES], None).get, []);
        assert_eq!(answered(&[ES], &[CA, ES], None).remove, []);
    }

    #[test]
    fn several_languages_are_the_ones_with_a_voice() {
        let expected = Answer {
            heard: None,
            get: vec![CA],
            remove: vec![EU],
        };
        assert_eq!(answered(&[ES, CA], &[ES, EU], None), expected);
    }

    #[test]
    fn a_fixed_reply_needs_no_other_voice_and_keeps_its_own() {
        for ticked in [&[ES, FR][..], &[FR]] {
            let answer = answered(ticked, &[EN, ES], Some(EN));
            assert_eq!((answer.get, answer.remove), (vec![], vec![]));
        }
        assert_eq!(answered(&[FR], &[EN], Some(EN)).heard, Some(FR));
    }

    #[test]
    fn next_unpicks_what_the_guide_picked() {
        let unpicked = |picked, usable, ticked: &[ArrayStr<2>]| {
            becomes_automatic(picked, usable, &ticked.iter().copied().collect())
        };
        let (english, multilingual) = (Picked::Other { english: true }, Picked::Other { english: false });
        assert!(unpicked(Picked::None, false, &[ES]));
        assert!(unpicked(Picked::Turbo, true, &[ES]));
        // Missing, or it couldn't be loaded
        assert!(unpicked(multilingual, false, &[ES]));
        assert!(unpicked(Picked::Parakeet, false, &[ES]));
        // It doesn't know the pilot's language
        assert!(unpicked(english, true, &[ES]));
        assert!(unpicked(english, true, &[EN, ES]));
        assert!(unpicked(Picked::Parakeet, true, &[CA]));
        assert!(unpicked(Picked::Parakeet, true, &[ES, CA]));
        // Chosen by the pilot, and fine
        assert!(!unpicked(multilingual, true, &[ES, CA]));
        assert!(!unpicked(english, true, &[EN]));
        assert!(!unpicked(Picked::Parakeet, true, &[ES]));
    }

    #[test]
    fn the_wanted_engine_does_not_change_as_voices_arrive() {
        // Spanish and Catalan ticked, and only Spanish's voice there yet
        let answer = answered(&[ES, CA], &[ES], None);
        assert_eq!(answer.get, [CA]);
        let wanted = |voices: &[ArrayStr<2>]| engine::wanted(answer.heard, voices, None, EN).0;
        // While Catalan's is set up, it counts
        assert_eq!(wanted(&[ES, CA]), Engine::Whisper);
        assert_eq!(wanted(&[ES]), Engine::Parakeet);
    }
}
