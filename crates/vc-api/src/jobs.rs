use axum::Json;
use axum::extract::Path;
use axum::extract::rejection::JsonRejection;
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use serde::Deserialize;
use serde_json::Value;
use sha2::{Digest, Sha256};
use vc_core::{ErrorCode, JobId, JobState, Language, Rate, VoiceId};
use vc_engine::{CancelOutcome, EnqueueError, QueueStatus};
use vc_store::{JobRow, NewJob, StoreError};
use vc_text::PlanError;

use crate::views::JobView;
use crate::{AppState, Problem, State, parse_id, rejection, store_err};

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CreateJob {
    pub voice_id: String,
    /// `en`, or `hi` for Hindi and Hinglish.
    pub language: String,
    pub text: String,
    /// Speaking rate, 0.75-1.5 (1.2 is 20 % faster). Default 1.0, the engine's own pace.
    /// Applied as a pitch-preserving time-stretch of the synthesized audio.
    #[serde(default)]
    pub rate: Option<f64>,
}

async fn view(state: &AppState, job: &JobRow) -> Result<Value, Problem> {
    let ctx = state.engine.context();
    let chunks = ctx.store.chunks(job.id).map_err(|e| store_err(e, "job"))?;
    let position = match job.state {
        JobState::Queued => match state.engine.status(job.id).await {
            QueueStatus::Queued { position } => Some(position),
            _ => None,
        },
        _ => None,
    };
    let v = JobView::new(job, chunks, position, ctx.backend.info());
    serde_json::to_value(v).map_err(|_| Problem::internal())
}

pub async fn create(
    state: State,
    body: Result<Json<CreateJob>, JsonRejection>,
) -> Result<Response, Problem> {
    let Json(req) = body.map_err(|r| rejection(r.status(), r.body_text()))?;
    let ctx = state.engine.context();
    let caps = ctx.backend.capabilities();

    let voice_id: VoiceId = req
        .voice_id
        .parse()
        .map_err(|_| Problem::not_found("voice"))?;
    let language: Language = req
        .language
        .parse()
        .map_err(|_| Problem::new(ErrorCode::UnsupportedLanguage, "language must be en or hi"))?;
    if !caps.languages.contains(&language) {
        return Err(Problem::new(
            ErrorCode::UnsupportedLanguage,
            "the engine does not support this language",
        ));
    }
    let rate = match req.rate {
        None => Rate::NATIVE,
        Some(r) => Rate::from_f64(r).map_err(|e| Problem::invalid(e.to_string()))?,
    };
    let chars = req.text.chars().count();
    if chars > state.config.max_job_chars {
        return Err(Problem::new(
            ErrorCode::TextTooLong,
            format!(
                "text is {chars} characters; the limit is {}",
                state.config.max_job_chars
            ),
        ));
    }
    let plan =
        vc_text::plan_speech(&req.text, language, caps.plan_limits_for(language)).map_err(|e| {
            match e {
                PlanError::Empty => Problem::invalid("text is empty"),
                PlanError::Unsplittable { .. } => {
                    Problem::new(ErrorCode::UnsplittableText, e.to_string())
                }
                PlanError::InvalidLimits => Problem::internal(),
            }
        })?;

    let job = NewJob {
        id: JobId::generate(),
        voice_id,
        language,
        text_sha256: hex::encode(Sha256::digest(req.text.as_bytes())),
        text: req.text,
        chunks_total: plan.chunks.len(),
        rate,
    };
    let id = job.id;
    match state.engine.enqueue(job).await {
        Ok(_) => {}
        Err(EnqueueError::QueueFull { retry_after_s }) => {
            return Err(Problem {
                retry_after_s: Some(retry_after_s),
                ..Problem::new(ErrorCode::QueueFull, "the engine queue is full")
            });
        }
        Err(EnqueueError::VoiceNotFound) => return Err(Problem::not_found("voice")),
        Err(e) => {
            tracing::error!("enqueue: {e}");
            return Err(Problem::internal());
        }
    }
    tracing::info!(job_id = %id, voice_id = %voice_id, chunks = plan.chunks.len(), "job accepted");
    let row = ctx.store.get_job(id).map_err(|e| store_err(e, "job"))?;
    let body = view(&state, &row).await?;
    let mut headers = HeaderMap::new();
    if let Ok(loc) = HeaderValue::from_str(&format!("/v1/jobs/{id}")) {
        headers.insert(header::LOCATION, loc);
    }
    Ok((StatusCode::ACCEPTED, headers, Json(body)).into_response())
}

pub async fn get(state: State, Path(id): Path<String>) -> Result<Json<Value>, Problem> {
    let id: JobId = parse_id(&id, "job")?;
    let row = state
        .engine
        .context()
        .store
        .get_job(id)
        .map_err(|e| store_err(e, "job"))?;
    Ok(Json(view(&state, &row).await?))
}

pub async fn audio(state: State, Path(id): Path<String>) -> Result<Response, Problem> {
    let id: JobId = parse_id(&id, "job")?;
    let ctx = state.engine.context();
    let row = ctx.store.get_job(id).map_err(|e| store_err(e, "job"))?;
    if row.state != JobState::Succeeded {
        return Err(Problem::new(
            ErrorCode::JobNotReady,
            format!(
                "job is {}; audio is available once it has succeeded",
                row.state
            ),
        ));
    }
    let bytes = tokio::fs::read(ctx.data.output_path(id))
        .await
        .map_err(|e| {
            tracing::error!(job_id = %id, "read output: {e}");
            Problem::internal()
        })?;
    let mut headers = HeaderMap::new();
    headers.insert(header::CONTENT_TYPE, HeaderValue::from_static("audio/wav"));
    if let Ok(v) = HeaderValue::from_str(&format!("attachment; filename=\"{id}.wav\"")) {
        headers.insert(header::CONTENT_DISPOSITION, v);
    }
    Ok((StatusCode::OK, headers, bytes).into_response())
}

/// Cancel a queued or running job (200 with the cancelled job), or delete an ended job
/// and its audio (204).
pub async fn delete(state: State, Path(id): Path<String>) -> Result<Response, Problem> {
    let id: JobId = parse_id(&id, "job")?;
    let ctx = state.engine.context();
    if state.engine.cancel(id).await == CancelOutcome::Cancelled {
        tracing::info!(job_id = %id, "job cancelled");
        let row = ctx.store.get_job(id).map_err(|e| store_err(e, "job"))?;
        return Ok(Json(view(&state, &row).await?).into_response());
    }
    let row = ctx.store.get_job(id).map_err(|e| store_err(e, "job"))?;
    if !row.state.is_terminal() {
        // The scheduler owns every queued and running job, so this is a bug.
        tracing::error!(job_id = %id, state = %row.state, "active job unknown to the scheduler");
        return Err(Problem::internal());
    }
    vc_store::files::remove_tree(&ctx.data.job_dir(id)).map_err(|e| {
        tracing::error!(job_id = %id, "delete job files: {e}");
        Problem::internal()
    })?;
    match ctx.store.delete_job(id) {
        Ok(()) | Err(StoreError::NotFound) => {}
        Err(e) => return Err(store_err(e, "job")),
    }
    tracing::info!(job_id = %id, "job deleted");
    Ok(StatusCode::NO_CONTENT.into_response())
}
