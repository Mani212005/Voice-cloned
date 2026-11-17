use std::collections::VecDeque;

use tokio::sync::{broadcast, mpsc, oneshot};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use vc_backend::{BackendError, VoiceRef};
use vc_core::{JobId, JobState, VoiceId};
use vc_store::{JobEnd, NewJob, StoreError};

use crate::runner::{RunResult, run_job};
use crate::{EngineContext, JobEvent};

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum EnqueueError {
    #[error("queue full")]
    QueueFull { retry_after_s: u64 },
    #[error("voice not found")]
    VoiceNotFound,
    #[error("store: {0}")]
    Store(String),
    #[error("engine stopped")]
    Stopped,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QueueStatus {
    Running,
    /// 1-based position; 1 means next to run.
    Queued {
        position: usize,
    },
    NotActive,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CancelOutcome {
    /// The job was queued or running and is now cancelled (a running engine has been
    /// killed and reaped).
    Cancelled,
    /// The job is not queued or running.
    NotActive,
}

enum Command {
    Enqueue {
        job: NewJob,
        reply: oneshot::Sender<Result<QueueStatus, EnqueueError>>,
    },
    Cancel {
        job: JobId,
        reply: oneshot::Sender<CancelOutcome>,
    },
    Status {
        job: JobId,
        reply: oneshot::Sender<QueueStatus>,
    },
    Finished {
        job: JobId,
        result: RunResult,
    },
    Prepare {
        voice: VoiceId,
    },
    Prepared {
        id: u64,
    },
    ReleaseVoice {
        voice: VoiceId,
        reply: oneshot::Sender<()>,
    },
    Shutdown {
        reply: oneshot::Sender<()>,
    },
}

struct Running {
    job: JobId,
    cancel: CancellationToken,
    handle: JoinHandle<()>,
    /// Callers waiting for this job's cancellation to complete.
    cancel_waiters: Vec<oneshot::Sender<CancelOutcome>>,
}

/// A warm-up of the backend for a voice, run while no job is running.
struct Warming {
    id: u64,
    voice: VoiceId,
    cancel: CancellationToken,
}

struct Scheduler {
    ctx: EngineContext,
    tx: mpsc::Sender<Command>,
    events: broadcast::Sender<JobEvent>,
    queue: VecDeque<JobId>,
    /// The one engine slot.
    running: Option<Running>,
    /// At most one warm-up, and only while `running` is empty: a job that starts for
    /// another voice cancels it. A job for the same voice waits for it inside the backend
    /// and then starts warm.
    warming: Option<Warming>,
    warm_seq: u64,
    shutting_down: Option<oneshot::Sender<()>>,
}

/// Handle to the scheduler task. Cheap to clone.
#[derive(Debug, Clone)]
pub struct Engine {
    tx: mpsc::Sender<Command>,
    events: broadcast::Sender<JobEvent>,
    ctx: EngineContext,
}

impl std::fmt::Debug for Command {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Command::Enqueue { .. } => "Enqueue",
            Command::Cancel { .. } => "Cancel",
            Command::Status { .. } => "Status",
            Command::Finished { .. } => "Finished",
            Command::Prepare { .. } => "Prepare",
            Command::Prepared { .. } => "Prepared",
            Command::ReleaseVoice { .. } => "ReleaseVoice",
            Command::Shutdown { .. } => "Shutdown",
        })
    }
}

impl Engine {
    /// Recover state from the store and start the scheduler task.
    pub fn start(ctx: EngineContext) -> Result<Self, StoreError> {
        let queue = recover(&ctx)?;
        let (tx, rx) = mpsc::channel(64);
        let (events, _) = broadcast::channel(256);
        let scheduler = Scheduler {
            ctx: ctx.clone(),
            tx: tx.clone(),
            events: events.clone(),
            queue,
            running: None,
            warming: None,
            warm_seq: 0,
            shutting_down: None,
        };
        tokio::spawn(scheduler.run(rx));
        Ok(Self { tx, events, ctx })
    }

    pub fn context(&self) -> &EngineContext {
        &self.ctx
    }

    pub fn subscribe(&self) -> broadcast::Receiver<JobEvent> {
        self.events.subscribe()
    }

    /// Persist and queue a job.
    pub async fn enqueue(&self, job: NewJob) -> Result<QueueStatus, EnqueueError> {
        let (reply, rx) = oneshot::channel();
        self.send(Command::Enqueue { job, reply }).await?;
        rx.await.map_err(|_| EnqueueError::Stopped)?
    }

    /// Cancel a queued or running job. For a running job this returns after the engine
    /// process has been killed and reaped.
    pub async fn cancel(&self, job: JobId) -> CancelOutcome {
        let (reply, rx) = oneshot::channel();
        if self.send(Command::Cancel { job, reply }).await.is_err() {
            return CancelOutcome::NotActive;
        }
        rx.await.unwrap_or(CancelOutcome::NotActive)
    }

    /// Warm the backend for `voice` while no job is running, so the next job with it
    /// starts without loading the engine or encoding the voice. A no-op for backends
    /// without warm state, and ignored while a job runs (every job end warms its own
    /// voice when the queue is empty).
    pub async fn prepare(&self, voice: VoiceId) {
        let _ = self.send(Command::Prepare { voice }).await;
    }

    /// Drop everything the backend holds for `voice` (its warm engine). Call once the
    /// voice is deleted; returns when that engine has been stopped.
    pub async fn release_voice(&self, voice: VoiceId) {
        let (reply, rx) = oneshot::channel();
        if self
            .send(Command::ReleaseVoice { voice, reply })
            .await
            .is_ok()
        {
            let _ = rx.await;
        }
    }

    pub async fn status(&self, job: JobId) -> QueueStatus {
        let (reply, rx) = oneshot::channel();
        if self.send(Command::Status { job, reply }).await.is_err() {
            return QueueStatus::NotActive;
        }
        rx.await.unwrap_or(QueueStatus::NotActive)
    }

    /// Stop the running engine and the scheduler. A job interrupted this way stays
    /// `running` in the store and is queued again at the next start.
    pub async fn shutdown(&self) {
        let (reply, rx) = oneshot::channel();
        if self.send(Command::Shutdown { reply }).await.is_ok() {
            let _ = rx.await;
        }
    }

    async fn send(&self, cmd: Command) -> Result<(), EnqueueError> {
        self.tx.send(cmd).await.map_err(|_| EnqueueError::Stopped)
    }
}

/// Jobs left `running` go back to the queue; queued jobs keep their order.
fn recover(ctx: &EngineContext) -> Result<VecDeque<JobId>, StoreError> {
    for job in ctx.store.jobs_in_state(JobState::Running)? {
        tracing::warn!(job_id = %job.id, "job was running at last shutdown; queueing it again");
        ctx.store.requeue_job(job.id)?;
    }
    Ok(ctx
        .store
        .jobs_in_state(JobState::Queued)?
        .into_iter()
        .map(|j| j.id)
        .collect())
}

impl Scheduler {
    async fn run(mut self, mut rx: mpsc::Receiver<Command>) {
        self.start_next();
        if self.running.is_none() {
            // Warm the newest voice (voice ids sort by creation time).
            match self.ctx.store.list_voices() {
                Ok(voices) => {
                    if let Some(v) = voices.last() {
                        self.prepare(v.id);
                    }
                }
                Err(e) => tracing::warn!("cannot list voices to warm the engine: {e}"),
            }
        }
        while let Some(cmd) = rx.recv().await {
            match cmd {
                Command::Enqueue { job, reply } => {
                    let _ = reply.send(self.enqueue(job));
                    self.start_next();
                }
                Command::Cancel { job, reply } => self.cancel(job, reply),
                Command::Status { job, reply } => {
                    let _ = reply.send(self.status(job));
                }
                Command::Finished { job, result } => {
                    self.finished(job, result).await;
                    if let Some(done) = self.shutting_down.take() {
                        self.stop_backend().await;
                        let _ = done.send(());
                        return;
                    }
                    self.start_next();
                    if self.running.is_none() {
                        // Idle again: get the engine ready for this voice's next job
                        // (restarting it now if it is near the RAM budget).
                        if let Ok(j) = self.ctx.store.get_job(job) {
                            self.prepare(j.voice_id);
                        }
                    }
                }
                Command::Prepare { voice } => self.prepare(voice),
                Command::Prepared { id } => {
                    if self.warming.as_ref().is_some_and(|w| w.id == id) {
                        self.warming = None;
                    }
                }
                Command::ReleaseVoice { voice, reply } => self.release_voice(voice, reply),
                Command::Shutdown { reply } => {
                    self.queue.clear();
                    match &self.running {
                        Some(r) => {
                            r.cancel.cancel();
                            self.shutting_down = Some(reply);
                        }
                        None => {
                            self.stop_backend().await;
                            let _ = reply.send(());
                            return;
                        }
                    }
                }
            }
        }
    }

    fn enqueue(&mut self, job: NewJob) -> Result<QueueStatus, EnqueueError> {
        if self.shutting_down.is_some() {
            return Err(EnqueueError::Stopped);
        }
        if self.queue.len() >= self.ctx.config.queue_capacity {
            return Err(EnqueueError::QueueFull {
                retry_after_s: self.estimate_wait_s(),
            });
        }
        match self.ctx.store.insert_job(&job, self.ctx.clock.now_ms()) {
            Ok(()) => {}
            Err(StoreError::NotFound) => return Err(EnqueueError::VoiceNotFound),
            Err(e) => return Err(EnqueueError::Store(e.to_string())),
        }
        self.queue.push_back(job.id);
        Ok(QueueStatus::Queued {
            position: self.queue.len(),
        })
    }

    /// Rough time until a new job could start; only used for `Retry-After`.
    fn estimate_wait_s(&self) -> u64 {
        let chunks_of = |id: JobId| {
            self.ctx
                .store
                .get_job(id)
                .map_or(1, |j| j.chunks_total.saturating_sub(j.chunks_done).max(1))
        };
        let pending: usize = self.queue.iter().map(|&id| chunks_of(id)).sum::<usize>()
            + self.running.as_ref().map_or(0, |r| chunks_of(r.job));
        (pending as u64).max(1) * self.ctx.config.chunk_estimate_s
    }

    fn status(&self, job: JobId) -> QueueStatus {
        if self.running.as_ref().is_some_and(|r| r.job == job) {
            return QueueStatus::Running;
        }
        match self.queue.iter().position(|&id| id == job) {
            Some(i) => QueueStatus::Queued { position: i + 1 },
            None => QueueStatus::NotActive,
        }
    }

    fn cancel(&mut self, job: JobId, reply: oneshot::Sender<CancelOutcome>) {
        if let Some(r) = self.running.as_mut().filter(|r| r.job == job) {
            r.cancel.cancel();
            r.cancel_waiters.push(reply);
            return;
        }
        if let Some(i) = self.queue.iter().position(|&id| id == job) {
            self.queue.remove(i);
            let outcome = self.persist_end(job, &JobEnd::Cancelled);
            let _ = reply.send(if outcome {
                CancelOutcome::Cancelled
            } else {
                CancelOutcome::NotActive
            });
            return;
        }
        let _ = reply.send(CancelOutcome::NotActive);
    }

    fn start_next(&mut self) {
        if self.running.is_some() || self.shutting_down.is_some() {
            return;
        }
        while let Some(job) = self.queue.pop_front() {
            if let Err(e) = self.ctx.store.start_job(job, self.ctx.clock.now_ms()) {
                tracing::error!(job_id = %job, "cannot start job: {e}");
                continue;
            }
            let voice = self.ctx.store.get_job(job).map(|j| j.voice_id).ok();
            if self
                .warming
                .as_ref()
                .is_some_and(|w| Some(w.voice) != voice)
            {
                self.cancel_warming();
            }
            let _ = self.events.send(JobEvent::Started { job });
            let cancel = CancellationToken::new();
            let handle = {
                let (ctx, cancel, tx, events) = (
                    self.ctx.clone(),
                    cancel.clone(),
                    self.tx.clone(),
                    self.events.clone(),
                );
                tokio::spawn(async move {
                    let result = run_job(&ctx, job, &cancel, &events).await;
                    let _ = tx.send(Command::Finished { job, result }).await;
                })
            };
            self.running = Some(Running {
                job,
                cancel,
                handle,
                cancel_waiters: Vec::new(),
            });
            return;
        }
    }

    async fn finished(&mut self, job: JobId, result: RunResult) {
        let Some(running) = self.running.take() else {
            return;
        };
        debug_assert_eq!(running.job, job);
        let _ = running.handle.await;
        if self.shutting_down.is_some() && result == RunResult::Cancelled {
            // Interrupted by shutdown: leave it `running` so the next start re-queues it.
            return;
        }
        let end = match result {
            RunResult::Succeeded {
                audio_ms,
                output_sha256,
            } => JobEnd::Succeeded {
                audio_ms,
                output_sha256,
            },
            RunResult::Failed(code) => JobEnd::Failed(code),
            RunResult::Cancelled => JobEnd::Cancelled,
        };
        let persisted = self.persist_end(job, &end);
        let cancelled = persisted && end == JobEnd::Cancelled;
        for w in running.cancel_waiters {
            let _ = w.send(if cancelled {
                CancelOutcome::Cancelled
            } else {
                CancelOutcome::NotActive
            });
        }
    }

    fn prepare(&mut self, voice: VoiceId) {
        if self.running.is_some() || self.shutting_down.is_some() {
            return;
        }
        if self.warming.as_ref().is_some_and(|w| w.voice == voice) {
            return;
        }
        self.cancel_warming();
        let row = match self.ctx.store.get_voice(voice) {
            Ok(row) => row,
            Err(e) => {
                tracing::debug!(voice_id = %voice, "not warming the engine: {e}");
                return;
            }
        };
        let voice_ref = VoiceRef {
            reference: self.ctx.data.reference_path(voice),
            reference_language: row.reference_language,
        };
        self.warm_seq += 1;
        let id = self.warm_seq;
        let cancel = CancellationToken::new();
        {
            let (backend, cancel, tx) = (self.ctx.backend.clone(), cancel.clone(), self.tx.clone());
            tokio::spawn(async move {
                match backend.prepare(&voice_ref, &cancel).await {
                    Ok(()) => tracing::debug!(voice_id = %voice, "engine ready for voice"),
                    Err(BackendError::Cancelled) => {
                        tracing::debug!(voice_id = %voice, "engine warm-up cancelled");
                    }
                    Err(e) => tracing::warn!(voice_id = %voice, "engine warm-up failed: {e}"),
                }
                let _ = tx.send(Command::Prepared { id }).await;
            });
        }
        self.warming = Some(Warming { id, voice, cancel });
    }

    /// Cancel the warm-up, if any. Its task ends on its own; the backend serializes it
    /// with whatever runs next.
    fn cancel_warming(&mut self) {
        if let Some(w) = self.warming.take() {
            w.cancel.cancel();
        }
    }

    fn release_voice(&mut self, voice: VoiceId, reply: oneshot::Sender<()>) {
        if self.warming.as_ref().is_some_and(|w| w.voice == voice) {
            self.cancel_warming();
        }
        let backend = self.ctx.backend.clone();
        let reference = self.ctx.data.reference_path(voice);
        tokio::spawn(async move {
            backend.release(Some(&reference)).await;
            let _ = reply.send(());
        });
    }

    /// End any warm-up and stop whatever the backend keeps running. The warm-up task is
    /// not awaited here (it reports back on this task's own channel); the backend's
    /// release waits for it instead.
    async fn stop_backend(&mut self) {
        self.cancel_warming();
        self.ctx.backend.release(None).await;
    }

    fn persist_end(&self, job: JobId, end: &JobEnd) -> bool {
        let now = self.ctx.clock.now_ms();
        match self
            .ctx
            .store
            .finish_job(job, end, now, self.ctx.config.output_ttl_ms)
        {
            Ok(()) => {
                let state = match end {
                    JobEnd::Succeeded { .. } => JobState::Succeeded,
                    JobEnd::Failed(_) => JobState::Failed,
                    JobEnd::Cancelled => JobState::Cancelled,
                };
                tracing::info!(job_id = %job, state = %state, "job ended");
                let _ = self.events.send(JobEvent::Finished { job, state });
                true
            }
            Err(e) => {
                tracing::error!(job_id = %job, "cannot record job end: {e}");
                false
            }
        }
    }
}
