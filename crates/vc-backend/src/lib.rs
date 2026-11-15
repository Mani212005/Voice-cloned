//! Speech engine backends.
//!
//! A [`Backend`] synthesizes one planned text chunk at a time and declares what it can do
//! ([`Capabilities`]). v1 ships [`CrispCliBackend`], which runs the pinned CrispASR CLI
//! as a child process per chunk with the `turbo2` recipe. [`FakeBackend`] is the
//! in-process test double; the `fake-engine` binary is a CLI-compatible stand-in for
//! `crispasr` used to test `CrispCliBackend` itself.

use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::time::Duration;

use serde::Serialize;
use tokio_util::sync::CancellationToken;
use vc_audio::MonoPcm;
use vc_core::{ErrorCode, Language};

pub mod crisp_cli;
pub mod fake;
pub mod manifest;
pub mod proc;

pub use crisp_cli::{CrispCliBackend, CrispOptions};
pub use fake::{FakeBackend, FakeControl, FakeOutcome};
pub use manifest::{RecipeManifest, RuntimePin};
pub use proc::ExitInfo;

pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// One chunk to synthesize.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChunkRequest {
    pub idx: usize,
    pub text: String,
    pub language: Language,
    /// Mono WAV reference of the voice.
    pub reference: PathBuf,
    /// The language the reference is spoken in.
    pub reference_language: Language,
    /// Private per-job working directory (exists, owner-only).
    pub work_dir: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChunkStats {
    /// Recorded for reporting, never asserted in tests.
    pub wall: Duration,
    /// `ru_maxrss` of the engine process in KiB; 0 for in-process backends.
    pub peak_rss_kib: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChunkOutput {
    pub pcm: MonoPcm,
    pub stats: ChunkStats,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum BackendError {
    #[error("engine failed: {exit}")]
    EngineCrashed { exit: ExitInfo, stats: ChunkStats },
    #[error("engine could not start: {0}")]
    Spawn(String),
    #[error("engine output invalid: {reason}")]
    OutputInvalid { reason: String, stats: ChunkStats },
    #[error("engine peak RSS {} KiB is over the {budget_kib} KiB budget", stats.peak_rss_kib)]
    MemoryBudgetExceeded { budget_kib: u64, stats: ChunkStats },
    #[error("cancelled")]
    Cancelled,
}

impl BackendError {
    pub fn code(&self) -> ErrorCode {
        match self {
            BackendError::EngineCrashed { .. } | BackendError::Spawn(_) => ErrorCode::EngineCrashed,
            BackendError::OutputInvalid { .. } => ErrorCode::EngineOutputInvalid,
            BackendError::MemoryBudgetExceeded { .. } => ErrorCode::MemoryBudgetExceeded,
            BackendError::Cancelled => ErrorCode::Cancelled,
        }
    }

    /// Stats of the engine run that produced this error, if one ran.
    pub fn stats(&self) -> Option<&ChunkStats> {
        match self {
            BackendError::EngineCrashed { stats, .. }
            | BackendError::OutputInvalid { stats, .. }
            | BackendError::MemoryBudgetExceeded { stats, .. } => Some(stats),
            BackendError::Spawn(_) | BackendError::Cancelled => None,
        }
    }
}

/// What the API may promise for a backend.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Capabilities {
    pub backend_id: String,
    pub languages: Vec<Language>,
    pub sample_rate_hz: u32,
    pub channels: u16,
    pub max_chunk_words: usize,
    pub max_chunk_chars: usize,
    /// Engine processes that may run at once.
    pub concurrency: u32,
    pub ram_budget_kib: u64,
    /// Output is a pure function of the inputs and the pinned seed.
    pub deterministic: bool,
    pub watermark: &'static str,
    pub spoken_disclaimer: bool,
    pub reference_min_s: f64,
    pub reference_max_s: f64,
}

/// Provenance of a backend: what produced the audio.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct BackendInfo {
    pub backend_id: String,
    pub runtime: Option<RuntimeInfo>,
    pub models: Vec<ModelInfo>,
    pub seed: u64,
    pub tts_steps: u32,
    pub threads: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RuntimeInfo {
    pub name: String,
    pub version: String,
    pub commit: String,
    pub tarball_sha256: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ModelInfo {
    pub role: String,
    pub name: String,
    pub sha256: String,
}

pub trait Backend: Send + Sync + 'static {
    fn capabilities(&self) -> &Capabilities;
    fn info(&self) -> &BackendInfo;
    /// Synthesize one chunk. Must stop promptly and return `Cancelled` when `cancel`
    /// fires.
    fn synthesize_chunk<'a>(
        &'a self,
        req: &'a ChunkRequest,
        cancel: &'a CancellationToken,
    ) -> BoxFuture<'a, Result<ChunkOutput, BackendError>>;
}

impl std::fmt::Debug for dyn Backend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Backend")
            .field("id", &self.capabilities().backend_id)
            .finish()
    }
}
