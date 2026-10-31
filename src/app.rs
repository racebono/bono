use crate::{
    app::{chat::Chat, onboarding::Onboarding, topbar::Topbar},
    button::{Button, ButtonWatcher, Joysticks},
    config::{Config, RawConfig},
    engineer::Engineer,
    sim::{ActiveSimulator, SimulatorSettings},
    spotter::Spotter,
    synthesizer::Synthesizer,
    track::TrackSync,
};
use anyhow::Context as AnyhowContext;
use camino::{Utf8Path, Utf8PathBuf};
use egui::FontFamily;
use egui_sdl3::EguiCanvas;
use futures::FutureExt;
use sdl3::{
    AudioSubsystem, Error, EventSubsystem, JoystickSubsystem,
    event::{Event, WindowEvent},
    filesystem::get_pref_path,
    get_error,
    render::WindowCanvas,
    sys::render::SDL_Renderer,
};
use sdl3_main::AppResult;
use std::{
    ffi::c_int,
    num::NonZeroU32,
    ops::{Deref, DerefMut},
    sync::Arc,
    time::Duration,
};
use tokio::runtime::Handle;

mod chat;
mod onboarding;
mod topbar;

pub struct App {
    state: AppState,
    ui: EguiCanvas,
    closing: Option<tokio::task::JoinHandle<()>>,
}

impl App {
    pub async fn new(rt: &Handle) -> anyhow::Result<Self> {
        let base_path = cfg_select! {
            debug_assertions => Utf8Path::new(env!("CARGO_MANIFEST_DIR")),
            _ => <&'static Utf8Path>::try_from(sdl3::filesystem::get_base_path()?)?
        };
        log::debug!("Base Path: {base_path}");

        let pref_path = Utf8PathBuf::try_from(get_pref_path(
            env!("CARGO_PKG_NAME"),
            env!("CARGO_PKG_NAME"),
        )?)?;
        log::debug!("Prefered Path: {pref_path}");

        let assets_path = base_path.join("assets");
        log::debug!("Assets Path: {assets_path}");

        // The talk and session buttons are pressed while the game has focus: without this, SDL
        // only reports joystick (wheel) buttons to a focused window
        if !sdl3::hint::set("SDL_JOYSTICK_ALLOW_BACKGROUND_EVENTS", "1") {
            log::warn!("Wheel buttons won't work while the game has focus");
        }

        // Initialize audio here (before loading config) so that audio device finder works correctly
        let sdl = sdl3::init()?;
        let audio = sdl.audio()?;
        let video = sdl.video()?;
        let joystick = sdl.joystick()?;
        let joysticks = Joysticks::new(&joystick);
        let event = sdl.event()?;
        let fonts = load_fonts()?;
        // Initial Steam registry discovery and watcher creation can wait on the OS too.
        let simulators = rt.spawn_blocking(ActiveSimulator::default);

        // Everything waited on happens before the window exists: SDL's init callback must return
        // before the first frame, so a window opened earlier would show unresponsive meanwhile.
        // It's all local (config and track cache files, the OS credential store), done at once
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
            .build()?;
        let account = async {
            let origin = option_env!("BONO_ACCOUNT_ORIGIN")?;
            match crate::account::Account::new(origin).await {
                Ok(account) => {
                    let restore = account.clone();
                    rt.spawn(async move {
                        if let Err(error) = restore.resume().await {
                            log::warn!("Account restoration: {error}");
                        }
                    });
                    Some(account)
                }
                Err(error) => {
                    log::warn!("Account initialization: {error}");
                    None
                }
            }
        };
        let (config, tracks, account) = tokio::join!(
            load_config(&pref_path),
            TrackSync::new(client.clone(), pref_path.as_std_path()),
            account,
        );
        let config = config?;
        let simulators = simulators.await?;
        let (width, height) = {
            let config = config.get();
            (*config.width(), *config.height())
        };

        let mut ui = EguiCanvas::new(
            video
                .window("Bono", width.get(), height.get())
                .resizable()
                .high_pixel_density()
                .build()?,
        );
        ui.ctx.set_fonts(fonts);
        set_vsync(&mut ui.painter.canvas, 1)?;

        let subsystems = Subsystems {
            audio,
            joystick,
            event,
        };

        let pref_path = Arc::<Utf8Path>::from(pref_path);
        let guard = config.get();
        let gui = Gui::new(&pref_path, &subsystems, &guard, &client, account.as_ref());
        let synth = Synthesizer::new(rt, &guard);
        let engineer = Engineer::new(client, account.clone());
        drop(guard);

        return Ok(Self {
            state: AppState {
                subsystems,
                joysticks,
                config,
                gui,
                synth,
                simulators,
                applied_opponent_frequency: None,
                tracks,
                account,
                engineer,
                spotter: Spotter::new(),
            },
            ui,
            closing: None,
        });
    }

    pub fn iterate(&mut self, rt: &Handle) -> AppResult {
        if let Some(closing) = &self.closing {
            return if closing.is_finished() && self.state.gui.topbar.audio.shutdown_ready() {
                AppResult::Success
            } else {
                AppResult::Continue
            };
        }
        self.ui.run(|ctx| {
            // Must happen before the root `Ui` is built, since it copies the style on creation
            let text_scale = *self.state.config.get().text_scale();
            if self.state.gui.applied_text_scale != Some(text_scale) {
                set_text_scale(ctx, text_scale);
                self.state.gui.applied_text_scale = Some(text_scale);
            }

            // egui-sdl3 only hands us the `Context`, so build a root `Ui` the same way `Context::run_ui` does
            let mut root_ui = egui::Ui::new(
                ctx.clone(),
                egui::Id::new("root"),
                egui::UiBuilder::new()
                    .layer_id(egui::LayerId::background())
                    .max_rect(ctx.viewport_rect()),
            );

            self.state.update(rt, &mut root_ui);
        });

        self.ui.paint();
        self.ui.present();
        AppResult::Continue
    }

    /// The app is quitting: what the website holds for it is given back.
    pub fn quit(&mut self, rt: &Handle) {
        // Normal quits finish background cleanup before SDL asks us to dispose the UI.
        // An OS-forced exit can skip the normal Quit event; start best-effort cleanup then too.
        self.begin_quit(rt);
    }

    fn begin_quit(&mut self, rt: &Handle) {
        if self.closing.is_some() {
            return;
        }
        self.ui.painter.canvas.window_mut().hide();
        self.state.gui.topbar.audio.begin_shutdown();
        let engineer = self.state.engineer.quit(rt);
        let simulators = self.state.simulators.shutdown();
        let voices = std::mem::take(&mut self.state.synth.inner);
        let speech = rt.spawn_blocking(move || drop(voices));
        self.closing = Some(rt.spawn(async move {
            let _ = tokio::join!(engineer, simulators, speech);
            // Flushing has a bounded wait, so it belongs on a worker after cleanup logs finish.
            let _ = tokio::task::spawn_blocking(|| log::logger().flush()).await;
        }));
    }

    pub fn event(&mut self, rt: &Handle, event: Event) -> AppResult {
        if matches!(event, Event::Quit { .. }) {
            self.begin_quit(rt);
            return AppResult::Continue;
        }
        if self.closing.is_some() {
            return AppResult::Continue;
        }
        self.state.joysticks.event(&event);
        match event {
            Event::AudioDeviceRemoved {
                which,
                iscapture: true,
                ..
            } => {
                if self.state.gui.topbar.audio.microphone_removed(which) {
                    self.state.engineer.microphone_removed();
                }
            }
            Event::AudioDeviceRemoved {
                which,
                iscapture: false,
                ..
            } => {
                self.state.speaker_removed(which);
            }
            Event::AudioDeviceAdded { iscapture, .. } => {
                let state = &mut self.state;
                state.gui.topbar.audio.device_added(
                    iscapture,
                    &state.subsystems.audio,
                    &state.config.get(),
                );
            }
            _ => {}
        }
        // A text field taking the talk button's release mustn't leave the microphone open
        if self.ui.on_event(&event).consumed
            && !(self.state.gui.topbar.audio.whisper).ends_recording(&self.state.config, &event)
        {
            return AppResult::Continue;
        }

        let new_session = (self.state.config.get().session_button().as_ref())
            .is_some_and(|button| button.pressed(&event));
        if new_session {
            self.state.engineer.new_session();
            return AppResult::Continue;
        }

        if !self.state.gui.event(rt, &self.state.config, &event) {
            match event {
                Event::Window {
                    window_id,
                    win_event,
                    ..
                } if window_id == self.ui.painter.canvas.window().id() => match win_event {
                    WindowEvent::Resized(w, h) => self.config.on_resize(w, h),
                    _ => {}
                },
                Event::Quit { .. } => return AppResult::Success,
                _ => {}
            }
        }

        return AppResult::Continue;
    }
}

impl Deref for App {
    type Target = AppState;

    #[inline(always)]
    fn deref(&self) -> &Self::Target {
        &self.state
    }
}

impl DerefMut for App {
    #[inline(always)]
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.state
    }
}

pub struct AppState {
    account: Option<crate::account::Account>,
    gui: Gui,
    config: Config,
    subsystems: Subsystems,
    /// Kept open so their buttons can be bound and pressed
    joysticks: Joysticks,
    synth: Synthesizer,
    simulators: ActiveSimulator,
    /// The recording settings last passed to the simulators
    applied_opponent_frequency: Option<NonZeroU32>,
    /// Keeps the running simulator's track description next to its database
    tracks: TrackSync,
    // FIXME: Do we want this to be `MaybeDone`?
    engineer: Engineer,
    /// Calls cars alongside and flags by itself, in the engineer's voice
    spotter: Spotter,
}

impl AppState {
    /// Clone the account handle to run account operations in a background task.
    pub fn account(&self) -> Option<&crate::account::Account> {
        self.account.as_ref()
    }

    /// The speaker `which` is gone (unplugged): whatever plays on it stops, and the engineer and
    /// the spotter carry on.
    fn speaker_removed(&mut self, which: u32) {
        let picked = self.gui.topbar.audio.speaker_removed(which);
        self.engineer.speaker_removed(which, picked);
        self.spotter.speaker_removed(which);
    }

    fn update(&mut self, rt: &Handle, ui: &mut egui::Ui) {
        self.update_simulators();
        self.tracks.update(rt, self.simulators.current_mut());

        // What the pilot says on the radio goes to the engineer, and its answers come back
        if let Some(heard) = self.gui.topbar.audio.transcription(rt) {
            self.engineer.hear(
                rt,
                self.simulators.current_mut(),
                &self.config,
                &mut self.synth,
                heard,
            );
        }
        let audio = &self.gui.topbar.audio;
        self.engineer.set_pilot_talking(audio.recording() || audio.transcribing());
        self.engineer.update(
            rt,
            self.simulators.current().is_some(),
            &self.config,
            &mut self.synth,
            self.gui.topbar.audio.speaker.get(),
        );
        self.spotter.update(
            rt,
            self.simulators.current_mut(),
            &self.config,
            &mut self.synth,
            self.gui.topbar.audio.speaker.get(),
            &mut self.engineer,
        );
        let typed = self.gui.update(
            rt,
            &self.subsystems,
            ui,
            &mut self.config,
            &mut self.synth,
            &mut self.simulators,
            &self.engineer,
            self.account.as_ref(),
        );

        // A question typed in the chat
        if let Some(question) = typed {
            self.engineer.ask_text(
                rt,
                self.simulators.current_mut(),
                &self.config,
                &mut self.synth,
                question,
            );
        }
    }

    /// Passes the recording settings on to every simulator when they change (and at startup), and
    /// follows which simulator is running.
    fn update_simulators(&mut self) {
        let opponent_frequency = *self.config.get().opponent_frequency();
        if self.applied_opponent_frequency != Some(opponent_frequency) {
            let settings = SimulatorSettings { opponent_frequency };
            for simulator in self.simulators.simulators() {
                simulator.apply(&settings);
            }
            self.applied_opponent_frequency = Some(opponent_frequency);
        }

        // Only looks: a change waiting is taken, otherwise it's looked for again next frame
        if let Some(current) = self.simulators.changed().now_or_never() {
            match current {
                Some(simulator) => log::info!("{} is running", simulator.name()),
                None => log::info!("No simulator is running"),
            }

            // The one that stopped lets go of its database
            for simulator in self.simulators.simulators() {
                simulator.release_if_stopped();
            }
        }
    }
}

impl Deref for AppState {
    type Target = Subsystems;

    #[inline(always)]
    fn deref(&self) -> &Self::Target {
        &self.subsystems
    }
}

impl DerefMut for AppState {
    #[inline(always)]
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.subsystems
    }
}

#[derive(Debug, Clone)]
pub struct Subsystems {
    pub audio: AudioSubsystem,
    pub joystick: JoystickSubsystem,
    pub event: EventSubsystem,
}

impl Subsystems {
    fn next_button(&self) -> impl Future<Output = anyhow::Result<Button>> + Send + 'static {
        let watch = ButtonWatcher::new(&self.event);
        return async move {
            let mut watch = watch?;
            return watch
                .next()
                .await
                .context("Unexpected error waiting for next button input");
        };
    }
}

pub struct Gui {
    pub topbar: Topbar,
    chat: Chat,
    onboarding: Onboarding,
    applied_text_scale: Option<f32>,
}

impl Gui {
    fn new(
        pref_path: &Arc<Utf8Path>,
        ss: &Subsystems,
        config: &RawConfig,
        client: &reqwest::Client,
        account: Option<&crate::account::Account>,
    ) -> Self {
        Self {
            topbar: Topbar::new(pref_path, ss, config, client, account),
            chat: Chat::default(),
            onboarding: Onboarding::new(config),
            applied_text_scale: None,
        }
    }

    /// `ui` must be the root `Ui`: edge panels have to be added before the `CentralPanel`,
    /// which then fills the remaining space.
    fn update(
        &mut self,
        rt: &Handle,
        ss: &Subsystems,
        ui: &mut egui::Ui,
        config: &mut Config,
        synth: &mut Synthesizer,
        simulators: &mut ActiveSimulator,
        engineer: &Engineer,
        account: Option<&crate::account::Account>,
    ) -> Option<String> {
        if self
            .topbar
            .update(rt, ss, ui, config, synth, simulators, account)
        {
            self.onboarding.open();
        }
        let question = self.chat.update(ui, engineer);
        self.onboarding.update(
            rt,
            ss,
            ui.ctx(),
            config,
            &mut self.topbar,
            synth,
            simulators,
            engineer,
            account,
        );
        question
    }

    fn event(&mut self, rt: &Handle, config: &Config, event: &Event) -> bool {
        self.topbar.event(rt, config, event)
    }
}

/// Scales every text style relative to egui's defaults, so repeated calls don't compound.
fn set_text_scale(ctx: &egui::Context, scale: f32) {
    let base = egui::style::default_text_styles();
    ctx.all_styles_mut(|style| {
        style.text_styles = base
            .iter()
            .map(|(text_style, font)| {
                let font = egui::FontId::new(font.size * scale, font.family.clone());
                (text_style.clone(), font)
            })
            .collect();
    });
}

#[inline]
fn set_vsync(canvas: &mut WindowCanvas, vsync: c_int) -> Result<(), Error> {
    unsafe extern "C" {
        fn SDL_SetRenderVSync(renderer: *mut SDL_Renderer, vsync: c_int) -> bool;
    }

    match unsafe { SDL_SetRenderVSync(canvas.raw(), vsync) } {
        true => Ok(()),
        false => Err(get_error()),
    }
}

async fn load_config(pref_path: &Utf8Path) -> anyhow::Result<Config> {
    Config::new(pref_path.join_os("config.json")).await
}

fn load_fonts() -> anyhow::Result<egui::FontDefinitions> {
    let mut fonts = egui::FontDefinitions::default();
    egui_phosphor::add_to_fonts(&mut fonts, egui_phosphor::Variant::Bold);

    fonts.font_data.insert(
        "proggy-forever".into(),
        Arc::new(egui::FontData::from_static(include_bytes!("../font.ttf"))),
    );

    // Each character comes from the first font that has it: Proggy only covers Latin-1, Ubuntu adds
    // the rest of the Latin, Greek and Cyrillic scripts and punctuation (dashes, √...), Noto the emoji
    fonts
        .families
        .entry(FontFamily::Monospace)
        .or_default()
        .insert(0, "proggy-forever".into());

    fonts
        .families
        .entry(FontFamily::Proportional)
        .or_default()
        .insert(0, "proggy-forever".into());

    Ok(fonts)
}
