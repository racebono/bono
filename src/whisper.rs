use crate::{
    arrstr,
    config::{ArrayStr, Config, RawConfig},
    task::{JoinHandle, spawn, spawn_blocking},
};
use anyhow::Context;
use camino::Utf8PathBuf;
use sdl3::{
    audio::{
        AudioDevice, AudioFormat, AudioRecordingCallback, AudioSpec, AudioStream,
        AudioStreamWithCallback,
    },
    get_error,
    sys::audio::SDL_GetAudioStreamData,
};
use std::{
    collections::btree_map,
    ffi::c_int,
    mem::MaybeUninit,
    num::{NonZero, NonZeroUsize},
    path::Path,
    time::Duration,
};
use whisper_rs::{
    FullParams, SamplingStrategy, WhisperContext, WhisperContextParameters, WhisperError,
    WhisperState, get_lang_id,
};

pub const RECORDING_SPEC: AudioSpec = AudioSpec {
    freq: Some(16000),
    channels: Some(1),
    format: Some(AudioFormat::f32_sys()),
};

pub struct Whisper {
    ctx: WhisperContext,
    threads: NonZeroUsize,
}

impl Whisper {
    pub async fn new(
        path: impl AsRef<Path> + Send + 'static,
        threads: Option<NonZeroUsize>,
    ) -> anyhow::Result<Self> {
        let threads = threads
            .or_else(|| std::thread::available_parallelism().ok())
            .unwrap_or(NonZeroUsize::MIN);

        let ctx = spawn_blocking(move || {
            WhisperContext::new_with_params(path, WhisperContextParameters::default())
        })
        .await?;

        return Ok(Self { ctx, threads });
    }

    pub fn begin(&self, config: &RawConfig) {
        todo!()
    }
}

pub struct WhisperStream {
    recv: oneshot::Receiver<anyhow::Result<Vec<f32>>>,
    session: WhisperSession,
    stream: AudioStreamWithCallback<Recorder>,
}

impl WhisperStream {
    pub fn finish(
        self,
        config: Config,
    ) -> JoinHandle<anyhow::Result<Option<(String, ArrayStr<2>)>>> {
        return self.session.finish(self.recv, self.stream, config);
    }
}

pub struct WhisperSession {
    state: WhisperState,
    threads: NonZeroUsize,
    multilingual: bool,
}

impl WhisperSession {
    fn finish(
        mut self,
        recv: oneshot::Receiver<anyhow::Result<Vec<f32>>>,
        stream: AudioStreamWithCallback<Recorder>,
        config: Config,
    ) -> JoinHandle<anyhow::Result<Option<(String, ArrayStr<2>)>>> {
        if let Err(e) = stream.pause() {
            return spawn(std::future::ready(Err(e.into())));
        }

        if let Err(e) = stream.flush() {
            return spawn(std::future::ready(Err(e.into())));
        }

        return spawn_blocking(move || {
            let samples = recv.recv()??;
            let config = config.get();

            self.state.pcm_to_mel(&samples, self.threads.get())?;
            drop(samples);

            if let Some(lang) = self.detect_language(config.voices.keys(), config.min_confidence)? {
                drop(config);
                return Ok(Some((self.speech_to_text(lang)?, lang)));
            } else {
                return Ok(None);
            }
        });
    }

    fn detect_language(
        &mut self,
        mut candidates: btree_map::Keys<'_, ArrayStr<2>, Utf8PathBuf>,
        min_confidence: f32,
    ) -> anyhow::Result<Option<ArrayStr<2>>> {
        if candidates.len() == 1 {
            return Ok(Some(
                *candidates.next().context("Expected a language candidate")?,
            ));
        }

        if !self.multilingual {
            if !candidates.any(|k| k == &arrstr!("en")) {
                return Ok(None);
            }
            return Ok(Some(arrstr!("en")));
        }

        let (_, probs) = self.state.lang_detect(0, self.threads.get())?;
        let mut chosen = None::<(ArrayStr<2>, f32)>;
        for candidate in candidates {
            let Some(id) = get_lang_id(candidate) else {
                log::warn!("Unsupported language ({candidate})");
                continue;
            };

            let Some(prob) = usize::try_from(id).ok().and_then(|i| probs.get(i)).copied() else {
                continue;
            };

            if prob < min_confidence {
                continue;
            }

            if chosen
                .as_ref()
                .is_none_or(|(_, cand_prob)| prob > *cand_prob)
            {
                chosen = Some((*candidate, prob));
            }
        }

        return Ok(chosen.map(|(name, _)| name));
    }

    fn speech_to_text(&mut self, lang: ArrayStr<2>) -> anyhow::Result<String> {
        let mut params = FullParams::new(SamplingStrategy::BeamSearch {
            beam_size: 5,
            patience: -1.0,
        });

        params.set_language(Some(&lang));
        params.set_print_special(false);
        params.set_print_progress(false);
        params.set_print_realtime(false);
        params.set_print_timestamps(false);

        self.state.full(params, &[])?;
        let text = self
            .state
            .as_iter()
            .map(|x| x.to_str())
            .collect::<Result<String, WhisperError>>()?;

        return Ok(trim_in_place(text));
    }
}

struct Segment {
    start: Duration,
    end: Duration,
    text: String,
}

fn trim_in_place(s: String) -> String {
    let trimmed = s.trim();
    if trimmed.is_empty() {
        return String::new();
    }

    let offset = unsafe { trimmed.as_ptr().offset_from_unsigned(s.as_ptr()) };
    let len = trimmed.len();

    let mut bytes = s.into_bytes();
    drop(bytes.drain(..offset));
    unsafe { bytes.set_len(len) };
    return unsafe { String::from_utf8_unchecked(bytes) };
}

struct Recorder {
    send: Option<oneshot::Sender<anyhow::Result<Vec<f32>>>>,
    samples: anyhow::Result<Vec<f32>>,
    remaining_buf: [u8; 3],
    remaining_count: usize,
}

impl Recorder {
    fn try_callback(
        stream: &mut AudioStream,
        available: i32,
        samples: &mut Vec<f32>,
        remaining_buf: &mut [u8; 3],
        remaining_count: &mut usize,
    ) -> anyhow::Result<()> {
        let Some(available) = NonZero::new(available.max(0) as usize) else {
            todo!()
        };

        unsafe {
            samples.try_reserve(
                available
                    .get()
                    .div_ceil(4)
                    .unchecked_add((*remaining_count > 0) as usize),
            )?;

            let spare_bytes = std::slice::from_raw_parts_mut(
                samples
                    .spare_capacity_mut()
                    .as_mut_ptr()
                    .cast::<MaybeUninit<u8>>(),
                available.get().unchecked_add(*remaining_count),
            );

            let (pre, post) = spare_bytes.split_at_mut_unchecked(*remaining_count);
            pre.write_copy_of_slice(remaining_buf.get_unchecked(..*remaining_count));

            match usize::try_from(SDL_GetAudioStreamData(
                stream.raw(),
                post.as_mut_ptr().cast(),
                post.len() as c_int,
            )) {
                Ok(count) => {
                    let count = remaining_count.unchecked_add(count);
                    let (full, partial) = spare_bytes
                        .get_unchecked(..count)
                        .assume_init_ref()
                        .as_chunks::<4>();

                    let len = samples.len();
                    samples.set_len(len.unchecked_add(full.len()));

                    std::ptr::copy_nonoverlapping(
                        partial.as_ptr(),
                        remaining_buf.as_mut_ptr(),
                        partial.len(),
                    );
                    *remaining_count = partial.len();
                }
                Err(_) => return Err(get_error().into()),
            }

            return Ok(());
        }
    }
}

impl AudioRecordingCallback<f32> for Recorder {
    fn callback(&mut self, stream: &mut AudioStream, available: i32) {
        if self.send.as_ref().is_none_or(oneshot::Sender::is_closed) {
            self.send = None;
            self.samples = Ok(Vec::new());
            return;
        }

        let Ok(ref mut samples) = self.samples else {
            return;
        };

        if let Err(e) = Self::try_callback(
            stream,
            available,
            samples,
            &mut self.remaining_buf,
            &mut self.remaining_count,
        ) {
            self.samples = Err(e);
            self.send = None;
        }
    }
}

impl Drop for Recorder {
    fn drop(&mut self) {
        let Some(send) = self.send.take() else { return };

        if self.remaining_count != 0 {
            log::warn!("Audio recording returned unexpected trailing bytes");
        }

        let _ = send.send(std::mem::replace(&mut self.samples, Ok(Vec::new())));
    }
}
