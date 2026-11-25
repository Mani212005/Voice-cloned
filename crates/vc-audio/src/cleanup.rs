//! Reference cleanup at enrollment: a high-pass for rumble, then RNNoise (Xiph's
//! recurrent denoiser, via the pure-Rust `nnnoiseless` port with its weights compiled in)
//! with a cap on how far it may turn anything down, so the voice is never carved thin,
//! then a high shelf that gives a muffled recording back the top end a clean close-mic
//! recording has (a recording that already has it is left alone). The speech level is
//! restored afterwards, so the engine hears the same loudness.
//!
//! The engine copies the reference's sound: its noise, and its muffled or bright tone.
//! Denoising before brightening matters, because brightening a noisy reference makes the
//! clone hiss.

use crate::MonoPcm;
use crate::dsp::{Biquad, integrated_loudness};

/// Settings of [`clean_reference`]. [`CleanupParams::default`] is the shipped setting.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CleanupParams {
    /// High-pass corner (Hz) applied before denoising.
    pub highpass_hz: f64,
    /// The most the denoiser may lower any part of the signal, in dB: the output is
    /// `denoised + 10^(-cap/20) * (input - denoised)`, so noise drops by at most this much
    /// and the denoiser's mistakes on the voice are softened by the same mix.
    pub max_reduction_db: f64,
    /// Brightening: a high shelf at `shelf_hz`, with the gain (at most `max_shelf_db`)
    /// that brings the 4-8 kHz band up to `target_tilt_db` relative to the 0.5-2 kHz band
    /// (see [`hf_tilt_db`]). It never cuts, so a bright reference is left as it is.
    pub shelf_hz: f64,
    pub target_tilt_db: f64,
    pub max_shelf_db: f64,
}

impl Default for CleanupParams {
    fn default() -> Self {
        Self {
            highpass_hz: 70.0,
            max_reduction_db: 18.0,
            shelf_hz: 3500.0,
            target_tilt_db: TARGET_TILT_DB,
            max_shelf_db: 6.0,
        }
    }
}

/// The 4-8 kHz tilt to restore: clean close-microphone recordings of the owner's voice
/// measure -17.6 dB at the median (-18.6 to -15.2 for the middle half, see
/// `docs/clean-voice.md`). A reference below that sounds muffled, and so does its clone.
const TARGET_TILT_DB: f64 = -17.0;

/// Level of the 4-8 kHz band relative to the 0.5-2 kHz band (dB), measured with
/// fourth-order Butterworth band filters over the whole signal. Speech dominates the
/// energy, so pauses barely count. `None` when the rate cannot carry 4-8 kHz or the
/// signal is silent.
pub fn hf_tilt_db(x: &[f64], rate: u32) -> Option<f64> {
    let r = f64::from(rate);
    if r / 2.0 < 6000.0 {
        return None;
    }
    let q = std::f64::consts::FRAC_1_SQRT_2;
    let band = |lo: f64, hi: f64| -> f64 {
        let mut y = x.to_vec();
        for _ in 0..2 {
            Biquad::highpass(r, lo, q).run(&mut y);
            if hi < r / 2.0 * 0.9 {
                Biquad::lowpass(r, hi, q).run(&mut y);
            }
        }
        y.iter().map(|v| v * v).sum::<f64>()
    };
    let (high, mid) = (band(4000.0, 8000.0), band(500.0, 2000.0));
    (mid > 0.0 && high > 0.0).then(|| 10.0 * (high / mid).log10())
}

/// RNNoise runs at 48 kHz on 10 ms frames.
const DENOISE_RATE: u32 = 48_000;

/// Clean a reference recording. The result has the input's rate and length.
pub fn clean_reference(pcm: &MonoPcm, p: &CleanupParams) -> MonoPcm {
    if pcm.samples.is_empty() || pcm.sample_rate == 0 {
        return pcm.clone();
    }
    let rate = pcm.sample_rate;
    let mut x: Vec<f64> = pcm
        .samples
        .iter()
        .map(|&s| f64::from(s) / 32768.0)
        .collect();
    Biquad::highpass(
        f64::from(rate),
        p.highpass_hz,
        std::f64::consts::FRAC_1_SQRT_2,
    )
    .run(&mut x);
    let before = integrated_loudness(&x, rate);

    let x48 = resample(&x, rate, DENOISE_RATE);
    let d48 = rnnoise(&x48);
    let keep = 10f64.powf(-p.max_reduction_db.max(0.0) / 20.0);
    let mixed: Vec<f64> = d48
        .iter()
        .zip(&x48)
        .map(|(&d, &n)| d + keep * (n - d))
        .collect();
    let mut y = resample(&mixed, DENOISE_RATE, rate);
    y.resize(x.len(), 0.0);

    let boost = shelf_gain_db(&y, rate, p);
    if boost > 0.0 {
        shelf(&mut y, rate, p.shelf_hz, boost);
    }

    // Restore the speech level the recording had (gated loudness ignores the pauses
    // that the denoiser made quieter).
    if let (Some(b), Some(a)) = (before, integrated_loudness(&y, rate)) {
        let g = 10f64.powf(((b - a) / 20.0).clamp(-0.5, 0.5));
        y.iter_mut().for_each(|v| *v *= g);
    }
    // The shelf can lift peaks; never let the cleaned reference clip.
    let peak = y.iter().fold(0.0f64, |m, v| m.max(v.abs()));
    let ceiling = 10f64.powf(-1.0 / 20.0);
    if peak > ceiling {
        y.iter_mut().for_each(|v| *v *= ceiling / peak);
    }
    MonoPcm {
        sample_rate: rate,
        samples: y
            .iter()
            .map(|&v| (v * 32768.0).round().clamp(-32768.0, 32767.0) as i16)
            .collect(),
    }
}

fn shelf(x: &mut [f64], rate: u32, freq: f64, db: f64) {
    Biquad::high_shelf(f64::from(rate), freq, std::f64::consts::FRAC_1_SQRT_2, db).run(x);
}

/// The shelf gain in `0..=max_shelf_db` whose result has a 4-8 kHz tilt closest to the
/// target: 0 when the signal is already as bright, the maximum when even that falls
/// short, otherwise found by bisection (tilt rises monotonically with the gain).
fn shelf_gain_db(x: &[f64], rate: u32, p: &CleanupParams) -> f64 {
    let max = p.max_shelf_db.max(0.0);
    let tilt_with = |db: f64| {
        let mut y = x.to_vec();
        shelf(&mut y, rate, p.shelf_hz, db);
        hf_tilt_db(&y, rate)
    };
    let Some(t0) = hf_tilt_db(x, rate) else {
        return 0.0;
    };
    if t0 >= p.target_tilt_db || max == 0.0 {
        return 0.0;
    }
    if tilt_with(max).is_none_or(|t| t <= p.target_tilt_db) {
        return max;
    }
    let (mut lo, mut hi) = (0.0, max);
    for _ in 0..12 {
        let mid = (lo + hi) / 2.0;
        if tilt_with(mid).is_some_and(|t| t < p.target_tilt_db) {
            lo = mid;
        } else {
            hi = mid;
        }
    }
    (lo + hi) / 2.0
}

/// RNNoise over a 48 kHz signal, time-aligned with its input: the denoiser answers each
/// 10 ms frame one frame late, so one extra frame is fed and the first answer dropped.
fn rnnoise(x: &[f64]) -> Vec<f64> {
    let n = nnnoiseless::DenoiseState::FRAME_SIZE;
    let mut st = nnnoiseless::DenoiseState::new();
    let frames = x.len().div_ceil(n) + 1;
    let mut out = Vec::with_capacity(frames * n);
    let mut inp = vec![0.0f32; n];
    let mut buf = vec![0.0f32; n];
    for f in 0..frames {
        for (i, slot) in inp.iter_mut().enumerate() {
            *slot = x.get(f * n + i).map_or(0.0, |&v| (v * 32768.0) as f32);
        }
        st.process_frame(&mut buf, &inp);
        if f > 0 {
            out.extend(buf.iter().map(|&v| f64::from(v) / 32768.0));
        }
    }
    out.truncate(x.len());
    out
}

/// Band-limited resampling by windowed-sinc interpolation (Blackman window, 24 zero
/// crossings each side at the lower of the two rates' Nyquist, cut at 95 % of it). The
/// kernel is tabulated at 1/512 of an input sample and interpolated linearly.
pub fn resample(x: &[f64], from: u32, to: u32) -> Vec<f64> {
    if from == to || x.is_empty() || from == 0 || to == 0 {
        return x.to_vec();
    }
    const STEPS: f64 = 512.0;
    let ratio = f64::from(to) / f64::from(from);
    // Cutoff in cycles per input sample.
    let fc = 0.5 * ratio.min(1.0) * 0.95;
    let half = 24.0 / (2.0 * fc);
    let pi = std::f64::consts::PI;
    let kernel = |d: f64| -> f64 {
        let s = if d.abs() < 1e-12 {
            2.0 * fc
        } else {
            (2.0 * pi * fc * d).sin() / (pi * d)
        };
        let w = 0.5 + d / (2.0 * half);
        s * (0.42 - 0.5 * (2.0 * pi * w).cos() + 0.08 * (4.0 * pi * w).cos())
    };
    // Even kernel: tabulate |d| in [0, half], one guard entry past the end.
    let n_tab = (half * STEPS).ceil() as usize + 2;
    let table: Vec<f64> = (0..n_tab)
        .map(|i| {
            let d = i as f64 / STEPS;
            if d <= half { kernel(d) } else { 0.0 }
        })
        .collect();
    let out_len = (x.len() as f64 * ratio).round() as usize;
    (0..out_len)
        .map(|j| {
            let t = j as f64 / ratio;
            let lo = (t - half).ceil().max(0.0) as usize;
            let hi = ((t + half).floor().max(0.0) as usize).min(x.len() - 1);
            let mut acc = 0.0;
            for (k, &v) in x.iter().enumerate().take(hi + 1).skip(lo) {
                let pos = (t - k as f64).abs() * STEPS;
                let i = pos as usize;
                if i + 1 >= n_tab {
                    continue;
                }
                let fr = pos - i as f64;
                acc += v * (table[i] + fr * (table[i + 1] - table[i]));
            }
            acc
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sine(rate: u32, f: f64, secs: f64) -> Vec<f64> {
        (0..(f64::from(rate) * secs) as usize)
            .map(|i| 0.5 * (2.0 * std::f64::consts::PI * f * i as f64 / f64::from(rate)).sin())
            .collect()
    }

    fn rms(x: &[f64]) -> f64 {
        (x.iter().map(|v| v * v).sum::<f64>() / x.len() as f64).sqrt()
    }

    #[test]
    fn resampling_keeps_in_band_tones_and_removes_aliases() {
        for (from, to) in [
            (16_000, 48_000),
            (48_000, 16_000),
            (44_100, 48_000),
            (48_000, 24_000),
        ] {
            let x = sine(from, 1000.0, 0.5);
            let y = resample(&x, from, to);
            assert_eq!(
                y.len(),
                (x.len() as f64 * f64::from(to) / f64::from(from)).round() as usize
            );
            let want = sine(to, 1000.0, 0.5);
            let mid = y.len() / 4..y.len() * 3 / 4;
            let err: Vec<f64> = y[mid.clone()]
                .iter()
                .zip(&want[mid.clone()])
                .map(|(a, b)| a - b)
                .collect();
            let snr = 20.0 * (rms(&want[mid]) / rms(&err)).log10();
            assert!(snr > 60.0, "{from}->{to}: {snr} dB");
        }
        // A 20 kHz tone cannot exist at 16 kHz: it must be filtered, not folded to 4 kHz.
        let y = resample(&sine(48_000, 20_000.0, 0.5), 48_000, 16_000);
        assert!(20.0 * (rms(&y[2000..6000]) / 0.354).log10() < -50.0);
    }

    /// Deterministic noise (xorshift64*), uniform in [-1, 1).
    struct Noise(u64);
    impl Noise {
        fn next(&mut self) -> f64 {
            self.0 ^= self.0 >> 12;
            self.0 ^= self.0 << 25;
            self.0 ^= self.0 >> 27;
            let v = self.0.wrapping_mul(0x2545_f491_4f6c_dd1d) >> 11;
            v as f64 / (1u64 << 52) as f64 - 1.0
        }
    }

    /// A synthetic talker: a glottal pulse train (pitch gliding 105-135 Hz) through three
    /// formant resonators, in 300 ms "syllables" separated by 200 ms pauses. Returns the
    /// signal and a mask of the pause samples.
    fn talker(rate: u32, secs: f64) -> (Vec<f64>, Vec<bool>) {
        let r = f64::from(rate);
        let n = (r * secs) as usize;
        let mut src = vec![0.0; n];
        let mut phase = 0.0;
        for (i, v) in src.iter_mut().enumerate() {
            let t = i as f64 / r;
            let f0 = 120.0 + 15.0 * (2.0 * std::f64::consts::PI * 0.7 * t).sin();
            phase += f0 / r;
            if phase >= 1.0 {
                phase -= 1.0;
                *v = 1.0;
            }
        }
        let mut voiced = vec![0.0; n];
        for (f, q, g) in [(700.0, 5.0, 1.0), (1200.0, 6.0, 0.6), (2600.0, 8.0, 0.35)] {
            let mut y = src.clone();
            let mut bp = crate::dsp::Biquad::peaking(r, f, q, 30.0);
            y.iter_mut().for_each(|v| *v = bp.tick(*v));
            voiced.iter_mut().zip(&y).for_each(|(o, v)| *o += g * v);
        }
        let peak = voiced.iter().fold(0.0f64, |m, v| m.max(v.abs()));
        let mut pause = vec![false; n];
        let mut x = vec![0.0; n];
        for i in 0..n {
            let t = (i as f64 / r) % 0.5;
            if t < 0.3 {
                let env = (std::f64::consts::PI * t / 0.3).sin();
                x[i] = 0.3 * env * voiced[i] / peak;
            } else {
                pause[i] = t > 0.33 && t < 0.47;
            }
        }
        (x, pause)
    }

    fn pcm(rate: u32, x: &[f64]) -> MonoPcm {
        MonoPcm {
            sample_rate: rate,
            samples: x
                .iter()
                .map(|&v| (v * 32768.0).round().clamp(-32768.0, 32767.0) as i16)
                .collect(),
        }
    }

    fn f64s(p: &MonoPcm) -> Vec<f64> {
        p.samples.iter().map(|&s| f64::from(s) / 32768.0).collect()
    }

    /// Scale-invariant signal-to-distortion ratio of `y` against the clean `s` (dB).
    fn si_sdr(s: &[f64], y: &[f64]) -> f64 {
        let a =
            s.iter().zip(y).map(|(a, b)| a * b).sum::<f64>() / s.iter().map(|v| v * v).sum::<f64>();
        let target: f64 = s.iter().map(|v| (a * v).powi(2)).sum();
        let err: f64 = s.iter().zip(y).map(|(v, w)| (w - a * v).powi(2)).sum();
        10.0 * (target / err).log10()
    }

    /// The clean signal as the cleanup should leave it: through the same high-pass.
    fn highpassed(rate: u32, x: &[f64]) -> Vec<f64> {
        let mut y = x.to_vec();
        crate::dsp::Biquad::highpass(
            f64::from(rate),
            CleanupParams::default().highpass_hz,
            std::f64::consts::FRAC_1_SQRT_2,
        )
        .run(&mut y);
        y
    }

    fn masked_rms_db(x: &[f64], mask: &[bool]) -> f64 {
        let v: Vec<f64> = x
            .iter()
            .zip(mask)
            .filter(|(_, m)| **m)
            .map(|(v, _)| *v)
            .collect();
        20.0 * rms(&v).log10()
    }

    #[test]
    fn noisy_reference_gets_cleaner() {
        for rate in [16_000, 44_100, 48_000] {
            let (clean, pause) = talker(rate, 6.0);
            let mut noise = Noise(0x9e37_79b9_7f4a_7c15 ^ u64::from(rate));
            // White noise 20 dB below the speech's peak level, plus 30 Hz rumble.
            let noisy: Vec<f64> = clean
                .iter()
                .enumerate()
                .map(|(i, v)| {
                    let t = i as f64 / f64::from(rate);
                    v + 0.012 * noise.next() + 0.02 * (2.0 * std::f64::consts::PI * 30.0 * t).sin()
                })
                .collect();
            let out = clean_reference(&pcm(rate, &noisy), &CleanupParams::default());
            assert_eq!(out.sample_rate, rate);
            assert_eq!(out.samples.len(), noisy.len());
            let y = f64s(&out);
            let floor_in = masked_rms_db(&noisy, &pause);
            let floor_out = masked_rms_db(&y, &pause);
            let target = highpassed(rate, &clean);
            let sdr_in = si_sdr(&target, &noisy);
            let sdr_out = si_sdr(&target, &y);
            // Measured: the pause floor falls 9.5-20 dB and SI-SDR rises 6.5-7.2 dB.
            assert!(
                floor_in - floor_out > 8.0,
                "{rate} Hz: pause floor {floor_in:.1} -> {floor_out:.1} dB"
            );
            assert!(
                sdr_out - sdr_in > 5.0,
                "{rate} Hz: SI-SDR {sdr_in:.1} -> {sdr_out:.1} dB"
            );
        }
    }

    #[test]
    fn a_muffled_reference_gets_its_top_end_back_and_a_bright_one_does_not() {
        let rate = 16_000;
        let (clean, _) = talker(rate, 4.0);
        let tilt = |x: &[f64]| hf_tilt_db(x, rate).unwrap();
        let mut muffled = clean.clone();
        // A 12 dB/octave roll-off above 3 kHz, like the owner's laptop recording.
        crate::dsp::Biquad::lowpass(f64::from(rate), 3000.0, std::f64::consts::FRAC_1_SQRT_2)
            .run(&mut muffled);
        let p = CleanupParams::default();
        let before = tilt(&muffled);
        assert!(
            before < p.target_tilt_db - 2.0,
            "fixture is not muffled: {before:.1} dB"
        );
        let after = tilt(&f64s(&clean_reference(&pcm(rate, &muffled), &p)));
        // Within reach of the shelf, so it lands on the target.
        assert!(
            (after - p.target_tilt_db).abs() < 0.5,
            "{before:.1} -> {after:.1} dB"
        );
        // A bright reference is not brightened further.
        let mut noise = Noise(7);
        let bright: Vec<f64> = clean.iter().map(|v| v + 0.02 * noise.next()).collect();
        let b0 = tilt(&bright);
        assert!(b0 > p.target_tilt_db, "fixture is not bright: {b0:.1} dB");
        let b1 = tilt(&f64s(&clean_reference(&pcm(rate, &bright), &p)));
        assert!(b1 <= b0 + 0.1, "{b0:.1} -> {b1:.1} dB");
    }

    #[test]
    fn cleanup_never_clips_and_passes_silence_through() {
        let rate = 16_000;
        let (clean, _) = talker(rate, 3.0);
        let peak = clean.iter().fold(0.0f64, |m, v| m.max(v.abs()));
        let hot: Vec<f64> = clean.iter().map(|v| v / peak).collect();
        let out = clean_reference(&pcm(rate, &hot), &CleanupParams::default());
        let ceiling = (10f64.powf(-1.0 / 20.0) * 32768.0).ceil() as i16;
        assert!(
            out.samples
                .iter()
                .all(|&s| s.unsigned_abs() <= ceiling.unsigned_abs())
        );
        let silent = MonoPcm {
            sample_rate: rate,
            samples: vec![0; 32_000],
        };
        assert!(
            clean_reference(&silent, &CleanupParams::default())
                .samples
                .iter()
                .all(|&s| s == 0)
        );
    }

    #[test]
    fn clean_reference_is_left_alone() {
        let rate = 16_000;
        let (clean, _) = talker(rate, 6.0);
        let y = f64s(&clean_reference(
            &pcm(rate, &clean),
            &CleanupParams::default(),
        ));
        let speech: Vec<bool> = clean.iter().map(|v| v.abs() > 0.0).collect();
        let sdr = si_sdr(&highpassed(rate, &clean), &y);
        let level = masked_rms_db(&y, &speech) - masked_rms_db(&clean, &speech);
        // Measured: 15.5 dB and +0.14 dB. The voice itself must come through intact.
        assert!(sdr > 13.0, "SI-SDR {sdr:.1} dB");
        assert!(level.abs() < 0.5, "level changed by {level:.2} dB");
    }
}
