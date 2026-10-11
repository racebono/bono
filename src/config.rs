use crate::{button::Button, error};
use camino::Utf8PathBuf;
use sdl3::audio::AudioDeviceID;
use serde::{Deserialize, Serialize};
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
    sync::watch::Ref,
};

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
    pub fn update(&mut self, f: impl FnOnce(&mut RawConfig)) {
        self.send.send_modify(f);
    }

    #[inline]
    pub fn maybe_update(&mut self, f: impl FnOnce(&mut RawConfig) -> bool) {
        self.send.send_if_modified(f);
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
    pub talk_button: Option<Button>,
    #[serde(default)]
    pub session_button: Option<Button>,
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
    #[serde(default = "default_true")]
    pub spotter: bool,
    #[serde(default)]
    pub spotter_language: Option<ArrayStr<2>>,
    #[serde(default = "default_opponent_frequency")]
    pub opponent_frequency: NonZeroU32,
    #[serde(default)]
    pub voices: BTreeMap<ArrayStr<2>, Utf8PathBuf>,
    #[serde(default = "default_silence_level")]
    pub silence_level: f32,
    #[serde(default)]
    pub whisper_model: Option<Utf8PathBuf>,
    #[serde(default = "default_claude_model")]
    pub claude_model: String,
    #[serde(default = "default_claude_effort")]
    pub claude_effort: String,
    #[serde(default = "default_text_scale")]
    pub text_scale: f32,
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(transparent)]
pub struct ArrayStr<const N: usize>([u8; N]);

impl<const N: usize> ArrayStr<N> {
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

        let devices = ((|| {
            let audio = sdl3::init()?.audio()?;
            match RECORDING {
                true => audio.audio_recording_device_ids(),
                false => audio.audio_playback_device_ids(),
            }
        })())
        .map_err(<D::Error as serde::de::Error>::custom)?;

        for device in devices {
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
    1.2
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
fn default_claude_model() -> String {
    String::from("claude-sonnet-5-5")
}

#[inline(always)]
fn default_claude_effort() -> String {
    String::from("high")
}
