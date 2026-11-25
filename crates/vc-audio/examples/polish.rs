//! `polish <in.wav> <out.wav> [key=value ...]`: apply vcd's output polish chain to a WAV
//! file. Keys override [`vc_audio::PolishParams`] fields by name (for listening tests).

use std::path::Path;

fn main() -> std::process::ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let [input, output, overrides @ ..] = args.as_slice() else {
        eprintln!("usage: polish <in.wav> <out.wav> [key=value ...]");
        return std::process::ExitCode::from(2);
    };
    let result = params(overrides).and_then(|p| {
        let pcm = vc_audio::read_wav_file(Path::new(input)).map_err(|e| e.to_string())?;
        let out = vc_audio::polish(&pcm, &p);
        let bytes = vc_audio::encode_wav_bytes(&out).map_err(|e| e.to_string())?;
        std::fs::write(output, bytes).map_err(|e| e.to_string())
    });
    match result {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("polish: {e}");
            std::process::ExitCode::FAILURE
        }
    }
}

fn params(overrides: &[String]) -> Result<vc_audio::PolishParams, String> {
    let mut p = vc_audio::PolishParams::default();
    for kv in overrides {
        let (k, v) = kv
            .split_once('=')
            .ok_or_else(|| format!("{kv}: expected key=value"))?;
        let v: f64 = v.parse().map_err(|_| format!("{kv}: not a number"))?;
        let slot = match k {
            "highpass_hz" => &mut p.highpass_hz,
            "deess_hz" => &mut p.deess_hz,
            "deess_threshold_db" => &mut p.deess_threshold_db,
            "deess_max_db" => &mut p.deess_max_db,
            "target_lufs" => &mut p.target_lufs,
            "max_gain_db" => &mut p.max_gain_db,
            "ceiling_dbfs" => &mut p.ceiling_dbfs,
            _ => return Err(format!("unknown key {k}")),
        };
        *slot = v;
    }
    Ok(p)
}
