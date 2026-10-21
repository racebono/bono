use sdl3::{
    EventSubsystem, IntegerOrSdlError, JoystickSubsystem,
    event::{Event, EventWatch, EventWatchCallback},
    joystick::{Joystick, JoystickId},
    keyboard::Scancode,
    sys::guid::SDL_GUID,
};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Every joystick plugged in (wheels, button boxes, pedals), kept open: SDL only reports the
/// buttons of the joysticks that are open.
pub struct Joysticks {
    subsystem: JoystickSubsystem,
    open: HashMap<JoystickId, Joystick>,
}

impl Joysticks {
    pub fn new(subsystem: &JoystickSubsystem) -> Self {
        let mut this = Self {
            subsystem: subsystem.clone(),
            open: HashMap::new(),
        };
        match subsystem.joysticks() {
            Ok(ids) => ids.into_iter().for_each(|id| this.open(id)),
            Err(e) => log::error!("Couldn't list the joysticks: {e}"),
        }
        return this;
    }

    /// Opens the joysticks plugged in later, and lets go of the ones unplugged.
    pub fn event(&mut self, event: &Event) {
        match *event {
            Event::JoyDeviceAdded { which, .. } => self.open(which),
            Event::JoyDeviceRemoved { which, .. } => {
                if let Some(joystick) = self.open.remove(&which) {
                    log::info!("Joystick unplugged: {}", joystick.name());
                }
            }
            _ => {}
        }
    }

    fn open(&mut self, id: JoystickId) {
        if self.open.contains_key(&id) {
            return;
        }
        match self.subsystem.open(id) {
            Ok(joystick) => {
                log::info!(
                    "Joystick: {} ({} buttons)",
                    joystick.name(),
                    joystick.num_buttons()
                );
                self.open.insert(id, joystick);
            }
            Err(e) => log::error!("Couldn't open joystick {}: {e}", id.raw()),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Button {
    Key(Scancode),
    Joystick(JoystickId, u8),
}

impl Button {
    pub fn to_config(
        self,
        joystick: &JoystickSubsystem,
    ) -> Result<ConfigButton, IntegerOrSdlError> {
        return Ok(match self {
            Button::Key(scancode) => ConfigButton::Key(scancode),
            Button::Joystick(id, button) => ConfigButton::Joystick {
                guid: joystick.open(id)?.guid().raw(),
                button,
            },
        });
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ConfigButton {
    Key(#[serde(with = "scancode")] Scancode),
    Joystick {
        #[serde(with = "guid")]
        guid: SDL_GUID,
        button: u8,
    },
}

impl ConfigButton {
    /// Whether `event` presses this button (key repeats aside).
    pub fn pressed(&self, event: &Event) -> bool {
        match (self, event) {
            (
                ConfigButton::Key(key),
                Event::KeyDown {
                    scancode: Some(scancode),
                    repeat: false,
                    ..
                },
            ) => key == scancode,
            (
                ConfigButton::Joystick { guid, button },
                Event::JoyButtonDown {
                    which, button_idx, ..
                },
            ) => {
                button == button_idx
                    && unsafe {
                        sdl3::sys::joystick::SDL_GetJoystickGUIDForID(
                            sdl3::sys::joystick::SDL_JoystickID(which.raw()),
                        )
                    } == *guid
            }
            _ => false,
        }
    }

    /// Whether `event` lets go of this button.
    pub fn released(&self, event: &Event) -> bool {
        match (self, event) {
            (
                ConfigButton::Key(key),
                Event::KeyUp {
                    scancode: Some(scancode),
                    ..
                },
            ) => key == scancode,
            (
                ConfigButton::Joystick { guid, button },
                Event::JoyButtonUp {
                    which, button_idx, ..
                },
            ) => {
                button == button_idx
                    && unsafe {
                        sdl3::sys::joystick::SDL_GetJoystickGUIDForID(
                            sdl3::sys::joystick::SDL_JoystickID(which.raw()),
                        )
                    } == *guid
            }
            _ => false,
        }
    }
}

pub struct ButtonWatcher {
    _watch: EventWatch<ButtonWatcherCallback>,
    recv: tokio::sync::mpsc::UnboundedReceiver<Button>,
}

impl ButtonWatcher {
    pub fn new(event: &EventSubsystem) -> anyhow::Result<Self> {
        let (send, recv) = tokio::sync::mpsc::unbounded_channel();
        let watch = event.add_event_watch(ButtonWatcherCallback { send });
        Ok(Self {
            _watch: watch,
            recv,
        })
    }

    pub async fn next(&mut self) -> Option<Button> {
        return self.recv.recv().await;
    }
}

struct ButtonWatcherCallback {
    send: tokio::sync::mpsc::UnboundedSender<Button>,
}

impl EventWatchCallback for ButtonWatcherCallback {
    fn callback(&mut self, event: Event) {
        match event {
            Event::KeyDown {
                scancode: Some(scancode),
                ..
            } => {
                let _ = self.send.send(Button::Key(scancode));
            }
            Event::JoyButtonDown {
                which, button_idx, ..
            } => {
                let _ = self.send.send(Button::Joystick(which, button_idx));
            }
            _ => {}
        }
    }
}

mod guid {
    use sdl3::sys::guid::SDL_GUID;
    use serde::{Deserialize, Deserializer, Serialize, Serializer};

    #[inline]
    pub fn serialize<S: Serializer>(v: &SDL_GUID, s: S) -> Result<S::Ok, S::Error> {
        u128::from_le_bytes(v.data).serialize(s)
    }

    #[inline]
    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<SDL_GUID, D::Error> {
        Ok(SDL_GUID {
            data: u128::deserialize(d)?.to_le_bytes(),
        })
    }
}

mod scancode {
    use sdl3::keyboard::Scancode;
    use serde::{Deserialize, Deserializer, Serialize, Serializer};

    #[inline]
    pub fn serialize<S: Serializer>(v: &Scancode, s: S) -> Result<S::Ok, S::Error> {
        v.name().serialize(s)
    }

    #[inline]
    pub fn deserialize<'de, D>(d: D) -> Result<Scancode, D::Error>
    where
        D: Deserializer<'de>,
    {
        let name = <&'de str>::deserialize(d)?;
        return Scancode::from_name(name).ok_or_else(|| {
            <D::Error as serde::de::Error>::custom("expected a valid scancode name")
        });
    }
}
