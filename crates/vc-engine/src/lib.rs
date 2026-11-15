//! The job scheduler: a bounded FIFO queue in front of exactly one engine slot.
//!
//! One task owns the queue and the slot; API handlers talk to it over a command channel
//! and never touch the engine. "Two engines running at once" is unrepresentable: the
//! slot is a single `Option` in that task. A running job is cancelled by killing its
//! engine process. At startup, jobs left running by a previous process are queued
//! again (output is deterministic, so a re-run is safe).

mod runner;
mod scheduler;
mod sweep;

use std::sync::Arc;

use vc_backend::Backend;
use vc_core::{Clock, JobId, JobState};
use vc_store::{DataDir, Store};

pub use scheduler::{CancelOutcome, Engine, EnqueueError, QueueStatus};
pub use sweep::sweep_expired;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EngineConfig {
    /// Jobs that may wait behind the running one.
    pub queue_capacity: usize,
    /// How long ended jobs and their audio are kept.
    pub output_ttl_ms: i64,
    /// Silence between chunks in the final WAV.
    pub inter_chunk_gap_ms: u32,
    /// Times a chunk is re-run after an engine crash before the job fails.
    pub crash_retries: u32,
    /// Rough wall time per chunk, for `Retry-After` and queue estimates only.
    pub chunk_estimate_s: u64,
}

impl Default for EngineConfig {
    fn default() -> Self {
        Self {
            queue_capacity: 16,
            output_ttl_ms: 24 * 3600 * 1000,
            inter_chunk_gap_ms: 200,
            crash_retries: 1,
            chunk_estimate_s: 30,
        }
    }
}

/// Everything a job run needs.
#[derive(Debug, Clone)]
pub struct EngineContext {
    pub store: Arc<Store>,
    pub data: DataDir,
    pub backend: Arc<dyn Backend>,
    pub clock: Arc<dyn Clock>,
    pub config: EngineConfig,
}

/// Progress notifications, for tests now and server-sent events later.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JobEvent {
    Started { job: JobId },
    ChunkDone { job: JobId, idx: usize },
    Finished { job: JobId, state: JobState },
}
