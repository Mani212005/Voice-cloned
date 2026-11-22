//! In-process test backend. Output is a pure function of the chunk text; every step a
//! test needs to observe is signalled on a channel, and a gated backend waits for an
//! explicit release instead of sleeping.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use tokio::sync::{Semaphore, mpsc};
use tokio_util::sync::CancellationToken;
use vc_audio::MonoPcm;
use vc_core::Language;

use crate::{
    Backend, BackendError, BackendInfo, BoxFuture, Capabilities, ChunkOutput, ChunkRequest,
    ChunkStats, ExitInfo, VoiceRef,
};

pub const FAKE_SAMPLE_RATE: u32 = 24_000;
/// Samples of audio per word of text.
pub const FAKE_SAMPLES_PER_WORD: usize = 2_400;

/// What the next chunk does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FakeOutcome {
    Ok,
    Crash,
    OverBudget,
    BadOutput,
}

#[derive(Debug, Default)]
struct State {
    script: VecDeque<FakeOutcome>,
    calls: Vec<ChunkRequest>,
}

#[derive(Debug)]
pub struct FakeBackend {
    capabilities: Capabilities,
    info: BackendInfo,
    state: Mutex<State>,
    started: Option<mpsc::UnboundedSender<ChunkRequest>>,
    gate: Option<Arc<Semaphore>>,
    warm: Option<(mpsc::UnboundedSender<WarmEvent>, Arc<Semaphore>)>,
}

/// What a [`FakeBackend`] with a warm gate reports about `prepare` and `release`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WarmEvent {
    /// `prepare` started and now waits for [`WarmControl::release_prepare`] or cancel.
    PrepareStarted(VoiceRef),
    PrepareEnded(VoiceRef, Result<(), BackendError>),
    Released(Option<PathBuf>),
}

/// Test-side handle of a [`FakeBackend`] with a warm gate.
#[derive(Debug)]
pub struct WarmControl {
    events: mpsc::UnboundedReceiver<WarmEvent>,
    gate: Arc<Semaphore>,
}

impl WarmControl {
    pub async fn next(&mut self) -> Option<WarmEvent> {
        self.events.recv().await
    }

    /// Let `n` started warm-ups finish.
    pub fn release_prepare(&self, n: usize) {
        self.gate.add_permits(n);
    }
}

/// Test-side handle of a gated [`FakeBackend`].
#[derive(Debug)]
pub struct FakeControl {
    started: mpsc::UnboundedReceiver<ChunkRequest>,
    gate: Arc<Semaphore>,
}

impl FakeControl {
    /// Wait until the backend has started its next chunk.
    pub async fn next_started(&mut self) -> Option<ChunkRequest> {
        self.started.recv().await
    }

    /// Let `n` started chunks finish.
    pub fn release(&self, n: usize) {
        self.gate.add_permits(n);
    }
}

impl Default for FakeBackend {
    fn default() -> Self {
        Self::new()
    }
}

impl FakeBackend {
    pub fn new() -> Self {
        Self::with_limits(19, 200)
    }

    pub fn with_limits(max_chunk_words: usize, max_chunk_chars: usize) -> Self {
        Self {
            capabilities: Capabilities {
                backend_id: "fake".into(),
                languages: Language::ALL.to_vec(),
                sample_rate_hz: FAKE_SAMPLE_RATE,
                channels: 1,
                max_chunk_words,
                max_chunk_words_per_language: Default::default(),
                max_chunk_chars,
                max_chunk_chars_per_language: Default::default(),
                char_weights_per_language: Default::default(),
                concurrency: 1,
                ram_budget_kib: 1_100_000,
                deterministic: true,
                watermark: "none",
                reference_min_s: 5.0,
                reference_max_s: 30.0,
            },
            info: BackendInfo {
                backend_id: "fake".into(),
                runtime: None,
                models: vec![],
                seed: 0,
                tts_steps: 0,
                threads: 1,
            },
            state: Mutex::default(),
            started: None,
            gate: None,
            warm: None,
        }
    }

    /// `prepare` reports on a channel and waits for [`WarmControl::release_prepare`] (or
    /// cancellation); `release` reports too. Without this both return at once.
    pub fn warm_gated(mut self) -> (Self, WarmControl) {
        let (tx, events) = mpsc::unbounded_channel();
        let gate = Arc::new(Semaphore::new(0));
        self.warm = Some((tx, gate.clone()));
        (self, WarmControl { events, gate })
    }

    /// A lower word cap for `language`.
    pub fn with_language_cap(mut self, language: Language, max_chunk_words: usize) -> Self {
        self.capabilities
            .max_chunk_words_per_language
            .insert(language, max_chunk_words);
        self
    }

    /// Every chunk waits for [`FakeControl::release`] (or cancellation) before finishing.
    pub fn gated(mut self) -> (Self, FakeControl) {
        let (tx, rx) = mpsc::unbounded_channel();
        let gate = Arc::new(Semaphore::new(0));
        self.started = Some(tx);
        self.gate = Some(gate.clone());
        (self, FakeControl { started: rx, gate })
    }

    /// Outcomes for the next chunks, in order; afterwards chunks succeed.
    pub fn script(&self, outcomes: impl IntoIterator<Item = FakeOutcome>) {
        self.lock().script.extend(outcomes);
    }

    /// Every chunk request seen so far.
    pub fn calls(&self) -> Vec<ChunkRequest> {
        self.lock().calls.clone()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    async fn run(
        &self,
        req: &ChunkRequest,
        cancel: &CancellationToken,
    ) -> Result<ChunkOutput, BackendError> {
        if cancel.is_cancelled() {
            return Err(BackendError::Cancelled);
        }
        self.lock().calls.push(req.clone());
        if let Some(tx) = &self.started {
            let _ = tx.send(req.clone());
        }
        if let Some(gate) = &self.gate {
            tokio::select! {
                permit = gate.acquire() => {
                    if let Ok(p) = permit {
                        p.forget();
                    }
                }
                () = cancel.cancelled() => return Err(BackendError::Cancelled),
            }
        }
        let outcome = self.lock().script.pop_front().unwrap_or(FakeOutcome::Ok);
        let stats = ChunkStats {
            wall: Duration::ZERO,
            peak_rss_kib: 0,
        };
        match outcome {
            FakeOutcome::Ok => Ok(ChunkOutput {
                pcm: fake_pcm(&req.text),
                stats,
            }),
            FakeOutcome::Crash => Err(BackendError::EngineCrashed {
                exit: ExitInfo::Signal(libc::SIGSEGV),
                stats,
            }),
            FakeOutcome::OverBudget => Err(BackendError::MemoryBudgetExceeded {
                budget_kib: self.capabilities.ram_budget_kib,
                stats: ChunkStats {
                    peak_rss_kib: self.capabilities.ram_budget_kib + 1,
                    ..stats
                },
            }),
            FakeOutcome::BadOutput => Err(BackendError::OutputInvalid {
                reason: "fake bad output".into(),
                stats,
            }),
        }
    }
}

impl FakeBackend {
    async fn run_prepare(
        &self,
        voice: &VoiceRef,
        cancel: &CancellationToken,
    ) -> Result<(), BackendError> {
        let Some((tx, gate)) = &self.warm else {
            return Ok(());
        };
        let _ = tx.send(WarmEvent::PrepareStarted(voice.clone()));
        let result = tokio::select! {
            permit = gate.acquire() => {
                if let Ok(p) = permit {
                    p.forget();
                }
                Ok(())
            }
            () = cancel.cancelled() => Err(BackendError::Cancelled),
        };
        let _ = tx.send(WarmEvent::PrepareEnded(voice.clone(), result.clone()));
        result
    }
}

impl Backend for FakeBackend {
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
        Box::pin(self.run(req, cancel))
    }

    fn prepare<'a>(
        &'a self,
        voice: &'a VoiceRef,
        cancel: &'a CancellationToken,
    ) -> BoxFuture<'a, Result<(), BackendError>> {
        Box::pin(self.run_prepare(voice, cancel))
    }

    fn release<'a>(&'a self, reference: Option<&'a Path>) -> BoxFuture<'a, ()> {
        if let Some((tx, _)) = &self.warm {
            let _ = tx.send(WarmEvent::Released(reference.map(Path::to_owned)));
        }
        Box::pin(async {})
    }
}

/// Deterministic audio for `text`: a tone whose pitch is keyed on the text hash, one
/// tenth of a second per word.
pub fn fake_pcm(text: &str) -> MonoPcm {
    let h = fnv1a(text.as_bytes());
    let words = text.split_whitespace().count().max(1);
    let freq = 200.0 + (h % 400) as f64;
    let rate = f64::from(FAKE_SAMPLE_RATE);
    let samples = (0..words * FAKE_SAMPLES_PER_WORD)
        .map(|i| (8_000.0 * (2.0 * std::f64::consts::PI * freq * i as f64 / rate).sin()) as i16)
        .collect();
    MonoPcm {
        sample_rate: FAKE_SAMPLE_RATE,
        samples,
    }
}

fn fnv1a(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf2_9ce4_8422_2325, |h, &b| {
        (h ^ u64::from(b)).wrapping_mul(0x0000_0100_0000_01b3)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn req(text: &str) -> ChunkRequest {
        ChunkRequest {
            idx: 0,
            text: text.into(),
            language: Language::En,
            reference: "/r".into(),
            reference_language: Language::En,
            work_dir: "/w".into(),
        }
    }

    #[tokio::test]
    async fn output_is_deterministic_and_sized_by_words() {
        let b = FakeBackend::new();
        let c = CancellationToken::new();
        let a1 = b.synthesize_chunk(&req("one two three"), &c).await.unwrap();
        let a2 = b.synthesize_chunk(&req("one two three"), &c).await.unwrap();
        assert_eq!(a1.pcm, a2.pcm);
        assert_eq!(a1.pcm.samples.len(), 3 * FAKE_SAMPLES_PER_WORD);
        assert_eq!(b.calls().len(), 2);
    }

    #[tokio::test]
    async fn scripted_outcomes_apply_in_order() {
        let b = FakeBackend::new();
        b.script([FakeOutcome::Crash, FakeOutcome::OverBudget]);
        let c = CancellationToken::new();
        let r = req("x");
        assert!(matches!(
            b.synthesize_chunk(&r, &c).await,
            Err(BackendError::EngineCrashed { .. })
        ));
        assert!(matches!(
            b.synthesize_chunk(&r, &c).await,
            Err(BackendError::MemoryBudgetExceeded { .. })
        ));
        assert!(b.synthesize_chunk(&r, &c).await.is_ok());
    }

    #[tokio::test]
    async fn gated_chunk_waits_for_release_or_cancel() {
        let (b, mut ctl) = FakeBackend::new().gated();
        let b = Arc::new(b);
        let cancel = CancellationToken::new();
        let task = {
            let (b, cancel) = (b.clone(), cancel.clone());
            tokio::spawn(async move { b.synthesize_chunk(&req("hi"), &cancel).await })
        };
        assert_eq!(ctl.next_started().await.unwrap().text, "hi");
        cancel.cancel();
        assert_eq!(task.await.unwrap(), Err(BackendError::Cancelled));

        let task = {
            let b = b.clone();
            tokio::spawn(async move {
                b.synthesize_chunk(&req("again"), &CancellationToken::new())
                    .await
            })
        };
        ctl.next_started().await.unwrap();
        ctl.release(1);
        assert!(task.await.unwrap().is_ok());
    }
}
