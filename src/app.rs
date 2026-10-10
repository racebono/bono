use crate::{
    button::{Button, ButtonWatcher},
    config::Config,
};
use anyhow::Context as AnyhowContext;
use camino::{Utf8Path, Utf8PathBuf};
use egui_sdl3::EguiCanvas;
use sdl3::{
    AudioSubsystem, Error, EventSubsystem, JoystickSubsystem, VideoSubsystem,
    event::Event,
    filesystem::get_pref_path,
    get_error,
    messagebox::{MessageBoxFlag, show_simple_message_box},
    render::{Canvas, WindowCanvas},
    surface::Surface,
    sys::render::SDL_Renderer,
    version::Version,
};
use sdl3_main::AppResult;
use std::{
    ffi::c_int,
    io::ErrorKind,
    ops::{Deref, DerefMut},
};

pub struct App {
    state: AppState,
    ui: EguiCanvas,
}

impl App {
    pub async fn new() -> anyhow::Result<Self> {
        let base_path = cfg_select! {
            debug_assertions => Utf8Path::new(env!("CARGO_MANIFEST_DIR")),
            _ => <&'static Utf8Path>::try_from(sdl3::filesystem::get_base_path()?)?
        };
        let pref_path = Utf8PathBuf::try_from(get_pref_path(
            env!("CARGO_PKG_NAME"),
            env!("CARGO_PKG_NAME"),
        )?)?;

        let config = {
            match tokio::fs::read(pref_path.join("config.json")).await {
                Ok(data) => serde_json::from_slice::<Config>(&data),
                Err(ref e) if e.kind() == ErrorKind::NotFound => {
                    serde_json::from_slice::<Config>(b"{}")
                }
                Err(e) => return Err(e.into()),
            }
        }?;

        let sdl = sdl3::init()?;
        let audio = sdl.audio()?;
        let video = sdl.video()?;
        let joystick = sdl.joystick()?;
        let event = sdl.event()?;

        let mut ui = EguiCanvas::new(
            video
                .window("Lambiase", config.width.get(), config.height.get())
                .resizable()
                .high_pixel_density()
                .build()?,
        );
        set_vsync(&mut ui.painter.canvas, 1)?;

        return Ok(Self {
            state: AppState {
                base_path,
                pref_path,
                config,
                audio,
                video,
                joystick,
                event,
            },
            ui,
        });
    }

    pub fn iterate(&mut self) -> AppResult {
        self.ui.run(|ctx| self.state.update(ctx));
        self.ui.paint();
        self.ui.present();
        AppResult::Continue
    }

    pub fn event(&mut self, event: Event) -> AppResult {
        self.ui.on_event(&event);
        match event {
            Event::Quit { .. } => return AppResult::Success,
            _ => {}
        }
        return AppResult::Continue;
    }
}

impl App {
    pub fn next_button(&self) -> impl Future<Output = anyhow::Result<Button>> + Send + 'static {
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
    base_path: &'static Utf8Path,
    pref_path: Utf8PathBuf,
    config: Config,
    audio: AudioSubsystem,
    video: VideoSubsystem,
    joystick: JoystickSubsystem,
    event: EventSubsystem,
}

impl AppState {
    fn update(&mut self, ctx: &egui::Context) {
        // egui-sdl3 only hands us the `Context`, so build a root `Ui` the same way `Context::run_ui` does
        let mut root_ui = egui::Ui::new(
            ctx.clone(),
            egui::Id::new("root"),
            egui::UiBuilder::new()
                .layer_id(egui::LayerId::background())
                .max_rect(ctx.viewport_rect()),
        );
        egui::CentralPanel::default().show(&mut root_ui, |ui| {
            ui.label("Hello world!");
        });
    }
}

unsafe extern "C" {
    fn SDL_SetRenderVSync(renderer: *mut SDL_Renderer, vsync: c_int) -> bool;
}

#[inline]
fn set_vsync(canvas: &mut WindowCanvas, vsync: c_int) -> Result<(), Error> {
    match unsafe { SDL_SetRenderVSync(canvas.raw(), vsync) } {
        true => Ok(()),
        false => Err(get_error()),
    }
}
