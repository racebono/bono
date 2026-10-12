use crate::{
    app::{
        Subsystems,
        topbar::audio::{
            voice::Voices,
            whisper::{RECORDING_SPEC, Whisper},
        },
    },
    arrstr,
    config::{AudioDevice, Config, RawConfig},
    error,
    synthesizer::{Speech, Synthesizer},
};
use camino::Utf8Path;
use sdl3::{
    AudioSubsystem,
    audio::{AudioDeviceID, AudioSpec},
    event::Event,
    get_error,
    sys::{
        audio::{SDL_GetAudioPlaybackDevices, SDL_GetAudioRecordingDevices},
        stdinc::SDL_free,
    },
};
use std::{
    ops::{Deref, DerefMut},
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::runtime::Handle;

mod voice;
mod whisper;

const DELAY: Duration = Duration::from_secs(1);

pub struct AudioTopbar {
    pub microphone: DeviceSelector,
    pub speaker: DeviceSelector,
    pub whisper: Whisper,
    pub voices: Voices,
    pub test: Option<tokio::sync::oneshot::Receiver<anyhow::Result<Speech>>>,
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
                Ok(Ok(speech)) => {
                    self.test = None;
                    log::debug!("Test synthesized");
                    if let Some(device) = self.speaker.get() {
                        if let Err(e) = speech.play(device) {
                            error("Error playing test speech", e);
                        }
                    } else {
                        log::warn!("No speaker selected (audio will not be played)");
                    }
                }
                Ok(Err(e)) => {
                    self.test = None;
                    error("Error synthesizing speech", e);
                }
                Err(..) => {}
            }
        }
    }

    pub fn event(&mut self, rt: &Handle, config: &Config, event: &Event) -> bool {
        if let Some(device) = self.microphone.get() {
            match self.whisper.event(rt, &event, config, device) {
                Ok(handle) => {
                    // FIXME(TODO): Save into AI session
                    tokio::spawn(async move {
                        log::debug!("TODO");
                        match handle.await {
                            Ok(Some((msg, lang))) => log::debug!("({lang}) {msg}"),
                            Ok(None) => log::debug!("Didn't understand"),
                            Err(e) => error("Failed speech to text", e),
                        }
                    });
                }
                Err(e) => return e,
            };
        }

        return false;
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
            let config = kind.config_mut(raw);

            // A sub-menu rather than a `ComboBox`: egui only tracks one open popup per viewport,
            // so opening a `ComboBox` popup would close the menu it lives in.
            let label = format!("{}: {}", kind.label(), config.name());
            return ui
                .menu_button(label, |ui| {
                    let mut updated = ui
                        .selectable_value(config, AudioDevice::Default, "System default")
                        .clicked();

                    for (device, name) in &self.devices {
                        updated |= ui
                            .selectable_value(
                                config,
                                AudioDevice::Device(*device, name.clone()),
                                name.deref(),
                            )
                            .clicked();
                    }

                    if kind == Kind::Playback {
                        ui.separator();
                        test = ui.button("Test").clicked();
                    }

                    if updated {
                        self.open = kind.open(&ss.audio, config);
                    }

                    return updated;
                })
                .inner
                .unwrap_or(false);
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
    pub const fn label(self) -> &'static str {
        match self {
            Kind::Playback => "Speaker",
            Kind::Recording => "Microphone",
        }
    }

    pub fn config<'a>(self, raw: &'a RawConfig) -> &'a AudioDevice {
        match self {
            Kind::Playback => raw.speaker.deref(),
            Kind::Recording => raw.microphone.deref(),
        }
    }

    pub fn config_mut<'a>(self, raw: &'a mut RawConfig) -> &'a mut AudioDevice {
        match self {
            Kind::Playback => raw.speaker.deref_mut(),
            Kind::Recording => raw.microphone.deref_mut(),
        }
    }

    pub fn open(
        self,
        audio: &AudioSubsystem,
        device: &AudioDevice,
    ) -> Option<sdl3::audio::AudioDevice> {
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
                        Kind::Playback => "Failed to open speaker device",
                        Kind::Recording => "Failed to open microphone device",
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
