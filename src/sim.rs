use crate::sqlite::pool::Pool;
use serde::{Deserialize, Serialize};
use std::num::NonZeroU32;

#[cfg(windows)]
mod lmu;

pub trait Simulator {
    fn name(&self) -> &'static str;
    fn track_data_id(&self) -> &'static str;
    fn database(&mut self) -> Option<&Pool>;
    fn apply(&mut self, settings: &SimulatorSettings);
    fn clear(&mut self);
}

#[derive(Debug, Serialize, Deserialize)]
pub struct SimulatorSettings {
    pub opponent_frequency: NonZeroU32,
}
