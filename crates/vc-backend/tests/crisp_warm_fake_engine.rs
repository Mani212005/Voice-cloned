//! `CrispWarmBackend` driven against `fake-engine --server`: warm reuse, the restart
//! threshold, voice changes, warm-up, crash, rejection, cancel-by-kill, the RSS budget
//! and release. Every wait is on a pipe, a process or a future; nothing sleeps.

#![allow(clippy::unwrap_used)]

use std::path::{Path, PathBuf};
use std::time::Duration;

use tokio_util::sync::CancellationToken;
use vc_backend::{
    Backend, BackendError, ChunkRequest, CrispWarmBackend, ExitInfo, RecipeManifest, RuntimePin,
    VoiceRef, WarmOptions, fake::fake_pcm,
};
use vc_core::Language;

struct Fixture {
    dir: tempfile::TempDir,
}

impl Fixture {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("work")).unwrap();
        for name in ["ref.wav", "other.wav"] {
            vc_audio::encode_wav(
                &fake_pcm(name),
                std::fs::File::create(dir.path().join(name)).unwrap(),
            )
            .unwrap();
        }
        Self { dir }
    }

    fn path(&self, p: &str) -> PathBuf {
        self.dir.path().join(p)
    }

    fn record(&self) -> String {
        self.path("record.txt").to_str().unwrap().to_owned()
    }

    /// A backend whose engine records into `record.txt`, with optional budget and
    /// headroom overrides.
    fn backend(&self, env: &[(&str, &str)], budget: Option<(u64, u64)>) -> CrispWarmBackend {
        self.backend_with_start_timeout(env, budget, Duration::from_secs(60))
    }

    fn backend_with_start_timeout(
        &self,
        env: &[(&str, &str)],
        budget: Option<(u64, u64)>,
        start_timeout: Duration,
    ) -> CrispWarmBackend {
        let mut manifest = RecipeManifest::turbo2();
        if let Some((budget_kib, headroom_kib)) = budget {
            manifest.budget.ram_budget_kib = budget_kib;
            manifest.budget.restart_headroom_kib = headroom_kib;
        }
        let mut extra_env = vec![("FAKE_ENGINE_RECORD".to_owned(), self.record())];
        extra_env.extend(env.iter().map(|(k, v)| ((*k).to_owned(), (*v).to_owned())));
        CrispWarmBackend::new(
            manifest,
            &RuntimePin::crispasr(),
            WarmOptions {
                runtime_bin: env!("CARGO_BIN_EXE_fake-engine").into(),
                model_dir: self.path("models"),
                engine_dir: self.path("engine"),
                threads: None,
                nice: 0,
                start_timeout,
                extra_env,
            },
        )
    }

    fn req(&self, text: &str, language: Language, voice: &str) -> ChunkRequest {
        ChunkRequest {
            idx: 0,
            text: text.into(),
            language,
            reference: self.path(voice),
            reference_language: Language::En,
            work_dir: self.path("work"),
        }
    }

    /// Engine pids in start order.
    /// Engines that came up (bound their port), in order. A spawn that lost its port to
    /// a parallel test before binding exits and is retried on a new port, as designed;
    /// counting it would make the restart assertions depend on scheduling.
    fn starts(&self) -> Vec<i32> {
        self.lines("LISTEN ")
            .iter()
            .map(|l| l.parse().unwrap())
            .collect()
    }

    /// Every engine process spawned, including ones that exited before listening.
    fn launches(&self) -> Vec<i32> {
        self.lines("START ")
            .iter()
            .map(|l| l.parse().unwrap())
            .collect()
    }

    /// Speech request bodies in order, across all engines.
    fn requests(&self) -> Vec<serde_json::Value> {
        self.lines("REQ ")
            .iter()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect()
    }

    fn lines(&self, prefix: &str) -> Vec<String> {
        std::fs::read_to_string(self.path("record.txt"))
            .unwrap_or_default()
            .lines()
            .filter_map(|l| l.strip_prefix(prefix).map(str::to_owned))
            .collect()
    }
}

async fn speak(
    b: &CrispWarmBackend,
    req: &ChunkRequest,
) -> Result<vc_backend::ChunkOutput, BackendError> {
    b.synthesize_chunk(req, &CancellationToken::new()).await
}

fn mkfifo(path: &Path) {
    let c = std::ffi::CString::new(path.to_str().unwrap()).unwrap();
    // SAFETY: valid C string.
    assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o600) }, 0);
}

/// The process is gone (reaped), or the pid belongs to something else now.
#[cfg(target_os = "linux")]
fn gone(pid: i32) -> bool {
    let cmdline = std::fs::read(format!("/proc/{pid}/cmdline")).unwrap_or_default();
    !String::from_utf8_lossy(&cmdline).contains("fake-engine")
}

/// No `/proc` outside Linux: the engine is reaped by its backend, so a signal-0 probe
/// finds no process once it is gone.
#[cfg(not(target_os = "linux"))]
fn gone(pid: i32) -> bool {
    // SAFETY: signal 0 only checks that the process exists.
    unsafe { libc::kill(pid, 0) != 0 }
}

#[tokio::test]
async fn one_warm_engine_serves_every_chunk_of_a_voice() {
    let f = Fixture::new();
    let b = f.backend(&[], None);
    for (text, lang) in [
        ("hi there", Language::En),
        ("namaste duniya", Language::Hi),
        ("bye now", Language::En),
    ] {
        let out = speak(&b, &f.req(text, lang, "ref.wav")).await.unwrap();
        assert_eq!(out.pcm, fake_pcm(text));
        assert!(out.stats.peak_rss_kib > 0);
    }
    assert_eq!(f.starts().len(), 1, "one engine for all three chunks");

    let reqs = f.requests();
    assert_eq!(reqs.len(), 3);
    assert_eq!(reqs[0]["input"], "hi there");
    assert_eq!(reqs[0]["voice"], "voice");
    assert_eq!(reqs[0]["language"], "en");
    assert_eq!(reqs[0]["response_format"], "wav");
    assert_eq!(reqs[0]["spoken_disclaimer"], false);
    assert!(reqs[0].get("source_lang").is_none());
    assert!(!reqs[0]["consent_attestation"].as_str().unwrap().is_empty());
    // Hindi from an English reference: the CLI's `-sl en`.
    assert_eq!(reqs[1]["language"], "hi");
    assert_eq!(reqs[1]["source_lang"], "en");

    let args = f.lines("ARG ");
    let after = |flag: &str| args[args.iter().position(|a| a == flag).unwrap() + 1].clone();
    assert!(args.iter().any(|a| a == "--server"));
    assert!(args.iter().any(|a| a == "--no-punctuation"));
    assert_eq!(after("--host"), "127.0.0.1");
    assert_eq!(after("--seed"), "0");
    assert_eq!(after("--tts-steps"), "2");
    let envs = f.lines("ENV ");
    let names: Vec<&str> = envs.iter().map(|e| e.split('=').next().unwrap()).collect();
    assert_eq!(
        names,
        [
            "CRISPASR_API_KEYS",
            "CRISPASR_S3GEN_FASTCONV",
            "FAKE_ENGINE_RECORD",
            "HOME",
            "MALLOC_ARENA_MAX",
            "MALLOC_MMAP_THRESHOLD_",
            "MALLOC_TRIM_THRESHOLD_",
            "TMPDIR",
            "XDG_CACHE_HOME",
        ],
        "the engine sees only the manifest environment, isolation and its key"
    );
    let key = envs
        .iter()
        .find_map(|e| e.strip_prefix("CRISPASR_API_KEYS="))
        .unwrap();
    assert_eq!(key.len(), 64);
    assert!(
        !args.iter().any(|a| a.contains(key)),
        "the key is never in argv"
    );

    // The engine's directory is private and links (does not copy) the reference.
    use std::os::unix::fs::PermissionsExt;
    let mode = std::fs::metadata(f.path("engine"))
        .unwrap()
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(mode, 0o700);
    assert_eq!(
        std::fs::read_link(f.path("engine/voices/voice.wav")).unwrap(),
        f.path("ref.wav")
    );
}

#[tokio::test]
async fn the_engine_is_restarted_before_a_chunk_that_could_cross_the_budget() {
    let f = Fixture::new();
    // Every request keeps 32 MiB (33,554 KiB) more resident. Restart above
    // 160,000 - 40,000 = 120,000 KiB: after three requests the peak is at most about
    // 111,000 KiB (reuse); after four it is at least about 134,000 KiB (restart).
    let b = f.backend(&[("FAKE_ENGINE_ALLOC_MB", "32")], Some((160_000, 40_000)));
    let mut peaks = Vec::new();
    for i in 0..6 {
        let text = format!("chunk {i}");
        let out = speak(&b, &f.req(&text, Language::En, "ref.wav"))
            .await
            .unwrap();
        assert_eq!(out.pcm, fake_pcm(&text));
        peaks.push(out.stats.peak_rss_kib);
    }
    let starts = f.starts();
    assert_eq!(starts.len(), 2, "peaks {peaks:?}");
    assert!(gone(starts[0]), "the first engine was stopped and reaped");
    assert!(peaks.iter().all(|&p| p <= 160_000), "peaks {peaks:?}");
    assert!(peaks[3] > 120_000 && peaks[4] < peaks[3], "peaks {peaks:?}");
}

#[tokio::test]
async fn another_voice_restarts_the_engine() {
    let f = Fixture::new();
    let b = f.backend(&[], None);
    speak(&b, &f.req("a", Language::En, "ref.wav"))
        .await
        .unwrap();
    speak(&b, &f.req("b", Language::En, "other.wav"))
        .await
        .unwrap();
    speak(&b, &f.req("c", Language::En, "other.wav"))
        .await
        .unwrap();
    let starts = f.starts();
    assert_eq!(starts.len(), 2);
    assert!(gone(starts[0]));
    assert_eq!(
        std::fs::read_link(f.path("engine/voices/voice.wav")).unwrap(),
        f.path("other.wav")
    );
}

#[tokio::test]
async fn prepare_encodes_the_voice_once_and_the_next_chunk_reuses_the_engine() {
    let f = Fixture::new();
    let b = f.backend(&[], None);
    let voice = VoiceRef {
        reference: f.path("ref.wav"),
        reference_language: Language::Hi,
    };
    let c = CancellationToken::new();
    b.prepare(&voice, &c).await.unwrap();
    b.prepare(&voice, &c).await.unwrap();
    let reqs = f.requests();
    assert_eq!(reqs.len(), 1, "a warm engine needs no second warm-up");
    assert_eq!(reqs[0]["input"], "Namaste.");
    assert_eq!(reqs[0]["language"], "hi");
    assert!(reqs[0].get("source_lang").is_none());

    speak(&b, &f.req("real chunk", Language::En, "ref.wav"))
        .await
        .unwrap();
    assert_eq!(f.starts().len(), 1);
    assert_eq!(f.requests().len(), 2);

    // Preparing another voice replaces the engine.
    let other = VoiceRef {
        reference: f.path("other.wav"),
        reference_language: Language::En,
    };
    b.prepare(&other, &c).await.unwrap();
    assert_eq!(f.starts().len(), 2);
    assert_eq!(f.requests()[2]["input"], "Hello.");
}

#[tokio::test]
async fn a_crashed_engine_is_reported_and_replaced() {
    let f = Fixture::new();
    let b = f.backend(&[("FAKE_ENGINE_MODE", "crash-on:2")], None);
    speak(&b, &f.req("one", Language::En, "ref.wav"))
        .await
        .unwrap();
    let err = speak(&b, &f.req("two", Language::En, "ref.wav"))
        .await
        .unwrap_err();
    assert!(
        matches!(
            err,
            BackendError::EngineCrashed {
                exit: ExitInfo::Signal(libc::SIGABRT),
                ..
            }
        ),
        "{err:?}"
    );
    let first = f.starts()[0];
    assert!(gone(first));
    let out = speak(&b, &f.req("three", Language::En, "ref.wav"))
        .await
        .unwrap();
    assert_eq!(out.pcm, fake_pcm("three"));
    assert_eq!(f.starts().len(), 2);
}

#[tokio::test]
async fn a_rejected_request_is_invalid_output_and_keeps_the_engine() {
    let f = Fixture::new();
    let b = f.backend(&[("FAKE_ENGINE_MODE", "reject-on:1")], None);
    let err = speak(&b, &f.req("one", Language::En, "ref.wav"))
        .await
        .unwrap_err();
    match err {
        BackendError::OutputInvalid { reason, .. } => {
            assert!(
                reason.contains("500") && reason.contains("synthesis_failed"),
                "{reason}"
            );
            assert!(
                !reason.contains("secret"),
                "the engine's message is not copied"
            );
        }
        other => panic!("expected invalid output, got {other:?}"),
    }
    speak(&b, &f.req("two", Language::En, "ref.wav"))
        .await
        .unwrap();
    assert_eq!(f.starts().len(), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancel_kills_and_reaps_the_warm_engine() {
    let f = Fixture::new();
    let started = f.path("started");
    let block = f.path("block");
    mkfifo(&started);
    mkfifo(&block);
    let b = std::sync::Arc::new(f.backend(
        &[
            ("FAKE_ENGINE_STARTED_FIFO", started.to_str().unwrap()),
            ("FAKE_ENGINE_BLOCK_FIFO", block.to_str().unwrap()),
        ],
        None,
    ));
    let cancel = CancellationToken::new();
    let req = f.req("block", Language::En, "ref.wav");
    let task = {
        let (b, cancel) = (b.clone(), cancel.clone());
        tokio::spawn(async move { b.synthesize_chunk(&req, &cancel).await })
    };
    let pid: i32 = tokio::task::spawn_blocking(move || std::fs::read_to_string(started))
        .await
        .unwrap()
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    cancel.cancel();
    assert_eq!(task.await.unwrap(), Err(BackendError::Cancelled));
    assert!(gone(pid));
    assert!(!f.path("engine").exists(), "the engine directory is wiped");
}

#[tokio::test]
async fn a_peak_over_the_budget_fails_the_chunk_and_stops_the_engine() {
    let f = Fixture::new();
    // 64 MiB resident against a 32 MB budget.
    let b = f.backend(&[("FAKE_ENGINE_ALLOC_MB", "64")], Some((32_000, 1_000)));
    let err = speak(&b, &f.req("x", Language::En, "ref.wav"))
        .await
        .unwrap_err();
    match err {
        BackendError::MemoryBudgetExceeded { budget_kib, stats } => {
            assert_eq!(budget_kib, 32_000);
            assert!(stats.peak_rss_kib > 64 * 1024, "{}", stats.peak_rss_kib);
        }
        other => panic!("expected budget failure, got {other:?}"),
    }
    assert!(gone(f.starts()[0]));
    assert!(!f.path("engine").exists());
}

#[tokio::test]
async fn release_stops_only_an_engine_holding_that_voice() {
    let f = Fixture::new();
    let b = f.backend(&[], None);
    speak(&b, &f.req("a", Language::En, "ref.wav"))
        .await
        .unwrap();
    let pid = f.starts()[0];
    b.release(Some(&f.path("other.wav"))).await;
    assert!(!gone(pid));
    b.release(Some(&f.path("ref.wav"))).await;
    assert!(gone(pid));
    assert!(!f.path("engine").exists());
    // Released means the next chunk starts cold again.
    speak(&b, &f.req("b", Language::En, "ref.wav"))
        .await
        .unwrap();
    assert_eq!(f.starts().len(), 2);
    b.release(None).await;
    assert!(gone(f.starts()[1]));
}

#[tokio::test]
async fn an_engine_that_exits_during_start_up_is_a_spawn_error() {
    let f = Fixture::new();
    let b = f.backend(&[("FAKE_ENGINE_MODE", "exit-at-start:3")], None);
    let err = speak(&b, &f.req("x", Language::En, "ref.wav"))
        .await
        .unwrap_err();
    assert!(
        matches!(err, BackendError::Spawn(ref m) if m.contains("exit code 3")),
        "{err:?}"
    );
    assert_eq!(f.launches().len(), 2, "one retry on a new port");
    assert!(f.starts().is_empty());
}

async fn started_pid(started: PathBuf) -> i32 {
    tokio::task::spawn_blocking(move || std::fs::read_to_string(started))
        .await
        .unwrap()
        .unwrap()
        .trim()
        .parse()
        .unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancel_while_the_engine_does_not_answer_its_health_check() {
    let f = Fixture::new();
    let started = f.path("started");
    mkfifo(&started);
    let b = std::sync::Arc::new(f.backend(
        &[
            ("FAKE_ENGINE_MODE", "silent-health"),
            ("FAKE_ENGINE_STARTED_FIFO", started.to_str().unwrap()),
        ],
        None,
    ));
    let cancel = CancellationToken::new();
    let req = f.req("hi", Language::En, "ref.wav");
    let task = {
        let (b, cancel) = (b.clone(), cancel.clone());
        tokio::spawn(async move { b.synthesize_chunk(&req, &cancel).await })
    };
    let pid = started_pid(started).await;
    cancel.cancel();
    assert_eq!(task.await.unwrap(), Err(BackendError::Cancelled));
    assert!(gone(pid));
    assert!(!f.path("engine").exists(), "the engine directory is wiped");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn start_timeout_applies_to_an_engine_that_does_not_answer_its_health_check() {
    let f = Fixture::new();
    let b = f.backend_with_start_timeout(
        &[("FAKE_ENGINE_MODE", "silent-health")],
        None,
        Duration::from_millis(300),
    );
    let err = speak(&b, &f.req("hi", Language::En, "ref.wav"))
        .await
        .unwrap_err();
    assert!(matches!(err, BackendError::Spawn(_)), "{err:?}");
    for pid in f.starts() {
        assert!(gone(pid));
    }
}
