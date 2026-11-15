//! Scheduler tests against the in-process fake backend. Every wait is on an explicit
//! event (a started chunk, a job event, a reply), never on time.

#![allow(clippy::unwrap_used)]

use std::sync::Arc;

use tokio::sync::broadcast;
use vc_audio::MonoPcm;
use vc_backend::fake::{FAKE_SAMPLES_PER_WORD, fake_pcm};
use vc_backend::{Backend, FakeBackend, FakeControl, FakeOutcome};
use vc_core::{Clock, ErrorCode, JobId, JobState, Language, ManualClock, VoiceId};
use vc_engine::{
    CancelOutcome, Engine, EngineConfig, EngineContext, EnqueueError, JobEvent, QueueStatus,
    sweep_expired,
};
use vc_store::{DataDir, NewConsent, NewJob, Store, VoiceRow};

const TTL: i64 = 24 * 3600 * 1000;

struct Fx {
    _dir: tempfile::TempDir,
    store: Arc<Store>,
    data: DataDir,
    clock: Arc<ManualClock>,
    voice: VoiceId,
}

impl Fx {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let data = DataDir::open(&dir.path().join("data")).unwrap();
        let store = Arc::new(Store::open(&data.db_path()).unwrap());
        let clock = Arc::new(ManualClock::new(1_000_000));
        let consent = store
            .insert_consent(
                NewConsent {
                    speaker_name: "Owner".into(),
                    relationship: "self".into(),
                    attestation: "own voice".into(),
                    languages: vec![Language::En, Language::Hi],
                },
                clock.now_ms(),
            )
            .unwrap();
        let voice = VoiceId::generate();
        vc_store::files::private_dir(&data.voice_dir(voice)).unwrap();
        vc_store::files::write_private(
            &data.reference_path(voice),
            &vc_audio::encode_wav_bytes(&fake_pcm("ref")).unwrap(),
        )
        .unwrap();
        store
            .insert_voice(&VoiceRow {
                id: voice,
                consent_id: consent.id,
                name: "me".into(),
                reference_language: Language::En,
                ref_sample_rate: 24_000,
                ref_ms: 100,
                ref_sha256: "00".repeat(32),
                created_at_ms: clock.now_ms(),
            })
            .unwrap();
        Self {
            _dir: dir,
            store,
            data,
            clock,
            voice,
        }
    }

    fn ctx(&self, backend: Arc<dyn Backend>, config: EngineConfig) -> EngineContext {
        EngineContext {
            store: self.store.clone(),
            data: self.data.clone(),
            backend,
            clock: self.clock.clone(),
            config,
        }
    }

    fn new_job(&self, text: &str, language: Language, max_words: usize) -> NewJob {
        let plan = vc_text::plan(
            text,
            vc_text::PlanLimits {
                max_words,
                max_chars: 200,
            },
        )
        .unwrap();
        NewJob {
            id: JobId::generate(),
            voice_id: self.voice,
            language,
            text: text.into(),
            text_sha256: "11".repeat(32),
            chunks_total: plan.chunks.len(),
        }
    }
}

fn gated(max_words: usize) -> (Arc<FakeBackend>, FakeControl) {
    let (b, c) = FakeBackend::with_limits(max_words, 200).gated();
    (Arc::new(b), c)
}

async fn finished(events: &mut broadcast::Receiver<JobEvent>, job: JobId) -> JobState {
    loop {
        if let JobEvent::Finished { job: j, state } = events.recv().await.unwrap()
            && j == job
        {
            return state;
        }
    }
}

#[tokio::test]
async fn one_engine_slot_runs_jobs_in_fifo_order() {
    let fx = Fx::new();
    let (backend, mut ctl) = gated(19);
    let engine = Engine::start(fx.ctx(backend.clone(), EngineConfig::default())).unwrap();
    let mut events = engine.subscribe();
    let a = fx.new_job("first job.", Language::En, 19);
    let b = fx.new_job("second job.", Language::En, 19);
    engine.enqueue(a.clone()).await.unwrap();
    engine.enqueue(b.clone()).await.unwrap();

    assert_eq!(ctl.next_started().await.unwrap().text, "first job.");
    assert_eq!(engine.status(a.id).await, QueueStatus::Running);
    assert_eq!(
        engine.status(b.id).await,
        QueueStatus::Queued { position: 1 }
    );
    // b cannot start while a holds the slot.
    assert_eq!(backend.calls().len(), 1);

    ctl.release(1);
    assert_eq!(finished(&mut events, a.id).await, JobState::Succeeded);
    assert_eq!(ctl.next_started().await.unwrap().text, "second job.");
    ctl.release(1);
    assert_eq!(finished(&mut events, b.id).await, JobState::Succeeded);
    assert_eq!(engine.status(b.id).await, QueueStatus::NotActive);
}

#[tokio::test]
async fn success_writes_chunks_and_a_gapped_output_and_forgets_the_text() {
    let fx = Fx::new();
    let backend = Arc::new(FakeBackend::with_limits(2, 200));
    let config = EngineConfig {
        inter_chunk_gap_ms: 100,
        ..EngineConfig::default()
    };
    let engine = Engine::start(fx.ctx(backend.clone(), config)).unwrap();
    let mut events = engine.subscribe();
    let job = fx.new_job("aap kaise hain? main theek hoon.", Language::Hi, 2);
    assert_eq!(job.chunks_total, 4);
    engine.enqueue(job.clone()).await.unwrap();
    assert_eq!(finished(&mut events, job.id).await, JobState::Succeeded);

    let row = fx.store.get_job(job.id).unwrap();
    assert_eq!(row.text, None);
    assert_eq!(row.chunks_done, 4);
    assert_eq!(row.expires_at_ms, Some(fx.clock.now_ms() + TTL));
    assert!(row.first_audio_at_ms.is_some());

    let texts: Vec<String> = backend.calls().into_iter().map(|c| c.text).collect();
    assert_eq!(texts, ["aap kaise", "hain?", "main theek", "hoon."]);
    for c in backend.calls() {
        assert_eq!(c.language, Language::Hi);
        assert_eq!(c.reference_language, Language::En);
        assert_eq!(c.reference, fx.data.reference_path(fx.voice));
    }

    let out = vc_audio::read_wav_file(&fx.data.output_path(job.id)).unwrap();
    let gap = 24_000 * 100 / 1000;
    assert_eq!(out.samples.len(), 6 * FAKE_SAMPLES_PER_WORD + 3 * gap);
    let expected = vc_audio::concat_with_gap(
        &texts.iter().map(|t| fake_pcm(t)).collect::<Vec<MonoPcm>>(),
        100,
    )
    .unwrap();
    assert_eq!(out, expected);
    for (i, text) in texts.iter().enumerate() {
        assert_eq!(
            vc_audio::read_wav_file(&fx.data.chunk_path(job.id, i)).unwrap(),
            fake_pcm(text)
        );
    }
    let bytes = std::fs::read(fx.data.output_path(job.id)).unwrap();
    use sha2::Digest;
    assert_eq!(
        row.output_sha256.unwrap(),
        hex::encode(sha2::Sha256::digest(&bytes))
    );
    assert_eq!(fx.store.chunks(job.id).unwrap().len(), 4);
}

#[tokio::test]
async fn cancel_a_queued_job() {
    let fx = Fx::new();
    let (backend, mut ctl) = gated(19);
    let engine = Engine::start(fx.ctx(backend, EngineConfig::default())).unwrap();
    let a = fx.new_job("a.", Language::En, 19);
    let b = fx.new_job("b.", Language::En, 19);
    engine.enqueue(a.clone()).await.unwrap();
    engine.enqueue(b.clone()).await.unwrap();
    ctl.next_started().await.unwrap();
    assert_eq!(engine.cancel(b.id).await, CancelOutcome::Cancelled);
    assert_eq!(fx.store.get_job(b.id).unwrap().state, JobState::Cancelled);
    assert_eq!(fx.store.get_job(b.id).unwrap().text, None);
    assert_eq!(engine.cancel(b.id).await, CancelOutcome::NotActive);
    assert_eq!(fx.store.get_job(a.id).unwrap().state, JobState::Running);
}

#[tokio::test]
async fn cancel_a_running_job_then_the_next_one_runs() {
    let fx = Fx::new();
    let (backend, mut ctl) = gated(19);
    let engine = Engine::start(fx.ctx(backend, EngineConfig::default())).unwrap();
    let mut events = engine.subscribe();
    let a = fx.new_job("a.", Language::En, 19);
    let b = fx.new_job("b.", Language::En, 19);
    engine.enqueue(a.clone()).await.unwrap();
    engine.enqueue(b.clone()).await.unwrap();
    ctl.next_started().await.unwrap();
    assert_eq!(engine.cancel(a.id).await, CancelOutcome::Cancelled);
    let row = fx.store.get_job(a.id).unwrap();
    assert_eq!(row.state, JobState::Cancelled);
    assert_eq!(row.error_code, Some(ErrorCode::Cancelled));
    assert_eq!(ctl.next_started().await.unwrap().text, "b.");
    ctl.release(1);
    assert_eq!(finished(&mut events, b.id).await, JobState::Succeeded);
}

#[tokio::test]
async fn an_engine_crash_is_retried_once() {
    let fx = Fx::new();
    let backend = Arc::new(FakeBackend::new());
    backend.script([FakeOutcome::Crash]);
    let engine = Engine::start(fx.ctx(backend.clone(), EngineConfig::default())).unwrap();
    let mut events = engine.subscribe();
    let job = fx.new_job("hello.", Language::En, 19);
    engine.enqueue(job.clone()).await.unwrap();
    assert_eq!(finished(&mut events, job.id).await, JobState::Succeeded);
    assert_eq!(backend.calls().len(), 2);
}

#[tokio::test]
async fn a_second_crash_fails_the_job() {
    let fx = Fx::new();
    let backend = Arc::new(FakeBackend::new());
    backend.script([FakeOutcome::Crash, FakeOutcome::Crash]);
    let engine = Engine::start(fx.ctx(backend.clone(), EngineConfig::default())).unwrap();
    let mut events = engine.subscribe();
    let job = fx.new_job("hello.", Language::En, 19);
    engine.enqueue(job.clone()).await.unwrap();
    assert_eq!(finished(&mut events, job.id).await, JobState::Failed);
    let row = fx.store.get_job(job.id).unwrap();
    assert_eq!(row.error_code, Some(ErrorCode::EngineCrashed));
    assert_eq!(row.text, None);
}

#[tokio::test]
async fn memory_budget_and_bad_output_fail_without_retry() {
    for (outcome, code) in [
        (FakeOutcome::OverBudget, ErrorCode::MemoryBudgetExceeded),
        (FakeOutcome::BadOutput, ErrorCode::EngineOutputInvalid),
    ] {
        let fx = Fx::new();
        let backend = Arc::new(FakeBackend::new());
        backend.script([outcome]);
        let engine = Engine::start(fx.ctx(backend.clone(), EngineConfig::default())).unwrap();
        let mut events = engine.subscribe();
        let job = fx.new_job("hello.", Language::En, 19);
        engine.enqueue(job.clone()).await.unwrap();
        assert_eq!(finished(&mut events, job.id).await, JobState::Failed);
        assert_eq!(fx.store.get_job(job.id).unwrap().error_code, Some(code));
        assert_eq!(backend.calls().len(), 1);
    }
}

#[tokio::test]
async fn a_full_queue_refuses_with_a_retry_estimate() {
    let fx = Fx::new();
    let (backend, mut ctl) = gated(19);
    let config = EngineConfig {
        queue_capacity: 1,
        chunk_estimate_s: 30,
        ..EngineConfig::default()
    };
    let engine = Engine::start(fx.ctx(backend, config)).unwrap();
    let a = fx.new_job("a.", Language::En, 19);
    let b = fx.new_job("b.", Language::En, 19);
    let c = fx.new_job("c.", Language::En, 19);
    engine.enqueue(a).await.unwrap();
    ctl.next_started().await.unwrap();
    assert_eq!(
        engine.enqueue(b).await.unwrap(),
        QueueStatus::Queued { position: 1 }
    );
    assert_eq!(
        engine.enqueue(c.clone()).await,
        Err(EnqueueError::QueueFull { retry_after_s: 60 })
    );
    // A refused job is never stored.
    assert!(!fx.store.job_exists(c.id).unwrap());
}

#[tokio::test]
async fn a_job_for_a_deleted_voice_is_refused() {
    let fx = Fx::new();
    let engine =
        Engine::start(fx.ctx(Arc::new(FakeBackend::new()), EngineConfig::default())).unwrap();
    fx.store.tombstone_voice(fx.voice, 1).unwrap();
    assert_eq!(
        engine.enqueue(fx.new_job("a.", Language::En, 19)).await,
        Err(EnqueueError::VoiceNotFound)
    );
}

#[tokio::test]
async fn restart_requeues_an_interrupted_job_and_finishes_it() {
    let fx = Fx::new();
    let job = fx.new_job("interrupted.", Language::En, 19);
    {
        let (backend, mut ctl) = gated(19);
        let engine = Engine::start(fx.ctx(backend, EngineConfig::default())).unwrap();
        engine.enqueue(job.clone()).await.unwrap();
        ctl.next_started().await.unwrap();
        engine.shutdown().await;
    }
    let row = fx.store.get_job(job.id).unwrap();
    assert_eq!(
        row.state,
        JobState::Running,
        "shutdown keeps the job for recovery"
    );
    assert!(row.text.is_some());

    let backend = Arc::new(FakeBackend::new());
    let engine = Engine::start(fx.ctx(backend.clone(), EngineConfig::default())).unwrap();
    let mut events = engine.subscribe();
    // Recovery queued it before the scheduler started; it may already be done.
    let state = match fx.store.get_job(job.id).unwrap().state {
        JobState::Succeeded => JobState::Succeeded,
        _ => finished(&mut events, job.id).await,
    };
    assert_eq!(state, JobState::Succeeded);
    assert_eq!(fx.store.get_job(job.id).unwrap().attempts, 2);
}

#[tokio::test]
async fn expired_jobs_and_orphan_dirs_are_swept() {
    let fx = Fx::new();
    let backend: Arc<dyn Backend> = Arc::new(FakeBackend::new());
    let ctx = fx.ctx(backend, EngineConfig::default());
    let engine = Engine::start(ctx.clone()).unwrap();
    let mut events = engine.subscribe();
    let job = fx.new_job("keep me a day.", Language::En, 19);
    engine.enqueue(job.clone()).await.unwrap();
    assert_eq!(finished(&mut events, job.id).await, JobState::Succeeded);
    let orphan = JobId::generate();
    vc_store::files::private_dir(&fx.data.job_dir(orphan)).unwrap();

    fx.clock.advance_ms(TTL - 1);
    assert_eq!(sweep_expired(&ctx), 0);
    assert!(fx.data.output_path(job.id).exists());
    assert!(!fx.data.job_dir(orphan).exists());

    fx.clock.advance_ms(1);
    assert_eq!(sweep_expired(&ctx), 1);
    assert!(!fx.data.job_dir(job.id).exists());
    assert!(!fx.store.job_exists(job.id).unwrap());
}
