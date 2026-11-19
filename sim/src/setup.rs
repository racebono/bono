//! The player's garage setup, as the game shows it: each setting's name and display text, for the
//! `setup_set` and `setup` tables (see `sql`). Only iRacing and LMU report one.

/// Where a setup was read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SetupSource {
    /// In the garage, or the first one read
    Garage,
    /// After a pit stop
    PitStop,
}

impl SetupSource {
    /// As stored in `setup_set.source`.
    pub fn as_str(self) -> &'static str {
        match self {
            SetupSource::Garage => "garage",
            SetupSource::PitStop => "pit stop",
        }
    }
}

/// One setting of the setup.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Setting {
    /// The game's own name: iRacing `Chassis/LeftFront/Camber`, LMU `WM_CAMBER-W_FL`
    pub name: String,
    /// The game's display text, trimmed (`-3.2 deg`, `P5`)
    pub value: String,
    /// LMU only: the step it's on
    pub step: Option<i64>,
    /// LMU only: the lowest step
    pub min_step: Option<i64>,
    /// LMU only: the top step
    pub max_step: Option<i64>,
    /// LMU only: what the loaded setup file says
    pub saved_value: Option<String>,
}

/// The setup on the player's car.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Setup {
    /// The setup file it was loaded from
    pub file_name: Option<String>,
    /// Whether it was changed since it was loaded
    pub modified: Option<bool>,
    pub source: Option<SetupSource>,
    /// Each name once
    pub settings: Vec<Setting>,
}
