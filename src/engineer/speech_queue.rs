//! PCM retained only until its word has been consumed by SDL.
//!
//! SDL's input queue is measured in bytes of the submitted format (mono f32),
//! independent of the output device's channel count and resampling rate. It
//! measures device consumption, not the later acoustic arrival at the listener.
use crate::synthesizer::Speech;
use std::collections::VecDeque;

#[derive(Default)]
pub(super) struct SpeechQueue {
    words: VecDeque<Speech>,
    samples: usize,
}

impl SpeechQueue {
    pub fn push(&mut self, speech: Speech) {
        self.samples += speech.samples.len();
        self.words.push_back(speech);
    }

    /// Forget fully consumed words; retain the entire partially consumed word.
    /// After requeueing `remaining`, the same calculation starts at its beginning.
    pub fn advance(&mut self, queued_bytes: i32) {
        let queued = (queued_bytes.max(0) as usize).div_ceil(size_of::<f32>());
        let mut consumed = self.samples.saturating_sub(queued);
        while let Some(word) = self.words.front() {
            let len = word.samples.len();
            if consumed < len {
                break;
            }
            consumed -= len;
            self.samples -= len;
            self.words.pop_front();
        }
    }

    pub fn remaining(&self) -> impl Iterator<Item = &Speech> {
        self.words.iter()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn queue() -> SpeechQueue {
        let mut queue = SpeechQueue::default();
        for (id, len) in [(1.0, 10), (2.0, 20), (3.0, 30)] {
            queue.push(Speech {
                samples: vec![id; len],
                sample_rate: 24000,
            });
        }
        queue
    }

    fn ids(queue: &SpeechQueue) -> Vec<f32> {
        queue.remaining().map(|word| word.samples[0]).collect()
    }

    #[test]
    fn interruption_replays_partial_word_but_not_completed_words() {
        let mut queue = queue();
        queue.advance(45 * 4); // first word and five samples of the second consumed
        assert_eq!(ids(&queue), [2.0, 3.0]);
        assert_eq!(queue.remaining().next().unwrap().samples.len(), 20);
        // Requeued second word is interrupted again, then completes exactly.
        queue.advance(49 * 4);
        assert_eq!(ids(&queue), [2.0, 3.0]);
        queue.advance(30 * 4);
        assert_eq!(ids(&queue), [3.0]);
        queue.advance(0);
        assert!(ids(&queue).is_empty());
    }

    #[test]
    fn progress_before_interrupt_and_newly_synthesized_words_are_accounted_once() {
        let mut queue = queue();
        queue.advance(50 * 4);
        queue.advance(45 * 4);
        queue.push(Speech {
            samples: vec![4.0; 40],
            sample_rate: 24000,
        });
        queue.advance(70 * 4);
        assert_eq!(ids(&queue), [3.0, 4.0]);
    }
}
