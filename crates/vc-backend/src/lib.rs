//! Speech engine backends.
//!
//! A [`Backend`] synthesizes one planned text chunk at a time and declares what it can do
//! ([`Capabilities`]). Two backends run the pinned CrispASR release with the `turbo2`
//! recipe: [`CrispWarmBackend`] keeps one engine process warm (models loaded, voice
//! encoded) across chunks and jobs and restarts it before it outgrows the RAM budget;
//! [`CrispCliBackend`] runs one cold CLI process per chunk. Both produce the same PCM.
//! [`FakeBackend`] is the in-process test double; the `fake-engine` binary is a stand-in
//! for `crispasr` (CLI and server modes) used to test the real backends.

use std::collections::BTreeMap;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::time::Duration;

use serde::Serialize;
use tokio_util::sync::CancellationToken;
use vc_audio::MonoPcm;
use vc_core::{ErrorCode, Language};

pub mod crisp_cli;
pub mod crisp_warm;
pub mod fake;
pub mod manifest;
pub mod proc;
pub mod warm_policy;

pub use crisp_cli::{CrispCliBackend, CrispOptions};
pub use crisp_warm::{CrispWarmBackend, WarmOptions};
pub use fake::{FakeBackend, FakeControl, FakeOutcome, WarmControl, WarmEvent};
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

/// A voice to get ready for: its reference and the language it is spoken in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VoiceRef {
    pub reference: PathBuf,
    pub reference_language: Language,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChunkStats {
    /// Recorded for reporting, never asserted in tests.
    pub wall: Duration,
    /// Peak RSS in KiB of the engine process that ran the chunk, over its whole life so
    /// far (`ru_maxrss`, or `VmHWM` for a warm engine); 0 for in-process backends.
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
    /// Word cap of a chunk; see [`Capabilities::max_chunk_words_for`].
    pub max_chunk_words: usize,
    /// Lower word caps for some languages, so every chunk stays within the RAM budget.
    pub max_chunk_words_per_language: BTreeMap<Language, usize>,
    /// Character cap of a chunk; see [`Capabilities::max_chunk_chars_for`].
    pub max_chunk_chars: usize,
    /// Lower character caps for some languages, so every chunk stays within the RAM budget.
    pub max_chunk_chars_per_language: BTreeMap<Language, usize>,
    /// Digits and acronym letters count as several characters in some languages; see
    /// [`Capabilities::plan_limits_for`].
    pub char_weights_per_language: BTreeMap<Language, manifest::CharWeights>,
    /// Engine processes that may run at once.
    pub concurrency: u32,
    pub ram_budget_kib: u64,
    /// Output is a pure function of the inputs and the pinned seed.
    pub deterministic: bool,
    pub watermark: &'static str,
    pub reference_min_s: f64,
    pub reference_max_s: f64,
}

impl Capabilities {
    /// Word cap of a chunk in `language`.
    pub fn max_chunk_words_for(&self, language: Language) -> usize {
        self.max_chunk_words_per_language
            .get(&language)
            .copied()
            .unwrap_or(self.max_chunk_words)
    }

    /// Character cap of a chunk in `language`.
    pub fn max_chunk_chars_for(&self, language: Language) -> usize {
        self.max_chunk_chars_per_language
            .get(&language)
            .copied()
            .unwrap_or(self.max_chunk_chars)
    }

    /// The chunk planner's limits for `language`: its word and character caps, with digits
    /// and acronym letters weighted where the recipe says so.
    pub fn plan_limits_for(&self, language: Language) -> vc_text::PlanLimits {
        vc_text::PlanLimits {
            max_words: self.max_chunk_words_for(language),
            max_chars: self.max_chunk_chars_for(language),
            weights: self
                .char_weights_per_language
                .get(&language)
                .map_or(vc_text::CharWeights::PLAIN, |&w| w.into()),
        }
    }
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

    /// Get ready so the next chunk with `voice` starts warm. Called only while no job is
    /// running; must stop promptly and return `Cancelled` when `cancel` fires. Backends
    /// without warm state have nothing to do.
    fn prepare<'a>(
        &'a self,
        _voice: &'a VoiceRef,
        _cancel: &'a CancellationToken,
    ) -> BoxFuture<'a, Result<(), BackendError>> {
        Box::pin(async { Ok(()) })
    }

    /// Drop warm state: everything held for the voice with this reference, or everything
    /// when `reference` is `None`. Returns once any engine holding it has been stopped.
    fn release<'a>(&'a self, _reference: Option<&'a Path>) -> BoxFuture<'a, ()> {
        Box::pin(async {})
    }
}

impl std::fmt::Debug for dyn Backend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Backend")
            .field("id", &self.capabilities().backend_id)
            .finish()
    }
}
