//! The warm backend: one long-lived CrispASR engine (the pinned release binary in its
//! `--server` mode) that keeps the models loaded and the voice encoded across chunks and
//! jobs.
//!
//! A cold process per chunk spends about 6 s (idle laptop) to 15 s and more (under load)
//! loading the models and encoding the reference before it synthesizes anything. The warm
//! engine pays that once, ideally while vcd is idle ([`Backend::prepare`] runs a one-word
//! warm-up synthesis), so a job's first chunk starts synthesizing at once.
//!
//! - **Same audio.** The engine re-seeds T3 and S3Gen from `--seed` on every request and
//!   caches only the voice conditioning, so its PCM is bit-identical to the CLI's for the
//!   same chunk, also after many earlier requests (measured on English and Hindi).
//! - **RAM budget.** A warm engine's peak RSS only goes up; with the recipe's malloc
//!   settings it levels off at the largest chunk it has served, just above a cold
//!   process. [`WarmPolicy`] restarts it before any chunk that could take it over the
//!   budget, and a chunk whose engine still ends above the budget fails as before.
//! - **One engine, one voice.** Another voice restarts the engine, so conditioning of a
//!   deleted or unused voice never lingers. [`Backend::release`] stops it on demand.
//! - **Cancel** kills the engine's process group, as with the CLI; the next chunk or
//!   warm-up starts a fresh one. A crashed engine is reaped and replaced the same way.
//! - **Isolation.** The engine listens on a loopback port that is picked per start and
//!   requires a per-start random API key (passed in its environment, never in argv). Its
//!   HOME, cache, TMPDIR, voice directory, consent log and log file live in a private
//!   directory that is wiped whenever the engine stops or starts. Automatic punctuation
//!   restoration, which would download an unpinned model, is off.

use std::ffi::OsString;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use bytes::Bytes;
use http_body_util::{BodyExt, Full, Limited};
use tokio_util::sync::CancellationToken;
use vc_core::Language;

use crate::crisp_cli::{backend_info, capabilities, engine_env, model_path};
use crate::manifest::{RecipeManifest, RuntimePin};
use crate::proc::{self, EngineChild, EngineCommand, ExitInfo, ProcOutcome};
use crate::warm_policy::{EngineView, Plan, WarmPolicy};
use crate::{
    Backend, BackendError, BackendInfo, BoxFuture, Capabilities, ChunkOutput, ChunkRequest,
    ChunkStats, VoiceRef,
};

/// The engine's name for the voice it serves (`<engine dir>/voices/voice.wav`).
const VOICE_NAME: &str = "voice";
/// Recorded by the engine in its consent log for every request.
const CONSENT_ATTESTATION: &str = "vcd: synthesis under the consent record of this voice";
/// Largest response accepted from the engine (a chunk is well under 2 MB of WAV).
const MAX_RESPONSE_BYTES: usize = 64 << 20;
/// How often a starting engine is asked whether it is ready.
const READY_POLL: Duration = Duration::from_millis(25);
/// Longest wait for one health probe; an engine that accepts the connection but does not
/// answer is probed again until `start_timeout`.
const HEALTH_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug, Clone)]
pub struct WarmOptions {
    /// The `crispasr` binary.
    pub runtime_bin: PathBuf,
    /// Directory holding the manifest's model files.
    pub model_dir: PathBuf,
    /// Private directory for the engine (HOME, caches, voice link, logs). It is wiped
    /// whenever an engine starts or stops, so it must be dedicated to this backend.
    pub engine_dir: PathBuf,
    /// Overrides the manifest's thread count.
    pub threads: Option<u32>,
    /// Minimum niceness of the engine (the laptop is shared).
    pub nice: i32,
    /// Longest wait for a started engine to answer its health check.
    pub start_timeout: Duration,
    /// Extra environment for the engine. Only tests use this, to steer `fake-engine`.
    pub extra_env: Vec<(String, String)>,
}

#[derive(Debug)]
pub struct CrispWarmBackend {
    manifest: RecipeManifest,
    options: WarmOptions,
    capabilities: Capabilities,
    info: BackendInfo,
    policy: WarmPolicy,
    /// The one warm engine. Held for the whole of a chunk or warm-up, so two engines
    /// never exist at once.
    slot: tokio::sync::Mutex<Option<WarmEngine>>,
}

#[derive(Debug)]
struct WarmEngine {
    child: EngineChild,
    port: u16,
    api_key: String,
    /// The reference of the voice it serves.
    voice: PathBuf,
    /// The voice conditioning is computed (it has served at least one synthesis).
    voice_ready: bool,
    /// `VmHWM` after its last synthesis, in KiB.
    peak_rss_kib: u64,
}

/// Why a request to the engine did not produce audio.
#[derive(Debug)]
enum SpeakError {
    Cancelled,
    /// The engine died, or the connection to it broke.
    Engine(ExitInfo, ChunkStats),
    /// The engine answered, but not with audio.
    Rejected(String, ChunkStats),
}

impl CrispWarmBackend {
    pub fn new(manifest: RecipeManifest, pin: &RuntimePin, options: WarmOptions) -> Self {
        let threads = options.threads.unwrap_or(manifest.engine.threads);
        let capabilities = capabilities(&manifest);
        let info = backend_info(&manifest, pin, threads);
        let policy = WarmPolicy {
            budget_kib: manifest.budget.ram_budget_kib,
            headroom_kib: manifest.budget.restart_headroom_kib,
        };
        Self {
            manifest,
            options,
            capabilities,
            info,
            policy,
            slot: tokio::sync::Mutex::new(None),
        }
    }

    pub fn policy(&self) -> WarmPolicy {
        self.policy
    }

    /// The exact argv of a warm engine. No shell is involved.
    pub fn argv(&self, dir: &Path, port: u16) -> Vec<OsString> {
        let e = &self.manifest.engine;
        let model = |role| model_path(&self.manifest, &self.options.model_dir, role);
        let mut a: Vec<OsString> = Vec::with_capacity(32);
        let mut push = |s: &dyn AsRef<std::ffi::OsStr>| a.push(s.as_ref().to_owned());
        push(&"--backend");
        push(&e.backend);
        push(&"-m");
        push(&model("t3"));
        push(&"--codec-model");
        push(&model("s3gen"));
        push(&"-t");
        push(&self.info.threads.to_string());
        push(&"--seed");
        push(&e.seed.to_string());
        push(&"--tts-steps");
        push(&e.tts_steps.to_string());
        push(&"--i-have-rights");
        push(&"--consent-log");
        push(&dir.join("consent.jsonl"));
        push(&"--no-spoken-disclaimer");
        push(&"--accept-marking-responsibility");
        // The server would otherwise download a punctuation model; TTS does not use it.
        push(&"--no-punctuation");
        push(&"--voice-dir");
        push(&dir.join("voices"));
        push(&"--server");
        push(&"--host");
        push(&"127.0.0.1");
        push(&"--port");
        push(&port.to_string());
        a
    }

    /// The complete engine environment: the CLI's, plus the API key.
    pub fn env(&self, dir: &Path, api_key: &str) -> Vec<(OsString, OsString)> {
        let mut env = engine_env(&self.manifest, &dir.join("home"), &[]);
        env.push(("CRISPASR_API_KEYS".into(), api_key.into()));
        env.extend(
            self.options
                .extra_env
                .iter()
                .map(|(k, v)| (k.into(), v.into())),
        );
        env
    }

    /// Wait for the slot, giving up when `cancel` fires (the holder may be a long start-up
    /// or warm-up).
    async fn lock_slot(
        &self,
        cancel: &CancellationToken,
    ) -> Result<tokio::sync::MutexGuard<'_, Option<WarmEngine>>, BackendError> {
        tokio::select! {
            guard = self.slot.lock() => Ok(guard),
            () = cancel.cancelled() => Err(BackendError::Cancelled),
        }
    }

    async fn run_chunk(
        &self,
        req: &ChunkRequest,
        cancel: &CancellationToken,
    ) -> Result<ChunkOutput, BackendError> {
        if cancel.is_cancelled() {
            return Err(BackendError::Cancelled);
        }
        let mut slot = self.lock_slot(cancel).await?;
        let started = Instant::now();
        self.ensure(&mut slot, &req.reference, cancel).await?;
        let wav = self
            .speak(
                &mut slot,
                &req.text,
                req.language,
                req.reference_language,
                cancel,
                started,
            )
            .await
            .map_err(|e| e.into_backend_error())?;
        let stats = ChunkStats {
            wall: started.elapsed(),
            peak_rss_kib: slot.as_ref().map_or(0, |e| e.peak_rss_kib),
        };
        let budget_kib = self.manifest.budget.ram_budget_kib;
        if stats.peak_rss_kib > budget_kib {
            self.stop(&mut slot).await;
            return Err(BackendError::MemoryBudgetExceeded { budget_kib, stats });
        }
        let pcm = vc_audio::decode_engine_output(&wav, self.manifest.engine.sample_rate_hz)
            .map_err(|e| BackendError::OutputInvalid {
                reason: e.to_string(),
                stats: stats.clone(),
            })?;
        Ok(ChunkOutput { pcm, stats })
    }

    async fn run_prepare(
        &self,
        voice: &VoiceRef,
        cancel: &CancellationToken,
    ) -> Result<(), BackendError> {
        if cancel.is_cancelled() {
            return Err(BackendError::Cancelled);
        }
        let mut slot = self.lock_slot(cancel).await?;
        let started = Instant::now();
        self.ensure(&mut slot, &voice.reference, cancel).await?;
        if slot.as_ref().is_some_and(|e| e.voice_ready) {
            return Ok(());
        }
        let lang = voice.reference_language;
        self.speak(&mut slot, warmup_text(lang), lang, lang, cancel, started)
            .await
            .map_err(|e| e.into_backend_error())?;
        let peak_rss_kib = slot.as_ref().map_or(0, |e| e.peak_rss_kib);
        tracing::info!(
            wall_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
            peak_rss_kib,
            "warm engine ready"
        );
        Ok(())
    }

    /// Make the slot hold a live engine for `voice` that may take another chunk.
    async fn ensure(
        &self,
        slot: &mut Option<WarmEngine>,
        voice: &Path,
        cancel: &CancellationToken,
    ) -> Result<(), BackendError> {
        if let Some(e) = slot.as_mut() {
            match e.child.try_reap() {
                Ok(None) => {}
                Ok(Some(outcome)) => {
                    tracing::warn!(exit = %outcome.exit, "warm engine had exited; starting a new one");
                    self.forget(slot);
                }
                Err(err) => {
                    tracing::warn!("warm engine status unknown ({err}); restarting it");
                    self.stop(slot).await;
                }
            }
        }
        let view = slot.as_ref().map(|e| EngineView {
            voice: &e.voice,
            peak_rss_kib: e.peak_rss_kib,
        });
        match self.policy.plan(view, voice) {
            Plan::Reuse => return Ok(()),
            Plan::Start => {}
            Plan::Restart(reason) => {
                tracing::info!(%reason, "restarting the warm engine");
                self.stop(slot).await;
            }
        }
        *slot = Some(self.start(voice, cancel).await?);
        Ok(())
    }

    /// Start an engine for `voice` and wait until it answers. A start whose engine exits
    /// before it is ready (for example because its port was taken in the meantime) is
    /// tried once more on a new port.
    async fn start(
        &self,
        voice: &Path,
        cancel: &CancellationToken,
    ) -> Result<WarmEngine, BackendError> {
        let mut last = String::new();
        for _attempt in 0..2 {
            match self.start_once(voice, cancel).await? {
                Ok(engine) => return Ok(engine),
                Err(why) => {
                    tracing::warn!("warm engine did not start: {why}");
                    last = why;
                }
            }
        }
        Err(BackendError::Spawn(last))
    }

    async fn start_once(
        &self,
        voice: &Path,
        cancel: &CancellationToken,
    ) -> Result<Result<WarmEngine, String>, BackendError> {
        let dir = &self.options.engine_dir;
        let spawn_err = |e: std::io::Error| BackendError::Spawn(format!("engine dir: {e}"));
        remove_tree(dir).map_err(spawn_err)?;
        for d in [
            dir.to_owned(),
            dir.join("home/cache"),
            dir.join("home/tmp"),
            dir.join("voices"),
        ] {
            private_dir(&d).map_err(spawn_err)?;
        }
        // A link, not a copy: deleting the voice leaves nothing behind here.
        std::os::unix::fs::symlink(voice, dir.join("voices").join(format!("{VOICE_NAME}.wav")))
            .map_err(spawn_err)?;
        let port = free_loopback_port().map_err(spawn_err)?;
        let api_key = random_key().map_err(spawn_err)?;
        let cmd = EngineCommand {
            program: self.options.runtime_bin.clone(),
            args: self.argv(dir, port),
            env: self.env(dir, &api_key),
            cwd: dir.clone(),
            log: dir.join("engine.log"),
            nice: self.options.nice,
        };
        let child = proc::spawn(&cmd).map_err(|e| BackendError::Spawn(e.to_string()))?;
        let mut engine = WarmEngine {
            child,
            port,
            api_key,
            voice: voice.to_owned(),
            voice_ready: false,
            peak_rss_kib: 0,
        };
        let started = Instant::now();
        tracing::info!(pid = engine.child.pid(), port, "starting the warm engine");
        loop {
            match engine.child.try_reap() {
                Ok(None) => {}
                Ok(Some(outcome)) => {
                    return Ok(Err(format!(
                        "engine exited during start-up: {}",
                        outcome.exit
                    )));
                }
                Err(e) => {
                    let _ = engine.child.kill_and_reap().await;
                    return Ok(Err(format!("engine status: {e}")));
                }
            }
            let wait = self
                .options
                .start_timeout
                .saturating_sub(started.elapsed())
                .min(HEALTH_TIMEOUT);
            let probe = tokio::select! {
                r = tokio::time::timeout(wait, request(&engine, "GET", "/health", None)) => r,
                () = cancel.cancelled() => {
                    let _ = engine.child.kill_and_reap().await;
                    let _ = remove_tree(dir);
                    return Err(BackendError::Cancelled);
                }
            };
            if let Ok(Ok((200, _))) = probe {
                engine.peak_rss_kib = engine.child.peak_rss_kib().unwrap_or(0);
                tracing::info!(
                    pid = engine.child.pid(),
                    ready_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
                    "warm engine listening"
                );
                return Ok(Ok(engine));
            }
            if started.elapsed() >= self.options.start_timeout {
                let _ = engine.child.kill_and_reap().await;
                return Ok(Err(format!(
                    "engine not ready after {} s",
                    self.options.start_timeout.as_secs()
                )));
            }
            tokio::select! {
                () = tokio::time::sleep(READY_POLL) => {}
                () = cancel.cancelled() => {
                    let _ = engine.child.kill_and_reap().await;
                    let _ = remove_tree(dir);
                    return Err(BackendError::Cancelled);
                }
            }
        }
    }

    /// One synthesis on the engine in `slot`. Any failure other than a rejected request
    /// leaves the slot empty with the engine reaped.
    async fn speak(
        &self,
        slot: &mut Option<WarmEngine>,
        text: &str,
        language: Language,
        reference_language: Language,
        cancel: &CancellationToken,
        started: Instant,
    ) -> Result<Vec<u8>, SpeakError> {
        let Some(engine) = slot.as_mut() else {
            return Err(SpeakError::Engine(
                ExitInfo::Signal(libc::SIGKILL),
                ChunkStats {
                    wall: started.elapsed(),
                    peak_rss_kib: 0,
                },
            ));
        };
        let mut body = serde_json::json!({
            "model": self.manifest.engine.backend,
            "input": text,
            "voice": VOICE_NAME,
            "language": language.code(),
            "response_format": "wav",
            "spoken_disclaimer": false,
            "consent_attestation": CONSENT_ATTESTATION,
        });
        if language != reference_language {
            // Cross-lingual, exactly as the CLI's `-sl`.
            body["source_lang"] = reference_language.code().into();
        }
        let body = body.to_string().into_bytes();
        let answer = tokio::select! {
            r = request(engine, "POST", "/v1/audio/speech", Some(body)) => r,
            () = cancel.cancelled() => {
                self.stop(slot).await;
                return Err(SpeakError::Cancelled);
            }
        };
        let stats = |peak_rss_kib| ChunkStats {
            wall: started.elapsed(),
            peak_rss_kib,
        };
        match answer {
            Ok((200, wav)) => match engine.child.peak_rss_kib() {
                Ok(peak) => {
                    engine.peak_rss_kib = peak;
                    engine.voice_ready = true;
                    Ok(wav.to_vec())
                }
                Err(e) => {
                    tracing::warn!("cannot read the warm engine's RSS ({e})");
                    let outcome = self.take_and_reap(slot).await;
                    Err(SpeakError::Engine(
                        outcome.exit,
                        stats(outcome.peak_rss_kib),
                    ))
                }
            },
            Ok((status, body)) => {
                let peak = engine.child.peak_rss_kib().unwrap_or(engine.peak_rss_kib);
                Err(SpeakError::Rejected(
                    format!("engine answered HTTP {status} ({})", error_code(&body)),
                    stats(peak),
                ))
            }
            Err(e) => {
                tracing::warn!("warm engine request failed: {e}");
                let outcome = self.take_and_reap(slot).await;
                Err(SpeakError::Engine(
                    outcome.exit,
                    stats(outcome.peak_rss_kib),
                ))
            }
        }
    }

    /// Reap the engine in `slot` (killing it if it is still alive) and empty the slot.
    async fn take_and_reap(&self, slot: &mut Option<WarmEngine>) -> ProcOutcome {
        let Some(mut engine) = slot.take() else {
            return ProcOutcome {
                exit: ExitInfo::Signal(libc::SIGKILL),
                peak_rss_kib: 0,
                wall: Duration::ZERO,
            };
        };
        let reaped = match engine.child.try_reap() {
            Ok(Some(outcome)) => Some(outcome),
            Ok(None) | Err(_) => None,
        };
        let outcome = match reaped {
            Some(o) => o,
            None => engine.child.kill_and_reap().await.unwrap_or(ProcOutcome {
                exit: ExitInfo::Signal(libc::SIGKILL),
                peak_rss_kib: 0,
                wall: Duration::ZERO,
            }),
        };
        let _ = remove_tree(&self.options.engine_dir);
        outcome
    }

    /// Stop the engine in `slot`, if any, and wipe its directory.
    async fn stop(&self, slot: &mut Option<WarmEngine>) {
        if slot.is_some() {
            let outcome = self.take_and_reap(slot).await;
            tracing::info!(peak_rss_kib = outcome.peak_rss_kib, "warm engine stopped");
        }
    }

    /// Drop an engine that has already been reaped.
    fn forget(&self, slot: &mut Option<WarmEngine>) {
        *slot = None;
        let _ = remove_tree(&self.options.engine_dir);
    }

    async fn run_release(&self, reference: Option<&Path>) {
        let mut slot = self.slot.lock().await;
        let serves = slot
            .as_ref()
            .is_some_and(|e| reference.is_none_or(|r| e.voice == r));
        if serves {
            self.stop(&mut slot).await;
        }
    }
}

impl SpeakError {
    fn into_backend_error(self) -> BackendError {
        match self {
            SpeakError::Cancelled => BackendError::Cancelled,
            SpeakError::Engine(exit, stats) => BackendError::EngineCrashed { exit, stats },
            SpeakError::Rejected(reason, stats) => BackendError::OutputInvalid { reason, stats },
        }
    }
}

impl Backend for CrispWarmBackend {
    fn capabilities(&self) -> &Capabilities {
        &self.capabilities
    }

    fn info(&self) -> &BackendInfo {
        &self.info
    }

    fn synthesize_chunk<'a>(
        &'a self,
        req: &'a ChunkRequest,
        cancel: &'a CancellationToken,
    ) -> BoxFuture<'a, Result<ChunkOutput, BackendError>> {
        Box::pin(self.run_chunk(req, cancel))
    }

    fn prepare<'a>(
        &'a self,
        voice: &'a VoiceRef,
        cancel: &'a CancellationToken,
    ) -> BoxFuture<'a, Result<(), BackendError>> {
        Box::pin(self.run_prepare(voice, cancel))
    }

    fn release<'a>(&'a self, reference: Option<&'a Path>) -> BoxFuture<'a, ()> {
        Box::pin(self.run_release(reference))
    }
}

/// The warm-up synthesis: one word in the reference's language. Its only purpose is to
/// make the engine encode the voice; the audio is discarded.
pub fn warmup_text(language: Language) -> &'static str {
    match language {
        Language::En => "Hello.",
        Language::Hi => "Namaste.",
    }
}

/// One HTTP/1.1 exchange with the engine. Returns the status and the body.
async fn request(
    engine: &WarmEngine,
    method: &str,
    path: &str,
    body: Option<Vec<u8>>,
) -> Result<(u16, Bytes), String> {
    let stream = tokio::net::TcpStream::connect(("127.0.0.1", engine.port))
        .await
        .map_err(|e| format!("connect: {e}"))?;
    let (mut sender, conn) =
        hyper::client::conn::http1::handshake(hyper_util::rt::TokioIo::new(stream))
            .await
            .map_err(|e| format!("handshake: {e}"))?;
    // Drives the connection; it ends when the exchange is done or the engine goes away.
    tokio::spawn(conn);
    let mut req = hyper::Request::builder()
        .method(method)
        .uri(path)
        .header(hyper::header::HOST, format!("127.0.0.1:{}", engine.port))
        .header(
            hyper::header::AUTHORIZATION,
            format!("Bearer {}", engine.api_key),
        );
    if body.is_some() {
        req = req.header(hyper::header::CONTENT_TYPE, "application/json");
    }
    let req = req
        .body(Full::new(Bytes::from(body.unwrap_or_default())))
        .map_err(|e| format!("request: {e}"))?;
    let res = sender
        .send_request(req)
        .await
        .map_err(|e| format!("send: {e}"))?;
    let status = res.status().as_u16();
    let bytes = Limited::new(res.into_body(), MAX_RESPONSE_BYTES)
        .collect()
        .await
        .map_err(|e| format!("body: {e}"))?
        .to_bytes();
    Ok((status, bytes))
}

/// The engine's machine-readable error code, never its message.
fn error_code(body: &[u8]) -> String {
    serde_json::from_slice::<serde_json::Value>(body)
        .ok()
        .and_then(|v| {
            let e = v.get("error")?;
            e.get("code")
                .or_else(|| e.get("type"))
                .and_then(|c| c.as_str())
                .map(str::to_owned)
        })
        .unwrap_or_else(|| "no error code".into())
}

/// A loopback port that is free right now. The engine binds it a moment later; if
/// something else takes it first, the engine exits and the start is retried.
fn free_loopback_port() -> std::io::Result<u16> {
    Ok(std::net::TcpListener::bind(("127.0.0.1", 0))?
        .local_addr()?
        .port())
}

/// 256 random bits, hex.
fn random_key() -> std::io::Result<String> {
    let mut buf = [0u8; 32];
    std::fs::File::open("/dev/urandom")?.read_exact(&mut buf)?;
    Ok(hex::encode(buf))
}

fn private_dir(dir: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)?;
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))
}

fn remove_tree(dir: &Path) -> std::io::Result<()> {
    match std::fs::remove_dir_all(dir) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e),
        _ => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn backend() -> CrispWarmBackend {
        CrispWarmBackend::new(
            RecipeManifest::turbo2(),
            &RuntimePin::crispasr(),
            WarmOptions {
                runtime_bin: "/opt/crispasr".into(),
                model_dir: "/m".into(),
                engine_dir: "/e".into(),
                threads: None,
                nice: 10,
                start_timeout: Duration::from_secs(1),
                extra_env: vec![],
            },
        )
    }

    fn strings(v: Vec<OsString>) -> Vec<String> {
        v.into_iter().map(|s| s.into_string().unwrap()).collect()
    }

    /// A caller queued behind the slot's holder gives up as soon as its own token fires,
    /// instead of waiting out a long start-up or warm-up.
    #[tokio::test]
    async fn waiting_for_the_slot_honors_cancel() {
        let b = backend();
        let voice = VoiceRef {
            reference: "/ref.wav".into(),
            reference_language: Language::En,
        };
        let req = ChunkRequest {
            idx: 0,
            text: "hi".into(),
            language: Language::En,
            reference: "/ref.wav".into(),
            reference_language: Language::En,
            work_dir: "/w".into(),
        };
        let held = b.slot.lock().await;

        let cancel = CancellationToken::new();
        let chunk = b.run_chunk(&req, &cancel);
        tokio::pin!(chunk);
        tokio::select! {
            biased;
            _ = &mut chunk => panic!("must wait for the slot"),
            () = async {} => {}
        }
        cancel.cancel();
        assert_eq!(chunk.await.unwrap_err(), BackendError::Cancelled);

        let cancel = CancellationToken::new();
        let prepare = b.run_prepare(&voice, &cancel);
        tokio::pin!(prepare);
        tokio::select! {
            biased;
            _ = &mut prepare => panic!("must wait for the slot"),
            () = async {} => {}
        }
        cancel.cancel();
        assert_eq!(prepare.await.unwrap_err(), BackendError::Cancelled);
        drop(held);
    }

    #[test]
    fn argv_is_the_turbo2_recipe_as_a_server() {
        let argv = strings(backend().argv(Path::new("/e"), 4321));
        assert_eq!(
            argv,
            [
                "--backend",
                "chatterbox",
                "-m",
                "/m/chatterbox-v3-t3-q4_k.gguf",
                "--codec-model",
                "/m/chatterbox-turbo-s3gen-q4_k-fd-q8_0.gguf",
                "-t",
                "6",
                "--seed",
                "0",
                "--tts-steps",
                "2",
                "--i-have-rights",
                "--consent-log",
                "/e/consent.jsonl",
                "--no-spoken-disclaimer",
                "--accept-marking-responsibility",
                "--no-punctuation",
                "--voice-dir",
                "/e/voices",
                "--server",
                "--host",
                "127.0.0.1",
                "--port",
                "4321",
            ]
        );
    }

    #[test]
    fn env_is_isolated_and_carries_the_key() {
        let env: Vec<(String, String)> = backend()
            .env(Path::new("/e"), "k")
            .into_iter()
            .map(|(k, v)| (k.into_string().unwrap(), v.into_string().unwrap()))
            .collect();
        assert_eq!(
            env,
            [
                ("CRISPASR_S3GEN_FASTCONV".into(), "0".into()),
                ("MALLOC_ARENA_MAX".into(), "1".into()),
                ("MALLOC_MMAP_THRESHOLD_".into(), "65536".into()),
                ("MALLOC_TRIM_THRESHOLD_".into(), "0".into()),
                ("HOME".into(), "/e/home".into()),
                ("XDG_CACHE_HOME".into(), "/e/home/cache".into()),
                ("TMPDIR".into(), "/e/home/tmp".into()),
                ("CRISPASR_API_KEYS".into(), "k".into()),
            ]
        );
    }

    #[test]
    fn policy_comes_from_the_manifest_budget() {
        let p = backend().policy();
        let budget = RecipeManifest::turbo2().budget;
        assert_eq!(p.budget_kib, budget.ram_budget_kib);
        assert_eq!(
            p.restart_above_kib(),
            budget.ram_budget_kib - budget.restart_headroom_kib
        );
        if cfg!(not(target_os = "macos")) {
            assert_eq!(p.budget_kib, 1_100_000);
            assert_eq!(p.restart_above_kib(), 1_000_000);
        }
    }

    #[test]
    fn error_codes_are_read_without_the_message() {
        assert_eq!(
            error_code(br#"{"error":{"message":"secret text","code":"invalid_voice"}}"#),
            "invalid_voice"
        );
        assert_eq!(error_code(b"not json"), "no error code");
    }

    #[test]
    fn keys_are_random_and_long() {
        let (a, b) = (random_key().unwrap(), random_key().unwrap());
        assert_eq!(a.len(), 64);
        assert_ne!(a, b);
    }
}
