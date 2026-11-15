//! RFC 9457 `application/problem+json` errors with a stable `code`.

use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use serde::Serialize;
use vc_core::ErrorCode;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Problem {
    pub code: ErrorCode,
    pub detail: String,
    pub retry_after_s: Option<u64>,
}

#[derive(Serialize)]
struct Body<'a> {
    #[serde(rename = "type")]
    kind: &'static str,
    title: &'static str,
    status: u16,
    code: &'static str,
    detail: &'a str,
}

impl Problem {
    pub fn new(code: ErrorCode, detail: impl Into<String>) -> Self {
        Self {
            code,
            detail: detail.into(),
            retry_after_s: None,
        }
    }

    pub fn invalid(detail: impl Into<String>) -> Self {
        Self::new(ErrorCode::InvalidRequest, detail)
    }

    pub fn not_found(what: &str) -> Self {
        Self::new(ErrorCode::NotFound, format!("{what} not found"))
    }

    pub fn internal() -> Self {
        Self::new(ErrorCode::Internal, "internal error")
    }
}

fn title(code: ErrorCode) -> &'static str {
    use ErrorCode::*;
    match code {
        InvalidRequest => "Invalid request",
        NotFound => "Not found",
        ConsentRequired => "Consent required",
        VoiceInUse => "Voice in use",
        JobNotReady => "Job not ready",
        PayloadTooLarge => "Payload too large",
        TextTooLong => "Text too long",
        ReferenceInvalid => "Reference audio invalid",
        UnsplittableText => "Text cannot be split into chunks",
        UnsupportedLanguage => "Unsupported language",
        QueueFull => "Queue full",
        EngineCrashed => "Engine crashed",
        EngineOutputInvalid => "Engine output invalid",
        MemoryBudgetExceeded => "Engine memory budget exceeded",
        Cancelled => "Cancelled",
        Internal => "Internal error",
    }
}

impl IntoResponse for Problem {
    fn into_response(self) -> Response {
        let status = StatusCode::from_u16(self.code.http_status())
            .unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
        let body = Body {
            kind: "about:blank",
            title: title(self.code),
            status: status.as_u16(),
            code: self.code.as_str(),
            detail: &self.detail,
        };
        let json = serde_json::to_vec(&body).unwrap_or_default();
        let mut resp = (status, json).into_response();
        let headers = resp.headers_mut();
        headers.insert(
            header::CONTENT_TYPE,
            HeaderValue::from_static("application/problem+json"),
        );
        if let Some(v) = self
            .retry_after_s
            .and_then(|s| HeaderValue::from_str(&s.to_string()).ok())
        {
            headers.insert(header::RETRY_AFTER, v);
        }
        resp
    }
}
