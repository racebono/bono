use crate::{
    button::ConfigButton,
    error,
    synthesizer::{Effects, Synthesizer, SynthesizerInner},
};
use camino::{Utf8Path, Utf8PathBuf};
use sdl3::audio::AudioDeviceID;
use serde::{Deserialize, Deserializer, Serialize};
use std::{
    collections::BTreeMap,
    fmt::{Debug, Display},
    num::NonZeroU32,
    ops::{Deref, DerefMut},
    path::Path,
    sync::Arc,
};
use tokio::{
    io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt},
    runtime::Handle,
    sync::watch::Ref,
};

#[derive(Clone)]
pub struct Config {
    send: tokio::sync::watch::Sender<RawConfig>,
}

impl Config {
    pub async fn new(path: impl AsRef<Path>) -> anyhow::Result<Self> {
        let mut file = tokio::fs::File::options()
            .read(true)
            .write(true)
            .create(true)
            .open(path)
            .await?;

        file.rewind().await?;
        let mut contents = Vec::new();
        if let Ok(len) = usize::try_from(file.metadata().await?.len()) {
            if len == 0 {
                contents.extend_from_slice(b"{}");
            } else {
                contents.try_reserve(len)?;
                file.read_to_end(&mut contents).await?;
            }
        }

        let (send, mut recv) =
            tokio::sync::watch::channel(serde_json::from_slice::<RawConfig>(&contents)?);

        tokio::spawn(async move {
            while recv.changed().await.is_ok() {
                let fut = async {
                    contents.clear();
                    {
                        let raw = recv.borrow();
                        serde_json::to_writer_pretty(&mut contents, &*raw)?;
                    };

                    file.set_len(0).await?;
                    file.rewind().await?;
                    file.write_all(&mut contents).await?;
                    Ok::<_, anyhow::Error>(())
                };

                if let Err(e) = fut.await {
                    error("Error saving config changes", e);
                }
            }
        });

        return Ok(Self { send });
    }

    #[inline]
    pub fn get(&self) -> Ref<'_, RawConfig> {
        self.send.borrow()
    }

    #[inline]
    pub fn update(&self, f: impl FnOnce(&mut RawConfig)) {
        self.send.send_modify(f);
    }

    #[inline]
    pub fn maybe_update(&self, f: impl FnOnce(&mut RawConfig) -> bool) {
        self.send.send_if_modified(f);
    }

    #[inline]
    pub fn watch(&self) -> tokio::sync::watch::Receiver<RawConfig> {
        self.send.subscribe()
    }

    pub(super) fn on_resize(&mut self, width: i32, height: i32) {
        let w = u32::try_from(width).ok().and_then(NonZeroU32::new);
        let h = u32::try_from(height).ok().and_then(NonZeroU32::new);

        let Some((w, h)) = w.zip(h) else {
            log::error!("Invalid window size");
            return;
        };

        self.send.send_modify(|raw| {
            raw.width = w;
            raw.height = h;
        });
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RawConfig {
    #[serde(default = "default_width")]
    pub width: NonZeroU32,
    #[serde(default = "default_height")]
    pub height: NonZeroU32,
    #[serde(default)]
    pub talk_button: Option<ConfigButton>,
    #[serde(default)]
    pub session_button: Option<ConfigButton>,
    #[serde(default)]
    pub microphone: RawAudioDevice<true>,
    #[serde(default)]
    pub speaker: RawAudioDevice<false>,
    #[serde(default = "default_radio_effect")]
    pub radio_effect: f32,
    #[serde(default = "default_volume")]
    pub master_volume: f32,
    #[serde(default = "default_volume")]
    pub engineer_volume: f32,
    #[serde(default = "default_volume")]
    pub spotter_volume: f32,
    #[serde(default = "default_volume")]
    pub beep_volume: f32,
    #[serde(default = "default_true")]
    pub auto_session: bool,
    #[serde(default, deserialize_with = "spotter_config")]
    pub spotter: SpotterConfig,
    #[serde(default = "default_opponent_frequency")]
    pub opponent_frequency: NonZeroU32,
    #[serde(default)]
    voices: BTreeMap<ArrayStr<2>, Utf8PathBuf>,
    #[serde(default = "default_silence_level")]
    pub silence_level: f32,
    #[serde(default)]
    pub whisper_model: Option<Arc<Utf8Path>>,
    #[serde(default = "default_claude_model")]
    pub claude_model: String,
    #[serde(default = "default_claude_effort")]
    pub claude_effort: String,
    #[serde(default = "default_text_scale")]
    pub text_scale: f32,
    #[serde(default = "default_min_confidence")]
    pub min_confidence: f32,
    #[serde(default = "default_skill_path")]
    pub skill_path: Utf8PathBuf,
}

impl RawConfig {
    #[inline]
    pub fn synth_effects(&self) -> Effects {
        Effects {
            radio: self.radio_effect,
            beep_volume: self.beep_volume,
        }
    }

    #[inline]
    pub fn voices(&self) -> &BTreeMap<ArrayStr<2>, Utf8PathBuf> {
        &self.voices
    }

    pub fn set_voice(
        &mut self,
        rt: &Handle,
        lang: ArrayStr<2>,
        model: Option<Utf8PathBuf>,
        synth: &mut Synthesizer,
    ) {
        match model.clone() {
            Some(model) => {
                self.voices.entry(lang).insert_entry(model);
            }
            None => {
                self.voices.remove(&lang);
            }
        }

        synth
            .inner
            .entry(lang)
            .or_insert_with(|| SynthesizerInner::new(rt.clone()))
            .set_model(model);
    }
}

#[macro_export(local_inner_macros)]
macro_rules! arrstr {
    ($s:literal) => {
        const {
            const fn r#impl<const N: usize>() -> $crate::config::ArrayStr<N> {
                match $crate::config::ArrayStr::<N>::from_str($s) {
                    Some(s) => s,
                    None => ::std::panic!("invalid string length"),
                }
            }
            r#impl()
        }
    };
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(transparent)]
pub struct ArrayStr<const N: usize>([u8; N]);

impl<const N: usize> ArrayStr<N> {
    pub const fn from_str(s: &str) -> Option<Self> {
        match s.as_bytes().as_array::<N>() {
            Some(arr) => Some(unsafe { Self::from_utf8_unchecked(*arr) }),
            None => None,
        }
    }

    #[inline]
    pub const unsafe fn from_utf8_unchecked(buf: [u8; N]) -> Self {
        Self(buf)
    }
}

impl<const N: usize> Debug for ArrayStr<N> {
    #[inline]
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        Debug::fmt(self.deref(), f)
    }
}

impl<const N: usize> Display for ArrayStr<N> {
    #[inline]
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        Display::fmt(self.deref(), f)
    }
}

impl<const N: usize> Deref for ArrayStr<N> {
    type Target = str;

    #[inline(always)]
    fn deref(&self) -> &Self::Target {
        unsafe { std::str::from_utf8_unchecked(&self.0) }
    }
}

impl<const N: usize> DerefMut for ArrayStr<N> {
    #[inline(always)]
    fn deref_mut(&mut self) -> &mut Self::Target {
        unsafe { std::str::from_utf8_unchecked_mut(&mut self.0) }
    }
}

impl<const N: usize> Serialize for ArrayStr<N> {
    #[inline]
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        self.deref().serialize(serializer)
    }
}

impl<'de, const N: usize> Deserialize<'de> for ArrayStr<N> {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let s = <&'de str>::deserialize(deserializer)?.as_bytes();
        let arr = s.as_array::<N>().ok_or_else(|| {
            let msg = format!("a {} byte{} UTF-8 string", N, if N == 1 { "" } else { "s" });
            let msg = msg.as_str();
            <D::Error as serde::de::Error>::invalid_length(s.len(), &msg)
        })?;

        return Ok(Self(*arr));
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum AudioDevice {
    Device(AudioDeviceID, Arc<str>),
    Unknown(Arc<str>),
    #[default]
    Default,
}

impl AudioDevice {
    pub fn name(&self) -> &str {
        match self {
            AudioDevice::Device(_, name) => name.deref(),
            AudioDevice::Unknown(name) => name.deref(),
            AudioDevice::Default => "System default",
        }
    }
}

#[derive(Debug, Clone, Default)]
#[repr(transparent)]
pub struct RawAudioDevice<const RECORDING: bool>(AudioDevice);

impl<const RECORDING: bool> Deref for RawAudioDevice<RECORDING> {
    type Target = AudioDevice;

    #[inline(always)]
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl<const RECORDING: bool> DerefMut for RawAudioDevice<RECORDING> {
    #[inline(always)]
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

impl<const RECORDING: bool> Serialize for RawAudioDevice<RECORDING> {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        (match &self.0 {
            AudioDevice::Device(_, name) => Some(name.deref()),
            AudioDevice::Unknown(name) => Some(name.deref()),
            AudioDevice::Default => None,
        })
        .serialize(serializer)
    }
}

impl<'de, const RECORDING: bool> Deserialize<'de> for RawAudioDevice<RECORDING> {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let Some(name) = Option::<String>::deserialize(deserializer)? else {
            return Ok(RawAudioDevice(AudioDevice::Default));
        };

        let audio = sdl3::init()
            .and_then(|sdl| sdl.audio())
            .map_err(<D::Error as serde::de::Error>::custom)?;

        let devices = match RECORDING {
            true => audio.audio_recording_device_ids(),
            false => audio.audio_playback_device_ids(),
        }
        .map_err(<D::Error as serde::de::Error>::custom)?;

        for device in devices {
            log::debug!("{:?} == {name}", device.name());
            if device.name().is_ok_and(|device_name| name == device_name) {
                return Ok(RawAudioDevice(AudioDevice::Device(device, name.into())));
            }
        }

        return Ok(RawAudioDevice(AudioDevice::Unknown(name.into())));
    }
}

#[inline(always)]
const fn default_width() -> NonZeroU32 {
    const { NonZeroU32::new(900).unwrap() }
}

#[inline(always)]
const fn default_height() -> NonZeroU32 {
    const { NonZeroU32::new(600).unwrap() }
}

/// The spotter: calls traffic and flags by itself, the moment they happen. The defaults are
/// my-engineer's (`spotter.cpp`). Distances are in metres, times in seconds.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct SpotterConfig {
    pub enabled: bool,
    /// The language of its calls: the pilot's (the last one they asked in) when `None`
    pub language: Option<ArrayStr<2>>,
    /// How often it looks at the newest telemetry
    pub poll_interval: f64,

    // Cars alongside. A car overlaps when its centre is within `enter_along` of yours along the car,
    // and `min_across` to `enter_across` to the side (beside you rather than in line with you). It
    // stops overlapping past the exit distances, which are wider so a car at the edge doesn't make the
    // spotter chatter
    pub enter_along: f64,
    pub exit_along: f64,
    pub min_across: f64,
    pub enter_across: f64,
    pub exit_across: f64,
    /// Cars further above or below (on a bridge, or under a crossover) are ignored
    pub max_height_difference: f64,
    /// How long a car must overlap before it's called
    pub debounce: f64,
    /// How long a side must have been taken before it's called clear
    pub min_hold_for_clear: f64,
    /// How often "still there" is called while a car stays alongside
    pub still_there_every: f64,
    /// Below this speed (m/s: the garage, the grid, a spin) it stays quiet
    pub min_speed: f64,

    /// A car this many seconds behind or closer is called...
    pub gap_behind_on: f64,
    /// ...again only once the gap has opened to at least this
    pub gap_behind_off: f64,

    // Weather, from 0 to 1 (about five updates a second). A call when the value moves into another
    // band, by at least `band_margin` past its edge, so a value sitting on an edge stays quiet
    /// Rain: none | light | moderate | heavy
    pub rain_bands: [f64; 3],
    /// Track wetness: dry | damp | wet | soaked
    pub wet_bands: [f64; 3],
    pub band_margin: f64,
    /// Rain on the way: a forecast point at least this likely to bring rain...
    pub forecast_rain_chance: f64,
    /// ...this many minutes ahead or less, while it isn't raining yet
    pub forecast_rain_lead_minutes: f64,
    /// Between two rain calls, and between two track calls
    pub weather_repeat: f64,
    /// Between two "wet patch" calls
    pub wet_patch_repeat: f64,
}

impl Default for SpotterConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            language: None,
            poll_interval: 0.033,
            enter_along: 4.8,
            exit_along: 5.8,
            min_across: 0.8,
            enter_across: 5.5,
            exit_across: 6.5,
            max_height_difference: 4.0,
            debounce: 0.12,
            min_hold_for_clear: 0.4,
            still_there_every: 6.0,
            min_speed: 8.0,
            gap_behind_on: 1.0,
            gap_behind_off: 1.5,
            rain_bands: [0.05, 0.3, 0.6],
            wet_bands: [0.05, 0.25, 0.6],
            band_margin: 0.03,
            forecast_rain_chance: 0.5,
            forecast_rain_lead_minutes: 10.0,
            weather_repeat: 60.0,
            wet_patch_repeat: 30.0,
        }
    }
}

/// `spotter` used to be only whether it's on (`true` or `false`): still read, with the defaults.
fn spotter_config<'de, D>(deserializer: D) -> Result<SpotterConfig, D::Error>
where
    D: Deserializer<'de>,
{
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Stored {
        Enabled(bool),
        Config(SpotterConfig),
    }

    Ok(match Stored::deserialize(deserializer)? {
        Stored::Enabled(enabled) => SpotterConfig {
            enabled,
            ..SpotterConfig::default()
        },
        Stored::Config(config) => config,
    })
}

#[inline(always)]
const fn default_radio_effect() -> f32 {
    0.65
}

#[inline(always)]
const fn default_volume() -> f32 {
    1.0
}

#[inline(always)]
const fn default_text_scale() -> f32 {
    1.3
}

#[inline(always)]
const fn default_true() -> bool {
    true
}

#[inline(always)]
const fn default_opponent_frequency() -> NonZeroU32 {
    const { NonZeroU32::new(5).unwrap() }
}

#[inline(always)]
const fn default_silence_level() -> f32 {
    0.003
}

#[inline(always)]
const fn default_min_confidence() -> f32 {
    0.1
}

#[inline(always)]
fn default_claude_model() -> String {
    String::from("claude-sonnet-5-5")
}

#[inline(always)]
fn default_claude_effort() -> String {
    String::from("high")
}

fn default_skill_path() -> Utf8PathBuf {
    Utf8PathBuf::from("skill")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_the_spotter_config() {
        let defaults = serde_json::from_str::<RawConfig>("{}").unwrap();
        assert_eq!(defaults.spotter, SpotterConfig::default());

        // Older configs only said whether it's on
        let off = serde_json::from_str::<RawConfig>(r#"{"spotter": false}"#).unwrap();
        assert!(!off.spotter.enabled);
        assert_eq!(off.spotter.debounce, SpotterConfig::default().debounce);

        // A partial one keeps the defaults for the rest
        let tuned = serde_json::from_str::<RawConfig>(
            r#"{"spotter": {"debounce": 0.2, "language": "es"}}"#,
        )
        .unwrap();
        assert!(tuned.spotter.enabled);
        assert_eq!(tuned.spotter.debounce, 0.2);
        assert_eq!(tuned.spotter.language, Some(crate::arrstr!("es")));
        assert_eq!(tuned.spotter.enter_along, 4.8);

        // And it's written back whole
        let written = serde_json::to_value(&tuned).unwrap();
        assert_eq!(written["spotter"]["debounce"], 0.2);
        assert_eq!(written["spotter"]["rain_bands"][1], 0.3);
    }

    #[test]
    fn keeps_the_spotter_config_while_disabled() {
        let disabled = serde_json::from_str::<RawConfig>(
            r#"{"spotter": {"enabled": false, "debounce": 0.2, "language": "es"}}"#,
        )
        .unwrap();

        // Saved and loaded again (as the app does on every change), switched off but still tuned
        let saved = serde_json::to_string(&disabled).unwrap();
        let loaded = serde_json::from_str::<RawConfig>(&saved).unwrap();
        assert!(!loaded.spotter.enabled);
        assert_eq!(loaded.spotter.debounce, 0.2);
        assert_eq!(loaded.spotter.language, Some(crate::arrstr!("es")));
        assert_eq!(loaded.spotter, disabled.spotter);
    }
}
