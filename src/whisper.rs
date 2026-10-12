use crate::{config::ArrayStr, task::spawn_blocking};
use sdl3::{
    audio::{AudioDevice, AudioFormat, AudioSpec, AudioStream, AudioStreamOwner},
    get_error,
    sys::audio::SDL_GetAudioStreamData,
};
use std::{ffi::c_int, mem::MaybeUninit, num::NonZero, path::Path, time::Duration};
use whisper_rs::{FullParams, SamplingStrategy, WhisperContext, WhisperContextParameters};

const SPEC: AudioSpec = AudioSpec {
    freq: Some(16000),
    channels: Some(1),
    format: Some(AudioFormat::f32_sys()),
};

pub struct Whisper {
    ctx: WhisperContext,
    device: AudioDevice,
}

impl Whisper {
    pub async fn new(path: impl AsRef<Path> + Send + 'static) -> anyhow::Result<Self> {
        let ctx = spawn_blocking(move || {
            WhisperContext::new_with_params(path, WhisperContextParameters::default())
        })
        .await?;

        todo!()
    }

    // FIXME: Should we track progress?
    fn speech_to_text_blocking(
        &self,
        mut stream: AudioStreamOwner,
        lang: Option<ArrayStr<2>>,
    ) -> anyhow::Result<Vec<Segment>> {
        let mut params = FullParams::new(SamplingStrategy::BeamSearch {
            beam_size: 5,
            patience: -1.0,
        });

        params.set_language(lang.as_deref());
        params.set_print_special(false);
        params.set_print_progress(false);
        params.set_print_realtime(false);
        params.set_print_timestamps(false);

        let samples = read_samples(&mut stream)?;
        let mut state = self.ctx.create_state()?;
        state.full(params, &samples)?;

        return state.as_iter().map(|seg| Segment {
            start: seg.start,
            end: todo!(),
            text: seg.to_string(),
        });
    }
}

struct Segment {
    start: Duration,
    end: Duration,
    text: String,
}

fn read_samples(stream: &mut AudioStream) -> anyhow::Result<Vec<f32>> {
    let mut samples = Vec::<f32>::new();
    let mut remaining_buf = [0; 3];
    let mut remaining_count = 0;

    while let Some(available) = NonZero::new(usize::try_from(stream.available_bytes()?)?) {
        unsafe {
            samples.try_reserve(
                available
                    .get()
                    .div_ceil(4)
                    .unchecked_add((remaining_count > 0) as usize),
            )?;

            let spare_bytes = std::slice::from_raw_parts_mut(
                samples
                    .spare_capacity_mut()
                    .as_mut_ptr()
                    .cast::<MaybeUninit<u8>>(),
                available.get().unchecked_add(remaining_count),
            );

            let (pre, post) = spare_bytes.split_at_mut_unchecked(remaining_count);
            pre.write_copy_of_slice(remaining_buf.get_unchecked(..remaining_count));

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
                    remaining_count = partial.len();
                }
                Err(_) => return Err(get_error().into()),
            }
        }
    }

    if remaining_count != 0 {
        log::warn!("Audio recording returned unexpected extra trailing bytes");
    }

    return Ok(samples);
}
