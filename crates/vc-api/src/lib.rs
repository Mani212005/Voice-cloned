//! The `vcd` HTTP API (v1).
//!
//! ```text
//! GET    /v1/health                liveness
//! GET    /v1/version               app, backend, runtime pin, model hashes, consent chain head
//! GET    /v1/models                backend capability declaration
//! POST   /v1/consents              JSON consent record (own voice only in v1)
//! GET    /v1/consents/{id}
//! POST   /v1/voices                multipart: consent_id, name, reference_language, file (WAV)
//! GET    /v1/voices
//! GET    /v1/voices/{id}
//! DELETE /v1/voices/{id}           409 voice_in_use while a job uses it
//! POST   /v1/jobs                  JSON {voice_id, language, text} -> 202 + Location
//! GET    /v1/jobs
//! GET    /v1/jobs/{id}             state, queue position, progress, timings, peak RSS
//! GET    /v1/jobs/{id}/audio       final WAV once succeeded
//! DELETE /v1/jobs/{id}             cancel (kills the engine) or delete an ended job
//! ```

use std::sync::Arc;

use axum::Router;
use axum::extract::DefaultBodyLimit;
use axum::routing::get;
use vc_engine::Engine;

mod consents;
mod jobs;
mod meta;
mod problem;
mod views;
mod voices;

pub use problem::Problem;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ApiConfig {
    /// Upper bound for any request body (voice uploads are the large ones).
    pub max_body_bytes: usize,
    /// Upper bound for a job's text, in characters.
    pub max_job_chars: usize,
}

impl Default for ApiConfig {
    fn default() -> Self {
        Self {
            max_body_bytes: 25_000_000,
            max_job_chars: 2_000,
        }
    }
}

#[derive(Debug, Clone)]
pub struct AppState {
    pub engine: Engine,
    pub config: ApiConfig,
    pub app_version: &'static str,
}

pub fn router(state: AppState) -> Router {
    let limit = state.config.max_body_bytes;
    Router::new()
        .route("/v1/health", get(meta::health))
        .route("/v1/version", get(meta::version))
        .route("/v1/models", get(meta::models))
        .route("/v1/consents", axum::routing::post(consents::create))
        .route("/v1/consents/{id}", get(consents::get))
        .route("/v1/voices", get(voices::list).post(voices::create))
        .route("/v1/voices/{id}", get(voices::get).delete(voices::delete))
        .route("/v1/jobs", get(jobs::list).post(jobs::create))
        .route("/v1/jobs/{id}", get(jobs::get).delete(jobs::delete))
        .route("/v1/jobs/{id}/audio", get(jobs::audio))
        .fallback(|| async { Problem::not_found("route") })
        .layer(DefaultBodyLimit::max(limit))
        .with_state(Arc::new(state))
}

type State = axum::extract::State<Arc<AppState>>;

/// Parse a path id, answering 404 for anything that is not a well-formed id.
fn parse_id<T: std::str::FromStr>(raw: &str, what: &str) -> Result<T, Problem> {
    raw.parse().map_err(|_| Problem::not_found(what))
}

fn store_err(e: vc_store::StoreError, what: &str) -> Problem {
    match e {
        vc_store::StoreError::NotFound => Problem::not_found(what),
        other => {
            tracing::error!("store: {other}");
            Problem::internal()
        }
    }
}

/// Map a body-extractor rejection to a problem.
fn rejection(status: axum::http::StatusCode, detail: String) -> Problem {
    if status == axum::http::StatusCode::PAYLOAD_TOO_LARGE {
        Problem::new(
            vc_core::ErrorCode::PayloadTooLarge,
            "request body too large",
        )
    } else {
        Problem::invalid(detail)
    }
}

/// Validate a short human label: 1..=max characters, no control characters.
fn label(value: &str, field: &str, max: usize) -> Result<String, Problem> {
    let v = value.trim();
    let n = v.chars().count();
    if n == 0 || n > max || v.chars().any(char::is_control) {
        return Err(Problem::invalid(format!(
            "{field} must be 1-{max} characters without control characters"
        )));
    }
    Ok(v.to_owned())
}
