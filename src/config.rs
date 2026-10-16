use crate::{
    button::ConfigButton,
    error,
    i18n::{self, set_locale},
    synthesizer::{Effects, Synthesizer, SynthesizerInner},
};
use camino::{Utf8Path, Utf8PathBuf};
use lambiase_i18n::t;
use sdl3::audio::AudioDeviceID;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
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

        let mut data = serde_json::from_slice::<RawConfig>(&contents)?;
        let changed = !set_locale(data.locale());

        if changed {
            log::warn!("Invalid config locale.");
            data.locale.set("en-US".into());
        }

        let (send, mut recv) = tokio::sync::watch::channel(data);
        if changed {
            recv.mark_changed();
        }

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
                    error(t!("error.saving_config"), e);
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

    pub(super) fn on_resize(&mut self, width: i32, height: i32) {
        let w = u32::try_from(width).ok().and_then(NonZeroU32::new);
        let h = u32::try_from(height).ok().and_then(NonZeroU32::new);

        let Some((w, h)) = w.zip(h) else {
            log::error!("Invalid window size");
            return;
        };

        self.send.send_modify(|raw| {
            raw.width.set(w);
            raw.height.set(h);
        });
    }
}

#[derive(Debug, Clone, Default, PartialEq)]
pub enum Setting<T> {
    #[default]
    Default,
    Set(T),
}

impl<T> Setting<T> {
    pub fn is_default(&self) -> bool {
        matches!(self, Setting::Default)
    }

    pub fn get_or<'a>(&'a self, default: &'a T) -> &'a T {
        match self {
            Setting::Default => default,
            Setting::Set(v) => v,
        }
    }

    #[inline]
    pub fn set(&mut self, value: T) {
        *self = Setting::Set(value);
    }

    /// Edits a copy of the `current` value (e.g. with a widget), which is set only if `edit`
    /// says it changed. Returns whether it did.
    pub fn edit(&mut self, mut current: T, edit: impl FnOnce(&mut T) -> bool) -> bool {
        let changed = edit(&mut current);
        if changed {
            self.set(current);
        }
        changed
    }
}

/// Whether nothing in it has been set: what isn't, isn't saved.
pub trait IsDefault {
    fn is_default(&self) -> bool;
}

impl<T> IsDefault for Setting<T> {
    #[inline]
    fn is_default(&self) -> bool {
        Setting::is_default(self)
    }
}

/// A group of settings. Each is a [`Setting`], saved only once it's been set, and read with the
/// method of the same name, which gives its default until then: a default changed here reaches
/// everyone who never set it. Its `groups` of settings are saved only once one of theirs has
/// been set.
macro_rules! settings {
    (
        $(#[$meta:meta])*
        $vis:vis struct $name:ident {
            $(
                $(#[$field_meta:meta])*
                $field_vis:vis $field:ident: $ty:ty = $default:expr,
            )*
        }
        $(
            groups {
                $(
                    $(#[$group_meta:meta])*
                    $group:ident: $group_ty:ty,
                )*
            }
        )?
    ) => {
        $(#[$meta])*
        #[derive(Clone, Default, Serialize, Deserialize)]
        $vis struct $name {
            $(
                $(#[$field_meta])*
                #[serde(default, skip_serializing_if = "Setting::is_default")]
                $field_vis $field: Setting<$ty>,
            )*
            $($(
                $(#[$group_meta])*
                #[serde(default, skip_serializing_if = "IsDefault::is_default")]
                pub $group: $group_ty,
            )*)?
        }

        impl $name {
            $(
                $(#[$field_meta])*
                pub fn $field(&self) -> &$ty {
                    static DEFAULT: std::sync::LazyLock<$ty> = std::sync::LazyLock::new(|| $default);
                    self.$field.get_or(&DEFAULT)
                }
            )*
        }

        impl IsDefault for $name {
            fn is_default(&self) -> bool {
                true $(&& self.$field.is_default())* $($(&& self.$group.is_default())*)?
            }
        }
    };
}

impl<T> Serialize for Setting<T>
where
    T: Serialize,
{
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        match self {
            Setting::Set(v) => v.serialize(s),
            // Only reached if someone forgets skip_serializing_if
            Setting::Default => Err(<S::Error as serde::ser::Error>::custom(
                "tried to serialize default value",
            )),
        }
    }
}

impl<'de, T> Deserialize<'de> for Setting<T>
where
    T: Deserialize<'de>,
{
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        // Only called when the field is present; absence goes through #[serde(default)]
        T::deserialize(d).map(Setting::Set)
    }
}

settings! {
    #[derive(Debug)]
    pub struct RawConfig {
        locale: String = "en-US".into(),
        pub width: NonZeroU32 = NonZeroU32::new(900).unwrap(),
        pub height: NonZeroU32 = NonZeroU32::new(600).unwrap(),
        pub talk_button: Option<ConfigButton> = None,
        pub session_button: Option<ConfigButton> = None,
        pub microphone: RawAudioDevice<true> = RawAudioDevice::default(),
        pub speaker: RawAudioDevice<false> = RawAudioDevice::default(),
        pub radio_effect: f32 = 0.65,
        pub master_volume: f32 = 1.0,
        pub engineer_volume: f32 = 1.0,
        pub spotter_volume: f32 = 1.0,
        pub beep_volume: f32 = 1.0,
        pub auto_session: bool = true,
        pub opponent_frequency: NonZeroU32 = NonZeroU32::new(5).unwrap(),
        /// Changed with `set_voice`, which loads the voice too
        voices: BTreeMap<ArrayStr<2>, Utf8PathBuf> = BTreeMap::new(),
        pub whisper_model: Option<Arc<Utf8Path>> = None,
        pub text_scale: f32 = 1.3,
        pub min_confidence: f32 = 0.1,
        pub skill_path: Utf8PathBuf = Utf8PathBuf::from("skill"),
    }
    groups {
        spotter: SpotterConfig,
        model: ModelConfig,
    }
}

impl RawConfig {
    /// Changes the app's language (e.g. `es`, or `en-US`). Returns whether it did: not for a
    /// locale that isn't valid.
    pub fn set_locale(&mut self, locale: &str) -> bool {
        if !i18n::set_locale(locale) {
            return false;
        }
        self.locale.set(locale.to_owned());
        true
    }

    #[inline]
    pub fn synth_effects(&self) -> Effects {
        Effects {
            radio: *self.radio_effect(),
            beep_volume: *self.beep_volume(),
        }
    }

    pub fn set_voice(
        &mut self,
        rt: &Handle,
        lang: ArrayStr<2>,
        model: Option<Utf8PathBuf>,
        synth: &mut Synthesizer,
    ) {
        let mut voices = self.voices().clone();
        match model.clone() {
            Some(model) => {
                voices.insert(lang, model);
            }
            None => {
                voices.remove(&lang);
            }
        }
        self.voices.set(voices);

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
            AudioDevice::Default => t!("audio.system_default"),
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

impl<const RECORDING: bool> From<AudioDevice> for RawAudioDevice<RECORDING> {
    #[inline]
    fn from(device: AudioDevice) -> Self {
        Self(device)
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

settings! {
    /// The spotter: calls traffic and flags by itself, the moment they happen. The defaults are
    /// my-engineer's (`spotter.cpp`). Distances are in metres, times in seconds.
    #[derive(Debug, PartialEq)]
    pub struct SpotterConfig {
        pub enabled: bool = true,
        /// The language of its calls: the pilot's (the last one they asked in) when `None`
        pub language: Option<ArrayStr<2>> = None,
        /// How often it looks at the newest telemetry
        pub poll_interval: f64 = 0.033,

        // Cars alongside. A car overlaps when its centre is within `enter_along` of yours along
        // the car, and `min_across` to `enter_across` to the side (beside you rather than in line
        // with you). It stops overlapping past the exit distances, which are wider so a car at the
        // edge doesn't make the spotter chatter
        pub enter_along: f64 = 4.8,
        pub exit_along: f64 = 5.8,
        pub min_across: f64 = 0.8,
        pub enter_across: f64 = 5.5,
        pub exit_across: f64 = 6.5,
        /// Cars further above or below (on a bridge, or under a crossover) are ignored
        pub max_height_difference: f64 = 4.0,
        /// How long a car must overlap before it's called
        pub debounce: f64 = 0.12,
        /// How long a side must have been taken before it's called clear
        pub min_hold_for_clear: f64 = 0.4,
        /// How often "still there" is called while a car stays alongside
        pub still_there_every: f64 = 6.0,
        /// Below this speed (m/s: the garage, the grid, a spin) it stays quiet
        pub min_speed: f64 = 8.0,

        /// A car this many seconds behind or closer is called...
        pub gap_behind_on: f64 = 1.0,
        /// ...again only once the gap has opened to at least this
        pub gap_behind_off: f64 = 1.5,

        // Weather, from 0 to 1 (about five updates a second). A call when the value moves into
        // another band, by at least `band_margin` past its edge, so a value sitting on an edge
        // stays quiet
        /// Rain: none | light | moderate | heavy
        pub rain_bands: [f64; 3] = [0.05, 0.3, 0.6],
        /// Track wetness: dry | damp | wet | soaked
        pub wet_bands: [f64; 3] = [0.05, 0.25, 0.6],
        pub band_margin: f64 = 0.03,
        /// Rain on the way: a forecast point at least this likely to bring rain...
        pub forecast_rain_chance: f64 = 0.5,
        /// ...this many minutes ahead or less, while it isn't raining yet
        pub forecast_rain_lead_minutes: f64 = 10.0,
        /// Between two rain calls, and between two track calls
        pub weather_repeat: f64 = 60.0,
        /// Between two "wet patch" calls
        pub wet_patch_repeat: f64 = 30.0,
    }
}

settings! {
    /// The language model the engineer talks through. Each provider's settings are kept while
    /// another one is in use.
    #[derive(Debug, PartialEq)]
    pub struct ModelConfig {
        pub provider: ModelProvider = ModelProvider::Claude,
    }
    groups {
        claude: ClaudeConfig,
        ollama: OllamaConfig,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ModelProvider {
    /// Claude, through the Claude API: needs `ANTHROPIC_API_KEY`
    Claude,
    /// A model Ollama runs, on this PC or another
    Ollama,
}

settings! {
    /// Claude, through the Claude API.
    #[derive(Debug, PartialEq)]
    pub struct ClaudeConfig {
        /// e.g. `claude-sonnet-5-5`
        pub model: String = "claude-sonnet-5-5".into(),
        /// `low`, `medium`, `high`, `xhigh` or `max` (the levels depend on the model)
        pub effort: String = "high".into(),
        /// The most tokens in one response, thinking included
        pub max_tokens: NonZeroU32 = NonZeroU32::new(16000).unwrap(),
        /// Has the API summarize the earlier conversation when it grows near the context window,
        /// so a long session never runs out of room. Only on models that support it
        pub compaction: bool = false,
    }
}

settings! {
    /// A model Ollama runs.
    #[derive(Debug, PartialEq)]
    pub struct OllamaConfig {
        /// Where Ollama listens, as `OLLAMA_HOST` says it (e.g. `192.168.1.5` or
        /// `http://pc:11434`): `OLLAMA_HOST`, or this PC, when `None`
        pub host: Option<String> = None,
        /// A model that's been pulled (`ollama pull`), e.g. `qwen3.5:9b`
        pub model: String = "qwen3.5:9b".into(),
        /// How much it thinks, on models that can: `none`, `low`, `medium` or `high`. Thinking on
        /// this PC's GPU, while the game uses it, makes answers slow to come
        pub effort: String = "none".into(),
        /// The context window it's loaded with, in tokens: Ollama silently drops the start of a
        /// conversation that doesn't fit. More takes more memory
        pub context_length: NonZeroU32 = NonZeroU32::new(32768).unwrap(),
        /// The most tokens in one response, thinking included
        pub max_tokens: NonZeroU32 = NonZeroU32::new(16000).unwrap(),
        /// How long Ollama keeps the model loaded after an answer, in seconds (negative: until
        /// it stops). Ollama's own when `None`: 5 minutes, unless `OLLAMA_KEEP_ALIVE` says
        /// otherwise
        pub keep_alive: Option<f64> = None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn saves_only_what_was_set() {
        let defaults = serde_json::from_str::<RawConfig>("{}").unwrap();
        assert!(defaults.is_default());
        assert_eq!(*defaults.master_volume(), 1.0);
        assert_eq!(*defaults.spotter.debounce(), 0.12);
        assert_eq!(*defaults.model.provider(), ModelProvider::Claude);
        assert_eq!(defaults.model.claude.model(), "claude-sonnet-5-5");
        assert_eq!(serde_json::to_string(&defaults).unwrap(), "{}");

        let mut config = defaults;
        config.master_volume.set(0.5);
        config.spotter.debounce.set(0.2);
        config.model.ollama.keep_alive.set(Some(-1.0));
        // Set to what the default is, it's still kept: the default may change
        config.auto_session.set(true);
        assert_eq!(
            serde_json::to_value(&config).unwrap(),
            serde_json::json!({
                "master_volume": 0.5,
                "auto_session": true,
                "spotter": {"debounce": 0.2},
                "model": {"ollama": {"keep_alive": -1.0}},
            })
        );

        let saved = serde_json::to_string(&config).unwrap();
        let loaded = serde_json::from_str::<RawConfig>(&saved).unwrap();
        assert_eq!(*loaded.master_volume(), 0.5);
        assert_eq!(*loaded.engineer_volume(), 1.0);
        assert_eq!(loaded.spotter, config.spotter);
        assert_eq!(loaded.model, config.model);
        assert_eq!(*loaded.model.ollama.keep_alive(), Some(-1.0));
        assert_eq!(
            *loaded.model.ollama.context_length(),
            NonZeroU32::new(32768).unwrap()
        );
    }

    #[test]
    fn edits_only_what_changed() {
        let mut config = serde_json::from_str::<RawConfig>("{}").unwrap();
        assert!(
            !config
                .master_volume
                .edit(*config.master_volume(), |_| false)
        );
        assert!(config.master_volume.is_default());

        assert!(
            config
                .master_volume
                .edit(*config.master_volume(), |volume| {
                    *volume /= 2.0;
                    true
                })
        );
        assert_eq!(*config.master_volume(), 0.5);
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
        assert!(!*loaded.spotter.enabled());
        assert_eq!(*loaded.spotter.debounce(), 0.2);
        assert_eq!(*loaded.spotter.language(), Some(crate::arrstr!("es")));
        assert_eq!(loaded.spotter, disabled.spotter);
    }
}
