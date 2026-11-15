use axum::Json;
use serde_json::{Value, json};

use crate::{Problem, State};

pub async fn health() -> Json<Value> {
    Json(json!({ "status": "ok" }))
}

pub async fn version(state: State) -> Result<Json<Value>, Problem> {
    let ctx = state.engine.context();
    let head = ctx.store.consent_chain_head().map_err(|e| {
        tracing::error!("consent chain head: {e}");
        Problem::internal()
    })?;
    Ok(Json(json!({
        "app": "vcd",
        "app_version": state.app_version,
        "api": "v1",
        "backend": ctx.backend.info(),
        "consent_chain_head": head,
    })))
}

pub async fn models(state: State) -> Json<Value> {
    let caps = state.engine.context().backend.capabilities();
    Json(json!({
        "default": caps.backend_id,
        "models": [{
            "capabilities": caps,
            "streaming": "none",
            "output_formats": ["wav"],
        }],
    }))
}
