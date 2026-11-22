//! Running one job: plan, synthesize every chunk in order, assemble the output.

use sha2::{Digest, Sha256};
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;
use vc_audio::MonoPcm;
use vc_backend::{BackendError, ChunkRequest};
use vc_core::{ErrorCode, JobId};
use vc_store::ChunkRow;
use vc_text::PlanLimits;

use crate::{EngineContext, JobEvent};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum RunResult {
    Succeeded {
        audio_ms: u64,
        output_sha256: String,
    },
    Failed(ErrorCode),
    Cancelled,
}

pub(crate) async fn run_job(
    ctx: &EngineContext,
    job_id: JobId,
    cancel: &CancellationToken,
    events: &broadcast::Sender<JobEvent>,
) -> RunResult {
    match run(ctx, job_id, cancel, events).await {
        Ok(r) => r,
        Err(code) => {
            tracing::error!(job_id = %job_id, code = %code, "job failed");
            RunResult::Failed(code)
        }
    }
}

async fn run(
    ctx: &EngineContext,
    job_id: JobId,
    cancel: &CancellationToken,
    events: &broadcast::Sender<JobEvent>,
) -> Result<RunResult, ErrorCode> {
    let internal = |what: &str, e: &dyn std::fmt::Display| {
        tracing::error!(job_id = %job_id, "{what}: {e}");
        ErrorCode::Internal
    };
    let job = ctx
        .store
        .get_job(job_id)
        .map_err(|e| internal("load job", &e))?;
    let voice = ctx
        .store
        .get_voice(job.voice_id)
        .map_err(|e| internal("load voice", &e))?;
    let text = job.text.ok_or_else(|| internal("job text", &"missing"))?;
    let caps = ctx.backend.capabilities();
    let plan = vc_text::plan_speech(
        &text,
        job.language,
        PlanLimits {
            max_words: caps.max_chunk_words_for(job.language),
            max_chars: caps.max_chunk_chars,
        },
    )
    .map_err(|_| ErrorCode::UnsplittableText)?;
    if plan.chunks.len() != job.chunks_total {
        return Err(internal(
            "chunk plan",
            &"changed since the job was accepted",
        ));
    }

    // Each attempt starts from an empty work directory.
    let work_dir = ctx.data.job_dir(job_id);
    vc_store::files::remove_tree(&work_dir).map_err(|e| internal("clear job dir", &e))?;
    vc_store::files::private_dir(&work_dir).map_err(|e| internal("job dir", &e))?;

    let mut outputs: Vec<MonoPcm> = Vec::with_capacity(plan.chunks.len());
    for chunk in &plan.chunks {
        let req = ChunkRequest {
            idx: chunk.idx,
            text: chunk.text.clone(),
            language: job.language,
            reference: ctx.data.reference_path(voice.id),
            reference_language: voice.reference_language,
            work_dir: work_dir.clone(),
        };
        let mut retries_left = ctx.config.crash_retries;
        let out = loop {
            match ctx.backend.synthesize_chunk(&req, cancel).await {
                Ok(out) => break out,
                Err(BackendError::Cancelled) => return Ok(RunResult::Cancelled),
                Err(e @ BackendError::EngineCrashed { .. }) if retries_left > 0 => {
                    retries_left -= 1;
                    tracing::warn!(job_id = %job_id, chunk = chunk.idx, "engine crashed ({e}); retrying chunk");
                }
                Err(e) => {
                    let stats = e.stats();
                    tracing::error!(
                        job_id = %job_id,
                        chunk = chunk.idx,
                        peak_rss_kib = stats.map_or(0, |s| s.peak_rss_kib),
                        "chunk failed: {e}"
                    );
                    return Ok(RunResult::Failed(e.code()));
                }
            }
        };
        if cancel.is_cancelled() {
            return Ok(RunResult::Cancelled);
        }
        let pcm = if job.rate.is_native() {
            out.pcm
        } else {
            let rate = job.rate.as_f64();
            let raw = out.pcm;
            tokio::task::spawn_blocking(move || vc_audio::time_stretch(&raw, rate))
                .await
                .map_err(|e| internal("time-stretch task", &e))?
                .map_err(|e| internal("time-stretch chunk", &e))?
        };
        let wav = vc_audio::encode_wav_bytes(&pcm).map_err(|e| internal("encode chunk", &e))?;
        let path = ctx.data.chunk_path(job_id, chunk.idx);
        vc_store::files::write_private(&path, &wav).map_err(|e| internal("write chunk", &e))?;
        let row = ChunkRow {
            idx: chunk.idx,
            words: chunk.words,
            chars: chunk.chars,
            audio_ms: pcm.duration_ms(),
            peak_rss_kib: out.stats.peak_rss_kib,
            wall_ms: u64::try_from(out.stats.wall.as_millis()).unwrap_or(u64::MAX),
            out_sha256: hex::encode(Sha256::digest(&wav)),
        };
        tracing::info!(
            job_id = %job_id,
            chunk = chunk.idx,
            words = row.words,
            audio_ms = row.audio_ms,
            wall_ms = row.wall_ms,
            peak_rss_kib = row.peak_rss_kib,
            "chunk done"
        );
        ctx.store
            .record_chunk(job_id, &row, ctx.clock.now_ms())
            .map_err(|e| internal("record chunk", &e))?;
        let _ = events.send(JobEvent::ChunkDone {
            job: job_id,
            idx: chunk.idx,
        });
        outputs.push(pcm);
    }

    // The pause between chunks scales with the rate, like the speech around it.
    let gap_ms = (f64::from(ctx.config.inter_chunk_gap_ms) / job.rate.as_f64()).round() as u32;
    let joined =
        vc_audio::concat_with_gap(&outputs, gap_ms).map_err(|_| ErrorCode::EngineOutputInvalid)?;
    let wav = vc_audio::encode_wav_bytes(&joined).map_err(|e| internal("encode output", &e))?;
    vc_store::files::write_private(&ctx.data.output_path(job_id), &wav)
        .map_err(|e| internal("write output", &e))?;
    Ok(RunResult::Succeeded {
        audio_ms: joined.duration_ms(),
        output_sha256: hex::encode(Sha256::digest(&wav)),
    })
}
