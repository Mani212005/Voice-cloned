//! Response bodies.

use serde::Serialize;
use time::OffsetDateTime;
use time::macros::format_description;
use vc_backend::BackendInfo;
use vc_core::{ConsentId, ErrorCode, JobId, JobState, Language, Rate, VoiceId};
use vc_store::{ChunkRow, JobRow, VoiceRow};

pub fn rfc3339(ms: i64) -> String {
    let nanos = i128::from(ms.max(0)) * 1_000_000;
    OffsetDateTime::from_unix_timestamp_nanos(nanos)
        .unwrap_or(OffsetDateTime::UNIX_EPOCH)
        .format(format_description!(
            "[year]-[month]-[day]T[hour]:[minute]:[second].[subsecond digits:3]Z"
        ))
        .unwrap_or_default()
}

#[derive(Debug, Serialize)]
pub struct VoiceView {
    pub id: VoiceId,
    pub name: String,
    pub consent_id: ConsentId,
    pub reference_language: Language,
    pub reference_seconds: f64,
    pub reference_sample_rate_hz: u32,
    pub reference_sha256: String,
    pub created_at: String,
}

impl From<VoiceRow> for VoiceView {
    fn from(v: VoiceRow) -> Self {
        Self {
            id: v.id,
            name: v.name,
            consent_id: v.consent_id,
            reference_language: v.reference_language,
            reference_seconds: v.ref_ms as f64 / 1000.0,
            reference_sample_rate_hz: v.ref_sample_rate,
            reference_sha256: v.ref_sha256,
            created_at: rfc3339(v.created_at_ms),
        }
    }
}

#[derive(Debug, Serialize)]
pub struct Timings {
    /// Time spent queued before the engine slot was free.
    pub queued_ms: Option<i64>,
    /// From the job starting to its first chunk of audio being ready.
    pub first_audio_ms: Option<i64>,
    /// From the job starting to the job ending.
    pub run_ms: Option<i64>,
}

#[derive(Debug, Serialize)]
pub struct Provenance<'a> {
    pub text_sha256: &'a str,
    pub output_sha256: Option<&'a str>,
    #[serde(flatten)]
    pub backend: &'a BackendInfo,
}

#[derive(Debug, Serialize)]
pub struct JobView<'a> {
    pub id: JobId,
    pub voice_id: VoiceId,
    pub state: JobState,
    pub language: Language,
    /// Speaking rate the audio is delivered at (1.0 is the engine's own pace).
    pub rate: Rate,
    /// 1-based position in the queue while queued.
    pub queue_position: Option<usize>,
    pub chunks_total: usize,
    pub chunks_done: usize,
    pub attempts: u32,
    pub error_code: Option<ErrorCode>,
    pub audio_seconds: Option<f64>,
    /// Highest engine peak RSS of any chunk so far, in KiB.
    pub peak_rss_kib: Option<u64>,
    pub created_at: String,
    pub started_at: Option<String>,
    pub finished_at: Option<String>,
    pub expires_at: Option<String>,
    pub timings: Timings,
    pub chunks: Vec<ChunkRow>,
    pub provenance: Provenance<'a>,
}

impl<'a> JobView<'a> {
    pub fn new(
        job: &'a JobRow,
        chunks: Vec<ChunkRow>,
        queue_position: Option<usize>,
        backend: &'a BackendInfo,
    ) -> Self {
        let since_start = |t: Option<i64>| Some(t? - job.started_at_ms?);
        Self {
            id: job.id,
            voice_id: job.voice_id,
            state: job.state,
            language: job.language,
            rate: job.rate,
            queue_position,
            chunks_total: job.chunks_total,
            chunks_done: job.chunks_done,
            attempts: job.attempts,
            error_code: job.error_code,
            audio_seconds: job.audio_ms.map(|ms| ms as f64 / 1000.0),
            peak_rss_kib: chunks.iter().map(|c| c.peak_rss_kib).max(),
            created_at: rfc3339(job.created_at_ms),
            started_at: job.started_at_ms.map(rfc3339),
            finished_at: job.finished_at_ms.map(rfc3339),
            expires_at: job.expires_at_ms.map(rfc3339),
            timings: Timings {
                queued_ms: job.started_at_ms.map(|s| s - job.created_at_ms),
                first_audio_ms: since_start(job.first_audio_at_ms),
                run_ms: since_start(job.finished_at_ms),
            },
            chunks,
            provenance: Provenance {
                text_sha256: &job.text_sha256,
                output_sha256: job.output_sha256.as_deref(),
                backend,
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_rfc3339() {
        assert_eq!(rfc3339(0), "1970-01-01T00:00:00.000Z");
        assert_eq!(rfc3339(1_763_200_000_123), "2025-11-15T09:46:40.123Z");
    }
}
