//! How long a question took to be answered, from the pilot's side: logged once for each question,
//! when everything holding its [`Timeline`] has let go of it.
//!
//! Marks are set from the UI thread and the conversation's task, and only ever store a time.

use std::{
    fmt::{self, Display, Formatter},
    sync::OnceLock,
    time::Instant,
};

pub struct Timeline {
    /// When the talk button was let go, for a question asked on the radio: what the rest is
    /// measured from. A typed one is measured from when it's asked.
    released: Option<Instant>,
    asked: Instant,
    acknowledged: OnceLock<Instant>,
    first_text: OnceLock<Instant>,
    answered: OnceLock<Instant>,
    first_audio: OnceLock<Instant>,
    played: OnceLock<Instant>,
}

impl Timeline {
    pub fn new(released: Option<Instant>, asked: Instant) -> Self {
        Self {
            released,
            asked,
            acknowledged: OnceLock::new(),
            first_text: OnceLock::new(),
            answered: OnceLock::new(),
            first_audio: OnceLock::new(),
            played: OnceLock::new(),
        }
    }

    /// What the rest is measured from: the talk button's release, or when a typed question was
    /// asked.
    pub fn start(&self) -> Instant {
        self.released.unwrap_or(self.asked)
    }

    /// The app acknowledged the question, the model having said nothing yet.
    #[inline]
    pub fn acknowledged(&self) {
        let _ = self.acknowledged.set(Instant::now());
    }

    /// The model wrote something to say. Only the first counts, as with every mark.
    #[inline]
    pub fn first_text(&self) {
        let _ = self.first_text.set(Instant::now());
    }

    /// The model's turn ended without an error, however it stopped (the `Engineer` log line says
    /// how). Without this mark, the turn failed or the pilot moved on.
    #[inline]
    pub fn answered(&self) {
        let _ = self.answered.set(Instant::now());
    }

    /// The first of the speech reached the speaker's stream: what's left is SDL's and the
    /// device's buffering.
    #[inline]
    pub fn first_audio(&self) {
        let _ = self.first_audio.set(Instant::now());
    }

    /// All of the speech was played. Without this mark, the answer was cut off or never spoken.
    /// Changing the voice ends the speech early the same way it ends normally, so an answer cut
    /// off by that still gets this mark. Speech that never reached the speaker's stream (see
    /// [`Self::first_audio`]) said nothing, and doesn't get it either.
    #[inline]
    pub fn played(&self) {
        if self.first_audio.get().is_some() {
            let _ = self.played.set(Instant::now());
        }
    }
}

impl Display for Timeline {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        struct Since(Instant, Option<Instant>);

        impl Display for Since {
            fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
                match self.1 {
                    Some(at) => write!(f, "{:.2}s", at.duration_since(self.0).as_secs_f64()),
                    None => f.write_str("never"),
                }
            }
        }

        let start = self.start();
        let since = |at: &OnceLock<Instant>| Since(start, at.get().copied());
        write!(f, "Question timeline, from ")?;
        match self.released {
            Some(_) => write!(
                f,
                "the talk button's release: asked {}, ",
                Since(start, Some(self.asked))
            )?,
            None => f.write_str("being typed: ")?,
        }
        write!(
            f,
            "acknowledged {}, first text {}, first audio {}, answered {}, played {}",
            since(&self.acknowledged),
            since(&self.first_text),
            since(&self.first_audio),
            since(&self.answered),
            since(&self.played),
        )
    }
}

impl Drop for Timeline {
    fn drop(&mut self) {
        log::info!("{self}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn measures_a_spoken_question_from_the_release() {
        let released = Instant::now();
        let timeline = Timeline::new(Some(released), released + Duration::from_millis(850));
        timeline
            .first_text
            .set(released + Duration::from_millis(4300))
            .unwrap();
        timeline
            .first_audio
            .set(released + Duration::from_millis(4500))
            .unwrap();
        timeline
            .answered
            .set(released + Duration::from_millis(4600))
            .unwrap();
        timeline
            .played
            .set(released + Duration::from_secs(8))
            .unwrap();
        assert_eq!(
            timeline.to_string(),
            "Question timeline, from the talk button's release: asked 0.85s, acknowledged never, \
             first text 4.30s, first audio 4.50s, answered 4.60s, played 8.00s"
        );
    }

    #[test]
    fn measures_the_acknowledgement() {
        let released = Instant::now();
        let timeline = Timeline::new(Some(released), released + Duration::from_millis(850));
        timeline
            .acknowledged
            .set(released + Duration::from_millis(1750))
            .unwrap();
        assert!(
            timeline.to_string().contains("acknowledged 1.75s, first text never"),
            "{timeline}"
        );
    }

    #[test]
    fn says_what_never_happened_and_measures_a_typed_question_from_being_asked() {
        let asked = Instant::now();
        let timeline = Timeline::new(None, asked);
        timeline
            .answered
            .set(asked + Duration::from_secs(2))
            .unwrap();
        assert_eq!(
            timeline.to_string(),
            "Question timeline, from being typed: acknowledged never, first text never, first \
             audio never, answered 2.00s, played never"
        );
    }

    #[test]
    fn only_the_first_mark_counts() {
        let timeline = Timeline::new(None, Instant::now());
        timeline.first_text();
        let first = *timeline.first_text.get().unwrap();
        std::thread::sleep(Duration::from_millis(2));
        timeline.first_text();
        assert_eq!(*timeline.first_text.get().unwrap(), first);
    }
}
