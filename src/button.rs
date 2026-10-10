use sdl3::{
    EventSubsystem,
    event::{Event, EventWatch, EventWatchCallback},
    keyboard::{Mod, Scancode},
    sys::guid::SDL_GUID,
};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Button {
    Key(#[serde(with = "scancode")] Scancode),
    Joystick {
        #[serde(with = "guid")]
        guid: SDL_GUID,
        button: u32,
    },
}

pub struct ButtonWatcher {
    watch: EventWatch<ButtonWatcherCallback>,
    recv: tokio::sync::mpsc::UnboundedReceiver<Button>,
}

impl ButtonWatcher {
    pub fn new(event: &EventSubsystem) -> anyhow::Result<Self> {
        let (send, recv) = tokio::sync::mpsc::unbounded_channel();
        let watch = event.add_event_watch(ButtonWatcherCallback { send });
        Ok(Self { watch, recv })
    }

    pub async fn next(&mut self) -> Option<Button> {
        self.recv.recv().await
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
                keymod: Mod::NOMOD,
                ..
            } => {
                let _ = self.send.send(Button::Key(scancode));
            }
            Event::JoyButtonDown {
                timestamp,
                which,
                button_idx,
            } => {}
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
