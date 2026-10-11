use crate::{
    app::Subsystems,
    config::{Config, RawConfig},
};
use egui::PopupCloseBehavior;
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
use std::time::{Duration, Instant};
use tokio::runtime::Runtime;

pub struct AudioTopbar {}

impl AudioTopbar {
    pub fn update(
        &mut self,
        rt: &Runtime,
        ss: &Subsystems,
        ui: &mut egui::Ui,
        config: &mut Config,
    ) {
        let now = Instant::now();
        todo!()
    }
}

struct DeviceSelector {
    salt: &'static str,
    next: Instant,
    delay: Duration,
    devices: Vec<AudioDeviceID>,
}

impl DeviceSelector {
    fn new(kind: Kind, ss: &Subsystems) {}

    fn update(
        &mut self,
        kind: Kind,
        rt: &Runtime,
        ss: &Subsystems,
        ui: &mut egui::Ui,
        config: &mut Config,
        now: Instant,
    ) {
        if now >= self.next {
            self.update_devices(kind, ss);
        }

        let mut selected = 0;
        let mut combo = egui::ComboBox::from_id_salt(self.salt)
            .close_behavior(PopupCloseBehavior::CloseOnClickOutside);

        config.maybe_update(|raw| {
            return combo
                .show_ui(ui, |ui| {
                    let config = kind.config(raw);

                    let mut updated = ui
                        .selectable_value(config, None, "System default")
                        .clicked();

                    for device in &self.devices {
                        updated |= ui.selectable_value(config, Some(), text).clicked();
                    }

                    return updated;
                })
                .inner
                .unwrap_or(false);
        });

        todo!()
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
    pub const fn default_device(self) -> AudioDeviceID {
        match self {
            Kind::Playback => AudioDeviceID::Device(SDL_AUDIO_DEVICE_DEFAULT_PLAYBACK),
            Kind::Recording => AudioDeviceID::Device(SDL_AUDIO_DEVICE_DEFAULT_RECORDING),
        }
    }

    pub const fn config<'a>(self, raw: &'a mut RawConfig) -> &'a mut Option<String> {
        match self {
            Kind::Playback => &mut raw.speaker,
            Kind::Recording => &mut raw.microphone,
        }
    }

    pub fn enumerate(
        self,
        _audio: &AudioSubsystem,
        dest: &mut Vec<AudioDeviceID>,
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
            let instance_id = unsafe { *devices.offset(i as isize) };
            dest.push(AudioDeviceID::Device(instance_id));
        }

        unsafe { SDL_free(devices.cast()) };
        return Ok(());
    }
}
