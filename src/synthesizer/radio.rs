use rand::{
    SeedableRng,
    distr::{Distribution, Uniform},
    rngs::{ThreadRng, Xoshiro128PlusPlus},
};
use rayon::iter::{IntoParallelRefIterator, ParallelIterator};
use std::f32::consts::{FRAC_1_SQRT_2, TAU};
use wide::f32x4;

#[cfg(target_arch = "x86")]
use std::arch::x86::*;
#[cfg(target_arch = "x86_64")]
use std::arch::x86_64::*;

#[derive(Debug, Clone)]
pub struct Tone {
    frequency: f32,
    seconds: f32,
}

#[derive(Debug, Clone)]
pub struct Radio {
    pub voice_low_cut: f32,
    pub voice_high_cut: f32,
    pub honk_frequency: f32,
    pub honk_gain_db: f32,
    // How hard its amplifier is driven at full strength (it clips well before that).
    pub max_drive: f32,
    // The digital link: the sample rate and bits it carries the voice in, which add a gritty edge.
    pub link_rate: f32,
    pub link_bits: f32,
    // The voice wobbles a little in level, as a real link does: how much and how fast (Hz).
    pub flutter_depth: f32,
    pub flutter_rate: f32,
    // The band the hiss is heard in.
    pub hiss_low_cut: f32,
    pub hiss_high_cut: f32,
    // Hiss under the voice, and the squelch bursts when the transmission opens and closes (seconds).
    pub hiss_level: f32,
    pub squelch_level: f32,
    pub squelch_open: f32,
    pub squelch_close: f32,
    pub beep_tones: [Tone; 2],
    pub beep_level: f32,
    pub beep_fade: f32,
}

impl Radio {
    pub fn apply(&self, src: &[f32], sample_rate: f32, strength: f32, squelch: bool) -> Vec<f32> {
        let s = strength.clamp(0.0, 1.0);
        if s <= 0.0 || src.is_empty() || sample_rate <= 0.0 {
            return src.to_vec();
        }

        let open = self.src_offset(sample_rate, strength, squelch);
        let close = match squelch {
            true => (self.squelch_close * sample_rate).ceil() as usize,
            false => 0,
        };

        let dry = std::iter::repeat_n(0.0, open)
            .chain(src.iter().copied())
            .chain(std::iter::repeat_n(0.0, close))
            .collect::<Vec<_>>();

        let mut dry_rms = None;
        let mut dry_rms = || *dry_rms.get_or_insert_with(|| rms(&dry));

        let mut voice_band = Band::new(self.voice_low_cut, self.voice_high_cut, sample_rate);
        let mut honk = Biquad::peak(self.honk_frequency, self.honk_gain_db, 1.2, sample_rate);
        let mut receiver = Band::new(
            0.8 * self.voice_low_cut,
            1.2 * self.voice_high_cut,
            sample_rate,
        );

        let drive = 1.0 + (self.max_drive - 1.0) * s;
        let hold = 1.0f32.max(sample_rate / self.link_rate);
        let steps = 2.0f32.powf(self.link_bits - 1.0);

        let mut held = 0.0;
        let mut since_hold = hold;

        let wet = dry
            .iter()
            .enumerate()
            .map(|(i, &dry)| {
                let shaped = honk.process(voice_band.process(dry));
                let clipped = (drive * shaped).tanh() / drive.tanh();
                since_hold += 1.0;
                if since_hold >= hold {
                    since_hold -= hold;
                    held = (clipped * steps).round() / steps;
                }

                let i = i as f32;
                let flutter = 1.0
                    - self.flutter_depth
                        * (0.5 + 0.5 * f32::sin(TAU * self.flutter_rate * i / sample_rate));

                return receiver.process(held) * flutter;
            })
            .collect::<Vec<_>>();

        let wet_rms = rms(&wet);
        let wet_gain = if wet_rms > 0.0 {
            dry_rms() / wet_rms
        } else {
            1.0
        };

        let voice = wet
            .iter()
            .zip(dry.iter())
            .map(|(wet, &dry)| (1.0 - s) * dry + s * wet * wet_gain)
            .collect::<Vec<_>>();

        let voice_rms = rms(&voice);
        let gain = if voice_rms > 0.0 {
            dry_rms() / voice_rms
        } else {
            1.0
        };

        let mut noise_band = Band::new(self.hiss_low_cut, self.hiss_high_cut, sample_rate);
        let mut rng = Xoshiro128PlusPlus::from_rng(&mut ThreadRng::default());
        let uniform = Uniform::new_inclusive(-1.0, 1.0).unwrap();

        let mut dest = Vec::with_capacity(dry.len());
        for (i, &voice) in voice.iter().enumerate() {
            let mut level = self.hiss_level;
            if i < open {
                level = self.squelch_level
            }
            if i >= (dry.len() - close) {
                let left = (dry.len() - i) as f32 / (close as f32);
                level = self.squelch_level * left;
            }
            let noise = noise_band.process(uniform.sample(&mut rng)) * level * 3.0;
            dest.push(voice * gain + s * noise);
        }

        return dest;
    }

    /// Where [`Self::apply`] puts the first sample of `src` in its output: after the squelch
    /// opening, if there is one.
    pub fn src_offset(&self, sample_rate: f32, strength: f32, squelch: bool) -> usize {
        if !squelch || strength.clamp(0.0, 1.0) <= 0.0 || sample_rate <= 0.0 {
            return 0;
        }
        return (self.squelch_open * sample_rate).ceil() as usize;
    }

    pub fn beep(&self, sample_rate: f32) -> Vec<f32> {
        struct Tone {
            frequency: f32,
            seconds: f32,
        }

        let mut dest = Vec::new();
        for tone in &self.beep_tones {
            let count = (tone.seconds * sample_rate).ceil() as usize;
            let edge = (self.beep_fade * sample_rate).ceil();

            dest.reserve(count);
            for i in 0..count {
                let j = i as f32;
                let envelope = 1.0f32.min(j / edge).min((count - i) as f32 / edge);
                dest.push(
                    self.beep_level * envelope * f32::sin(TAU * tone.frequency * j / sample_rate),
                );
            }
        }

        return dest;
    }
}

impl Default for Radio {
    fn default() -> Self {
        Self {
            voice_low_cut: 500.0,
            voice_high_cut: 2700.0,
            honk_frequency: 1700.0,
            honk_gain_db: 7.0,
            max_drive: 14.0,
            link_rate: 7000.0,
            link_bits: 6.0,
            flutter_depth: 0.12,
            flutter_rate: 7.0,
            hiss_low_cut: 350.0,
            hiss_high_cut: 3200.0,
            hiss_level: 0.015,
            squelch_level: 0.12,
            squelch_open: 0.07,
            squelch_close: 0.12,
            beep_tones: [
                Tone {
                    frequency: 1050.0,
                    seconds: 0.06,
                },
                Tone {
                    frequency: 1040.0,
                    seconds: 0.07,
                },
            ],
            beep_level: 0.35,
            beep_fade: 0.005,
        }
    }
}

struct Biquad {
    b0: f32,
    b12a12: f32x4,
    x12y12: f32x4,
}

impl Biquad {
    pub fn high_pass(cutoff: f32, sample_rate: f32) -> Self {
        Self::make(cutoff, sample_rate, true)
    }

    pub fn low_pass(cutoff: f32, sample_rate: f32) -> Self {
        Self::make(cutoff, sample_rate, false)
    }

    pub fn peak(freq: f32, gain_db: f32, q: f32, sample_rate: f32) -> Self {
        let a = 10.0f32.powf(gain_db / 40.0);
        let w = TAU * freq.min(0.45 * sample_rate) / sample_rate;
        let (s, c) = w.sin_cos();
        let alpha = s / (2.0 * q);
        let a0 = 1.0 + alpha / a;
        let b0 = (1.0 + alpha * a) / a0;
        let b1 = -2.0 * c / a0;
        let b2 = (1.0 - alpha * a) / a0;
        let a1 = -b1;
        let a2 = -(1.0 - alpha / a) / a0;

        return Self {
            b0,
            b12a12: f32x4::new([b1, b2, a1, a2]),
            x12y12: f32x4::splat(0.0),
        };
    }

    fn make(cutoff: f32, sample_rate: f32, high: bool) -> Self {
        let w = TAU * cutoff.min(0.45 * sample_rate) / sample_rate;
        let (s, cos_w) = w.sin_cos();
        let alpha = s / (2.0 * FRAC_1_SQRT_2);
        let a0 = 1.0 + alpha;

        let (b0, b1) = match high {
            true => ((1.0 + cos_w) / 2.0 / a0, -(1.0 + cos_w) / a0),
            false => ((1.0 - cos_w) / 2.0 / a0, (1.0 - cos_w) / a0),
        };

        let b2 = b0;
        let a1 = 2.0 * cos_w / a0;
        let a2 = (alpha - 1.0) / a0;

        return Self {
            b0,
            b12a12: f32x4::new([b1, b2, a1, a2]),
            x12y12: f32x4::splat(0.0),
        };
    }

    pub fn process(&mut self, x: f32) -> f32 {
        let y = self
            .b0
            .mul_add(x, unsafe { reduce_add(self.b12a12 * self.x12y12) });
        let [x1, _, y1, _] = *self.x12y12.as_array();
        self.x12y12 = f32x4::new([x, x1, y, y1]);
        return y;
    }
}

struct Band {
    stages: [Biquad; 4],
}

impl Band {
    pub fn new(low_cut: f32, high_cut: f32, sample_rate: f32) -> Self {
        Self {
            stages: [
                Biquad::high_pass(low_cut, sample_rate),
                Biquad::high_pass(low_cut, sample_rate),
                Biquad::low_pass(high_cut, sample_rate),
                Biquad::low_pass(high_cut, sample_rate),
            ],
        }
    }

    #[inline]
    pub fn process(&mut self, mut x: f32) -> f32 {
        x = self.stages[0].process(x);
        x = self.stages[1].process(x);
        x = self.stages[2].process(x);
        x = self.stages[3].process(x);
        return x;
    }
}

#[cfg_attr(
    any(target_arch = "x86", target_arch = "x86_64"),
    target_feature(enable = "sse")
)]
#[inline]
fn reduce_add(x: f32x4) -> f32 {
    cfg_select! {
        target_feature = "sse" => {
            #[allow(non_snake_case)]
            const fn MM_SHUFFLE(z: u32, y: u32, x: u32, w: u32) -> i32 {
                ((z << 6) | (y << 4) | (x << 2) | w) as i32
            }

            let x = __m128::from(x);
            let shuf = cfg_select! {
                target_feature = "sse3" => unsafe { _mm_movehdup_ps(x) },
                _ => _mm_shuffle_ps::<{MM_SHUFFLE(2, 3, 0, 1)}>(x, x),
            };
            let sums = _mm_add_ps(x, shuf);
            let shuf = _mm_movehl_ps(shuf, sums);
            let sums = _mm_add_ss(sums, shuf);
            return _mm_cvtss_f32(sums);
        },
        _ => x.reduce_add(),
    }
}

#[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
#[target_feature(enable = "avx")]
#[inline]
fn reduce_add_8(x: __m256) -> f32 {
    let vlow = _mm256_castps256_ps128(x);
    let vhigh = _mm256_extractf128_ps(x, 1);
    let vlow = _mm_add_ps(vlow, vhigh);
    return reduce_add(f32x4::from(vlow));
}

#[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
#[target_feature(enable = "avx512f")]
#[inline]
fn reduce_add_16(x: __m512) -> f32 {
    let vlow = _mm512_castps512_ps256(x);
    let vhigh = cfg_select! {
        target_feature = "avx512dq" => _mm512_extractf32x8_ps(x, 1),
        _ => _mm256_castpd_ps(_mm512_extractf64x4_pd::<1>(_mm512_castps_pd(x))),
    };
    let vlow = _mm256_add_ps(vlow, vhigh);
    return reduce_add_8(vlow);
}

fn rms(samples: &[f32]) -> f32 {
    if samples.is_empty() {
        return 0.0;
    }

    #[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
    let sum = if is_x86_feature_detected!("avx512f") {
        unsafe { sum_squares_16(samples) }
    } else if is_x86_feature_detected!("fma") {
        unsafe { sum_squares_8_fma(samples) }
    } else if is_x86_feature_detected!("avx") {
        unsafe { sum_squares_8(samples) }
    } else {
        sum_squares_4(samples)
    };

    #[cfg(not(any(target_arch = "x86", target_arch = "x86_64")))]
    let sum = sum_squares_4(samples);

    return f32::sqrt(sum / samples.len() as f32);
}

/// Sums the squares of the samples that don't fill a whole vector.
#[inline]
fn sum_squares_scalar(pre: &[f32], post: &[f32]) -> f32 {
    pre.iter().chain(post).map(|&s| s * s).sum()
}

// The vector math is written directly inside these `#[target_feature]` functions because
// closures inherit their target features, so the intrinsics in rayon's `fold`/`reduce` get
// inlined. Through a generic helper or `Add`/`Mul` impls they'd be compiled without the
// features, turning every vector operation into a function call.

#[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
#[target_feature(enable = "avx512f")]
fn sum_squares_16(samples: &[f32]) -> f32 {
    let (pre, samples, post) = unsafe { samples.align_to::<__m512>() };
    let zero = || _mm512_setzero_ps();
    let sum = samples
        .par_iter()
        .fold(zero, |acc, &x| _mm512_fmadd_ps(x, x, acc))
        .reduce(zero, |lhs, rhs| _mm512_add_ps(lhs, rhs));
    return sum_squares_scalar(pre, post) + reduce_add_16(sum);
}

#[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
#[target_feature(enable = "avx")]
fn sum_squares_8(samples: &[f32]) -> f32 {
    let (pre, samples, post) = unsafe { samples.align_to::<__m256>() };
    let zero = || _mm256_setzero_ps();
    let sum = samples
        .par_iter()
        .fold(zero, |acc, &x| _mm256_add_ps(acc, _mm256_mul_ps(x, x)))
        .reduce(zero, |lhs, rhs| _mm256_add_ps(lhs, rhs));
    return sum_squares_scalar(pre, post) + reduce_add_8(sum);
}

#[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
#[target_feature(enable = "fma")]
fn sum_squares_8_fma(samples: &[f32]) -> f32 {
    let (pre, samples, post) = unsafe { samples.align_to::<__m256>() };
    let zero = || _mm256_setzero_ps();
    let sum = samples
        .par_iter()
        .fold(zero, |acc, &x| _mm256_fmadd_ps(x, x, acc))
        .reduce(zero, |lhs, rhs| _mm256_add_ps(lhs, rhs));
    return sum_squares_scalar(pre, post) + reduce_add_8(sum);
}

fn sum_squares_4(samples: &[f32]) -> f32 {
    let (pre, samples, post) = unsafe { samples.align_to::<f32x4>() };
    let zero = || f32x4::splat(0.0);
    let sum = samples
        .par_iter()
        .fold(zero, |acc, &x| x.mul_add(x, acc))
        .reduce(zero, |lhs, rhs| lhs + rhs);
    return sum_squares_scalar(pre, post) + unsafe { reduce_add(sum) };
}
