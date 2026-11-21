//! `fake-engine`: a stand-in for `crispasr`, for tests only.
//!
//! **CLI mode** accepts the argv `CrispCliBackend` builds and writes a deterministic
//! 24 kHz mono WAV to `--tts-output`. Its environment steers it:
//!
//! - `FAKE_ENGINE_MODE`: `ok` (default), `exit:<code>`, `signal:<n>`, `bad-wav`,
//!   `no-output`, `stereo`, `wrong-rate`
//! - `FAKE_ENGINE_ALLOC_MB`: touch this many MiB before writing output (drives the RSS
//!   budget check)
//! - `FAKE_ENGINE_STARTED_FIFO`: write our pid to this FIFO once started (the open blocks
//!   until the test reads it, which proves the engine is running without sleeping)
//! - `FAKE_ENGINE_BLOCK_FIFO`: then block reading this FIFO until a writer closes it
//! - `FAKE_ENGINE_RECORD`: write argv (`ARG <a>`) and environment (`ENV <k>=<v>`) here
//!
//! **Server mode** (`--server`, the argv `CrispWarmBackend` builds) serves `GET /health`
//! and `POST /v1/audio/speech` on `--port`, one request per connection, requiring the
//! key from `CRISPASR_API_KEYS` as a bearer token and the voice to exist in
//! `--voice-dir`. Speech is `fake_pcm(input)` as a WAV. Steering:
//!
//! - `FAKE_ENGINE_RECORD`: append `START <pid>`, then `ARG`/`ENV` lines, at start-up,
//!   `LISTEN <pid>` once the port is bound, and `REQ <json body>` for every speech request
//! - `FAKE_ENGINE_ALLOC_MB`: touch and keep this many more MiB on every speech request
//!   (a warm engine whose peak RSS grows per chunk)
//! - `FAKE_ENGINE_MODE`: `ok` (default), `exit-at-start:<code>`, `crash-on:<n>` (abort
//!   during the n-th speech request), `reject-on:<n>` (answer the n-th with HTTP 500),
//!   `silent-health` (accept `GET /health` connections but never answer them; the first
//!   one writes the pid to `FAKE_ENGINE_STARTED_FIFO`)
//! - `FAKE_ENGINE_STARTED_FIFO` / `FAKE_ENGINE_BLOCK_FIFO`: as in CLI mode, during every
//!   speech request whose input is `block`

use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
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
    if args.iter().any(|a| a == "--server") {
        return serve(&args);
    }
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

    announce_and_block()?;
    let ballast = allocate();
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

/// Write our pid to `FAKE_ENGINE_STARTED_FIFO`, if set.
fn announce_started() -> Result<(), String> {
    if let Ok(fifo) = std::env::var("FAKE_ENGINE_STARTED_FIFO") {
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .open(fifo)
            .map_err(|e| e.to_string())?;
        writeln!(f, "{}", std::process::id()).map_err(|e| e.to_string())?;
    }
    Ok(())
}

/// `FAKE_ENGINE_STARTED_FIFO` and `FAKE_ENGINE_BLOCK_FIFO`.
fn announce_and_block() -> Result<(), String> {
    announce_started()?;
    if let Ok(fifo) = std::env::var("FAKE_ENGINE_BLOCK_FIFO") {
        let mut buf = Vec::new();
        std::fs::File::open(fifo)
            .and_then(|mut f| f.read_to_end(&mut buf))
            .map_err(|e| e.to_string())?;
    }
    Ok(())
}

/// `FAKE_ENGINE_ALLOC_MB` MiB, every page touched so it is resident.
fn allocate() -> Vec<u8> {
    let Some(mb) = std::env::var("FAKE_ENGINE_ALLOC_MB")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
    else {
        return Vec::new();
    };
    let mut ballast = vec![1u8; mb << 20];
    for i in (0..ballast.len()).step_by(4096) {
        ballast[i] = (i % 251) as u8;
    }
    ballast
}

fn append(path: &str, text: &str) -> Result<(), String> {
    std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .and_then(|mut f| f.write_all(text.as_bytes()))
        .map_err(|e| e.to_string())
}

fn serve(args: &[String]) -> Result<ExitCode, String> {
    let value = |flag: &str| {
        args.iter()
            .position(|a| a == flag)
            .and_then(|i| args.get(i + 1))
            .cloned()
    };
    let record = std::env::var("FAKE_ENGINE_RECORD").ok();
    if let Some(path) = &record {
        let mut out = format!("START {}\n", std::process::id());
        for a in args {
            out.push_str(&format!("ARG {a}\n"));
        }
        let mut vars: Vec<(String, String)> = std::env::vars().collect();
        vars.sort();
        for (k, v) in vars {
            out.push_str(&format!("ENV {k}={v}\n"));
        }
        append(path, &out)?;
    }
    let mode = std::env::var("FAKE_ENGINE_MODE").unwrap_or_else(|_| "ok".into());
    let nth = |prefix: &str| {
        mode.strip_prefix(prefix)
            .and_then(|n| n.parse::<usize>().ok())
    };
    if let Some(code) = nth("exit-at-start:") {
        return Ok(ExitCode::from(u8::try_from(code).unwrap_or(1)));
    }
    let port: u16 = value("--port")
        .and_then(|p| p.parse().ok())
        .ok_or("missing --port")?;
    let voice_dir = PathBuf::from(value("--voice-dir").ok_or("missing --voice-dir")?);
    let key = std::env::var("CRISPASR_API_KEYS").map_err(|_| "missing CRISPASR_API_KEYS")?;
    let listener =
        std::net::TcpListener::bind(("127.0.0.1", port)).map_err(|e| format!("bind: {e}"))?;
    if let Some(path) = &record {
        append(path, &format!("LISTEN {}\n", std::process::id()))?;
    }
    eprintln!("fake-engine: serving on {port}");

    let mut kept: Vec<Vec<u8>> = Vec::new();
    let mut held: Vec<std::net::TcpStream> = Vec::new();
    let mut requests = 0usize;
    for stream in listener.incoming() {
        let mut stream = stream.map_err(|e| e.to_string())?;
        let Ok((method, path, headers, body)) = read_request(&mut stream) else {
            continue;
        };
        if mode == "silent-health" && method == "GET" && path == "/health" {
            if held.is_empty() {
                announce_started()?;
            }
            held.push(stream);
            continue;
        }
        let authorized = headers
            .iter()
            .any(|(k, v)| k.eq_ignore_ascii_case("authorization") && *v == format!("Bearer {key}"));
        let reply = if !authorized {
            respond(
                401,
                "application/json",
                br#"{"error":{"code":"invalid_api_key"}}"#,
            )
        } else if method == "GET" && path == "/health" {
            respond(200, "application/json", br#"{"status": "ok"}"#)
        } else if method == "POST" && path == "/v1/audio/speech" {
            requests += 1;
            if let Some(path) = &record {
                append(path, &format!("REQ {}\n", String::from_utf8_lossy(&body)))?;
            }
            speech(&body, &voice_dir, requests, &nth, &mut kept)?
        } else {
            respond(
                404,
                "application/json",
                br#"{"error":{"code":"not_found"}}"#,
            )
        };
        let _ = stream.write_all(&reply);
    }
    Ok(ExitCode::SUCCESS)
}

fn speech(
    body: &[u8],
    voice_dir: &Path,
    n: usize,
    nth: &dyn Fn(&str) -> Option<usize>,
    kept: &mut Vec<Vec<u8>>,
) -> Result<Vec<u8>, String> {
    let req: serde_json::Value = serde_json::from_slice(body).map_err(|e| e.to_string())?;
    let field = |k: &str| req.get(k).and_then(|v| v.as_str()).unwrap_or_default();
    let input = field("input").to_owned();
    if !voice_dir.join(format!("{}.wav", field("voice"))).is_file() {
        return Ok(respond(
            400,
            "application/json",
            br#"{"error":{"code":"invalid_voice"}}"#,
        ));
    }
    if field("consent_attestation").is_empty() {
        return Ok(respond(
            400,
            "application/json",
            br#"{"error":{"code":"consent_required"}}"#,
        ));
    }
    if input == "block" {
        announce_and_block()?;
    }
    if nth("crash-on:") == Some(n) {
        std::process::abort();
    }
    if nth("reject-on:") == Some(n) {
        return Ok(respond(
            500,
            "application/json",
            br#"{"error":{"message":"secret","code":"synthesis_failed"}}"#,
        ));
    }
    kept.push(allocate());
    std::hint::black_box(&kept);
    let mut wav = std::io::Cursor::new(Vec::new());
    vc_audio::encode_wav(&vc_backend::fake::fake_pcm(&input), &mut wav)
        .map_err(|e| e.to_string())?;
    Ok(respond(200, "audio/wav", wav.get_ref()))
}

type Request = (String, String, Vec<(String, String)>, Vec<u8>);

fn read_request(stream: &mut std::net::TcpStream) -> std::io::Result<Request> {
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut line = String::new();
    reader.read_line(&mut line)?;
    let mut parts = line.split_whitespace();
    let method = parts.next().unwrap_or_default().to_owned();
    let path = parts.next().unwrap_or_default().to_owned();
    let mut headers = Vec::new();
    loop {
        line.clear();
        reader.read_line(&mut line)?;
        let l = line.trim_end();
        if l.is_empty() {
            break;
        }
        if let Some((k, v)) = l.split_once(':') {
            headers.push((k.trim().to_owned(), v.trim().to_owned()));
        }
    }
    let len = headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case("content-length"))
        .and_then(|(_, v)| v.parse::<usize>().ok())
        .unwrap_or(0);
    let mut body = vec![0u8; len];
    reader.read_exact(&mut body)?;
    Ok((method, path, headers, body))
}

fn respond(status: u16, content_type: &str, body: &[u8]) -> Vec<u8> {
    let mut out = format!(
        "HTTP/1.1 {status} X\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    )
    .into_bytes();
    out.extend_from_slice(body);
    out
}
