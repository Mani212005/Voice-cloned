use axum::Json;
use axum::extract::rejection::JsonRejection;
use axum::http::StatusCode;
use serde::Deserialize;
use vc_core::Language;
use vc_store::{ConsentRecord, NewConsent};

use crate::{Problem, State, label, rejection, store_err};

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CreateConsent {
    pub speaker_name: String,
    /// v1 enrolls the owner's own voice only, so this must be `self`.
    pub relationship: String,
    pub attestation: String,
    pub languages: Vec<Language>,
}

pub async fn create(
    state: State,
    body: Result<Json<CreateConsent>, JsonRejection>,
) -> Result<(StatusCode, Json<ConsentRecord>), Problem> {
    let Json(req) = body.map_err(|r| rejection(r.status(), r.body_text()))?;
    if req.relationship != "self" {
        return Err(Problem::invalid(
            "relationship must be \"self\": v1 clones only the owner's own voice",
        ));
    }
    if req.languages.is_empty() {
        return Err(Problem::invalid("languages must not be empty"));
    }
    let new = NewConsent {
        speaker_name: label(&req.speaker_name, "speaker_name", 100)?,
        relationship: req.relationship,
        attestation: label(&req.attestation, "attestation", 2_000)?,
        languages: req.languages,
    };
    let ctx = state.engine.context();
    let record = ctx
        .store
        .insert_consent(new, ctx.clock.now_ms())
        .map_err(|e| store_err(e, "consent"))?;
    tracing::info!(consent_id = %record.id, seq = record.seq, "consent recorded");
    Ok((StatusCode::CREATED, Json(record)))
}
