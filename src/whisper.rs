use crate::task::spawn_blocking;
use std::path::Path;
use whisper_rs::{WhisperContext, WhisperContextParameters};

pub struct Whisper {}

impl Whisper {
    pub async fn new(model: impl AsRef<Path> + Send + 'static) -> anyhow::Result<Self> {
        let ctx = spawn_blocking(move || {
            let ctx = WhisperContext::new_with_params(path, WhisperContextParameters::default())?;
            let state = ctx.create_state()?;
        })
        .await?;

        todo!()
    }
}
