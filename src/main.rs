use crate::app::App;
use sdl3::{
    event::Event,
    messagebox::{MessageBoxFlag, show_simple_message_box},
    sys::events::SDL_Event,
};
use sdl3_main::{AppResult, MainThreadData, MainThreadToken, app_impl};
use simple_logger::SimpleLogger;
use std::cell::RefCell;
use tokio::runtime::Runtime;

mod app;
mod button;
mod config;
mod task;

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
        let event = Event::from_ll(*event);
        let mut result = AppResult::Continue;
        let handle = self.rt.handle().clone();

        if !self.app.get_on_main_thread(|app| {
            let _guard = handle.enter();
            result = app.borrow_mut().event(event);
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

        return result;
    }
}

fn init(token: MainThreadToken) -> anyhow::Result<AppImpl> {
    let rt = Runtime::new()?;
    let app = MainThreadData::new(token, RefCell::new(rt.block_on(App::new())?));
    Ok(AppImpl { app, rt })
}
