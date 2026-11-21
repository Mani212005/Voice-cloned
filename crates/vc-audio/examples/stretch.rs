//! `stretch <rate> <in.wav> <out.wav>`: apply vcd's speaking-rate change to a WAV file.
//! A debugging aid for hearing or measuring a rate without running the service.

use std::path::Path;

fn main() -> std::process::ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let [rate, input, output] = args.as_slice() else {
        eprintln!("usage: stretch <rate> <in.wav> <out.wav>");
        return std::process::ExitCode::from(2);
    };
    let result = rate
        .parse::<f64>()
        .map_err(|e| e.to_string())
        .and_then(|rate| {
            let pcm = vc_audio::read_wav_file(Path::new(input)).map_err(|e| e.to_string())?;
            let out = vc_audio::time_stretch(&pcm, rate).map_err(|e| e.to_string())?;
            let bytes = vc_audio::encode_wav_bytes(&out).map_err(|e| e.to_string())?;
            std::fs::write(output, bytes).map_err(|e| e.to_string())
        });
    match result {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("stretch: {e}");
            std::process::ExitCode::FAILURE
        }
    }
}
