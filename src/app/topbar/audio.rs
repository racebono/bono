use crate::{
    app::{
        Subsystems,
        topbar::{
            audio::{
                voice::Voices,
                whisper::{RECORDING_SPEC, Whisper},
            },
            setup_label,
        },
    },
    arrstr,
    config::{ArrayStr, AudioDevice, Config, RawConfig},
    engineer::Heard,
    error,
    synthesizer::{Speech, Synthesizer, plays_on},
    task::Task,
};
use bono_i18n::t;
use camino::Utf8Path;
use sdl3::{
    AudioSubsystem,
    audio::{
        AudioDeviceID, AudioRecordingCallback, AudioSpec, AudioStream, AudioStreamOwner,
        AudioStreamWithCallback,
    },
    event::Event,
    get_error,
    sys::{
        audio::{
            SDL_GetAudioPlaybackDevices, SDL_GetAudioRecordingDevices,
            SDL_SetAudioStreamPutCallback,
        },
        stdinc::SDL_free,
    },
};
use std::{
    ops::{Deref, DerefMut},
    sync::{
        Arc,
        atomic::{AtomicU32, Ordering},
    },
    time::{Duration, Instant},
};
use tokio::{runtime::Handle, sync::oneshot::error::TryRecvError};

mod voice;
mod whisper;

const DELAY: Duration = Duration::from_secs(1);

/// Something the setup guide downloads, as it shows it.
pub enum SetupItem {
    /// Its catalog is still being fetched
    Loading,
    /// Its catalog couldn't be fetched
    Unavailable,
    Found {
        name: String,
        /// In bytes
        size: Option<u64>,
        state: SetupState,
    },
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum SetupState {
    Missing,
    /// How far along, from 0 to 1
    Downloading(f64),
    /// The download stopped before it was done
    Failed,
    Ready,
}

pub struct AudioTopbar {
    pub microphone: DeviceSelector,
    pub speaker: DeviceSelector,
    pub whisper: Whisper,
    pub voices: Voices,
    pub test: Option<tokio::sync::oneshot::Receiver<anyhow::Result<Speech>>>,
    /// The test speech's stream, kept until it's done playing (dropping it stops playback)
    test_playback: Option<AudioStreamOwner>,
    /// The beep when the talk button is pressed, kept until the next one (dropping it stops
    /// playback)
    beep_playback: Option<AudioStreamOwner>,
    /// What the pilot last said on the radio, being transcribed
    transcription: Option<Task<anyhow::Result<Heard>>>,
    /// Listening to the microphone for the setup guide: `None` until it's asked for, and
    /// `Some(None)` if it couldn't
    meter: Option<Option<MicMeter>>,
}

impl AudioTopbar {
    pub fn new(
        pref_path: &Arc<Utf8Path>,
        ss: &Subsystems,
        config: &RawConfig,
        client: &reqwest::Client,
    ) -> Self {
        Self {
            microphone: DeviceSelector::new(Kind::Recording, &ss.audio, config, Instant::now()),
            speaker: DeviceSelector::new(Kind::Playback, &ss.audio, config, Instant::now()),
            whisper: Whisper::new(pref_path, config, client.clone(), None),
            voices: Voices::new(pref_path, client.clone()),
            test: None,
            test_playback: None,
            beep_playback: None,
            transcription: None,
            meter: None,
        }
    }

    /// Whether something here isn't set up yet: a device that won't open, no speech to text
    /// model, or no voice.
    pub fn needs_setup(&self, config: &Config) -> bool {
        let config = config.get();
        self.microphone.get().is_none()
            || self.speaker.get().is_none()
            || config
                .whisper_model()
                .as_ref()
                .is_none_or(|path| !path.is_file())
            || self.whisper.failed()
            || config.voices().is_empty()
            || config.voices().values().any(|path| !voice_on_disk(path))
    }

    /// Called every frame, open or not: loads the speech to text model picked, and sets up what
    /// the setup guide asked for.
    pub fn poll(&mut self, rt: &Handle, config: &Config, synth: &mut Synthesizer) {
        self.whisper.poll(rt, config);
        self.voices.poll(rt, config, synth);
        self.play_test(config);
    }

    /// Says something in the engineer's voice for the app's language, on the speaker, as loud
    /// as the engineer.
    pub fn test_voice(&mut self, rt: &Handle, config: &Config, synth: &mut Synthesizer) {
        if self.test.is_some() {
            return;
        }
        log::debug!("Start test");
        let (lang, effects) = {
            let config = config.get();
            (app_language(config.locale()), config.synth_effects())
        };
        self.test = Some(synth.synthesize(rt, lang, t!("audio.test_phrase").into(), effects));
    }

    /// Whether the test speech is being synthesized, or played.
    pub fn testing(&self) -> bool {
        self.test.is_some() || self.test_playback.is_some()
    }

    /// Whether the microphone is open: the talk button is held.
    pub fn recording(&self) -> bool {
        self.whisper.recording()
    }

    /// Whether what the pilot said on the radio is being transcribed.
    pub fn transcribing(&self) -> bool {
        self.transcription.is_some()
    }

    /// Whether the speech to text model is loaded, and can listen.
    pub fn listening(&mut self, rt: &Handle) -> bool {
        self.whisper.ready(rt)
    }

    /// The microphone, to pick from a list (the setup guide's). Returns whether it changed.
    pub fn pick_microphone(&mut self, ss: &Subsystems, ui: &mut egui::Ui, config: &mut Config) {
        let now = Instant::now();
        if self.microphone.list(Kind::Recording, ss, ui, config, now) {
            // It listens to the new one
            self.meter = None;
        }
    }

    /// The speaker, to pick from a list (the setup guide's).
    pub fn pick_speaker(&mut self, ss: &Subsystems, ui: &mut egui::Ui, config: &mut Config) {
        self.speaker
            .list(Kind::Playback, ss, ui, config, Instant::now());
    }

    /// How loud the microphone is, from 0 to 1, for the setup guide to show it hears the pilot:
    /// it listens until [`Self::stop_meter`]. `None` if it can't.
    pub fn microphone_level(&mut self) -> Option<f32> {
        let microphone = self.microphone.get();
        let meter = self
            .meter
            .get_or_insert_with(|| MicMeter::open(microphone?))
            .as_mut()?;
        Some(meter.level())
    }

    /// Stops listening for [`Self::microphone_level`].
    pub fn stop_meter(&mut self) {
        self.meter = None;
    }

    /// Plays the test speech once it's synthesized, and lets go of it once it's played.
    fn play_test(&mut self, config: &Config) {
        if let Some(ref mut test) = self.test {
            match test.try_recv() {
                Ok(Ok(mut speech)) => {
                    self.test = None;
                    log::debug!("Test synthesized");
                    // Played like the engineer's answers, so it previews the volume
                    let gain = {
                        let config = config.get();
                        config.master_volume() * config.engineer_volume()
                    };
                    for sample in &mut speech.samples {
                        *sample *= gain;
                    }
                    if let Some(device) = self.speaker.get() {
                        match speech.play(device) {
                            Ok(stream) => self.test_playback = Some(stream),
                            Err(e) => error(t!("error.test_speech_playing"), e),
                        }
                    } else {
                        log::warn!("No speaker selected (audio will not be played)");
                    }
                }
                Ok(Err(e)) => {
                    self.test = None;
                    error(t!("error.synthesizing"), e);
                }
                Err(TryRecvError::Empty) => {}
                Err(TryRecvError::Closed) => {
                    self.test = None;
                    log::error!("Test speech was dropped before it was synthesized");
                }
            }
        }

        // Once SDL has consumed all of the test speech, its stream can go
        if let Some(ref stream) = self.test_playback
            && stream.queued_bytes().is_ok_and(|queued| queued == 0)
        {
            self.test_playback = None;
        }
    }

    /// Downloads the recommended speech to text model, and voice for the app's language (again,
    /// those that failed), and uses them once they're on disk (each, if none is picked by then).
    pub fn setup(&mut self, config: &Config) {
        let locale = config.get().locale().clone();
        self.whisper.setup();
        self.voices.setup(&locale);
    }

    /// What `setup` downloads, as the setup guide shows it: each with what it's for.
    pub fn setup_items(&mut self, rt: &Handle, config: &Config) -> Vec<(&'static str, SetupItem)> {
        let locale = config.get().locale().clone();
        let whisper = self.whisper.setup_item(rt, config);
        let voice = self.voices.setup_item(rt, config, &locale);
        [
            (t!("audio.speech_to_text"), whisper),
            (t!("audio.voice"), voice),
        ]
        .into_iter()
        .filter_map(|(what, item)| Some((what, item?)))
        .collect()
    }

    /// What the pilot said on the radio, once it's transcribed.
    pub fn transcription(&mut self, rt: &Handle) -> Option<Heard> {
        let result = self.transcription.as_mut()?.update(rt)?;
        self.transcription = None;
        // Errors go to the engineer, who tells the pilot: a dialog could hide the game
        let heard = result.unwrap_or_else(Heard::Failed);
        match &heard {
            Heard::Question { text, lang } => log::info!("Pilot ({lang}): {text}"),
            Heard::Unvoiced { text, lang } => {
                log::info!("Pilot ({lang}, without a voice): {text}")
            }
            Heard::Nothing => log::info!("Nothing understood on the radio"),
            Heard::NoVoice(_) => log::info!("No voice to answer the radio with"),
            Heard::TooShort => log::debug!("The talk button was let go too soon"),
            Heard::NotListening(why) => log::info!("Not listening to the radio: {why:?}"),
            Heard::Failed(e) => log::error!("Error hearing the radio: {e:#}"),
        }
        Some(heard)
    }

    pub fn update(
        &mut self,
        rt: &Handle,
        ss: &Subsystems,
        ui: &mut egui::Ui,
        config: &mut Config,
        synth: &mut Synthesizer,
    ) {
        let now = Instant::now();
        self.microphone.update(Kind::Recording, ss, ui, config, now);
        if self.speaker.update(Kind::Playback, ss, ui, config, now) {
            self.test_voice(rt, config, synth);
        }

        self.whisper.update(rt, config, ui);
        self.voices.update(rt, config, ui, synth);
    }

    pub fn event(&mut self, rt: &Handle, config: &Config, event: &Event) -> bool {
        // Without a microphone, pressing the talk button still tells the pilot why nothing hears
        let was_recording = self.whisper.recording();
        let microphone = self.microphone.get();
        let result = self.whisper.event(rt, &event, config, microphone);
        if !was_recording && self.whisper.recording() {
            self.beep(config);
        }

        return match result {
            // Picked up by `transcription`. One still being transcribed is dropped: the pilot
            // already said something newer
            Ok(handle) => {
                self.transcription = Some(handle.into());
                false
            }
            Err(e) => e,
        };
    }

    /// Stop using an unplugged microphone, including any unfinished recording.
    pub fn microphone_removed(&mut self, which: u32) -> bool {
        if !self.microphone.removed(which) {
            return false;
        }
        self.whisper.cancel_recording();
        self.meter = None;
        true
    }

    /// Retry only a missing device, preserving an already open device and the saved pick.
    pub fn device_added(&mut self, recording: bool, audio: &AudioSubsystem, config: &RawConfig) {
        let (selector, kind) = if recording {
            (&mut self.microphone, Kind::Recording)
        } else {
            (&mut self.speaker, Kind::Playback)
        };
        selector.next = Instant::now();
        if selector.open.is_none() {
            selector.open = kind.open(audio, kind.config(config));
            if recording {
                self.meter = None;
            }
        }
    }

    /// The speaker `which` is gone (unplugged): nothing plays on it any more, and if it's the one
    /// picked, nothing is played until it reconnects or another is picked. Returns whether it was.
    pub fn speaker_removed(&mut self, which: u32) -> bool {
        for playback in [&mut self.test_playback, &mut self.beep_playback] {
            if playback
                .as_ref()
                .is_some_and(|stream| plays_on(stream, which))
            {
                *playback = None;
            }
        }
        self.speaker.removed(which)
    }

    /// Tells the pilot the microphone is open.
    fn beep(&mut self, config: &Config) {
        let volume = {
            let config = config.get();
            config.master_volume() * config.beep_volume()
        };
        if volume <= 0.0 {
            return;
        }

        let Some(device) = self.speaker.get() else {
            return;
        };
        match Speech::talk_beep(volume).play(device) {
            Ok(stream) => self.beep_playback = Some(stream),
            Err(e) => log::warn!("Error playing the talk beep: {e}"),
        }
    }
}

pub struct DeviceSelector {
    open: Option<sdl3::audio::AudioDevice>,
    next: Instant,
    devices: Vec<(AudioDeviceID, Arc<str>)>,
}

impl DeviceSelector {
    fn new(kind: Kind, audio: &AudioSubsystem, config: &RawConfig, now: Instant) -> Self {
        Self {
            open: kind.open(audio, kind.config(config)),
            next: now,
            devices: Vec::new(),
        }
    }

    #[inline]
    pub fn get(&self) -> Option<&sdl3::audio::AudioDevice> {
        self.open.as_ref()
    }

    /// The device `which` is gone: it's closed if it's the one open. Returns whether it was.
    fn removed(&mut self, which: u32) -> bool {
        let open = self
            .open
            .as_ref()
            .is_some_and(|open| open.id().id() == which);
        if open {
            self.open = None;
        }
        open
    }

    fn update<'a>(
        &'a mut self,
        kind: Kind,
        ss: &Subsystems,
        ui: &mut egui::Ui,
        config: &mut Config,
        now: Instant,
    ) -> bool {
        self.refresh(kind, ss, now);

        let mut test = false;
        config.maybe_update(|raw| {
            let mut device = kind.config(raw).clone();

            // A sub-menu rather than a `ComboBox`: egui only tracks one open popup per viewport,
            // so opening a `ComboBox` popup would close the menu it lives in.
            let label = setup_label(
                self.open.is_none(),
                format!("{}: {}", kind.label(), device.name()),
            );
            let updated = ui
                .menu_button(label, |ui| {
                    let updated = self.options(kind, ss, ui, &mut device);
                    if kind == Kind::Playback {
                        ui.separator();
                        test = ui.button(t!("audio.test")).clicked();
                    }
                    return updated;
                })
                .inner
                .unwrap_or(false);

            if updated {
                kind.set(raw, device);
            }
            return updated;
        });

        return test;
    }

    /// Every device, to pick one from a list rather than a menu. Returns whether the pick
    /// changed.
    fn list(
        &mut self,
        kind: Kind,
        ss: &Subsystems,
        ui: &mut egui::Ui,
        config: &mut Config,
        now: Instant,
    ) -> bool {
        self.refresh(kind, ss, now);

        let mut changed = false;
        config.maybe_update(|raw| {
            let mut device = kind.config(raw).clone();
            changed = self.options(kind, ss, ui, &mut device);
            if changed {
                kind.set(raw, device);
            }
            changed
        });
        changed
    }

    /// The system's default and every device, `device` the one picked: one picked is opened.
    /// Returns whether the pick changed.
    fn options(
        &mut self,
        kind: Kind,
        ss: &Subsystems,
        ui: &mut egui::Ui,
        device: &mut AudioDevice,
    ) -> bool {
        let mut updated = ui
            .selectable_value(device, AudioDevice::Default, t!("audio.system_default"))
            .clicked();

        for (id, name) in &self.devices {
            updated |= ui
                .selectable_value(device, AudioDevice::Device(*id, name.clone()), name.deref())
                .clicked();
        }

        if updated {
            self.open = kind.open(&ss.audio, device);
        }
        updated
    }

    /// Lists the devices again, every `DELAY`.
    fn refresh(&mut self, kind: Kind, ss: &Subsystems, now: Instant) {
        if now >= self.next {
            self.update_devices(kind, ss);
            self.next = match self.next.checked_add(DELAY) {
                Some(next) => next.max(now),
                None => now,
            };
        }
    }

    fn update_devices(&mut self, kind: Kind, ss: &Subsystems) {
        self.devices.clear();
        if let Err(e) = kind.enumerate(&ss.audio, &mut self.devices) {
            log::error!("Error updating audio devices: {e}");
        }
    }
}

/// The language the app is in (`locale`, e.g. `en-US`): `en` if it's none.
pub fn app_language(locale: &str) -> ArrayStr<2> {
    ArrayStr::from_str(locale.split('-').next().unwrap_or_default()).unwrap_or(arrstr!("en"))
}

/// A recording stream with a callback, that stops calling it before it's dropped.
///
/// sdl3 frees an [`AudioStreamWithCallback`]'s callback before destroying its stream, while
/// SDL's audio thread may still be running it, or about to: that would use it after it's freed.
struct RecordingStream<CB>(AudioStreamWithCallback<CB>);

impl<CB> RecordingStream<CB> {
    fn new(stream: AudioStreamWithCallback<CB>) -> Self {
        Self(stream)
    }
}

impl<CB> Deref for RecordingStream<CB> {
    type Target = AudioStreamWithCallback<CB>;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl<CB> DerefMut for RecordingStream<CB> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

impl<CB> Drop for RecordingStream<CB> {
    fn drop(&mut self) {
        let _ = self.0.pause();
        // SDL calls the callback with the stream locked, and this locks it too: it waits for a
        // call in progress, and none can start after it
        unsafe { SDL_SetAudioStreamPutCallback(self.0.raw(), None, std::ptr::null_mut()) };
    }
}

/// Listens to the microphone, for how loud it is.
struct MicMeter {
    /// Dropping it stops listening
    _stream: RecordingStream<Meter>,
    /// The loudest sample since it was last looked at, as an `f32`'s bits
    peak: Arc<AtomicU32>,
    /// The level shown, from 0 to 1: it falls slowly, so peaks can be seen
    shown: f32,
}

impl MicMeter {
    fn open(device: &sdl3::audio::AudioDevice) -> Option<Self> {
        let peak = Arc::new(AtomicU32::new(0));
        let meter = Meter {
            peak: peak.clone(),
            buf: [0.0; 512],
        };
        let open = || {
            let stream = RecordingStream::new(
                device.open_recording_stream_with_callback(&RECORDING_SPEC, meter)?,
            );
            stream.resume()?;
            Ok::<_, sdl3::Error>(stream)
        };
        match open() {
            Ok(stream) => Some(Self {
                _stream: stream,
                peak,
                shown: 0.0,
            }),
            Err(e) => {
                log::warn!("Couldn't listen to the microphone for its level: {e}");
                None
            }
        }
    }

    /// The level since last looked at, from 0 (-60 dB or less) to 1 (0 dB).
    fn level(&mut self) -> f32 {
        let peak = f32::from_bits(self.peak.swap(0, Ordering::Relaxed));
        let level = ((20.0 * peak.max(1e-6).log10() + 60.0) / 60.0).clamp(0.0, 1.0);
        self.shown = level.max(self.shown - 0.02);
        self.shown
    }
}

struct Meter {
    peak: Arc<AtomicU32>,
    buf: [f32; 512],
}

impl AudioRecordingCallback<f32> for Meter {
    fn callback(&mut self, stream: &mut AudioStream, _available: i32) {
        while let Ok(read @ 1..) = stream.read_f32_samples(&mut self.buf) {
            let peak =
                (self.buf[..read].iter()).fold(0.0f32, |peak, sample| peak.max(sample.abs()));
            // Positive floats order like their bits
            self.peak.fetch_max(peak.to_bits(), Ordering::Relaxed);
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum Kind {
    Playback,
    Recording,
}

impl Kind {
    pub fn label(self) -> &'static str {
        match self {
            Kind::Playback => t!("audio.speaker"),
            Kind::Recording => t!("audio.microphone"),
        }
    }

    pub fn config<'a>(self, raw: &'a RawConfig) -> &'a AudioDevice {
        match self {
            Kind::Playback => raw.speaker().deref(),
            Kind::Recording => raw.microphone().deref(),
        }
    }

    pub fn set(self, raw: &mut RawConfig, device: AudioDevice) {
        match self {
            Kind::Playback => raw.speaker.set(device.into()),
            Kind::Recording => raw.microphone.set(device.into()),
        }
    }

    pub fn open(
        self,
        audio: &AudioSubsystem,
        device: &AudioDevice,
    ) -> Option<sdl3::audio::AudioDevice> {
        log::debug!("{device:#?}");
        // SDL IDs are transient: a reconnected device has a new ID. Resolve the saved
        // name whenever opening, including a device that was absent at startup.
        let resolved;
        let device = match device {
            AudioDevice::Default => None,
            AudioDevice::Device(_, name) | AudioDevice::Unknown(name) => {
                let mut devices = Vec::new();
                if let Err(e) = self.enumerate(audio, &mut devices) {
                    log::warn!("Couldn't resolve audio device: {e}");
                    return None;
                }
                resolved = resolve_device(name, &devices)?;
                Some(&resolved)
            }
        };

        let res = match self {
            Kind::Playback => {
                sdl3::audio::AudioDevice::open_playback(audio, device, &AudioSpec::default())
            }
            Kind::Recording => {
                sdl3::audio::AudioDevice::open_recording(audio, device, &RECORDING_SPEC)
            }
        };

        match res {
            Ok(device) => Some(device),
            Err(e) => {
                error(
                    match self {
                        Kind::Playback => t!("error.open_speaker"),
                        Kind::Recording => t!("error.open_microphone"),
                    },
                    e,
                );
                None
            }
        }
    }

    pub fn enumerate(
        self,
        _audio: &AudioSubsystem,
        dest: &mut Vec<(AudioDeviceID, Arc<str>)>,
    ) -> Result<(), sdl3::Error> {
        let get_devices = match self {
            Kind::Playback => SDL_GetAudioPlaybackDevices,
            Kind::Recording => SDL_GetAudioRecordingDevices,
        };

        let mut num_devices = 0;
        let devices = unsafe { get_devices(&mut num_devices) };
        if devices.is_null() {
            return Err(get_error());
        }

        // SDL owns the allocator; release the list even if a device name lookup fails.
        struct DeviceList(*mut sdl3::sys::audio::SDL_AudioDeviceID);
        impl Drop for DeviceList {
            fn drop(&mut self) {
                unsafe { SDL_free(self.0.cast()) };
            }
        }
        let devices = DeviceList(devices);

        dest.reserve(num_devices as usize);
        for i in 0..num_devices {
            let instance_id = AudioDeviceID::Device(unsafe { *devices.0.offset(i as isize) });
            let name = instance_id.name()?;
            dest.push((instance_id, name.into()));
        }

        return Ok(());
    }
}

/// Match names only within the requested kind's current device inventory.
fn resolve_device(name: &str, devices: &[(AudioDeviceID, Arc<str>)]) -> Option<AudioDeviceID> {
    devices
        .iter()
        .find(|(_, candidate)| candidate.as_ref() == name)
        .map(|(id, _)| *id)
}

#[cfg(test)]
mod hotplug_tests {
    use super::*;

    #[test]
    fn saved_name_resolves_to_the_current_id_after_reconnection() {
        let old = [(
            AudioDeviceID::Device(sdl3::sys::audio::SDL_AudioDeviceID(10)),
            Arc::from("Headset"),
        )];
        let new = [(
            AudioDeviceID::Device(sdl3::sys::audio::SDL_AudioDeviceID(20)),
            Arc::from("Headset"),
        )];
        assert_eq!(
            resolve_device("Headset", &old),
            Some(AudioDeviceID::Device(sdl3::sys::audio::SDL_AudioDeviceID(
                10
            )))
        );
        assert_eq!(resolve_device("Headset", &[]), None);
        assert_eq!(
            resolve_device("Headset", &new),
            Some(AudioDeviceID::Device(sdl3::sys::audio::SDL_AudioDeviceID(
                20
            )))
        );
        assert_eq!(resolve_device("Other microphone", &new), None);
    }
}

/// Piper needs both the model and its sidecar configuration.
fn voice_on_disk(path: &Utf8Path) -> bool {
    path.is_file() && path.with_added_extension("json").is_file()
}

#[cfg(test)]
mod model_files_tests {
    use super::*;

    #[test]
    fn a_voice_needs_both_files_after_redownloading() {
        let dir = tempfile::tempdir().unwrap();
        let path = camino::Utf8PathBuf::from_path_buf(dir.path().join("voice.onnx")).unwrap();
        assert!(!voice_on_disk(&path));
        std::fs::write(&path, []).unwrap();
        assert!(!voice_on_disk(&path));
        std::fs::write(path.with_added_extension("json"), []).unwrap();
        assert!(voice_on_disk(&path));
        std::fs::remove_file(&path).unwrap();
        assert!(!voice_on_disk(&path));
    }
}
