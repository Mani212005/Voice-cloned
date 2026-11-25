//! Small DSP building blocks: RBJ biquads and ITU-R BS.1770 loudness.

use std::f64::consts::PI;

/// A second-order IIR section (transposed direct form II), coefficients normalized by a0.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Biquad {
    b: [f64; 3],
    a: [f64; 2],
    z: [f64; 2],
}

impl Biquad {
    fn new(b: [f64; 3], a0: f64, a1: f64, a2: f64) -> Self {
        Self {
            b: [b[0] / a0, b[1] / a0, b[2] / a0],
            a: [a1 / a0, a2 / a0],
            z: [0.0; 2],
        }
    }

    /// Butterworth-style high-pass (RBJ cookbook) at `freq` Hz with quality `q`.
    pub fn highpass(rate: f64, freq: f64, q: f64) -> Self {
        let w0 = 2.0 * PI * freq / rate;
        let (s, c) = w0.sin_cos();
        let alpha = s / (2.0 * q);
        Self::new(
            [(1.0 + c) / 2.0, -(1.0 + c), (1.0 + c) / 2.0],
            1.0 + alpha,
            -2.0 * c,
            1.0 - alpha,
        )
    }

    /// Butterworth-style low-pass (RBJ cookbook) at `freq` Hz with quality `q`.
    pub fn lowpass(rate: f64, freq: f64, q: f64) -> Self {
        let w0 = 2.0 * PI * freq / rate;
        let (s, c) = w0.sin_cos();
        let alpha = s / (2.0 * q);
        Self::new(
            [(1.0 - c) / 2.0, 1.0 - c, (1.0 - c) / 2.0],
            1.0 + alpha,
            -2.0 * c,
            1.0 - alpha,
        )
    }

    /// Peaking EQ (RBJ cookbook): `gain_db` at `freq` Hz, bandwidth set by `q`.
    pub fn peaking(rate: f64, freq: f64, q: f64, gain_db: f64) -> Self {
        let a = 10f64.powf(gain_db / 40.0);
        let w0 = 2.0 * PI * freq / rate;
        let (s, c) = w0.sin_cos();
        let alpha = s / (2.0 * q);
        Self::new(
            [1.0 + alpha * a, -2.0 * c, 1.0 - alpha * a],
            1.0 + alpha / a,
            -2.0 * c,
            1.0 - alpha / a,
        )
    }

    /// High shelf (RBJ cookbook): `gain_db` above about `freq` Hz.
    pub fn high_shelf(rate: f64, freq: f64, q: f64, gain_db: f64) -> Self {
        let a = 10f64.powf(gain_db / 40.0);
        let w0 = 2.0 * PI * freq / rate;
        let (s, c) = w0.sin_cos();
        let alpha = s / (2.0 * q);
        let sa = 2.0 * a.sqrt() * alpha;
        Self::new(
            [
                a * ((a + 1.0) + (a - 1.0) * c + sa),
                -2.0 * a * ((a - 1.0) + (a + 1.0) * c),
                a * ((a + 1.0) + (a - 1.0) * c - sa),
            ],
            (a + 1.0) - (a - 1.0) * c + sa,
            2.0 * ((a - 1.0) - (a + 1.0) * c),
            (a + 1.0) - (a - 1.0) * c - sa,
        )
    }

    #[inline]
    pub fn tick(&mut self, x: f64) -> f64 {
        let y = self.b[0] * x + self.z[0];
        self.z[0] = self.b[1] * x - self.a[0] * y + self.z[1];
        self.z[1] = self.b[2] * x - self.a[1] * y;
        y
    }

    pub fn run(mut self, x: &mut [f64]) {
        for v in x {
            *v = self.tick(*v);
        }
    }
}

/// Integrated loudness in LUFS (ITU-R BS.1770-4: K-weighting, 400 ms blocks at 75 %
/// overlap, absolute gate at -70 LUFS and relative gate 10 LU below the mean). `None`
/// for audio shorter than one block or entirely below the absolute gate.
pub fn integrated_loudness(x: &[f64], rate: u32) -> Option<f64> {
    let rate_f = f64::from(rate);
    let mut k = x.to_vec();
    Biquad::high_shelf(rate_f, 1500.0, std::f64::consts::FRAC_1_SQRT_2, 4.0).run(&mut k);
    Biquad::highpass(rate_f, 38.0, 0.5).run(&mut k);
    let block = (rate_f * 0.4).round() as usize;
    let step = (rate_f * 0.1).round() as usize;
    if block == 0 || step == 0 || k.len() < block {
        return None;
    }
    // Running sum of squares, so each block costs O(1).
    let mut cum = Vec::with_capacity(k.len() + 1);
    cum.push(0.0);
    let mut acc = 0.0;
    for v in &k {
        acc += v * v;
        cum.push(acc);
    }
    let blocks: Vec<f64> = (0..=(k.len() - block) / step)
        .map(|i| (cum[i * step + block] - cum[i * step]) / block as f64)
        .collect();
    let lufs = |z: f64| -0.691 + 10.0 * z.log10();
    let mean_over = |gate: f64| -> Option<f64> {
        let kept: Vec<f64> = blocks
            .iter()
            .copied()
            .filter(|&z| z > 0.0 && lufs(z) > gate)
            .collect();
        (!kept.is_empty()).then(|| kept.iter().sum::<f64>() / kept.len() as f64)
    };
    let abs_mean = mean_over(-70.0)?;
    let rel_gate = lufs(abs_mean) - 10.0;
    mean_over(rel_gate.max(-70.0)).map(lufs)
}

/// One-pole smoothing coefficient for a time constant of `ms` milliseconds.
pub fn one_pole(rate: f64, ms: f64) -> f64 {
    if ms <= 0.0 {
        return 0.0;
    }
    (-1.0 / (rate * ms / 1000.0)).exp()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sine(rate: u32, freq: f64, secs: f64, amp: f64) -> Vec<f64> {
        let n = (f64::from(rate) * secs) as usize;
        (0..n)
            .map(|i| amp * (2.0 * PI * freq * i as f64 / f64::from(rate)).sin())
            .collect()
    }

    fn rms_db(x: &[f64]) -> f64 {
        10.0 * (x.iter().map(|v| v * v).sum::<f64>() / x.len() as f64).log10()
    }

    #[test]
    fn full_scale_1k_sine_reads_minus_3_lufs() {
        // BS.1770 calibration: a 0 dBFS 997 Hz sine reads -3.01 LKFS. These RBJ-designed
        // K-weighting filters (shelf 4 dB at 1500 Hz, Q 0.707; high-pass 38 Hz, Q 0.5)
        // read 0.04-0.07 dB lower, exactly as pyloudnorm does; the expected values are
        // pyloudnorm 0.1.1's readings of the same signals.
        for (rate, pyloudnorm) in [
            (48_000, -3.051_680_7),
            (24_000, -3.063_456_0),
            (16_000, -3.083_215_7),
        ] {
            let l = integrated_loudness(&sine(rate, 997.0, 3.0, 1.0), rate).unwrap();
            assert!((l - pyloudnorm).abs() < 1e-3, "{rate} Hz: {l}");
            assert!((l + 3.01).abs() < 0.1, "{rate} Hz: {l}");
        }
    }

    #[test]
    fn loudness_tracks_gain_and_gates_silence() {
        let a = integrated_loudness(&sine(24_000, 500.0, 6.0, 0.1), 24_000).unwrap();
        let b = integrated_loudness(&sine(24_000, 500.0, 6.0, 0.2), 24_000).unwrap();
        assert!((b - a - 6.02).abs() < 0.02);
        // Padding with as much silence again barely lowers integrated loudness (gating);
        // only the few blocks straddling the edge count, half full.
        let mut padded = sine(24_000, 500.0, 6.0, 0.1);
        padded.extend(vec![0.0; 6 * 24_000]);
        let c = integrated_loudness(&padded, 24_000).unwrap();
        assert!((c - a).abs() < 0.2, "{a} vs {c}");
        assert!(integrated_loudness(&vec![0.0; 48_000], 24_000).is_none());
        assert!(integrated_loudness(&[0.5; 100], 24_000).is_none());
    }

    #[test]
    fn filters_do_what_they_say() {
        let rate = 24_000;
        let low = sine(rate, 30.0, 1.0, 0.5);
        let mid = sine(rate, 1000.0, 1.0, 0.5);
        let mut l = low.clone();
        Biquad::highpass(f64::from(rate), 80.0, std::f64::consts::FRAC_1_SQRT_2).run(&mut l);
        assert!(rms_db(&l[rate as usize / 2..]) - rms_db(&low) < -15.0);
        let mut m = mid.clone();
        Biquad::highpass(f64::from(rate), 80.0, std::f64::consts::FRAC_1_SQRT_2).run(&mut m);
        assert!((rms_db(&m[rate as usize / 2..]) - rms_db(&mid)).abs() < 0.1);
        let mut lp = sine(rate, 6000.0, 1.0, 0.5);
        Biquad::lowpass(f64::from(rate), 2000.0, std::f64::consts::FRAC_1_SQRT_2).run(&mut lp);
        assert!(rms_db(&lp[rate as usize / 2..]) - rms_db(&mid) < -18.0);
        let mut p = mid.clone();
        Biquad::peaking(f64::from(rate), 1000.0, 1.0, 6.0).run(&mut p);
        assert!((rms_db(&p[rate as usize / 2..]) - rms_db(&mid) - 6.0).abs() < 0.1);
        let hi = sine(rate, 9000.0, 1.0, 0.5);
        let mut h = hi.clone();
        Biquad::high_shelf(
            f64::from(rate),
            3000.0,
            std::f64::consts::FRAC_1_SQRT_2,
            4.0,
        )
        .run(&mut h);
        assert!((rms_db(&h[rate as usize / 2..]) - rms_db(&hi) - 4.0).abs() < 0.3);
    }
}
