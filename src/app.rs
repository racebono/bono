use crate::{
    app::{chat::Chat, topbar::Topbar},
    button::{Button, ButtonWatcher},
    config::{Config, RawConfig},
    engineer::Engineer,
    sim::{ActiveSimulator, Sim, SimulatorSettings},
    spotter::Spotter,
    synthesizer::Synthesizer,
    task::MaybeDone,
    track::TrackSync,
};
use anyhow::Context as AnyhowContext;
use camino::{Utf8Path, Utf8PathBuf};
use egui::{FontFamily, FontTweak};
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
use tokio::{runtime::Handle, try_join};

mod chat;
mod topbar;

pub struct App {
    state: AppState,
    ui: EguiCanvas,
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
        let event = sdl.event()?;

        let (config, fonts) = try_join!(load_config(&pref_path), load_fonts(&assets_path))?;
        let (width, height) = {
            let config = config.get();
            (*config.width(), *config.height())
        };

        let mut ui = EguiCanvas::new(
            video
                .window("Lambiase", width.get(), height.get())
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
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
            .build()?;
        let tracks = TrackSync::new(client.clone(), pref_path.as_std_path());

        let guard = config.get();
        let gui = Gui::new(&pref_path, &subsystems, &guard, &client);
        let synth = Synthesizer::new(rt, &guard);
        let (tracks, engineer) = tokio::try_join!(
            async move { Ok(tracks.await) },
            Engineer::new(assets_path.as_std_path(), &guard, client)
        )?;
        drop(guard);

        return Ok(Self {
            state: AppState {
                subsystems,
                config,
                gui,
                synth,
                simulators: ActiveSimulator::default(),
                applied_opponent_frequency: None,
                tracks,
                engineer,
                spotter: Spotter::new(),
            },
            ui,
        });
    }

    pub fn iterate(&mut self, rt: &Handle) -> AppResult {
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

    pub fn event(&mut self, rt: &Handle, event: Event) -> AppResult {
        if self.ui.on_event(&event).consumed {
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
    gui: Gui,
    config: Config,
    subsystems: Subsystems,
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
    fn update(&mut self, rt: &Handle, ui: &mut egui::Ui) {
        self.update_simulators();
        self.tracks.update(rt, self.simulators.current_mut());

        // What the pilot says on the radio goes to the engineer, and its answers come back
        if let Some((question, lang)) = self.gui.topbar.audio.transcription(rt) {
            self.engineer.ask(
                rt,
                self.simulators.current_mut(),
                &self.config,
                &mut self.synth,
                question,
                lang,
            );
        }
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
        let simulator = self.simulators.current_mut();
        let typed = self.gui.update(
            rt,
            &self.subsystems,
            ui,
            &mut self.config,
            &mut self.synth,
            simulator,
            &self.engineer,
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
    applied_text_scale: Option<f32>,
}

impl Gui {
    fn new(
        pref_path: &Arc<Utf8Path>,
        ss: &Subsystems,
        config: &RawConfig,
        client: &reqwest::Client,
    ) -> Self {
        Self {
            topbar: Topbar::new(pref_path, ss, config, client),
            chat: Chat::default(),
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
        simulator: Option<&mut Sim>,
        engineer: &Engineer,
    ) -> Option<String> {
        self.topbar.update(rt, ss, ui, config, synth, simulator);
        self.chat.update(ui, engineer)
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

async fn load_fonts(assets_path: &Utf8Path) -> anyhow::Result<egui::FontDefinitions> {
    let mut fonts = egui::FontDefinitions::default();
    let (proggy,) = tokio::try_join!(tokio::fs::read(
        assets_path.join_os("ProggyForever-Regular.ttf")
    ),)?;

    fonts.font_data.insert(
        "proggy-forever".into(),
        Arc::new(egui::FontData::from_owned(proggy)),
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
