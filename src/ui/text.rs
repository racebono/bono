use camino::Utf8Path;
use sdl3::{
    render::WindowCanvas,
    ttf::{self, Font, Text, TextEngine},
};

pub struct TextContext {
    font: Font<'static>,
    engine: TextEngine,
}

impl TextContext {
    pub(super) fn new(assets_dir: &Utf8Path, canvas: &WindowCanvas) -> anyhow::Result<Self> {
        let ttf = ttf::init()?;
        let font = ttf.load_font(assets_dir.join_os("ProggyForever-Regular.ttf"), 26.0)?;
        let engine = TextEngine::new(&canvas.texture_creator())?;
        return Ok(Self { font, engine });
    }

    pub fn text(&self, text: &str) -> anyhow::Result<Text> {
        return Ok(self.engine.create_text(
            &self.font,
            match text.is_empty() {
                true => &"\0"[0..0],
                false => text,
            },
        )?);
    }
}
