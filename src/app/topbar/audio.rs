use crate::{
    app::Subsystems,
    config::{AudioDevice, Config, RawConfig},
};
use sdl3::{
    AudioSubsystem,
    audio::AudioDeviceID,
    get_error,
    sys::{
        audio::{
            SDL_AUDIO_DEVICE_DEFAULT_PLAYBACK, SDL_AUDIO_DEVICE_DEFAULT_RECORDING,
            SDL_GetAudioPlaybackDevices, SDL_GetAudioRecordingDevices,
        },
        stdinc::SDL_free,
    },
};
use std::{
    ops::{Deref, DerefMut},
    sync::Arc,
    time::{Duration, Instant},
};

pub struct AudioTopbar {
    microphone: DeviceSelector,
    speaker: DeviceSelector,
}

impl AudioTopbar {
    pub fn update(&mut self, ss: &Subsystems, ui: &mut egui::Ui, config: &mut Config) {
        let now = Instant::now();
        self.microphone.update(Kind::Recording, ss, ui, config, now);
        self.speaker.update(Kind::Playback, ss, ui, config, now);
    }
}

impl Default for AudioTopbar {
    fn default() -> Self {
        let now = Instant::now();
        Self {
            microphone: DeviceSelector {
                next: now,
                delay: Duration::from_secs(1),
                devices: Vec::new(),
            },
            speaker: DeviceSelector {
                next: now,
                delay: Duration::from_secs(1),
                devices: Vec::new(),
            },
        }
    }
}

struct DeviceSelector {
    next: Instant,
    delay: Duration,
    devices: Vec<(AudioDeviceID, Arc<str>)>,
}

impl DeviceSelector {
    fn update(
        &mut self,
        kind: Kind,
        ss: &Subsystems,
        ui: &mut egui::Ui,
        config: &mut Config,
        now: Instant,
    ) {
        if now >= self.next {
            self.update_devices(kind, ss);
            self.next = match self.next.checked_add(self.delay) {
                Some(next) => next.max(now),
                None => now,
            };
        }

        config.maybe_update(|raw| {
            let config = kind.config(raw);

            // A sub-menu rather than a `ComboBox`: egui only tracks one open popup per viewport,
            // so opening a `ComboBox` popup would close the menu it lives in.
            let label = format!("{}: {}", kind.label(), config.name());
            return ui
                .menu_button(label, move |ui| {
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

                    ui.separator();
                    if ui.button("Test").clicked() {
                        log::debug!("TODO");
                    }

                    return updated;
                })
                .inner
                .unwrap_or(false);
        });
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

    pub const fn default_device(self) -> AudioDeviceID {
        match self {
            Kind::Playback => AudioDeviceID::Device(SDL_AUDIO_DEVICE_DEFAULT_PLAYBACK),
            Kind::Recording => AudioDeviceID::Device(SDL_AUDIO_DEVICE_DEFAULT_RECORDING),
        }
    }

    pub fn config<'a>(self, raw: &'a mut RawConfig) -> &'a mut AudioDevice {
        match self {
            Kind::Playback => raw.speaker.deref_mut(),
            Kind::Recording => raw.microphone.deref_mut(),
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
