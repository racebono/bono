use crate::{
    app::topbar::Topbar,
    button::{Button, ButtonWatcher, ConfigButton},
    config::{Config, RawConfig},
};
use anyhow::Context as AnyhowContext;
use camino::{Utf8Path, Utf8PathBuf};
use egui_sdl3::EguiCanvas;
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
    ops::{Deref, DerefMut},
    sync::Arc,
};
use tokio::{runtime::Runtime, try_join};

mod topbar;

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
        log::debug!("Base Path: {base_path}");

        let pref_path = Utf8PathBuf::try_from(get_pref_path(
            env!("CARGO_PKG_NAME"),
            env!("CARGO_PKG_NAME"),
        )?)?;
        log::debug!("Prefered Path: {pref_path}");

        let (config, fonts) = try_join!(load_config(&pref_path), load_fonts(&base_path))?;
        let sdl = sdl3::init()?;
        let audio = sdl.audio()?;
        let video = sdl.video()?;
        let joystick = sdl.joystick()?;
        let event = sdl.event()?;

        let (width, height) = {
            let config = config.get();
            (config.width, config.height)
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

        let guard = config.get();
        let gui = Gui::new(&subsystems, &guard);
        drop(guard);

        return Ok(Self {
            state: AppState {
                subsystems,
                base_path,
                pref_path,
                config,
                gui,
            },
            ui,
        });
    }

    pub fn iterate(&mut self, rt: &Runtime) -> AppResult {
        self.ui.run(|ctx| {
            // Must happen before the root `Ui` is built, since it copies the style on creation
            let text_scale = self.state.config.get().text_scale;
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

    pub fn event(&mut self, event: Event) -> AppResult {
        if !self.ui.on_event(&event).consumed {
            match event {
                Event::Window {
                    window_id,
                    win_event,
                    ..
                } if window_id == self.ui.painter.canvas.window().id() => match win_event {
                    WindowEvent::Resized(w, h) => self.config.on_resize(w, h),
                    _ => {}
                },
                Event::JoyButtonDown {
                    timestamp,
                    which,
                    button_idx,
                } => {
                    todo!()
                }
                Event::Quit { .. } => return AppResult::Success,
                _ => {}
            }
        }
        AppResult::Continue
    }

    fn test_button(&self, button: ConfigButton) -> bool {
        todo!()
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
    gui: Gui,
    config: Config,
    subsystems: Subsystems,
}

impl AppState {
    fn update(&mut self, rt: &Runtime, ui: &mut egui::Ui) {
        self.gui.update(rt, &self.subsystems, ui, &mut self.config);
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
    applied_text_scale: Option<f32>,
}

impl Gui {
    fn new(ss: &Subsystems, config: &RawConfig) -> Self {
        Self {
            topbar: Topbar::new(ss, config),
            applied_text_scale: None,
        }
    }

    /// `ui` must be the root `Ui`: edge panels have to be added before the `CentralPanel`,
    /// which then fills the remaining space.
    fn update(&mut self, rt: &Runtime, ss: &Subsystems, ui: &mut egui::Ui, config: &mut Config) {
        self.topbar.update(rt, ss, ui, config);
        egui::CentralPanel::default().show(ui, |_ui| {});
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

async fn load_fonts(base_path: &Utf8Path) -> anyhow::Result<egui::FontDefinitions> {
    let mut fonts = egui::FontDefinitions::default();
    let data = egui::FontData::from_owned(
        tokio::fs::read(base_path.join_os("assets/ProggyForever-Regular.ttf")).await?,
    );

    fonts
        .font_data
        .insert("proggy-forever".into(), Arc::new(data));

    // Ponerla la primera en Proportional (texto normal)
    fonts
        .families
        .entry(egui::FontFamily::Proportional)
        .or_default()
        .insert(0, "proggy-forever".into());

    // Opcional: también para Monospace
    fonts
        .families
        .entry(egui::FontFamily::Monospace)
        .or_default()
        .insert(0, "proggy-forever".into());

    Ok(fonts)
}
