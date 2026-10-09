use crate::app::App;
use sdl3::{
    event::Event,
    messagebox::{MessageBoxFlag, show_simple_message_box},
    sys::events::SDL_Event,
};
use sdl3_main::{AppResult, MainThreadData, MainThreadToken, app_impl};
use std::{
    cell::RefCell,
    hint::unreachable_unchecked,
    sync::atomic::{AtomicU8, Ordering},
};
use tokio::runtime::Runtime;

mod app;
mod button;
mod config;
mod task;
mod ui;

struct AppImpl {
    app: MainThreadData<RefCell<App>>,
    rt: Runtime,
}

#[app_impl]
impl AppImpl {
    fn app_init() -> Option<Box<Self>> {
        match init(MainThreadToken::get()?) {
            Ok(app) => Some(Box::new(app)),
            Err(e) => {
                log::error!("Error initializing application: {e}");
                let _ =
                    show_simple_message_box(MessageBoxFlag::ERROR, "Error", &e.to_string(), None);
                None
            }
        }
    }

    fn app_iterate(&self) -> AppResult {
        let Some(token) = MainThreadToken::get() else {
            return AppResult::Continue;
        };

        let _guard = self.rt.enter();
        return self.app.get(token).borrow_mut().iterate();
    }

    fn app_event(&self, event: &SDL_Event) -> AppResult {
        const CONTINUE: u8 = AppResult::Continue as u8;
        const SUCCESS: u8 = AppResult::Success as u8;
        const FAILURE: u8 = AppResult::Failure as u8;

        let event = Event::from_ll(*event);
        let result = AtomicU8::new(CONTINUE);
        let handle = self.rt.handle().clone();

        if !self.app.get_on_main_thread(|app| {
            let _guard = handle.enter();
            result.store(
                match app.borrow_mut().event(event) {
                    AppResult::Continue => CONTINUE,
                    AppResult::Success => SUCCESS,
                    AppResult::Failure => FAILURE,
                },
                Ordering::Release,
            );
        }) {
            log::error!("Unexpected error handling event");
            let _ = show_simple_message_box(
                MessageBoxFlag::ERROR,
                "Error",
                "Unexpected error handling event",
                None,
            );
            return AppResult::Failure;
        }

        return match result.load(Ordering::Acquire) {
            CONTINUE => AppResult::Continue,
            SUCCESS => AppResult::Success,
            FAILURE => AppResult::Failure,
            _ => unsafe { unreachable_unchecked() },
        };
    }
}

fn init(token: MainThreadToken) -> anyhow::Result<AppImpl> {
    let rt = Runtime::new()?;
    let app = MainThreadData::new(token, RefCell::new(rt.block_on(App::new())?));
    Ok(AppImpl { app, rt })
}
