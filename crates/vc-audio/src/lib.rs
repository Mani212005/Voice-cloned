//! WAV decode/encode and the checks applied to voice references and engine output.
//!
//! v1 accepts WAV only (PCM 8/16/24/32-bit integer or 32-bit float, any rate, any channel
//! count). Everything is converted to mono 16-bit PCM at the source rate: the engine
//! resamples the reference itself, and keeping the source rate avoids a second resampler
//! in the chain.

use std::io::{Read, Seek, Write};
use std::path::Path;

#[derive(Debug, thiserror::Error)]
pub enum AudioError {
    #[error("not a readable WAV file: {0}")]
    Decode(#[from] hound::Error),
    #[error("unsupported WAV format: {0}")]
    Unsupported(&'static str),
    #[error("{0}")]
    Invalid(String),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
}

/// Mono signed 16-bit PCM.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MonoPcm {
    pub sample_rate: u32,
    pub samples: Vec<i16>,
}

impl MonoPcm {
    pub fn duration_s(&self) -> f64 {
        if self.sample_rate == 0 {
            return 0.0;
        }
        self.samples.len() as f64 / f64::from(self.sample_rate)
    }

    pub fn duration_ms(&self) -> u64 {
        if self.sample_rate == 0 {
            return 0;
        }
        self.samples.len() as u64 * 1000 / u64::from(self.sample_rate)
    }
}

/// Decode a WAV stream to mono i16, averaging channels.
pub fn decode_wav<R: Read>(reader: R) -> Result<MonoPcm, AudioError> {
    let mut wav = hound::WavReader::new(reader)?;
    let spec = wav.spec();
    let channels = usize::from(spec.channels);
    if channels == 0 {
        return Err(AudioError::Unsupported("zero channels"));
    }
    if spec.sample_rate < 8_000 || spec.sample_rate > 192_000 {
        return Err(AudioError::Unsupported("sample rate outside 8-192 kHz"));
    }
    let interleaved: Vec<f32> = match (spec.sample_format, spec.bits_per_sample) {
        (hound::SampleFormat::Float, 32) => wav.samples::<f32>().collect::<Result<_, _>>()?,
        (hound::SampleFormat::Int, bits @ (8 | 16 | 24 | 32)) => {
            let scale = 1.0 / (1u64 << (bits - 1)) as f32;
            wav.samples::<i32>()
                .map(|s| s.map(|v| v as f32 * scale))
                .collect::<Result<_, _>>()?
        }
        _ => return Err(AudioError::Unsupported("sample format")),
    };
    if !interleaved.len().is_multiple_of(channels) {
        return Err(AudioError::Invalid("truncated frame".into()));
    }
    let samples = interleaved
        .chunks_exact(channels)
        .map(|frame| {
            let mean = frame.iter().sum::<f32>() / channels as f32;
            to_i16(mean)
        })
        .collect();
    Ok(MonoPcm {
        sample_rate: spec.sample_rate,
        samples,
    })
}

pub fn read_wav_file(path: &Path) -> Result<MonoPcm, AudioError> {
    decode_wav(std::io::BufReader::new(std::fs::File::open(path)?))
}

fn to_i16(x: f32) -> i16 {
    if x.is_nan() {
        return 0;
    }
    (x * 32768.0).round().clamp(-32768.0, 32767.0) as i16
}

/// Encode mono i16 PCM as a canonical 44-byte-header WAV.
pub fn encode_wav<W: Write + Seek>(pcm: &MonoPcm, writer: W) -> Result<(), AudioError> {
    let spec = hound::WavSpec {
        channels: 1,
        sample_rate: pcm.sample_rate,
        bits_per_sample: 16,
        sample_format: hound::SampleFormat::Int,
    };
    let mut w = hound::WavWriter::new(writer, spec)?;
    {
        let mut i16w = w.get_i16_writer(pcm.samples.len() as u32);
        for &s in &pcm.samples {
            i16w.write_sample(s);
        }
        i16w.flush()?;
    }
    w.finalize()?;
    Ok(())
}

pub fn encode_wav_bytes(pcm: &MonoPcm) -> Result<Vec<u8>, AudioError> {
    let mut cursor = std::io::Cursor::new(Vec::with_capacity(44 + pcm.samples.len() * 2));
    encode_wav(pcm, &mut cursor)?;
    Ok(cursor.into_inner())
}

/// Join chunks with `gap_ms` of silence between them. All chunks must share one rate.
pub fn concat_with_gap(chunks: &[MonoPcm], gap_ms: u32) -> Result<MonoPcm, AudioError> {
    let Some(first) = chunks.first() else {
        return Err(AudioError::Invalid("no chunks".into()));
    };
    let rate = first.sample_rate;
    if chunks.iter().any(|c| c.sample_rate != rate) {
        return Err(AudioError::Invalid("chunks differ in sample rate".into()));
    }
    let gap = (u64::from(rate) * u64::from(gap_ms) / 1000) as usize;
    let total = chunks.iter().map(|c| c.samples.len()).sum::<usize>() + gap * (chunks.len() - 1);
    let mut samples = Vec::with_capacity(total);
    for (i, c) in chunks.iter().enumerate() {
        if i > 0 {
            samples.resize(samples.len() + gap, 0);
        }
        samples.extend_from_slice(&c.samples);
    }
    Ok(MonoPcm {
        sample_rate: rate,
        samples,
    })
}

/// Speaking rates [`time_stretch`] accepts: 0.5x (half speed) to 2x.
pub const RATE_RANGE: std::ops::RangeInclusive<f64> = 0.5..=2.0;

/// Change the speaking rate of speech without changing its pitch (WSOLA: waveform
/// similarity overlap-add). `rate` above 1 is faster and shorter: the output has
/// `len / rate` samples. Rate 1 returns the input unchanged, sample for sample.
///
/// 25 ms Hann frames at 50 % overlap are taken from the input at `rate` times the output
/// hop; each frame is shifted by up to 10 ms to the position that best continues the
/// waveform already written, so pitch periods line up and no phasing or echo appears.
pub fn time_stretch(pcm: &MonoPcm, rate: f64) -> Result<MonoPcm, AudioError> {
    if !RATE_RANGE.contains(&rate) {
        return Err(AudioError::Invalid(format!(
            "rate {rate} is outside {}-{}",
            RATE_RANGE.start(),
            RATE_RANGE.end()
        )));
    }
    if rate == 1.0 || pcm.samples.is_empty() {
        return Ok(pcm.clone());
    }
    let sr = pcm.sample_rate as usize;
    // Frame length (even) and synthesis hop.
    let n = ((sr * 25 / 1000) / 2 * 2).max(16);
    let hs = n / 2;
    let tol = (sr / 100).max(1);
    let x: Vec<f32> = pcm.samples.iter().map(|&s| f32::from(s)).collect();
    let len = x.len();
    let target = ((len as f64) / rate).round() as usize;
    let at = |i: isize| -> f32 {
        usize::try_from(i)
            .ok()
            .and_then(|i| x.get(i))
            .copied()
            .unwrap_or(0.0)
    };
    // Periodic Hann: overlapping copies at hop n/2 sum to exactly 1.
    let win: Vec<f32> = (0..n)
        .map(|i| {
            let ph = 2.0 * std::f64::consts::PI * i as f64 / n as f64;
            (0.5 - 0.5 * ph.cos()) as f32
        })
        .collect();
    let frames = target / hs + 2;
    let mut y = vec![0.0f32; frames * hs + n];
    let mut wsum = vec![0.0f32; frames * hs + n];
    let mut prev: isize = 0;
    for k in 0..frames {
        let nominal = (k as f64 * hs as f64 * rate).round() as isize;
        let pos = if k == 0 {
            0
        } else {
            // The natural continuation of the previous frame, which the chosen frame
            // should resemble.
            let natural = prev + hs as isize;
            let mut best = nominal;
            let mut best_score = f32::NEG_INFINITY;
            for d in -(tol as isize)..=(tol as isize) {
                let cand = nominal + d;
                if cand < 0 {
                    continue;
                }
                let mut dot = 0.0f32;
                let mut energy = 1e-6f32;
                // Every other sample is enough to rank candidates and halves the cost.
                for i in (0..n).step_by(2) {
                    let c = at(cand + i as isize);
                    dot += c * at(natural + i as isize);
                    energy += c * c;
                }
                let score = dot / energy.sqrt();
                if score > best_score {
                    best_score = score;
                    best = cand;
                }
            }
            best
        };
        let out0 = k * hs;
        for i in 0..n {
            y[out0 + i] += win[i] * at(pos + i as isize);
            wsum[out0 + i] += win[i];
        }
        prev = pos;
    }
    let samples = y
        .iter()
        .zip(&wsum)
        .take(target)
        .map(|(&v, &w)| {
            let v = if w > 1e-3 { v / w } else { v };
            v.round().clamp(-32768.0, 32767.0) as i16
        })
        .collect();
    Ok(MonoPcm {
        sample_rate: pcm.sample_rate,
        samples,
    })
}

/// Limits for an enrollment reference.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ReferenceLimits {
    pub min_s: f64,
    pub max_s: f64,
}

/// Summary of a reference, after validation.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ReferenceStats {
    pub duration_s: f64,
    pub rms_dbfs: f64,
    pub clipped_ratio: f64,
}

/// Validate a reference: duration within limits, not silent, not heavily clipped.
pub fn validate_reference(
    pcm: &MonoPcm,
    limits: ReferenceLimits,
) -> Result<ReferenceStats, AudioError> {
    let duration_s = pcm.duration_s();
    if duration_s < limits.min_s || duration_s > limits.max_s {
        return Err(AudioError::Invalid(format!(
            "reference is {duration_s:.1} s; it must be {}-{} s",
            limits.min_s, limits.max_s
        )));
    }
    let n = pcm.samples.len() as f64;
    let energy: f64 = pcm.samples.iter().map(|&s| f64::from(s).powi(2)).sum();
    let rms = (energy / n).sqrt() / 32768.0;
    let rms_dbfs = if rms > 0.0 {
        20.0 * rms.log10()
    } else {
        f64::NEG_INFINITY
    };
    if rms_dbfs < -50.0 {
        return Err(AudioError::Invalid("reference is silent".into()));
    }
    let clipped = pcm
        .samples
        .iter()
        .filter(|&&s| s == i16::MAX || s == i16::MIN)
        .count();
    let clipped_ratio = clipped as f64 / n;
    if clipped_ratio > 0.001 {
        return Err(AudioError::Invalid("reference is clipped".into()));
    }
    Ok(ReferenceStats {
        duration_s,
        rms_dbfs,
        clipped_ratio,
    })
}

/// Checks on one engine output chunk.
pub fn validate_engine_output(pcm: &MonoPcm, expected_rate: u32) -> Result<(), AudioError> {
    if pcm.sample_rate != expected_rate {
        return Err(AudioError::Invalid(format!(
            "engine wrote {} Hz, expected {expected_rate} Hz",
            pcm.sample_rate
        )));
    }
    if pcm.samples.is_empty() {
        return Err(AudioError::Invalid("engine wrote no samples".into()));
    }
    Ok(())
}

/// Read engine output from a file and require mono, so a stereo file is not silently
/// downmixed.
pub fn read_engine_output(path: &Path, expected_rate: u32) -> Result<MonoPcm, AudioError> {
    decode_engine_output(&std::fs::read(path)?, expected_rate)
}

/// [`read_engine_output`] for a WAV already in memory (a warm engine answers over HTTP).
pub fn decode_engine_output(wav: &[u8], expected_rate: u32) -> Result<MonoPcm, AudioError> {
    if hound::WavReader::new(wav)?.spec().channels != 1 {
        return Err(AudioError::Invalid("engine output is not mono".into()));
    }
    let pcm = decode_wav(wav)?;
    validate_engine_output(&pcm, expected_rate)?;
    Ok(pcm)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tone(rate: u32, seconds: f64, amp: f64) -> MonoPcm {
        let n = (f64::from(rate) * seconds) as usize;
        MonoPcm {
            sample_rate: rate,
            samples: (0..n)
                .map(|i| {
                    let t = i as f64 / f64::from(rate);
                    (amp * 32767.0 * (2.0 * std::f64::consts::PI * 220.0 * t).sin()) as i16
                })
                .collect(),
        }
    }

    #[test]
    fn wav_round_trip_is_lossless() {
        let pcm = tone(16_000, 0.5, 0.5);
        let bytes = encode_wav_bytes(&pcm).unwrap();
        assert_eq!(decode_wav(std::io::Cursor::new(bytes)).unwrap(), pcm);
    }

    #[test]
    fn stereo_is_averaged_to_mono() {
        let spec = hound::WavSpec {
            channels: 2,
            sample_rate: 24_000,
            bits_per_sample: 16,
            sample_format: hound::SampleFormat::Int,
        };
        let mut cur = std::io::Cursor::new(Vec::new());
        {
            let mut w = hound::WavWriter::new(&mut cur, spec).unwrap();
            for (l, r) in [(1000i16, 3000i16), (-2000, 0)] {
                w.write_sample(l).unwrap();
                w.write_sample(r).unwrap();
            }
            w.finalize().unwrap();
        }
        let pcm = decode_wav(std::io::Cursor::new(cur.into_inner())).unwrap();
        assert_eq!(pcm.samples, vec![2000, -1000]);
        assert_eq!(pcm.sample_rate, 24_000);
    }

    #[test]
    fn float_wav_is_converted() {
        let spec = hound::WavSpec {
            channels: 1,
            sample_rate: 48_000,
            bits_per_sample: 32,
            sample_format: hound::SampleFormat::Float,
        };
        let mut cur = std::io::Cursor::new(Vec::new());
        {
            let mut w = hound::WavWriter::new(&mut cur, spec).unwrap();
            for s in [0.5f32, -1.0, 2.0] {
                w.write_sample(s).unwrap();
            }
            w.finalize().unwrap();
        }
        let pcm = decode_wav(std::io::Cursor::new(cur.into_inner())).unwrap();
        assert_eq!(pcm.samples, vec![16384, -32768, 32767]);
    }

    #[test]
    fn garbage_is_rejected() {
        assert!(decode_wav(std::io::Cursor::new(b"not a wav".to_vec())).is_err());
    }

    #[test]
    fn concat_inserts_exact_gap() {
        let a = MonoPcm {
            sample_rate: 24_000,
            samples: vec![1; 10],
        };
        let b = MonoPcm {
            sample_rate: 24_000,
            samples: vec![2; 5],
        };
        let out = concat_with_gap(&[a, b], 200).unwrap();
        assert_eq!(out.samples.len(), 10 + 4800 + 5);
        assert!(out.samples[10..4810].iter().all(|&s| s == 0));
        assert_eq!(out.samples[4810], 2);
    }

    #[test]
    fn concat_rejects_mixed_rates() {
        let a = MonoPcm {
            sample_rate: 24_000,
            samples: vec![1],
        };
        let b = MonoPcm {
            sample_rate: 16_000,
            samples: vec![1],
        };
        assert!(concat_with_gap(&[a, b], 0).is_err());
    }

    /// Frequency of the strongest autocorrelation peak between 80 and 1000 Hz.
    fn pitch_hz(p: &MonoPcm) -> f64 {
        let x: Vec<f64> = p.samples.iter().map(|&s| f64::from(s)).collect();
        let sr = p.sample_rate as usize;
        let mid = &x[x.len() / 4..x.len() * 3 / 4];
        let (lo, hi) = (sr / 1000, sr / 80);
        let lag = (lo..hi)
            .max_by(|&a, &b| {
                let ra: f64 = mid.windows(a + 1).map(|w| w[0] * w[a]).sum();
                let rb: f64 = mid.windows(b + 1).map(|w| w[0] * w[b]).sum();
                ra.total_cmp(&rb)
            })
            .unwrap();
        sr as f64 / lag as f64
    }

    fn rms(p: &MonoPcm) -> f64 {
        let n = p.samples.len() as f64;
        (p.samples.iter().map(|&s| f64::from(s).powi(2)).sum::<f64>() / n).sqrt()
    }

    #[test]
    fn rate_one_is_the_identity() {
        let pcm = tone(24_000, 1.0, 0.4);
        assert_eq!(time_stretch(&pcm, 1.0).unwrap(), pcm);
    }

    #[test]
    fn stretch_changes_length_but_not_pitch_or_level() {
        let pcm = tone(24_000, 2.0, 0.4);
        for rate in [0.75, 1.2, 1.5] {
            let out = time_stretch(&pcm, rate).unwrap();
            let want = (pcm.samples.len() as f64 / rate).round() as usize;
            assert_eq!(out.samples.len(), want, "rate {rate}");
            assert!(
                (pitch_hz(&out) - 220.0).abs() < 3.0,
                "rate {rate}: {} Hz",
                pitch_hz(&out)
            );
            let db = 20.0 * (rms(&out) / rms(&pcm)).log10();
            assert!(db.abs() < 0.5, "rate {rate}: level changed by {db} dB");
        }
    }

    #[test]
    fn stretch_keeps_silence_silent_and_rejects_bad_rates() {
        let silent = MonoPcm {
            sample_rate: 24_000,
            samples: vec![0; 24_000],
        };
        let out = time_stretch(&silent, 1.2).unwrap();
        assert!(out.samples.iter().all(|&s| s == 0));
        for bad in [0.0, 0.49, 2.01, f64::NAN, f64::INFINITY] {
            assert!(time_stretch(&silent, bad).is_err(), "{bad}");
        }
        let empty = MonoPcm {
            sample_rate: 24_000,
            samples: vec![],
        };
        assert!(time_stretch(&empty, 1.2).unwrap().samples.is_empty());
    }

    #[test]
    fn stretch_does_not_overflow_at_full_scale() {
        let loud = tone(24_000, 0.5, 1.0);
        let out = time_stretch(&loud, 1.3).unwrap();
        let peak = out.samples.iter().map(|s| s.unsigned_abs()).max().unwrap();
        assert!(peak >= 32_000);
    }

    #[test]
    fn reference_validation() {
        let limits = ReferenceLimits {
            min_s: 5.0,
            max_s: 30.0,
        };
        assert!(validate_reference(&tone(16_000, 10.0, 0.3), limits).is_ok());
        assert!(validate_reference(&tone(16_000, 2.0, 0.3), limits).is_err());
        assert!(validate_reference(&tone(16_000, 31.0, 0.3), limits).is_err());
        assert!(validate_reference(&tone(16_000, 10.0, 0.0), limits).is_err());
        assert!(validate_reference(&tone(16_000, 10.0, 2.0), limits).is_err());
    }

    #[test]
    fn engine_output_checks() {
        let ok = tone(24_000, 0.1, 0.3);
        assert!(validate_engine_output(&ok, 24_000).is_ok());
        assert!(validate_engine_output(&ok, 16_000).is_err());
        let empty = MonoPcm {
            sample_rate: 24_000,
            samples: vec![],
        };
        assert!(validate_engine_output(&empty, 24_000).is_err());
    }
}
