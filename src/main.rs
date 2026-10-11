use crate::{app::App, config::ArrayStr};
use sdl3::{
    event::Event,
    messagebox::{MessageBoxFlag, show_simple_message_box},
    sys::{events::SDL_Event, guid::SDL_GUID},
};
use sdl3_main::{AppResult, MainThreadData, MainThreadToken, app_impl};
use simple_logger::SimpleLogger;
use std::{cell::RefCell, fmt::Display};
use tokio::runtime::Runtime;

mod app;
mod button;
mod config;
mod task;
mod whisper;

struct AppImpl {
    app: MainThreadData<RefCell<App>>,
    rt: Runtime,
}

#[app_impl]
impl AppImpl {
    fn app_init() -> Option<Box<Self>> {
        SimpleLogger::new().init().unwrap();
        match init(MainThreadToken::get()?) {
            Ok(app) => Some(Box::new(app)),
            Err(e) => {
                error_blocking("Error initializing application", e);
                None
            }
        }
    }

    fn app_iterate(&self) -> AppResult {
        let Some(token) = MainThreadToken::get() else {
            return AppResult::Continue;
        };

        let _guard = self.rt.enter();
        return self.app.get(token).borrow_mut().iterate(&self.rt);
    }

    fn app_event(&self, event: &SDL_Event) -> AppResult {
        let event = Event::from_ll(*event);
        let mut result = AppResult::Continue;
        let handle = self.rt.handle().clone();

        if !self.app.get_on_main_thread(|app| {
            let _guard = handle.enter();
            result = app.borrow_mut().event(event);
        }) {
            error_blocking("Error", "Unexpected error handling event");
            return AppResult::Failure;
        }

        return result;
    }
}

fn init(token: MainThreadToken) -> anyhow::Result<AppImpl> {
    let rt = Runtime::new()?;
    let app = MainThreadData::new(token, RefCell::new(rt.block_on(App::new())?));
    Ok(AppImpl { app, rt })
}

pub(crate) fn error(title: &'static str, message: impl Display) {
    let message = message.to_string();
    log::error!("{title}: {message}");
    std::thread::spawn(move || {
        let _ = show_simple_message_box(MessageBoxFlag::ERROR, title, &message, None);
    });
}

pub(crate) fn error_blocking(title: &str, message: impl Display) {
    let message = message.to_string();
    log::error!("{title}: {message}");
    let _ = show_simple_message_box(MessageBoxFlag::ERROR, title, &message, None);
}

pub(crate) fn guid_to_str(guid: &SDL_GUID) -> ArrayStr<32> {
    const HEX: &[u8; 16] = b"0123456789abcdef";

    #[inline(always)]
    fn out(buf: &mut [u8; 32], i: usize) -> &mut u8 {
        unsafe { buf.get_unchecked_mut(i) }
    }

    #[inline(always)]
    fn hex(i: usize) -> u8 {
        unsafe { *HEX.get_unchecked(i) }
    }

    let mut inner = [0u8; 32];
    for (i, &b) in guid.data.iter().enumerate() {
        *out(&mut inner, 2 * i) = hex((b >> 4) as usize);
        *out(&mut inner, 2 * i + 1) = hex((b & 0x0F) as usize);
    }

    return unsafe { ArrayStr::from_utf8_unchecked(inner) };
}
