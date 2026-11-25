//! A light finishing chain for synthesized speech: a high-pass for rumble and DC, a
//! de-esser, loudness normalization and a look-ahead peak limiter. Deterministic and
//! sample-rate independent. There is deliberately no compressor or EQ: in listening
//! measurements both made the cloned voice less natural (see `docs/clean-voice.md`).

use crate::MonoPcm;
use crate::dsp::{Biquad, integrated_loudness, one_pole};

/// Settings of [`polish`]. [`PolishParams::default`] is the shipped chain.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PolishParams {
    /// High-pass corner (Hz): removes rumble and DC below the voice.
    pub highpass_hz: f64,
    /// De-esser: the band above `deess_hz` is turned down by up to `deess_max_db` while
    /// it holds more than `deess_threshold_db` of the full-band level. 0 dB disables it.
    pub deess_hz: f64,
    pub deess_threshold_db: f64,
    pub deess_max_db: f64,
    /// Integrated loudness target (LUFS) and the largest gain applied to reach it.
    pub target_lufs: f64,
    pub max_gain_db: f64,
    /// Sample-peak ceiling (dBFS) enforced by a look-ahead limiter.
    pub ceiling_dbfs: f64,
}

impl Default for PolishParams {
    fn default() -> Self {
        Self {
            highpass_hz: 40.0,
            deess_hz: 5500.0,
            deess_threshold_db: -12.0,
            deess_max_db: 6.0,
            target_lufs: -16.0,
            max_gain_db: 20.0,
            ceiling_dbfs: -1.0,
        }
    }
}

/// Apply the finishing chain. Silence stays silent; audio too short to measure loudness
/// (under 0.4 s) is filtered but not level-normalized.
pub fn polish(pcm: &MonoPcm, p: &PolishParams) -> MonoPcm {
    let rate = f64::from(pcm.sample_rate);
    let mut x: Vec<f64> = pcm
        .samples
        .iter()
        .map(|&s| f64::from(s) / 32768.0)
        .collect();
    if x.is_empty() || x.iter().all(|&v| v == 0.0) || rate <= 0.0 {
        return pcm.clone();
    }
    Biquad::highpass(rate, p.highpass_hz, std::f64::consts::FRAC_1_SQRT_2).run(&mut x);
    if p.deess_max_db > 0.0 && p.deess_hz < rate / 2.0 * 0.9 {
        deess(&mut x, rate, p);
    }
    if let Some(l) = integrated_loudness(&x, pcm.sample_rate) {
        let g = 10f64.powf((p.target_lufs - l).min(p.max_gain_db) / 20.0);
        x.iter_mut().for_each(|v| *v *= g);
    }
    limit(&mut x, rate, 10f64.powf(p.ceiling_dbfs / 20.0));
    MonoPcm {
        sample_rate: pcm.sample_rate,
        samples: x
            .iter()
            .map(|&v| (v * 32768.0).round().clamp(-32768.0, 32767.0) as i16)
            .collect(),
    }
}

/// Split-band de-esser: y = x - (1 - g) * hp(x), so with g = 1 the signal is untouched.
fn deess(x: &mut [f64], rate: f64, p: &PolishParams) {
    let mut hp = Biquad::highpass(rate, p.deess_hz, std::f64::consts::FRAC_1_SQRT_2);
    let (att, rel) = (one_pole(rate, 1.0), one_pole(rate, 40.0));
    let (mut e_hi, mut e_all) = (0.0f64, 0.0f64);
    let follow = |e: f64, v: f64| {
        let c = if v > e { att } else { rel };
        c * e + (1.0 - c) * v
    };
    for v in x.iter_mut() {
        let h = hp.tick(*v);
        e_hi = follow(e_hi, h * h);
        e_all = follow(e_all, *v * *v);
        let ratio_db = 10.0 * ((e_hi + 1e-12) / (e_all + 1e-12)).log10();
        let over = ratio_db - p.deess_threshold_db;
        if over > 0.0 {
            let red_db = (over * 0.5).min(p.deess_max_db);
            let g = 10f64.powf(-red_db / 20.0);
            *v -= (1.0 - g) * h;
        }
    }
}

/// Look-ahead peak limiter: never lets |x| exceed `ceiling`. The gain at each sample is
/// at most the gain every sample within `look` of it needs, smoothed by a box filter
/// that cannot rise above that bound, then released slowly.
fn limit(x: &mut [f64], rate: f64, ceiling: f64) {
    let n = x.len();
    let need: Vec<f64> = x
        .iter()
        .map(|v| {
            let a = v.abs();
            if a > ceiling { ceiling / a } else { 1.0 }
        })
        .collect();
    if need.iter().all(|&g| g >= 1.0) {
        return;
    }
    let look = ((rate * 0.003) as usize).max(1);
    // Sliding minimum over [i - look, i + look] (monotone deque).
    let mut m = vec![1.0; n];
    let mut dq: std::collections::VecDeque<usize> = std::collections::VecDeque::new();
    let mut next = 0;
    for (i, slot) in m.iter_mut().enumerate() {
        while next < n && next <= i + look {
            while dq.back().is_some_and(|&j| need[j] >= need[next]) {
                dq.pop_back();
            }
            dq.push_back(next);
            next += 1;
        }
        while dq.front().is_some_and(|&j| j + look < i) {
            dq.pop_front();
        }
        *slot = dq.front().map_or(1.0, |&j| need[j]);
    }
    // Box mean over [i - half, i + half]: every term is a minimum over a window that
    // contains i, so the mean is <= need[i].
    let half = look / 2;
    let mut cum = Vec::with_capacity(n + 1);
    cum.push(0.0);
    for v in &m {
        cum.push(cum.last().copied().unwrap_or(0.0) + v);
    }
    let rel = one_pole(rate, 50.0);
    let mut g = 1.0f64;
    for i in 0..n {
        let (lo, hi) = (i.saturating_sub(half), (i + half + 1).min(n));
        let s = (cum[hi] - cum[lo]) / (hi - lo) as f64;
        g = s.min(rel * g + (1.0 - rel));
        x[i] *= g.min(need[i]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pcm(rate: u32, x: &[f64]) -> MonoPcm {
        MonoPcm {
            sample_rate: rate,
            samples: x
                .iter()
                .map(|&v| (v * 32768.0).round().clamp(-32768.0, 32767.0) as i16)
                .collect(),
        }
    }

    fn as_f64(p: &MonoPcm) -> Vec<f64> {
        p.samples.iter().map(|&s| f64::from(s) / 32768.0).collect()
    }

    /// A vowel-like test signal: 120 Hz harmonics with a falling spectrum, in syllables.
    fn voice(rate: u32, secs: f64, amp: f64) -> Vec<f64> {
        let n = (f64::from(rate) * secs) as usize;
        (0..n)
            .map(|i| {
                let t = i as f64 / f64::from(rate);
                let syl = (std::f64::consts::PI * t * 4.0).sin().abs();
                let v: f64 = (1..30)
                    .map(|k| {
                        let f = 120.0 * k as f64;
                        (2.0 * std::f64::consts::PI * f * t).sin() / k as f64
                    })
                    .sum();
                amp * syl * v / 3.0
            })
            .collect()
    }

    #[test]
    fn reaches_the_loudness_target_under_the_ceiling() {
        let p = PolishParams::default();
        for rate in [16_000, 24_000] {
            for amp in [0.1, 0.3, 0.9] {
                let out = polish(&pcm(rate, &voice(rate, 4.0, amp)), &p);
                let y = as_f64(&out);
                let l = integrated_loudness(&y, rate).unwrap();
                assert!((l - p.target_lufs).abs() < 1.0, "{rate} {amp}: {l} LUFS");
                let peak = y.iter().fold(0.0f64, |m, v| m.max(v.abs()));
                assert!(
                    20.0 * peak.log10() <= p.ceiling_dbfs + 0.01,
                    "{rate} {amp}: peak {peak}"
                );
                assert_eq!(out.samples.len(), (f64::from(rate) * 4.0) as usize);
            }
        }
    }

    #[test]
    fn gain_is_capped_for_near_silent_input() {
        let p = PolishParams::default();
        let x = voice(24_000, 4.0, 0.002);
        let before = integrated_loudness(&x, 24_000).unwrap();
        let after = integrated_loudness(&as_f64(&polish(&pcm(24_000, &x), &p)), 24_000).unwrap();
        assert!(after - before <= p.max_gain_db + 0.5, "{before} -> {after}");
        assert!(after < p.target_lufs - 5.0);
    }

    #[test]
    fn removes_rumble_and_keeps_silence() {
        let rate = 24_000;
        let mut x = voice(rate, 3.0, 0.1);
        let rumble = |i: usize| 0.05 * (2.0 * std::f64::consts::PI * 15.0 * i as f64 / 24e3).sin();
        x.iter_mut().enumerate().for_each(|(i, v)| *v += rumble(i));
        let out = as_f64(&polish(&pcm(rate, &x), &PolishParams::default()));
        // Share of the 15 Hz tone in the signal (correlation over total level), before
        // and after: the high-pass must cut it by more than 12 dB relative to the voice.
        let share = |y: &[f64]| -> f64 {
            let c = y
                .iter()
                .enumerate()
                .map(|(i, v)| v * rumble(i))
                .sum::<f64>()
                .abs();
            c / y.iter().map(|v| v * v).sum::<f64>().sqrt()
        };
        assert!(
            share(&out) / share(&x) < 0.25,
            "{}",
            share(&out) / share(&x)
        );
        let silent = MonoPcm {
            sample_rate: rate,
            samples: vec![0; 24_000],
        };
        assert_eq!(polish(&silent, &PolishParams::default()), silent);
    }

    #[test]
    fn limiter_holds_the_ceiling_on_full_scale_clicks() {
        let rate = 24_000.0;
        let mut x = vec![0.0; 24_000];
        for k in (1000..24_000).step_by(3000) {
            x[k] = 1.0;
            x[k + 1] = -1.0;
        }
        let c = 10f64.powf(-1.0 / 20.0);
        limit(&mut x, rate, c);
        assert!(x.iter().all(|v| v.abs() <= c + 1e-12));
    }

    /// Energy of `x` above 5 kHz over the samples where `mask` holds (dB).
    fn hf_db(x: &[f64], rate: f64, mask: &[bool]) -> f64 {
        let mut y = x.to_vec();
        for _ in 0..2 {
            Biquad::highpass(rate, 5000.0, std::f64::consts::FRAC_1_SQRT_2).run(&mut y);
        }
        let e: f64 = y
            .iter()
            .zip(mask)
            .filter(|(_, m)| **m)
            .map(|(v, _)| v * v)
            .sum();
        10.0 * e.log10()
    }

    #[test]
    fn deesser_tames_sibilants_and_leaves_vowels_alone() {
        let rate = 24_000;
        let r = f64::from(rate);
        let vowel = voice(rate, 4.0, 0.3);
        // Every half second, 80 ms of hiss (white noise above 5 kHz) as loud as the vowel:
        // an over-bright "s".
        let mut state = 0x853c_49e6_748f_ea9bu64;
        let mut hiss: Vec<f64> = (0..vowel.len())
            .map(|_| {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                (state >> 11) as f64 / (1u64 << 53) as f64 * 2.0 - 1.0
            })
            .collect();
        for _ in 0..2 {
            Biquad::highpass(r, 5000.0, std::f64::consts::FRAC_1_SQRT_2).run(&mut hiss);
        }
        let s_mask: Vec<bool> = (0..vowel.len())
            .map(|i| (i as f64 / r) % 0.5 < 0.08)
            .collect();
        let x: Vec<f64> = vowel
            .iter()
            .zip(&hiss)
            .zip(&s_mask)
            .map(|((v, h), &m)| v + if m { 0.25 * h } else { 0.0 })
            .collect();
        let on = PolishParams::default();
        let off = PolishParams {
            deess_max_db: 0.0,
            ..on
        };
        let with = as_f64(&polish(&pcm(rate, &x), &on));
        let without = as_f64(&polish(&pcm(rate, &x), &off));
        let cut = hf_db(&without, r, &s_mask) - hf_db(&with, r, &s_mask);
        assert!(cut > 2.0, "sibilants cut by only {cut:.1} dB");
        // A plain vowel never trips it: the output is the same with or without it.
        let v_with = polish(&pcm(rate, &vowel), &on);
        let v_without = polish(&pcm(rate, &vowel), &off);
        let diff = v_with
            .samples
            .iter()
            .zip(&v_without.samples)
            .map(|(a, b)| (i32::from(*a) - i32::from(*b)).unsigned_abs())
            .max()
            .unwrap();
        assert!(diff <= 1, "vowel changed by up to {diff} LSB");
    }
}
