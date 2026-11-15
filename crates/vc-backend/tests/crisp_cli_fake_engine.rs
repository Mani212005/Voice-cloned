//! `CrispCliBackend` driven against the `fake-engine` binary: argv and environment
//! contract, success, crash, signal, RSS budget, malformed output and cancel-by-kill.

#![allow(clippy::unwrap_used)]

use std::path::{Path, PathBuf};

use tokio_util::sync::CancellationToken;
use vc_backend::{
    Backend, BackendError, ChunkRequest, CrispCliBackend, CrispOptions, ExitInfo, RecipeManifest,
    RuntimePin, fake::fake_pcm,
};
use vc_core::Language;

struct Fixture {
    dir: tempfile::TempDir,
}

impl Fixture {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("work")).unwrap();
        vc_audio::encode_wav(
            &fake_pcm("reference"),
            std::fs::File::create(dir.path().join("ref.wav")).unwrap(),
        )
        .unwrap();
        Self { dir }
    }

    fn path(&self, p: &str) -> PathBuf {
        self.dir.path().join(p)
    }

    fn backend(&self, env: &[(&str, &str)], budget_kib: Option<u64>) -> CrispCliBackend {
        let mut manifest = RecipeManifest::turbo2();
        if let Some(b) = budget_kib {
            manifest.budget.ram_budget_kib = b;
        }
        CrispCliBackend::new(
            manifest,
            &RuntimePin::crispasr(),
            CrispOptions {
                runtime_bin: env!("CARGO_BIN_EXE_fake-engine").into(),
                model_dir: self.path("models"),
                threads: None,
                nice: 0,
                spoken_disclaimer: false,
                extra_env: env
                    .iter()
                    .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
                    .collect(),
            },
        )
    }

    fn req(&self, text: &str, language: Language) -> ChunkRequest {
        ChunkRequest {
            idx: 7,
            text: text.into(),
            language,
            reference: self.path("ref.wav"),
            reference_language: Language::En,
            work_dir: self.path("work"),
        }
    }
}

fn mkfifo(path: &Path) {
    let c = std::ffi::CString::new(path.to_str().unwrap()).unwrap();
    // SAFETY: valid C string.
    assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o600) }, 0);
}

#[tokio::test]
async fn success_returns_engine_pcm_and_cleans_its_raw_output() {
    let f = Fixture::new();
    let record = f.path("record.txt");
    let b = f.backend(&[("FAKE_ENGINE_RECORD", record.to_str().unwrap())], None);
    let out = b
        .synthesize_chunk(
            &f.req("namaste duniya", Language::Hi),
            &CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(out.pcm, fake_pcm("namaste duniya"));
    assert!(out.stats.peak_rss_kib > 0);
    assert!(!f.path("work/chunk-0007.engine.wav").exists());
    assert!(f.path("work/chunk-0007.engine.log").exists());
    assert!(f.path("work/engine-home/tmp").is_dir());

    let rec = std::fs::read_to_string(record).unwrap();
    let args: Vec<&str> = rec.lines().filter_map(|l| l.strip_prefix("ARG ")).collect();
    let envs: Vec<&str> = rec.lines().filter_map(|l| l.strip_prefix("ENV ")).collect();
    let after = |flag: &str| args[args.iter().position(|a| *a == flag).unwrap() + 1];
    assert_eq!(after("-l"), "hi");
    assert_eq!(after("-sl"), "en");
    assert_eq!(after("--tts"), "namaste duniya");
    assert_eq!(after("--tts-steps"), "2");
    assert_eq!(after("--seed"), "0");
    let names: Vec<&str> = envs.iter().map(|e| e.split('=').next().unwrap()).collect();
    assert_eq!(
        names,
        [
            "CRISPASR_S3GEN_FASTCONV",
            "FAKE_ENGINE_RECORD",
            "HOME",
            "TMPDIR",
            "XDG_CACHE_HOME"
        ],
        "the engine sees only the manifest environment plus the isolation variables"
    );
}

#[tokio::test]
async fn english_job_from_english_reference_has_no_source_language() {
    let f = Fixture::new();
    let record = f.path("record.txt");
    let b = f.backend(&[("FAKE_ENGINE_RECORD", record.to_str().unwrap())], None);
    b.synthesize_chunk(&f.req("hello", Language::En), &CancellationToken::new())
        .await
        .unwrap();
    let rec = std::fs::read_to_string(record).unwrap();
    assert!(!rec.lines().any(|l| l == "ARG -sl"));
}

#[tokio::test]
async fn nonzero_exit_is_a_crash() {
    let f = Fixture::new();
    let b = f.backend(&[("FAKE_ENGINE_MODE", "exit:3")], None);
    let err = b
        .synthesize_chunk(&f.req("x", Language::En), &CancellationToken::new())
        .await
        .unwrap_err();
    assert!(matches!(
        err,
        BackendError::EngineCrashed {
            exit: ExitInfo::Code(3),
            ..
        }
    ));
}

#[tokio::test]
async fn death_by_signal_is_a_crash() {
    let f = Fixture::new();
    let b = f.backend(&[("FAKE_ENGINE_MODE", "signal:6")], None);
    let err = b
        .synthesize_chunk(&f.req("x", Language::En), &CancellationToken::new())
        .await
        .unwrap_err();
    assert!(matches!(
        err,
        BackendError::EngineCrashed {
            exit: ExitInfo::Signal(6),
            ..
        }
    ));
}

#[tokio::test]
async fn peak_rss_over_budget_fails_loudly() {
    let f = Fixture::new();
    // 64 MiB resident against a 32 MB budget.
    let b = f.backend(&[("FAKE_ENGINE_ALLOC_MB", "64")], Some(32_000));
    let err = b
        .synthesize_chunk(&f.req("x", Language::En), &CancellationToken::new())
        .await
        .unwrap_err();
    match err {
        BackendError::MemoryBudgetExceeded { budget_kib, stats } => {
            assert_eq!(budget_kib, 32_000);
            assert!(stats.peak_rss_kib > 64 * 1024, "{}", stats.peak_rss_kib);
        }
        other => panic!("expected budget failure, got {other:?}"),
    }
}

#[tokio::test]
async fn invalid_outputs_are_rejected() {
    for mode in ["bad-wav", "no-output", "stereo", "wrong-rate"] {
        let f = Fixture::new();
        let b = f.backend(&[("FAKE_ENGINE_MODE", mode)], None);
        let err = b
            .synthesize_chunk(&f.req("x", Language::En), &CancellationToken::new())
            .await
            .unwrap_err();
        assert!(
            matches!(err, BackendError::OutputInvalid { .. }),
            "{mode}: {err:?}"
        );
    }
}

#[tokio::test]
async fn missing_binary_is_a_spawn_error() {
    let f = Fixture::new();
    let mut manifest = RecipeManifest::turbo2();
    manifest.budget.ram_budget_kib = 1;
    let b = CrispCliBackend::new(
        manifest,
        &RuntimePin::crispasr(),
        CrispOptions {
            runtime_bin: f.path("no-such-engine"),
            model_dir: f.path("models"),
            threads: None,
            nice: 0,
            spoken_disclaimer: false,
            extra_env: vec![],
        },
    );
    let err = b
        .synthesize_chunk(&f.req("x", Language::En), &CancellationToken::new())
        .await
        .unwrap_err();
    assert!(matches!(err, BackendError::Spawn(_)));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancel_kills_and_reaps_a_running_engine() {
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
    let req = f.req("never finishes", Language::En);
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
    // Reaped by wait4: the pid no longer exists, not even as a zombie (or it already
    // belongs to an unrelated new process).
    let cmdline = std::fs::read(format!("/proc/{pid}/cmdline")).unwrap_or_default();
    assert!(!String::from_utf8_lossy(&cmdline).contains("fake-engine"));
}
