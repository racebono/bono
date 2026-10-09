use std::io::ErrorKind;

use crate::{
    button::{Button, ButtonWatcher},
    config::Config,
};
use anyhow::Context;
use camino::Utf8PathBuf;
use sdl3::{
    AudioSubsystem, EventSubsystem, JoystickSubsystem, VideoSubsystem,
    event::Event,
    filesystem::get_pref_path,
    get_error,
    messagebox::{MessageBoxFlag, show_simple_message_box},
    render::WindowCanvas,
};
use sdl3_main::AppResult;

pub struct App {
    pref_path: Utf8PathBuf,
    config: Config,
    audio: AudioSubsystem,
    video: VideoSubsystem,
    joystick: JoystickSubsystem,
    event: EventSubsystem,
    canvas: WindowCanvas,
}

impl App {
    pub async fn new() -> anyhow::Result<Self> {
        let pref_path = Utf8PathBuf::try_from(get_pref_path(
            env!("CARGO_PKG_NAME"),
            env!("CARGO_PKG_NAME"),
        )?)?;

        let config = {
            let data = match tokio::fs::read(pref_path.join("config.json")).await {
                Ok(data) => data,
                Err(ref e) if e.kind() == ErrorKind::NotFound => Vec::from(b"{}"),
                Err(e) => return Err(e.into()),
            };
            serde_json::from_slice::<Config>(&data)
        }?;

        let sdl = sdl3::init()?;
        let audio = sdl.audio()?;
        let video = sdl.video()?;
        let joystick = sdl.joystick()?;
        let event = sdl.event()?;

        let canvas =
            video.window_and_renderer("Lambiese", config.width.get(), config.height.get())?;

        return Ok(Self {
            pref_path,
            config,
            audio,
            video,
            joystick,
            event,
            canvas,
        });
    }

    pub fn iterate(&mut self) -> AppResult {
        self.canvas.clear();

        // FIXME(TODO): Render Window

        if !self.canvas.present() {
            let e = get_error();
            log::error!("Unexpected error presenting frame: {e}");
            let _ = show_simple_message_box(
                MessageBoxFlag::ERROR,
                "Unexpected error presenting frame",
                &e.to_string(),
                self.canvas.window(),
            );
            return AppResult::Failure;
        }

        AppResult::Continue
    }

    pub fn event(&mut self, event: Event) -> AppResult {
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
