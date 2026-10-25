use crate::{
    button::ConfigButton,
    error,
    i18n::{self, set_locale},
    synthesizer::{Effects, Synthesizer, SynthesizerInner},
};
use bono_i18n::t;
use camino::{Utf8Path, Utf8PathBuf};
use sdl3::audio::AudioDeviceID;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::{
    collections::BTreeMap,
    fmt::{Debug, Display},
    io::Write,
    num::NonZeroU32,
    ops::{Deref, DerefMut},
    path::Path,
    sync::Arc,
};
use tokio::{runtime::Handle, sync::watch::Ref};

#[derive(Clone)]
pub struct Config {
    send: tokio::sync::watch::Sender<RawConfig>,
}

impl Config {
    pub async fn new(path: impl AsRef<Path>) -> anyhow::Result<Self> {
        let path = path.as_ref().to_owned();
        let mut data = load_config(&path).await?;
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
                    let contents = {
                        let raw = recv.borrow();
                        serde_json::to_vec_pretty(&*raw)?
                    };
                    let path = path.clone();
                    tokio::task::spawn_blocking(move || save_config(&path, &contents)).await??;
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

/// Never open the destination for writing: readers see either the old or the new JSON.
fn save_config(path: &Path, contents: &[u8]) -> anyhow::Result<()> {
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let mut temp = tempfile::NamedTempFile::new_in(parent)?;
    temp.write_all(contents)?;
    temp.flush()?;
    temp.as_file().sync_all()?;
    temp.persist(path)?;
    Ok(())
}

async fn load_config(path: &Path) -> anyhow::Result<RawConfig> {
    let contents = match tokio::fs::read(path).await {
        Ok(contents) => contents,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(RawConfig::default()),
        Err(e) => return Err(e.into()),
    };
    match serde_json::from_slice(&contents) {
        Ok(data) => Ok(data),
        Err(e) => {
            // Keep earlier recovery backups too. Renaming requires no second copy of secrets.
            let mut backup = path.as_os_str().to_owned();
            backup.push(".bak");
            let mut backup = std::path::PathBuf::from(backup);
            let mut index = 1;
            while tokio::fs::try_exists(&backup).await? {
                let mut name = path.as_os_str().to_owned();
                name.push(format!(".bak.{index}"));
                backup = name.into();
                index += 1;
            }
            match tokio::fs::rename(path, &backup).await {
                Ok(()) => log::warn!(
                    "Invalid config at {} ({e}); preserved at {}. Using defaults.",
                    path.display(),
                    backup.display()
                ),
                Err(backup_error) => log::warn!(
                    "Invalid config at {} ({e}); could not back it up: {backup_error}. Using defaults.",
                    path.display()
                ),
            }
            Ok(RawConfig::default())
        }
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
        /// The setup guide's version the pilot last finished (or skipped): an older one shows it
        /// again
        pub onboarding: u32 = 0,
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
        let value = String::deserialize(deserializer)?;
        let s = value.as_bytes();
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

/// The language model the engineer talks through. Each provider's settings are kept while
/// another one is in use.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ModelConfig {
    #[serde(
        default,
        rename = "provider",
        skip_serializing_if = "Setting::is_default",
        deserialize_with = "deserialize_provider"
    )]
    fallback_provider: Setting<ModelProvider>,
    /// Older builds ignore this and keep using the previous provider.
    #[serde(default, skip_serializing_if = "Setting::is_default")]
    bono_cloud: Setting<bool>,
    #[serde(default, skip_serializing_if = "IsDefault::is_default")]
    pub claude: ClaudeConfig,
    #[serde(default, skip_serializing_if = "IsDefault::is_default")]
    pub ollama: OllamaConfig,
    #[serde(default, skip_serializing_if = "IsDefault::is_default")]
    pub openai: OpenAiConfig,
}

impl IsDefault for ModelConfig {
    fn is_default(&self) -> bool {
        self.fallback_provider.is_default()
            && self.bono_cloud.is_default()
            && self.claude.is_default()
            && self.ollama.is_default()
            && self.openai.is_default()
    }
}

impl ModelConfig {
    /// Bono Cloud unless another provider was picked.
    pub fn provider(&self) -> &ModelProvider {
        match (&self.bono_cloud, &self.fallback_provider) {
            (Setting::Set(true), _) | (_, Setting::Default) => &ModelProvider::BonoCloud,
            (_, Setting::Set(provider)) => provider,
        }
    }

    pub fn set_provider(&mut self, provider: ModelProvider) {
        match provider {
            ModelProvider::BonoCloud => self.bono_cloud.set(true),
            ModelProvider::Claude | ModelProvider::Ollama | ModelProvider::OpenAi => {
                self.fallback_provider.set(provider);
                self.bono_cloud = Setting::Default;
            }
        }
    }

    pub fn edit_provider(
        &mut self,
        mut current: ModelProvider,
        edit: impl FnOnce(&mut ModelProvider) -> bool,
    ) -> bool {
        let changed = edit(&mut current);
        if changed {
            self.set_provider(current);
        }
        changed
    }
}

/// A newer provider must not send the whole config through corruption recovery.
fn deserialize_provider<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> Result<Setting<ModelProvider>, D::Error> {
    Ok(match String::deserialize(deserializer)?.as_str() {
        "claude" => Setting::Set(ModelProvider::Claude),
        "ollama" => Setting::Set(ModelProvider::Ollama),
        "openai" => Setting::Set(ModelProvider::OpenAi),
        _ => Setting::Default,
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ModelProvider {
    /// Bono Cloud: Claude, through Bono's website, for signed-in accounts with access to it. The
    /// website chooses the model and its settings (and ignores any sent), so it has no settings
    BonoCloud,
    /// Claude, through the Claude API: needs an API key
    Claude,
    /// A model Ollama runs, on this PC or another
    Ollama,
    /// Any OpenAI-compatible API: llama.cpp's `llama-server`, LM Studio, vLLM, OpenAI...
    OpenAi,
}

settings! {
    /// A model behind an OpenAI-compatible API.
    #[derive(Debug, PartialEq)]
    pub struct OpenAiConfig {
        /// The API's base, with its version, e.g. `http://127.0.0.1:8080/v1`: llama.cpp's
        /// `llama-server` on this PC when `None`
        pub base_url: Option<String> = None,
        /// Sent as a bearer token: `OPENAI_API_KEY` when `None` (local servers need none)
        pub api_key: Option<String> = None,
        /// As the server names it (one serving a single model takes any)
        pub model: String = String::new(),
        /// `reasoning_effort`: `none`, `low`, `medium` or `high`, or empty for the model's default
        pub effort: String = String::new(),
        /// The most tokens in one response, thinking included
        pub max_tokens: NonZeroU32 = NonZeroU32::new(16000).unwrap(),
    }
}

/// How long Claude keeps both prompt-cache breakpoints.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ClaudeCacheTtl {
    #[serde(rename = "5m")]
    FiveMinutes,
    #[serde(rename = "1h")]
    OneHour,
}

impl ClaudeCacheTtl {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::FiveMinutes => "5m",
            Self::OneHour => "1h",
        }
    }
}

settings! {
    /// Claude, through the Claude API.
    #[derive(Debug, PartialEq)]
    pub struct ClaudeConfig {
        /// Prompt-cache lifetime: `5m` or `1h`, shared by system and conversation caching.
        pub cache_ttl: ClaudeCacheTtl = ClaudeCacheTtl::OneHour,
        /// `ANTHROPIC_API_KEY` (or else `ANTHROPIC_AUTH_TOKEN`, as a bearer token) when `None`
        pub api_key: Option<String> = None,
        /// Sent as `anthropic-workspace-id`: `ANTHROPIC_WORKSPACE_ID`, if set, when `None`
        pub workspace_id: Option<String> = None,
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
    fn array_strings_accept_json_escapes_and_check_byte_length() {
        for value in ["\\\"", "\n\t", "é"] {
            let original = ArrayStr::<2>::from_str(value).unwrap();
            let saved = serde_json::to_vec(&original).unwrap();
            let loaded: ArrayStr<2> = serde_json::from_slice(&saved).unwrap();
            assert_eq!(loaded, original);
        }
        assert!(serde_json::from_slice::<ArrayStr<2>>(br#""\u0065\u006e""#).is_ok());
        assert!(serde_json::from_slice::<ArrayStr<2>>(br#""\n""#).is_err());
    }

    #[tokio::test]
    async fn backslash_bindings_load_without_recovering_config() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        let mut data = RawConfig::default();
        data.talk_button
            .set(Some(ConfigButton::Key(sdl3::keyboard::Scancode::Backslash)));
        data.session_button
            .set(Some(ConfigButton::Key(sdl3::keyboard::Scancode::Backslash)));
        data.master_volume.set(0.5);
        data.onboarding.set(3);
        data.model.claude.api_key.set(Some("test-key".into()));
        let contents = serde_json::to_vec_pretty(&data).unwrap();
        save_config(&path, &contents).unwrap();
        let loaded = load_config(&path).await.unwrap();
        for button in [loaded.talk_button(), loaded.session_button()] {
            assert!(matches!(
                button,
                Some(ConfigButton::Key(sdl3::keyboard::Scancode::Backslash))
            ));
        }
        assert_eq!(*loaded.master_volume(), 0.5);
        assert_eq!(*loaded.onboarding(), 3);
        assert_eq!(loaded.model.claude.api_key().as_deref(), Some("test-key"));
        assert_eq!(std::fs::read(&path).unwrap(), contents);
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
    }

    #[derive(Debug, Clone, Copy, PartialEq, Deserialize)]
    #[serde(rename_all = "lowercase")]
    enum ReleasedProvider {
        Claude,
        Ollama,
        OpenAi,
    }

    #[derive(Deserialize)]
    struct ReleasedModelConfig {
        #[serde(default)]
        provider: Setting<ReleasedProvider>,
        #[serde(default)]
        claude: ClaudeConfig,
        #[serde(default)]
        ollama: OllamaConfig,
        #[serde(default)]
        openai: OpenAiConfig,
    }

    #[derive(Deserialize)]
    struct ReleasedConfig {
        #[serde(default)]
        master_volume: Setting<f32>,
        #[serde(default)]
        voices: Setting<BTreeMap<ArrayStr<2>, Utf8PathBuf>>,
        model: ReleasedModelConfig,
    }

    #[tokio::test]
    async fn bono_cloud_round_trips_with_legacy_fallback() {
        for fallback in [
            None,
            Some(ModelProvider::Claude),
            Some(ModelProvider::Ollama),
            Some(ModelProvider::OpenAi),
        ] {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("config.json");
            let mut data = RawConfig::default();
            if let Some(fallback) = fallback {
                data.model.set_provider(fallback);
            }
            data.model.set_provider(ModelProvider::BonoCloud);
            data.master_volume.set(0.5);
            data.voices.set(BTreeMap::from([(
                arrstr!("en"),
                Utf8PathBuf::from("voices/engineer.onnx"),
            )]));
            data.model.claude.api_key.set(Some("claude-key".into()));
            data.model.ollama.model.set("local-model".into());
            data.model.openai.api_key.set(Some("openai-key".into()));

            let contents = serde_json::to_vec_pretty(&data).unwrap();
            let json: serde_json::Value = serde_json::from_slice(&contents).unwrap();
            assert_eq!(json["model"]["bono_cloud"], true);
            assert_ne!(json["model"]["provider"], "bonocloud");
            save_config(&path, &contents).unwrap();
            let loaded = load_config(&path).await.unwrap();
            assert_eq!(*loaded.model.provider(), ModelProvider::BonoCloud);
            assert_eq!(loaded.model, data.model);
            assert_eq!(loaded.voices(), data.voices());
            assert_eq!(*loaded.master_volume(), 0.5);
            assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);

            let released: ReleasedConfig = serde_json::from_slice(&contents).unwrap();
            let expected = match fallback {
                None | Some(ModelProvider::Claude) => ReleasedProvider::Claude,
                Some(ModelProvider::Ollama) => ReleasedProvider::Ollama,
                Some(ModelProvider::OpenAi) => ReleasedProvider::OpenAi,
                Some(ModelProvider::BonoCloud) => unreachable!(),
            };
            assert_eq!(
                *released.model.provider.get_or(&ReleasedProvider::Claude),
                expected
            );
            assert_eq!(released.master_volume, data.master_volume);
            assert_eq!(released.voices, data.voices);
            assert_eq!(released.model.claude, data.model.claude);
            assert_eq!(released.model.ollama, data.model.ollama);
            assert_eq!(released.model.openai, data.model.openai);
        }
    }

    #[test]
    fn released_config_rejects_unknown_provider_but_ignores_extra_fields() {
        let error = serde_json::from_str::<ReleasedConfig>(r#"{"model":{"provider":"bonocloud"}}"#)
            .err()
            .unwrap();
        assert!(error.to_string().contains("unknown variant `bonocloud`"));
        let released: ReleasedConfig = serde_json::from_str(
            r#"{"future_setting":true,"model":{"provider":"ollama","bono_cloud":true}}"#,
        )
        .unwrap();
        assert_eq!(
            released.model.provider,
            Setting::Set(ReleasedProvider::Ollama)
        );
    }

    #[tokio::test]
    async fn unknown_provider_defaults_without_recovering_config() {
        for provider in ["future-provider", "bonocloud"] {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("config.json");
            let contents = serde_json::to_vec(&serde_json::json!({
                "master_volume": 0.5,
                "voices": {"en": "voices/engineer.onnx"},
                "model": {
                    "provider": provider,
                    "claude": {"api_key": "claude-key"},
                    "openai": {"api_key": "openai-key"}
                }
            }))
            .unwrap();
            std::fs::write(&path, &contents).unwrap();
            let loaded = load_config(&path).await.unwrap();
            assert_eq!(*loaded.model.provider(), ModelProvider::BonoCloud);
            assert!(loaded.model.fallback_provider.is_default());
            assert_eq!(*loaded.master_volume(), 0.5);
            assert_eq!(loaded.voices()[&arrstr!("en")], "voices/engineer.onnx");
            assert_eq!(loaded.model.claude.api_key().as_deref(), Some("claude-key"));
            assert_eq!(loaded.model.openai.api_key().as_deref(), Some("openai-key"));
            assert_eq!(std::fs::read(&path).unwrap(), contents);
            assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
            let saved = serde_json::to_value(&loaded).unwrap();
            assert!(saved["model"].get("provider").is_none());
        }
    }

    #[test]
    fn switching_away_from_bono_cloud_clears_override() {
        let mut model = ModelConfig::default();
        model.set_provider(ModelProvider::Ollama);
        assert!(model.edit_provider(*model.provider(), |provider| {
            *provider = ModelProvider::BonoCloud;
            true
        }));
        assert!(!model.edit_provider(*model.provider(), |_| false));
        assert_eq!(*model.provider(), ModelProvider::BonoCloud);
        for provider in [
            ModelProvider::Claude,
            ModelProvider::Ollama,
            ModelProvider::OpenAi,
        ] {
            model.set_provider(ModelProvider::BonoCloud);
            assert!(model.edit_provider(*model.provider(), |current| {
                *current = provider;
                true
            }));
            let saved = serde_json::to_value(&model).unwrap();
            assert!(saved.get("bono_cloud").is_none());
            let loaded: ModelConfig = serde_json::from_value(saved).unwrap();
            assert_eq!(*loaded.provider(), provider);
        }
    }

    #[test]
    fn atomic_save_replaces_json_and_cleans_up() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        std::fs::write(&path, b"old contents that must be replaced").unwrap();
        let mut data = RawConfig::default();
        data.master_volume.set(0.5);
        save_config(&path, &serde_json::to_vec_pretty(&data).unwrap()).unwrap();
        let loaded: RawConfig = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(*loaded.master_volume(), 0.5);
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
    }

    #[test]
    fn failed_save_cleans_up_temp_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        std::fs::create_dir(&path).unwrap();
        assert!(save_config(&path, b"{}").is_err());
        assert!(path.is_dir());
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
    }

    #[tokio::test]
    async fn corrupt_config_recovers_and_preserves_backups() {
        for contents in [b"".as_slice(), b"{", b"invalid", br#"{"width":0}"#] {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("config.json");
            let backup = dir.path().join("config.json.bak");
            std::fs::write(&backup, b"earlier backup").unwrap();
            std::fs::write(&path, contents).unwrap();
            assert!(load_config(&path).await.unwrap().is_default());
            assert_eq!(std::fs::read(&backup).unwrap(), b"earlier backup");
            assert_eq!(
                std::fs::read(dir.path().join("config.json.bak.1")).unwrap(),
                contents
            );
            assert!(!path.exists());
            save_config(&path, b"{}").unwrap();
            assert!(load_config(&path).await.unwrap().is_default());
        }
    }

    #[tokio::test]
    async fn missing_and_valid_config_load_without_backups() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        assert!(load_config(&path).await.unwrap().is_default());
        std::fs::write(&path, br#"{"master_volume":0.5}"#).unwrap();
        assert_eq!(*load_config(&path).await.unwrap().master_volume(), 0.5);
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
    }

    #[test]
    fn saves_only_what_was_set() {
        let defaults = serde_json::from_str::<RawConfig>("{}").unwrap();
        assert!(defaults.is_default());
        assert_eq!(*defaults.master_volume(), 1.0);
        assert_eq!(*defaults.spotter.debounce(), 0.12);
        assert_eq!(*defaults.model.provider(), ModelProvider::BonoCloud);
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
