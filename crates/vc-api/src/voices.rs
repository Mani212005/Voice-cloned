use axum::Json;
use axum::extract::multipart::MultipartRejection;
use axum::extract::{Multipart, Path};
use axum::http::StatusCode;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use vc_audio::ReferenceLimits;
use vc_core::{ConsentId, ErrorCode, Language, VoiceId};
use vc_store::{StoreError, VoiceRow};

use crate::views::VoiceView;
use crate::{Problem, State, label, parse_id, rejection, store_err};

#[derive(Debug, Default)]
struct Upload {
    consent_id: Option<String>,
    name: Option<String>,
    reference_language: Option<String>,
    file: Option<Vec<u8>>,
}

async fn read_upload(mut mp: Multipart) -> Result<Upload, Problem> {
    let mut up = Upload::default();
    loop {
        let field = match mp.next_field().await {
            Ok(Some(f)) => f,
            Ok(None) => break,
            Err(e) => return Err(rejection(e.status(), e.body_text())),
        };
        let name = field.name().unwrap_or_default().to_owned();
        let slot_taken = match name.as_str() {
            "file" => up.file.is_some(),
            "consent_id" => up.consent_id.is_some(),
            "name" => up.name.is_some(),
            "reference_language" => up.reference_language.is_some(),
            other => return Err(Problem::invalid(format!("unknown field {other:?}"))),
        };
        if slot_taken {
            return Err(Problem::invalid(format!(
                "field {name:?} given twice; v1 takes exactly one reference file"
            )));
        }
        let bytes = field
            .bytes()
            .await
            .map_err(|e| rejection(e.status(), e.body_text()))?;
        let text = || {
            String::from_utf8(bytes.to_vec())
                .map_err(|_| Problem::invalid(format!("field {name:?} must be UTF-8 text")))
        };
        match name.as_str() {
            "file" => up.file = Some(bytes.to_vec()),
            "consent_id" => up.consent_id = Some(text()?),
            "name" => up.name = Some(text()?),
            _ => up.reference_language = Some(text()?),
        }
    }
    Ok(up)
}

pub async fn create(
    state: State,
    mp: Result<Multipart, MultipartRejection>,
) -> Result<(StatusCode, Json<VoiceView>), Problem> {
    let mp = mp.map_err(|r| rejection(r.status(), r.body_text()))?;
    let up = read_upload(mp).await?;
    let ctx = state.engine.context();
    let caps = ctx.backend.capabilities();

    let consent_id: ConsentId = up
        .consent_id
        .as_deref()
        .ok_or_else(|| Problem::new(ErrorCode::ConsentRequired, "consent_id is required"))?
        .trim()
        .parse()
        .map_err(|_| Problem::new(ErrorCode::ConsentRequired, "consent_id is not a consent"))?;
    let consent = match ctx.store.get_consent(consent_id) {
        Ok(c) => c,
        Err(StoreError::NotFound) => {
            return Err(Problem::new(
                ErrorCode::ConsentRequired,
                "consent not found",
            ));
        }
        Err(e) => return Err(store_err(e, "consent")),
    };
    let name = label(up.name.as_deref().unwrap_or("default"), "name", 100)?;
    let reference_language: Language = up
        .reference_language
        .as_deref()
        .unwrap_or("en")
        .trim()
        .parse()
        .map_err(|_| {
            Problem::new(
                ErrorCode::UnsupportedLanguage,
                "reference_language must be en or hi",
            )
        })?;
    if !caps.languages.contains(&reference_language) {
        return Err(Problem::new(
            ErrorCode::UnsupportedLanguage,
            "the engine does not support this reference language",
        ));
    }
    if !consent.languages.contains(&reference_language) {
        return Err(Problem::new(
            ErrorCode::ConsentRequired,
            "the consent does not cover the reference language",
        ));
    }
    let bytes = up
        .file
        .ok_or_else(|| Problem::invalid("file (a WAV reference) is required"))?;

    let pcm = vc_audio::decode_wav(std::io::Cursor::new(bytes))
        .map_err(|e| Problem::new(ErrorCode::ReferenceInvalid, e.to_string()))?;
    vc_audio::validate_reference(
        &pcm,
        ReferenceLimits {
            min_s: caps.reference_min_s,
            max_s: caps.reference_max_s,
        },
    )
    .map_err(|e| Problem::new(ErrorCode::ReferenceInvalid, e.to_string()))?;
    // Only the normalized mono reference is kept; the upload itself is dropped here.
    let normalized = vc_audio::encode_wav_bytes(&pcm).map_err(|_| Problem::internal())?;

    let row = VoiceRow {
        id: VoiceId::generate(),
        consent_id,
        name,
        reference_language,
        ref_sample_rate: pcm.sample_rate,
        ref_ms: pcm.duration_ms(),
        ref_sha256: hex::encode(Sha256::digest(&normalized)),
        created_at_ms: ctx.clock.now_ms(),
    };
    let dir = ctx.data.voice_dir(row.id);
    let written = vc_store::files::private_dir(&dir).and_then(|()| {
        vc_store::files::write_private(&ctx.data.reference_path(row.id), &normalized)
    });
    if let Err(e) = written {
        tracing::error!(voice_id = %row.id, "write reference: {e}");
        let _ = vc_store::files::remove_tree(&dir);
        return Err(Problem::internal());
    }
    if let Err(e) = ctx.store.insert_voice(&row) {
        let _ = vc_store::files::remove_tree(&dir);
        return Err(store_err(e, "voice"));
    }
    tracing::info!(voice_id = %row.id, ref_ms = row.ref_ms, "voice enrolled");
    // The new voice is the one most likely to be used next.
    state.engine.prepare(row.id).await;
    Ok((StatusCode::CREATED, Json(row.into())))
}

pub async fn list(state: State) -> Result<Json<Value>, Problem> {
    let voices = state
        .engine
        .context()
        .store
        .list_voices()
        .map_err(|e| store_err(e, "voice"))?;
    let views: Vec<VoiceView> = voices.into_iter().map(Into::into).collect();
    Ok(Json(json!({ "voices": views })))
}

pub async fn get(state: State, Path(id): Path<String>) -> Result<Json<VoiceView>, Problem> {
    let id: VoiceId = parse_id(&id, "voice")?;
    let v = state
        .engine
        .context()
        .store
        .get_voice(id)
        .map_err(|e| store_err(e, "voice"))?;
    Ok(Json(v.into()))
}

pub async fn delete(state: State, Path(id): Path<String>) -> Result<StatusCode, Problem> {
    let id: VoiceId = parse_id(&id, "voice")?;
    let ctx = state.engine.context();
    match ctx.store.tombstone_voice(id, ctx.clock.now_ms()) {
        Ok(()) => {}
        Err(StoreError::Conflict(_)) => {
            return Err(Problem::new(
                ErrorCode::VoiceInUse,
                "a queued or running job uses this voice; cancel it first",
            ));
        }
        Err(e) => return Err(store_err(e, "voice")),
    }
    // A warm engine holds this voice's conditioning in memory: stop it first.
    state.engine.release_voice(id).await;
    vc_store::files::remove_tree(&ctx.data.voice_dir(id)).map_err(|e| {
        tracing::error!(voice_id = %id, "delete reference: {e}");
        Problem::internal()
    })?;
    tracing::info!(voice_id = %id, "voice deleted");
    Ok(StatusCode::NO_CONTENT)
}
