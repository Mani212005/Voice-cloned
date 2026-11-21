//! Scheduler tests against the in-process fake backend. Every wait is on an explicit
//! event (a started chunk, a job event, a reply), never on time.

#![allow(clippy::unwrap_used)]

use std::sync::Arc;

use tokio::sync::broadcast;
use vc_audio::MonoPcm;
use vc_backend::fake::{FAKE_SAMPLES_PER_WORD, fake_pcm};
use vc_backend::{
    Backend, BackendError, FakeBackend, FakeControl, FakeOutcome, VoiceRef, WarmControl, WarmEvent,
};
use vc_core::{Clock, ErrorCode, JobId, JobState, Language, ManualClock, Rate, VoiceId};
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
    consent: vc_core::ConsentId,
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
        let mut fx = Self {
            _dir: dir,
            store,
            data,
            clock,
            voice: VoiceId::generate(),
            consent: consent.id,
        };
        fx.voice = fx.add_voice(Language::En);
        fx
    }

    /// Enroll another voice; it is the newest one.
    fn add_voice(&self, reference_language: Language) -> VoiceId {
        let voice = VoiceId::generate();
        vc_store::files::private_dir(&self.data.voice_dir(voice)).unwrap();
        vc_store::files::write_private(
            &self.data.reference_path(voice),
            &vc_audio::encode_wav_bytes(&fake_pcm("ref")).unwrap(),
        )
        .unwrap();
        self.store
            .insert_voice(&VoiceRow {
                id: voice,
                consent_id: self.consent,
                name: "me".into(),
                reference_language,
                ref_sample_rate: 24_000,
                ref_ms: 100,
                ref_sha256: "00".repeat(32),
                created_at_ms: self.clock.now_ms(),
            })
            .unwrap();
        voice
    }

    fn voice_ref(&self, voice: VoiceId, reference_language: Language) -> VoiceRef {
        VoiceRef {
            reference: self.data.reference_path(voice),
            reference_language,
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
        let plan = vc_text::plan_speech(
            text,
            language,
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
            rate: Rate::NATIVE,
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
async fn a_faster_rate_shortens_chunks_and_gaps_without_new_engine_text() {
    let fx = Fx::new();
    let backend = Arc::new(FakeBackend::with_limits(2, 200));
    let config = EngineConfig {
        inter_chunk_gap_ms: 120,
        ..EngineConfig::default()
    };
    let engine = Engine::start(fx.ctx(backend.clone(), config)).unwrap();
    let mut events = engine.subscribe();
    let rate = Rate::from_f64(1.2).unwrap();
    let job = NewJob {
        rate,
        ..fx.new_job("aap kaise hain? main theek hoon.", Language::Hi, 2)
    };
    engine.enqueue(job.clone()).await.unwrap();
    assert_eq!(finished(&mut events, job.id).await, JobState::Succeeded);

    let texts: Vec<String> = backend.calls().into_iter().map(|c| c.text).collect();
    assert_eq!(texts, ["aap kaise", "hain?", "main theek", "hoon."]);
    let stretched: Vec<MonoPcm> = texts
        .iter()
        .map(|t| vc_audio::time_stretch(&fake_pcm(t), 1.2).unwrap())
        .collect();
    for (i, want) in stretched.iter().enumerate() {
        assert_eq!(
            &vc_audio::read_wav_file(&fx.data.chunk_path(job.id, i)).unwrap(),
            want
        );
    }
    // 120 ms of gap at 1.2x is 100 ms.
    let expected = vc_audio::concat_with_gap(&stretched, 100).unwrap();
    let out = vc_audio::read_wav_file(&fx.data.output_path(job.id)).unwrap();
    assert_eq!(out, expected);
    let row = fx.store.get_job(job.id).unwrap();
    assert_eq!(row.rate, rate);
    assert_eq!(row.audio_ms, Some(expected.duration_ms()));
}

#[tokio::test]
async fn english_text_reaches_the_engine_in_spoken_form() {
    let fx = Fx::new();
    let backend = Arc::new(FakeBackend::new());
    let engine = Engine::start(fx.ctx(backend.clone(), EngineConfig::default())).unwrap();
    let mut events = engine.subscribe();
    let job = fx.new_job("Hi. It ran 467 CPU tasks in 40-80 ms.", Language::En, 19);
    engine.enqueue(job.clone()).await.unwrap();
    assert_eq!(finished(&mut events, job.id).await, JobState::Succeeded);
    let texts: Vec<String> = backend.calls().into_iter().map(|c| c.text).collect();
    assert_eq!(
        texts,
        [
            "Hi.",
            "It ran four hundred sixty-seven see pee you tasks in forty to eighty milliseconds."
        ]
    );
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

fn warm_gated() -> (Arc<FakeBackend>, WarmControl) {
    let (b, w) = FakeBackend::new().warm_gated();
    (Arc::new(b), w)
}

/// The next two warm events, in either order (they come from two tasks).
async fn next_two(warm: &mut WarmControl) -> Vec<WarmEvent> {
    let mut two = vec![warm.next().await.unwrap(), warm.next().await.unwrap()];
    two.sort_by_key(|e| format!("{e:?}"));
    two
}

#[tokio::test]
async fn the_newest_voice_is_warmed_at_start_and_a_jobs_voice_when_it_ends() {
    let fx = Fx::new();
    let newest = fx.add_voice(Language::Hi);
    let (b, mut warm) = warm_gated();
    let engine = Engine::start(fx.ctx(b, EngineConfig::default())).unwrap();
    let hi = fx.voice_ref(newest, Language::Hi);
    assert_eq!(
        warm.next().await,
        Some(WarmEvent::PrepareStarted(hi.clone()))
    );
    warm.release_prepare(1);
    assert_eq!(warm.next().await, Some(WarmEvent::PrepareEnded(hi, Ok(()))));

    // A job for the older voice runs, then that voice is warmed for the next one.
    let mut events = engine.subscribe();
    let job = fx.new_job("hello there", Language::En, 19);
    engine.enqueue(job.clone()).await.unwrap();
    assert_eq!(finished(&mut events, job.id).await, JobState::Succeeded);
    let en = fx.voice_ref(fx.voice, Language::En);
    assert_eq!(warm.next().await, Some(WarmEvent::PrepareStarted(en)));
}

#[tokio::test]
async fn a_job_for_another_voice_cancels_the_warm_up() {
    let fx = Fx::new();
    let newest = fx.add_voice(Language::En);
    let (b, mut warm) = warm_gated();
    let engine = Engine::start(fx.ctx(b, EngineConfig::default())).unwrap();
    let warming = fx.voice_ref(newest, Language::En);
    assert_eq!(
        warm.next().await,
        Some(WarmEvent::PrepareStarted(warming.clone()))
    );
    let mut events = engine.subscribe();
    let job = fx.new_job("for the older voice", Language::En, 19);
    engine.enqueue(job.clone()).await.unwrap();
    assert_eq!(
        warm.next().await,
        Some(WarmEvent::PrepareEnded(
            warming,
            Err(BackendError::Cancelled)
        ))
    );
    assert_eq!(finished(&mut events, job.id).await, JobState::Succeeded);
    assert_eq!(
        warm.next().await,
        Some(WarmEvent::PrepareStarted(
            fx.voice_ref(fx.voice, Language::En)
        ))
    );
}

#[tokio::test]
async fn a_job_for_the_warming_voice_lets_the_warm_up_finish() {
    let fx = Fx::new();
    let (b, mut warm) = warm_gated();
    let engine = Engine::start(fx.ctx(b, EngineConfig::default())).unwrap();
    let voice = fx.voice_ref(fx.voice, Language::En);
    assert_eq!(
        warm.next().await,
        Some(WarmEvent::PrepareStarted(voice.clone()))
    );
    let mut events = engine.subscribe();
    let job = fx.new_job("same voice", Language::En, 19);
    engine.enqueue(job.clone()).await.unwrap();
    assert_eq!(finished(&mut events, job.id).await, JobState::Succeeded);
    // Not cancelled, and not started twice: the job's end finds it already warming.
    warm.release_prepare(1);
    assert_eq!(
        warm.next().await,
        Some(WarmEvent::PrepareEnded(voice, Ok(())))
    );
}

#[tokio::test]
async fn an_explicit_prepare_switches_the_warm_up_to_that_voice() {
    let fx = Fx::new();
    let (b, mut warm) = warm_gated();
    let engine = Engine::start(fx.ctx(b, EngineConfig::default())).unwrap();
    let first = fx.voice_ref(fx.voice, Language::En);
    assert_eq!(
        warm.next().await,
        Some(WarmEvent::PrepareStarted(first.clone()))
    );
    let enrolled = fx.add_voice(Language::Hi);
    engine.prepare(enrolled).await;
    let second = fx.voice_ref(enrolled, Language::Hi);
    assert_eq!(
        next_two(&mut warm).await,
        [
            WarmEvent::PrepareEnded(first, Err(BackendError::Cancelled)),
            WarmEvent::PrepareStarted(second),
        ]
    );
}

#[tokio::test]
async fn releasing_a_voice_cancels_its_warm_up_and_stops_its_engine() {
    let fx = Fx::new();
    let (b, mut warm) = warm_gated();
    let engine = Engine::start(fx.ctx(b, EngineConfig::default())).unwrap();
    let voice = fx.voice_ref(fx.voice, Language::En);
    assert_eq!(
        warm.next().await,
        Some(WarmEvent::PrepareStarted(voice.clone()))
    );
    engine.release_voice(fx.voice).await;
    assert_eq!(
        next_two(&mut warm).await,
        [
            WarmEvent::PrepareEnded(voice.clone(), Err(BackendError::Cancelled)),
            WarmEvent::Released(Some(voice.reference)),
        ]
    );
}

#[tokio::test]
async fn shutdown_stops_the_warm_engine() {
    let fx = Fx::new();
    let (b, mut warm) = warm_gated();
    let engine = Engine::start(fx.ctx(b, EngineConfig::default())).unwrap();
    let voice = fx.voice_ref(fx.voice, Language::En);
    assert_eq!(
        warm.next().await,
        Some(WarmEvent::PrepareStarted(voice.clone()))
    );
    engine.shutdown().await;
    assert_eq!(
        next_two(&mut warm).await,
        [
            WarmEvent::PrepareEnded(voice, Err(BackendError::Cancelled)),
            WarmEvent::Released(None),
        ]
    );
}
