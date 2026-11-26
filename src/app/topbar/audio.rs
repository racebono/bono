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
    engineer::{Heard, acknowledgement},
    error, language,
    synthesizer::{Speech, Synthesizer, VoiceLoad, plays_on},
    task::{MaybeDone, Task},
    updating,
};
use bono_i18n::t;
use camino::{Utf8Path, Utf8PathBuf};
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
            SDL_AUDIO_DEVICE_DEFAULT_PLAYBACK, SDL_AUDIO_DEVICE_DEFAULT_RECORDING,
            SDL_AudioDeviceID, SDL_CloseAudioDevice, SDL_GetAudioPlaybackDevices,
            SDL_GetAudioRecordingDevices, SDL_OpenAudioDevice, SDL_SetAudioStreamPutCallback,
        },
        stdinc::SDL_free,
    },
};
use std::{
    collections::{HashMap, HashSet},
    ops::{Deref, DerefMut},
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU32, AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};
use tokio::{runtime::Handle, sync::oneshot::error::TryRecvError};

pub(in crate::app) mod engine;
mod gpu;
mod parakeet;
pub(in crate::app) mod voice;
mod whisper;

const DELAY: Duration = Duration::from_secs(1);
static DEVICE_WORKERS: AtomicUsize = AtomicUsize::new(0);

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
    /// On disk and picked, and being loaded
    Loading,
    /// On disk and picked, but it couldn't be loaded
    LoadFailed,
    Ready,
}

pub struct AudioTopbar {
    pub microphone: DeviceSelector,
    pub speaker: DeviceSelector,
    pub whisper: Whisper,
    pub voices: Voices,
    /// Whether the configured models are on disk
    files: ModelFiles,
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
    /// Stop scheduling audio work before SDL teardown, retaining the subsystem until workers end.
    pub fn begin_shutdown(&mut self) {
        self.test = None;
        self.transcription = None;
        self.meter = None;
        self.whisper.cancel_recording();
        self.test_playback = None;
        self.beep_playback = None;
        self.microphone.begin_shutdown();
        self.speaker.begin_shutdown();
    }

    pub fn shutdown_ready(&mut self) -> bool {
        self.microphone.collect_retired();
        self.speaker.collect_retired();
        DEVICE_WORKERS.load(Ordering::Acquire) == 0
            && self.microphone.retiring.is_empty()
            && self.speaker.retiring.is_empty()
            && !self.whisper.deleting()
            && !self.voices.deleting()
    }

    pub fn new(
        pref_path: &Arc<Utf8Path>,
        ss: &Subsystems,
        config: &RawConfig,
        client: &reqwest::Client,
    ) -> Self {
        let files = ModelFiles::new();
        Self {
            microphone: DeviceSelector::new(Kind::Recording, &ss.audio, config, Instant::now()),
            speaker: DeviceSelector::new(Kind::Playback, &ss.audio, config, Instant::now()),
            whisper: Whisper::new(pref_path, config, client.clone(), None, files.clone()),
            voices: Voices::new(pref_path, client.clone(), files.clone()),
            files,
            test: None,
            test_playback: None,
            beep_playback: None,
            transcription: None,
            meter: None,
        }
    }

    /// Whether something here isn't set up yet: a device that won't open, no speech to text
    /// model, or no voice (or one that won't load).
    pub fn needs_setup(&self, config: &Config, synth: &Synthesizer) -> bool {
        let config = config.get();
        self.microphone.missing()
            || self.speaker.missing()
            || self.whisper.missing(&config, &self.voices.getting())
            || config.voices().is_empty()
            || config.voices().values().any(|path| !self.files.voice(path))
            || (config.voices().keys()).any(|&lang| synth.load(lang) == Some(VoiceLoad::Failed))
    }

    /// Called every frame, open or not: loads the speech to text model picked, and sets up what
    /// the setup guide asked for.
    pub fn poll(&mut self, rt: &Handle, config: &Config, synth: &mut Synthesizer) {
        if self.microphone.poll(Kind::Recording) {
            // It listens to the new one
            self.meter = None;
        }
        self.speaker.poll(Kind::Playback);
        self.whisper.poll(rt, config, &self.voices.getting());
        self.voices.poll(rt, config, synth);
        self.play_test(config);
    }

    /// Says something in the engineer's voice for [`Self::guide_language`], on the speaker, as
    /// loud as the engineer.
    pub fn test_voice(&mut self, rt: &Handle, config: &Config, synth: &mut Synthesizer) {
        if self.test.is_some() {
            return;
        }
        log::debug!("Start test");
        let (lang, effects) = {
            let config = config.get();
            (self.guide_language(&config), config.synth_effects())
        };
        self.test = Some(synth.synthesize(rt, lang, test_phrase(lang).into(), effects));
    }

    /// The language the setup guide speaks in: see [`guide_language`].
    pub fn guide_language(&self, config: &RawConfig) -> ArrayStr<2> {
        guide_language(config, app_language(config.locale()), &self.voices.getting())
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
        // The meter listens to the new one once it's open (see `poll`)
        self.microphone
            .list(Kind::Recording, ss, ui, config, Instant::now());
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
                    speech.amplify(gain);
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

    /// Downloads `engine`'s model for Automatic (`None`: none), and a voice for each of `langs`
    /// (again, those that failed), and uses them once they're on disk (each voice, if none is
    /// picked by then).
    pub fn setup(&mut self, langs: &[ArrayStr<2>], engine: Option<engine::Engine>) {
        self.whisper.setup(engine);
        self.voices.setup(langs);
    }

    /// The languages whose voice is set and usable: on disk, and not failed to load.
    pub fn usable_voices(&self, config: &RawConfig, synth: &Synthesizer) -> Vec<ArrayStr<2>> {
        (config.voices().iter())
            .filter(|&(&lang, path)| {
                self.files.voice(path) && synth.load(lang) != Some(VoiceLoad::Failed)
            })
            .map(|(&lang, _)| lang)
            .collect()
    }

    /// What `setup` downloads for `langs`, and `engine`'s model (`None`: the model picked stays),
    /// as the setup guide shows it: each with what it's for.
    pub fn setup_items(
        &mut self,
        rt: &Handle,
        config: &Config,
        synth: &Synthesizer,
        langs: &[ArrayStr<2>],
        engine: Option<engine::Engine>,
    ) -> Vec<(&'static str, SetupItem)> {
        let whisper = self.whisper.setup_item(rt, engine);
        let voices = self.voices.setup_items(rt, config, synth, langs);
        let voices = voices.into_iter().map(|voice| (t!("audio.voice"), Some(voice)));
        std::iter::once((t!("audio.speech_to_text"), whisper))
            .chain(voices)
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
            Heard::Question { text, lang, .. } => {
                let lang = lang.as_deref().unwrap_or("language not said");
                log::info!("Pilot ({lang}): {text}")
            }
            Heard::Unvoiced { text, lang } => {
                log::info!("Pilot ({lang}, without a voice): {text}")
            }
            Heard::Nothing => log::info!("Nothing understood on the radio"),
            Heard::NoVoice(_) => log::info!("No voice to answer the radio with"),
            Heard::UnsupportedLanguage(lang) => {
                log::info!("The speech to text model in use cannot hear {lang}")
            }
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

        self.whisper.update(rt, config, ui, &self.voices.getting());
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

    /// Retry a missing device or one still being opened, preserving an already open device (until
    /// the pick replaces it) and the saved pick.
    pub fn device_added(&mut self, recording: bool, _audio: &AudioSubsystem, config: &RawConfig) {
        let (selector, kind) = if recording {
            (&mut self.microphone, Kind::Recording)
        } else {
            (&mut self.speaker, Kind::Playback)
        };
        selector.next = Instant::now();
        // An open still in progress may have missed the new device: it's started again. The meter
        // listens to it once it's open (see `poll`)
        if selector.open.is_none() || selector.opening.is_some() {
            selector.opening = Some(kind.open(kind.config(config)));
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
    /// Devices closing on a worker, with the subsystem reference their forgotten handle held,
    /// dropped once they're closed
    retiring: Vec<(AudioSubsystem, tokio::sync::oneshot::Receiver<()>)>,
    /// The device picked, being opened in the background: until then, the one open stays
    opening: Option<OpeningDevice>,
    /// Devices removed while one is being opened: it may be one of them, by the time it's open
    gone: Vec<u32>,
    /// To own the devices opened in the background
    audio: AudioSubsystem,
    next: Instant,
    devices: Vec<(AudioDeviceID, Arc<str>)>,
}

impl DeviceSelector {
    fn new(kind: Kind, audio: &AudioSubsystem, config: &RawConfig, now: Instant) -> Self {
        Self {
            open: None,
            retiring: Vec::new(),
            opening: Some(kind.open(kind.config(config))),
            gone: Vec::new(),
            audio: audio.clone(),
            next: now,
            devices: Vec::new(),
        }
    }

    #[inline]
    pub fn get(&self) -> Option<&sdl3::audio::AudioDevice> {
        self.open.as_ref()
    }

    /// Whether no device is open, nor being opened.
    fn missing(&self) -> bool {
        self.open.is_none() && self.opening.is_none()
    }

    /// Uses the device opened in the background, once it is. Returns whether it changed.
    fn poll(&mut self, kind: Kind) -> bool {
        self.collect_retired();
        let Some(opening) = &mut self.opening else {
            return false;
        };
        let opened = match opening.try_recv() {
            Ok(opened) => opened,
            Err(TryRecvError::Empty) => return false,
            Err(TryRecvError::Closed) => Ok(None),
        };
        self.opening = None;
        let gone = std::mem::take(&mut self.gone);
        self.retire_open();
        self.open = match opened {
            // Unplugged before it got here: it's closed as it's dropped
            Ok(Some(opened)) if gone.contains(&opened.0.0) => None,
            Ok(Some(opened)) => Some(sdl3::audio::AudioDevice::new(
                AudioDeviceID::Device(opened.into_id()),
                self.audio.clone(),
            )),
            Ok(None) => None,
            Err(e) => {
                error(
                    match kind {
                        Kind::Playback => t!("error.open_speaker"),
                        Kind::Recording => t!("error.open_microphone"),
                    },
                    e,
                );
                None
            }
        };
        true
    }

    /// The device `which` is gone: it's closed if it's the one open. Returns whether it was.
    fn removed(&mut self, which: u32) -> bool {
        if self.opening.is_some() {
            self.gone.push(which);
        }
        let open = self
            .open
            .as_ref()
            .is_some_and(|open| open.id().id() == which);
        if open {
            self.retire_open();
        }
        open
    }

    fn retire_open(&mut self) {
        if let Some(device) = self.open.take() {
            let id = device.id().id();
            // The worker's close is the only one: sdl3's would close the ID again, which SDL
            // doesn't document as allowed. Forgetting it keeps its subsystem reference counted
            // until the close is done, released then by the stand-in below.
            std::mem::forget(device);
            // SAFETY: what sdl3's `AudioSubsystem::new_unchecked` does, which it allows once per
            // one forgotten: it's a marker for a global count, and the forgotten one was counted.
            const { assert!(size_of::<AudioSubsystem>() == 0) };
            let forgotten = unsafe { std::ptr::read(&self.audio) };
            let done = in_background(move || unsafe { SDL_CloseAudioDevice(id) });
            self.retiring.push((forgotten, done));
        }
    }

    fn collect_retired(&mut self) {
        self.retiring
            .retain_mut(|(_, done)| matches!(done.try_recv(), Err(TryRecvError::Empty)));
    }

    fn begin_shutdown(&mut self) {
        self.opening = None;
        self.retire_open();
    }

    fn update<'a>(
        &'a mut self,
        kind: Kind,
        _ss: &Subsystems,
        ui: &mut egui::Ui,
        config: &mut Config,
        now: Instant,
    ) -> bool {
        self.refresh(kind, now);

        let mut test = false;
        config.maybe_update(|raw| {
            let mut device = kind.config(raw).clone();

            // A sub-menu rather than a `ComboBox`: egui only tracks one open popup per viewport,
            // so opening a `ComboBox` popup would close the menu it lives in.
            let label = setup_label(
                self.missing(),
                format!("{}: {}", kind.label(), device.name()),
            );
            let updated = ui
                .menu_button(label, |ui| {
                    let updated = self.options(kind, ui, &mut device);
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
        _ss: &Subsystems,
        ui: &mut egui::Ui,
        config: &mut Config,
        now: Instant,
    ) -> bool {
        self.refresh(kind, now);

        let mut changed = false;
        config.maybe_update(|raw| {
            let mut device = kind.config(raw).clone();
            changed = self.options(kind, ui, &mut device);
            if changed {
                kind.set(raw, device);
            }
            changed
        });
        changed
    }

    /// The system's default and every device, `device` the one picked: one picked is opened.
    /// Returns whether the pick changed.
    fn options(&mut self, kind: Kind, ui: &mut egui::Ui, device: &mut AudioDevice) -> bool {
        let mut updated = ui
            .selectable_value(device, AudioDevice::Default, t!("audio.system_default"))
            .clicked();

        for (id, name) in &self.devices {
            updated |= ui
                .selectable_value(device, AudioDevice::Device(*id, name.clone()), name.deref())
                .clicked();
        }

        if updated {
            // One still being opened isn't wanted any more: it's closed once it's open
            self.opening = Some(kind.open(device));
        }
        updated
    }

    /// Lists the devices again, every `DELAY`.
    fn refresh(&mut self, kind: Kind, now: Instant) {
        if now >= self.next {
            self.update_devices(kind);
            self.next = match self.next.checked_add(DELAY) {
                Some(next) => next.max(now),
                None => now,
            };
        }
    }

    fn update_devices(&mut self, kind: Kind) {
        self.devices.clear();
        if let Err(e) = kind.enumerate(&mut self.devices) {
            log::error!("Error updating audio devices: {e}");
        }
    }
}

/// The language the setup guide speaks in: the fixed reply language's, if it has a voice; or else
/// the Heard language; or else the `app`'s, if it has a voice or is `getting` one; or else the
/// first that has or is; or else the app's.
fn guide_language(config: &RawConfig, app: ArrayStr<2>, getting: &[ArrayStr<2>]) -> ArrayStr<2> {
    let mut voiced = config.voices().keys().chain(getting).copied();
    let lang = (config.engineer_reply_language())
        .or(*config.heard_language())
        .or_else(|| voiced.clone().any(|lang| lang == app).then_some(app))
        .or_else(|| voiced.next())
        .unwrap_or(app);
    language::voice_code(lang)
}

/// What the voice test says in `lang` (a voice code): the radio acknowledgement if the test
/// phrase isn't translated into it, as that's translated into nearly every language.
fn test_phrase(lang: ArrayStr<2>) -> &'static str {
    let lang: &str = &lang;
    let phrase = t!("audio.test_phrase": lang);
    match lang == "en" || phrase != t!("audio.test_phrase": "en") {
        true => phrase,
        false => acknowledgement::phrase(lang).unwrap_or(phrase),
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
    /// Blocks the UI thread, briefly: this must happen before sdl3 frees the callback, which is
    /// dropped with the stream on the thread that owns it, and sdl3's streams can't be sent to
    /// another (they hold the `!Send` `AudioSubsystem`). It waits for at most one callback call,
    /// which only copies the little audio that's arrived since the last (microseconds of work).
    /// Destroying the stream afterwards closes its own logical device, not the physical one,
    /// which the picked device keeps open.
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

    /// Opens `device` in the background: opening one can take the OS a while (WASAPI activates
    /// it, a Bluetooth headset may wake up), which would freeze the UI. What's received is
    /// [`DeviceSelector::poll`]ed.
    fn open(self, device: &AudioDevice) -> OpeningDevice {
        log::debug!("{device:#?}");
        let device = device.clone();
        // Nobody may want it any more (another was picked meanwhile): it closes as it's dropped
        in_background(move || self.open_raw(&device).map(|id| id.map(OpenedDevice)))
    }

    /// Opens `device`, on any thread: the SDL device functions used are thread safe, unlike
    /// sdl3's `AudioSubsystem` (which the ID is wrapped with on the UI thread). `None` if it's
    /// not plugged in.
    fn open_raw(self, device: &AudioDevice) -> Result<Option<SDL_AudioDeviceID>, sdl3::Error> {
        // SDL IDs are transient: a reconnected device has a new ID. Resolve the saved
        // name whenever opening, including a device that was absent at startup.
        let device = match device {
            AudioDevice::Default => match self {
                Kind::Playback => SDL_AUDIO_DEVICE_DEFAULT_PLAYBACK,
                Kind::Recording => SDL_AUDIO_DEVICE_DEFAULT_RECORDING,
            },
            AudioDevice::Device(_, name) | AudioDevice::Unknown(name) => {
                let mut devices = Vec::new();
                if let Err(e) = self.enumerate(&mut devices) {
                    log::warn!("Couldn't resolve audio device: {e}");
                    return Ok(None);
                }
                match resolve_device(name, &devices) {
                    Some(id) => id.id(),
                    None => return Ok(None),
                }
            }
        };

        let spec: sdl3::sys::audio::SDL_AudioSpec = match self {
            Kind::Playback => AudioSpec::default(),
            Kind::Recording => RECORDING_SPEC,
        }
        .into();
        let id = unsafe { SDL_OpenAudioDevice(device, &spec) };
        if id.0 == 0 {
            return Err(get_error());
        }
        Ok(Some(id))
    }

    /// The devices of this kind. It only copies SDL's list, which SDL keeps up to date on its own
    /// threads: it's quick enough for the UI thread, which lists them every `DELAY`.
    pub fn enumerate(self, dest: &mut Vec<(AudioDeviceID, Arc<str>)>) -> Result<(), sdl3::Error> {
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

/// A device being opened in the background: `None` if it's not plugged in.
type OpeningDevice = tokio::sync::oneshot::Receiver<Result<Option<OpenedDevice>, sdl3::Error>>;

/// A device opened in the background, closed if it's dropped before sdl3 is given it: whether
/// on its thread (nobody waits for it) or in the channel (nobody received it).
struct OpenedDevice(SDL_AudioDeviceID);

impl OpenedDevice {
    /// For sdl3 to close it instead.
    fn into_id(self) -> SDL_AudioDeviceID {
        let id = self.0;
        std::mem::forget(self);
        id
    }
}

impl Drop for OpenedDevice {
    fn drop(&mut self) {
        let id = self.0;
        // This value can be discarded from a result channel on the UI thread too.
        drop(in_background(move || unsafe { SDL_CloseAudioDevice(id) }));
    }
}

/// Runs `work` on its own thread, for its result to be received. If it's no longer wanted, it's
/// dropped where it is: `T` must let go of what it holds then.
fn in_background<T: Send + 'static>(
    work: impl FnOnce() -> T + Send + 'static,
) -> tokio::sync::oneshot::Receiver<T> {
    let (send, recv) = tokio::sync::oneshot::channel();
    struct Worker;
    impl Drop for Worker {
        fn drop(&mut self) {
            DEVICE_WORKERS.fetch_sub(1, Ordering::Release);
        }
    }
    DEVICE_WORKERS.fetch_add(1, Ordering::AcqRel);
    let worker = Worker;
    std::thread::spawn(move || {
        let _worker = worker;
        let _ = send.send(work());
    });
    recv
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

    /// Tells when it's dropped, like a device that's closed.
    struct Released(std::sync::mpsc::Sender<()>);

    impl Drop for Released {
        fn drop(&mut self) {
            let _ = self.0.send(());
        }
    }

    #[test]
    fn a_result_nobody_waits_for_is_let_go_of() {
        let (gone, released) = std::sync::mpsc::channel();
        let (go, wait) = std::sync::mpsc::channel::<()>();
        let recv = in_background(move || {
            wait.recv().unwrap();
            Released(gone)
        });
        // Dropped while it's still working, like a device picked over one being opened
        drop(recv);
        go.send(()).unwrap();
        assert_eq!(released.recv_timeout(Duration::from_secs(5)), Ok(()));
    }

    #[test]
    fn a_result_nobody_received_is_let_go_of() {
        let (gone, released) = std::sync::mpsc::channel();
        let recv = in_background(move || Released(gone));
        // Done, but picked over before the UI got to it
        while recv.is_empty() {
            std::thread::yield_now();
        }
        assert!(released.try_recv().is_err());
        drop(recv);
        assert_eq!(released.try_recv(), Ok(()));
    }

    #[test]
    fn a_result_wanted_is_received_without_waiting_for_it() {
        let (go, wait) = std::sync::mpsc::channel::<()>();
        let mut recv = in_background(move || {
            wait.recv().unwrap();
            7
        });
        assert_eq!(recv.try_recv(), Err(TryRecvError::Empty));
        go.send(()).unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            match recv.try_recv() {
                Ok(result) => break assert_eq!(result, 7),
                Err(TryRecvError::Empty) if Instant::now() < deadline => std::thread::yield_now(),
                Err(e) => panic!("{e}"),
            }
        }
    }

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

/// Whether configured model files are on disk, checked off the UI thread: a path is asked about
/// from the UI, and polled in the background from then on.
#[derive(Clone)]
pub struct ModelFiles {
    asked: Arc<Mutex<HashSet<Utf8PathBuf>>>,
    /// Whether each is a file, and when the check that found out started
    found: tokio::sync::watch::Receiver<HashMap<Utf8PathBuf, (bool, Instant)>>,
}

impl ModelFiles {
    pub fn new() -> Self {
        let asked = Arc::new(Mutex::new(HashSet::<Utf8PathBuf>::new()));
        let found = updating(Duration::from_millis(1234), HashMap::new(), {
            let asked = asked.clone();
            move |started| {
                let paths = asked.lock().unwrap().iter().cloned().collect::<Vec<_>>();
                async move {
                    let mut found = HashMap::with_capacity(paths.len());
                    for path in paths {
                        let file = tokio::fs::metadata(&path)
                            .await
                            .is_ok_and(|metadata| metadata.is_file());
                        found.insert(path, (file, started));
                    }
                    found
                }
            }
        });
        Self { asked, found }
    }

    /// Whether `path` is a file, as of the last check: `None` until it's been checked.
    pub fn checked(&self, path: &Utf8Path) -> Option<bool> {
        self.checked_since(path, None)
    }

    /// Like [`Self::checked`], but `None` until a check that started at or after `since`.
    pub fn checked_since(&self, path: &Utf8Path, since: Option<Instant>) -> Option<bool> {
        let (file, started) = self.found(path)?;
        since.is_none_or(|since| started >= since).then_some(file)
    }

    /// Whether `path` is a file, and when the check that found out started: `None` until it's been
    /// checked.
    pub fn found(&self, path: &Utf8Path) -> Option<(bool, Instant)> {
        if let Some(&found) = self.found.borrow().get(path) {
            return Some(found);
        }
        self.asked.lock().unwrap().insert(path.to_path_buf());
        None
    }

    /// Whether `path` is a file, assumed to be until it's been checked, so nothing is flagged
    /// missing for the first moments.
    pub fn is_file(&self, path: &Utf8Path) -> bool {
        self.checked(path).unwrap_or(true)
    }

    /// Whether a voice's files are on disk: Piper needs both the model and its sidecar
    /// configuration. `None` until they've been checked.
    pub fn voice_checked(&self, path: &Utf8Path) -> Option<bool> {
        self.voice_checked_since(path, None)
    }

    /// Like [`Self::voice_checked`], but `None` until a check that started at or after `since`.
    pub fn voice_checked_since(&self, path: &Utf8Path, since: Option<Instant>) -> Option<bool> {
        let model = self.checked_since(path, since);
        let json = self.checked_since(&path.with_added_extension("json"), since);
        Some(model? && json?)
    }

    /// Whether a voice's files are on disk, assumed to be until they've been checked.
    pub fn voice(&self, path: &Utf8Path) -> bool {
        self.voice_checked(path).unwrap_or(true)
    }
}

/// Deletes a downloaded model's files in the background. Its result is when it ended: checks of
/// the files that started before then may have found them still there.
#[derive(Default)]
struct Deletion(Option<MaybeDone<Instant>>);

impl Deletion {
    /// Deletes `files` in order, after `release` closes what may have them open. Those already
    /// missing count as deleted. It stops at the first that can't be deleted (its error is shown),
    /// so the rest are still there to use it: a voice's model goes before its config.
    fn start(&mut self, release: impl FnOnce() + Send + 'static, files: Vec<PathBuf>) {
        self.0 = Some(MaybeDone::spawn_blocking(move || {
            release();
            for file in files {
                match std::fs::remove_file(&file) {
                    Ok(()) => log::info!("Deleted {}", file.display()),
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                    Err(e) => {
                        error(t!("error.deleting"), format!("{}: {e}", file.display()));
                        break;
                    }
                }
            }
            Instant::now()
        }));
    }

    /// Whether the model is on disk, given what the last check `found` and when that check
    /// started. `None` if it's not known: also while it's deleted, and then until a check that
    /// started since tells.
    fn on_disk(&mut self, rt: &Handle, found: Option<(bool, Instant)>) -> Option<bool> {
        if let Some(task) = &mut self.0 {
            let ended = *task.update(rt)?;
            if found.is_none_or(|(_, started)| started < ended) {
                return None;
            }
            self.0 = None;
        }
        found.map(|(on_disk, _)| on_disk)
    }

    /// Whether a check that `found` (what, and when it started) may be from before the files were
    /// deleted: while they're deleted, or it started before that ended. Unlike [`Self::on_disk`], for
    /// another check of the same files.
    fn stale(&mut self, rt: &Handle, found: Option<(bool, Instant)>) -> bool {
        match &mut self.0 {
            None => false,
            Some(task) => match task.update(rt) {
                None => true,
                Some(ended) => found.is_none_or(|(_, started)| started < *ended),
            },
        }
    }

    /// Whether it was started, and [`Self::on_disk`] doesn't know how it went yet.
    fn pending(&self) -> bool {
        self.0.is_some()
    }

    /// Whether the files are still being deleted.
    fn running(&self) -> bool {
        matches!(&self.0, Some(MaybeDone::Task(task)) if !task.is_finished())
    }
}

/// Leaves room for a list's scroll bar, which otherwise widens over the buttons at its rows' ends.
fn scroll_bar_beside(ui: &mut egui::Ui) {
    let scroll = &mut ui.spacing_mut().scroll;
    scroll.floating_allocated_width = scroll.bar_width;
}

#[cfg(test)]
mod deletion_tests {
    use super::*;

    /// What the row goes by once the deletion is done, given a check that `found` the files and
    /// started at `started` (now if `None`).
    async fn after(
        deletion: &mut Deletion,
        found: Option<bool>,
        started: Option<Instant>,
    ) -> Option<bool> {
        let deadline = Instant::now() + Duration::from_secs(10);
        while deletion.running() {
            assert!(Instant::now() < deadline, "timed out");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let found = found.map(|found| (found, started.unwrap_or_else(Instant::now)));
        tokio::task::block_in_place(|| deletion.on_disk(&Handle::current(), found))
    }

    /// Takes a while to let go of a file, as a voice's worker does, and notes whether it was still
    /// there.
    struct User(PathBuf, Arc<AtomicU32>);

    impl Drop for User {
        fn drop(&mut self) {
            std::thread::sleep(Duration::from_millis(50));
            self.1.store(1 + self.0.exists() as u32, Ordering::Release);
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn deletes_every_file_once_nothing_uses_them() {
        let dir = tempfile::tempdir().unwrap();
        let files = ["voice.onnx", "voice.onnx.json"].map(|file| dir.path().join(file));
        for file in &files {
            std::fs::write(file, []).unwrap();
        }
        let other = dir.path().join("other.onnx");
        std::fs::write(&other, []).unwrap();

        let dropped = Arc::new(AtomicU32::new(0));
        let mut deletion = Deletion::default();
        let user = User(files[0].clone(), dropped.clone());
        deletion.start(move || drop(user), files.to_vec());
        assert!(deletion.pending());
        assert_eq!(after(&mut deletion, Some(false), None).await, Some(false));
        assert!(!deletion.pending());
        assert_eq!(dropped.load(Ordering::Acquire), 2);
        assert!(files.iter().all(|file| !file.exists()));
        assert!(other.exists());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn only_a_check_started_since_the_deletion_is_believed() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("model.bin");
        std::fs::write(&file, []).unwrap();
        let mut deletion = Deletion::default();
        assert_eq!(after(&mut deletion, Some(true), None).await, Some(true));

        let before = Instant::now();
        deletion.start(|| (), vec![file]);
        for _ in 0..2 {
            assert_eq!(after(&mut deletion, Some(true), Some(before)).await, None);
        }
        // Not checked (successfully) since
        assert_eq!(after(&mut deletion, None, None).await, None);
        // Back already, e.g. downloaded by another program
        assert_eq!(after(&mut deletion, Some(true), None).await, Some(true));
        assert!(!deletion.pending());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn files_already_gone_count_as_deleted() {
        let dir = tempfile::tempdir().unwrap();
        let mut deletion = Deletion::default();
        let before = Instant::now();
        deletion.start(|| (), vec![dir.path().join("missing.bin")]);
        assert_eq!(after(&mut deletion, Some(true), Some(before)).await, None);
        assert_eq!(after(&mut deletion, Some(false), None).await, Some(false));
        // Downloaded again
        assert_eq!(after(&mut deletion, Some(true), None).await, Some(true));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn what_cant_be_deleted_stays_usable() {
        let dir = tempfile::tempdir().unwrap();
        // A folder can't be deleted as a file
        let stuck = dir.path().join("stuck.bin");
        std::fs::create_dir(&stuck).unwrap();
        let file = dir.path().join("file.bin");
        std::fs::write(&file, []).unwrap();

        let mut deletion = Deletion::default();
        deletion.start(|| (), vec![stuck.clone(), file.clone()]);
        assert_eq!(after(&mut deletion, Some(true), None).await, Some(true));
        assert!(!deletion.pending());
        // The files after it are kept, so it can still be used
        assert!(stuck.exists());
        assert!(file.exists());
    }
}

#[cfg(test)]
mod model_files_tests {
    use super::*;

    #[tokio::test(start_paused = true)]
    async fn a_voice_needs_both_files_after_redownloading() {
        let dir = tempfile::tempdir().unwrap();
        let path = camino::Utf8PathBuf::from_path_buf(dir.path().join("voice.onnx")).unwrap();
        let files = ModelFiles::new();
        let mut found = files.found.clone();
        let mut check = async || {
            files.voice(&path);
            found.changed().await.unwrap();
            found.changed().await.unwrap();
            files.voice(&path)
        };
        assert!(!check().await);
        std::fs::write(&path, []).unwrap();
        assert!(!check().await);
        std::fs::write(path.with_added_extension("json"), []).unwrap();
        assert!(check().await);
        std::fs::remove_file(&path).unwrap();
        assert!(!check().await);
    }
}

#[cfg(test)]
mod guide_tests {
    use super::*;

    #[tokio::test(flavor = "multi_thread")]
    async fn the_guide_speaks_the_pilots_language() {
        let rt = Handle::current();
        let [en, es, ca, fr] = [arrstr!("en"), arrstr!("es"), arrstr!("ca"), arrstr!("fr")];
        let mut synth = Synthesizer::new(&rt, &RawConfig::default());
        let mut config = RawConfig::default();
        // Nothing to go by
        assert_eq!(guide_language(&config, es, &[]), es);
        // The voices being set up count
        assert_eq!(guide_language(&config, es, &[ca, es]), es);
        assert_eq!(guide_language(&config, en, &[ca, es]), ca);

        for lang in [ca, es, en] {
            config.set_voice(&rt, lang, format!("{}.onnx", &*lang).into(), &mut synth);
        }
        assert_eq!(guide_language(&config, es, &[]), es);
        assert_eq!(guide_language(&config, fr, &[]), ca);
        config.heard_language.set(Some(fr));
        assert_eq!(guide_language(&config, es, &[]), fr);
        // Norwegian, as its voices have it
        config.heard_language.set(Some(arrstr!("nb")));
        assert_eq!(guide_language(&config, es, &[]), arrstr!("no"));
        // The fixed reply language, if it has a voice
        config.spoken_language.set(Some(en));
        assert_eq!(guide_language(&config, es, &[]), en);
        config.spoken_language.set(Some(fr));
        assert_eq!(guide_language(&config, es, &[]), arrstr!("no"));
    }

    #[test]
    fn the_test_phrase_is_in_the_voices_language() {
        assert_eq!(test_phrase(arrstr!("es")), t!("audio.test_phrase": "es"));
        assert_eq!(test_phrase(arrstr!("en")), t!("audio.test_phrase": "en"));
        // Not translated: the acknowledgement is
        assert_eq!(test_phrase(arrstr!("fr")), t!("radio.acknowledgement": "fr"));
    }
}
