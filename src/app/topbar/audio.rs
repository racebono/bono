use crate::{
    app::{
        Subsystems,
        topbar::audio::{
            voice::Voices,
            whisper::{RECORDING_SPEC, Whisper},
        },
    },
    arrstr,
    config::{ArrayStr, AudioDevice, Config, RawConfig},
    error,
    synthesizer::{Speech, Synthesizer},
    task::Task,
};
use camino::Utf8Path;
use lambiase_i18n::t;
use sdl3::{
    AudioSubsystem,
    audio::{AudioDeviceID, AudioSpec, AudioStreamOwner},
    event::Event,
    get_error,
    sys::{
        audio::{SDL_GetAudioPlaybackDevices, SDL_GetAudioRecordingDevices},
        stdinc::SDL_free,
    },
};
use std::{
    ops::Deref,
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::{runtime::Handle, sync::oneshot::error::TryRecvError};

mod voice;
mod whisper;

const DELAY: Duration = Duration::from_secs(1);

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
    transcription: Option<Task<anyhow::Result<Option<(String, ArrayStr<2>)>>>>,
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
        }
    }

    /// What the pilot said on the radio, with its language, once it's transcribed.
    pub fn transcription(&mut self, rt: &Handle) -> Option<(String, ArrayStr<2>)> {
        let result = self.transcription.as_mut()?.update(rt)?;
        self.transcription = None;
        match result {
            Ok(Some((text, lang))) => {
                log::info!("Pilot ({lang}): {text}");
                Some((text, lang))
            }
            Ok(None) => {
                log::debug!("Nothing understood on the radio");
                None
            }
            Err(e) => {
                error(t!("error.speech_to_text"), e);
                None
            }
        }
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
        if self.speaker.update(Kind::Playback, ss, ui, config, now) && self.test.is_none() {
            log::debug!("Start test");
            self.test = Some(synth.synthesize(
                rt,
                arrstr!("en"),
                "Hello, world!".into(),
                config.get().synth_effects(),
            ));
        }

        self.whisper.update(rt, config, ui);
        self.voices.update(rt, config, ui, synth);

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

    pub fn event(&mut self, rt: &Handle, config: &Config, event: &Event) -> bool {
        if let Some(device) = self.microphone.get() {
            let was_recording = self.whisper.recording();
            let result = self.whisper.event(rt, &event, config, device);
            if !was_recording && self.whisper.recording() {
                self.beep(config);
            }

            match result {
                // Picked up by `transcription`. One still being transcribed is dropped: the
                // pilot already said something newer
                Ok(handle) => self.transcription = Some(handle.into()),
                Err(e) => return e,
            };
        }

        return false;
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

    fn update<'a>(
        &'a mut self,
        kind: Kind,
        ss: &Subsystems,
        ui: &mut egui::Ui,
        config: &mut Config,
        now: Instant,
    ) -> bool {
        if now >= self.next {
            self.update_devices(kind, ss);
            self.next = match self.next.checked_add(DELAY) {
                Some(next) => next.max(now),
                None => now,
            };
        }

        let mut test = false;
        config.maybe_update(|raw| {
            let mut device = kind.config(raw).clone();

            // A sub-menu rather than a `ComboBox`: egui only tracks one open popup per viewport,
            // so opening a `ComboBox` popup would close the menu it lives in.
            let label = format!("{}: {}", kind.label(), device.name());
            let updated = ui
                .menu_button(label, |ui| {
                    let mut updated = ui
                        .selectable_value(
                            &mut device,
                            AudioDevice::Default,
                            t!("audio.system_default"),
                        )
                        .clicked();

                    for (id, name) in &self.devices {
                        updated |= ui
                            .selectable_value(
                                &mut device,
                                AudioDevice::Device(*id, name.clone()),
                                name.deref(),
                            )
                            .clicked();
                    }

                    if kind == Kind::Playback {
                        ui.separator();
                        test = ui.button(t!("audio.test")).clicked();
                    }

                    if updated {
                        self.open = kind.open(&ss.audio, &device);
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

    fn update_devices(&mut self, kind: Kind, ss: &Subsystems) {
        self.devices.clear();
        if let Err(e) = kind.enumerate(&ss.audio, &mut self.devices) {
            log::error!("Error updating audio devices: {e}");
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
        let device = match device {
            AudioDevice::Default => None,
            AudioDevice::Device(id, _) => Some(id),
            AudioDevice::Unknown(_) => return None,
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

        dest.reserve(num_devices as usize);
        for i in 0..num_devices {
            let instance_id = AudioDeviceID::Device(unsafe { *devices.offset(i as isize) });
            let name = instance_id.name()?;
            dest.push((instance_id, name.into()));
        }

        unsafe { SDL_free(devices.cast()) };
        return Ok(());
    }
}
