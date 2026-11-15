//! `fake-engine`: a CLI-compatible stand-in for `crispasr`, for tests only.
//!
//! It accepts the argv `CrispCliBackend` builds and writes a deterministic 24 kHz mono
//! WAV to `--tts-output`. Its environment steers it:
//!
//! - `FAKE_ENGINE_MODE`: `ok` (default), `exit:<code>`, `signal:<n>`, `bad-wav`,
//!   `no-output`, `stereo`, `wrong-rate`
//! - `FAKE_ENGINE_ALLOC_MB`: touch this many MiB before writing output (drives the RSS
//!   budget check)
//! - `FAKE_ENGINE_STARTED_FIFO`: write our pid to this FIFO once started (the open blocks
//!   until the test reads it, which proves the engine is running without sleeping)
//! - `FAKE_ENGINE_BLOCK_FIFO`: then block reading this FIFO until a writer closes it
//! - `FAKE_ENGINE_RECORD`: write argv (`ARG <a>`) and environment (`ENV <k>=<v>`) here

use std::io::{Read, Write};
use std::path::PathBuf;
use std::process::ExitCode;

fn main() -> ExitCode {
    match run() {
        Ok(code) => code,
        Err(e) => {
            eprintln!("fake-engine: {e}");
            ExitCode::from(70)
        }
    }
}

fn run() -> Result<ExitCode, String> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let env = |k: &str| std::env::var(k).ok();

    if let Some(path) = env("FAKE_ENGINE_RECORD") {
        let mut out = String::new();
        for a in &args {
            out.push_str(&format!("ARG {a}\n"));
        }
        let mut vars: Vec<(String, String)> = std::env::vars().collect();
        vars.sort();
        for (k, v) in vars {
            out.push_str(&format!("ENV {k}={v}\n"));
        }
        std::fs::write(path, out).map_err(|e| e.to_string())?;
    }

    let value = |flag: &str| {
        args.iter()
            .position(|a| a == flag)
            .and_then(|i| args.get(i + 1))
            .cloned()
    };
    let text = value("--tts").ok_or("missing --tts")?;
    let output = PathBuf::from(value("--tts-output").ok_or("missing --tts-output")?);
    let voice = PathBuf::from(value("--voice").ok_or("missing --voice")?);
    value("-l").ok_or("missing -l")?;
    if !args.iter().any(|a| a == "--i-have-rights") {
        return Err("voice cloning needs --i-have-rights".into());
    }
    if !voice.is_file() {
        return Err(format!("voice {} not found", voice.display()));
    }
    eprintln!("fake-engine: synthesizing chunk");

    if let Some(fifo) = env("FAKE_ENGINE_STARTED_FIFO") {
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .open(fifo)
            .map_err(|e| e.to_string())?;
        writeln!(f, "{}", std::process::id()).map_err(|e| e.to_string())?;
    }
    if let Some(fifo) = env("FAKE_ENGINE_BLOCK_FIFO") {
        let mut buf = Vec::new();
        std::fs::File::open(fifo)
            .and_then(|mut f| f.read_to_end(&mut buf))
            .map_err(|e| e.to_string())?;
    }
    let mut ballast = Vec::new();
    if let Some(mb) = env("FAKE_ENGINE_ALLOC_MB").and_then(|v| v.parse::<usize>().ok()) {
        ballast = vec![1u8; mb << 20];
        // Touch every page so it is resident.
        for i in (0..ballast.len()).step_by(4096) {
            ballast[i] = (i % 251) as u8;
        }
    }
    std::hint::black_box(&ballast);

    let pcm = vc_backend::fake::fake_pcm(&text);
    let mode = env("FAKE_ENGINE_MODE").unwrap_or_else(|| "ok".into());
    match mode.split_once(':') {
        Some(("exit", code)) => {
            let code: u8 = code.parse().map_err(|_| "bad exit code")?;
            return Ok(ExitCode::from(code));
        }
        Some(("signal", sig)) => {
            let sig: i32 = sig.parse().map_err(|_| "bad signal")?;
            // SAFETY: raising a signal on ourselves.
            unsafe {
                libc::raise(sig);
            }
            return Err("signal did not terminate".into());
        }
        _ => {}
    }
    match mode.as_str() {
        "ok" => write_wav(&output, &pcm, 1, 24_000)?,
        "bad-wav" => std::fs::write(&output, b"RIFF\0\0\0\0WAVEjunk").map_err(|e| e.to_string())?,
        "no-output" => {}
        "stereo" => write_wav(&output, &pcm, 2, 24_000)?,
        "wrong-rate" => write_wav(&output, &pcm, 1, 16_000)?,
        other => return Err(format!("unknown FAKE_ENGINE_MODE {other}")),
    }
    Ok(ExitCode::SUCCESS)
}

fn write_wav(
    path: &std::path::Path,
    pcm: &vc_audio::MonoPcm,
    channels: u16,
    rate: u32,
) -> Result<(), String> {
    let spec = hound_spec(channels, rate);
    let mut w = hound::WavWriter::create(path, spec).map_err(|e| e.to_string())?;
    for &s in &pcm.samples {
        for _ in 0..channels {
            w.write_sample(s).map_err(|e| e.to_string())?;
        }
    }
    w.finalize().map_err(|e| e.to_string())
}

fn hound_spec(channels: u16, sample_rate: u32) -> hound::WavSpec {
    hound::WavSpec {
        channels,
        sample_rate,
        bits_per_sample: 16,
        sample_format: hound::SampleFormat::Int,
    }
}
