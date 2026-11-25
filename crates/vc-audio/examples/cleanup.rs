//! `cleanup <in.wav> <out.wav> [max_reduction_db]`: apply vcd's enrollment cleanup to a
//! reference WAV (high-pass, then RNNoise capped at `max_reduction_db`, default 18).

use std::path::Path;

fn main() -> std::process::ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let (input, output, cap) = match args.as_slice() {
        [i, o] => (i, o, None),
        [i, o, c] => (i, o, Some(c)),
        _ => {
            eprintln!("usage: cleanup <in.wav> <out.wav> [max_reduction_db]");
            return std::process::ExitCode::from(2);
        }
    };
    let result = (|| {
        let mut p = vc_audio::CleanupParams::default();
        if let Some(c) = cap {
            p.max_reduction_db = c.parse().map_err(|_| format!("{c}: not a number"))?;
        }
        let pcm = vc_audio::read_wav_file(Path::new(input)).map_err(|e| e.to_string())?;
        let out = vc_audio::clean_reference(&pcm, &p);
        let tilt = |m: &vc_audio::MonoPcm| {
            let x: Vec<f64> = m.samples.iter().map(|&s| f64::from(s) / 32768.0).collect();
            vc_audio::hf_tilt_db(&x, m.sample_rate)
        };
        eprintln!(
            "4-8 kHz tilt: {:?} dB in, {:?} dB out",
            tilt(&pcm),
            tilt(&out)
        );
        let bytes = vc_audio::encode_wav_bytes(&out).map_err(|e| e.to_string())?;
        std::fs::write(output, bytes).map_err(|e| e.to_string())
    })();
    match result {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("cleanup: {e}");
            std::process::ExitCode::FAILURE
        }
    }
}
