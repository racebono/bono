use crate::ui::text::TextContext;
use camino::Utf8Path;
use sdl3::render::WindowCanvas;

pub mod text;

pub struct Context {
    pub text: TextContext,
}

impl Context {
    pub(super) fn new(assets_dir: &Utf8Path, canvas: &WindowCanvas) -> anyhow::Result<Self> {
        return Ok(Self {
            text: TextContext::new(assets_dir, canvas)?,
        });
    }
}
