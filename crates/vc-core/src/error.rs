//! Stable error codes. Each maps to one HTTP status and is the `code` field of an
//! `application/problem+json` response (or a failed job's `error_code`).

use std::fmt;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    InvalidRequest,
    NotFound,
    ConsentRequired,
    VoiceInUse,
    JobNotReady,
    PayloadTooLarge,
    TextTooLong,
    ReferenceInvalid,
    UnsplittableText,
    UnsupportedLanguage,
    QueueFull,
    EngineCrashed,
    EngineOutputInvalid,
    MemoryBudgetExceeded,
    Cancelled,
    Internal,
}

impl ErrorCode {
    pub const ALL: [ErrorCode; 16] = [
        ErrorCode::InvalidRequest,
        ErrorCode::NotFound,
        ErrorCode::ConsentRequired,
        ErrorCode::VoiceInUse,
        ErrorCode::JobNotReady,
        ErrorCode::PayloadTooLarge,
        ErrorCode::TextTooLong,
        ErrorCode::ReferenceInvalid,
        ErrorCode::UnsplittableText,
        ErrorCode::UnsupportedLanguage,
        ErrorCode::QueueFull,
        ErrorCode::EngineCrashed,
        ErrorCode::EngineOutputInvalid,
        ErrorCode::MemoryBudgetExceeded,
        ErrorCode::Cancelled,
        ErrorCode::Internal,
    ];

    pub const fn http_status(self) -> u16 {
        use ErrorCode::*;
        match self {
            InvalidRequest | ConsentRequired => 400,
            NotFound => 404,
            VoiceInUse | JobNotReady | Cancelled => 409,
            PayloadTooLarge | TextTooLong => 413,
            ReferenceInvalid | UnsplittableText | UnsupportedLanguage => 422,
            QueueFull => 429,
            EngineCrashed | EngineOutputInvalid | MemoryBudgetExceeded | Internal => 500,
        }
    }

    pub const fn as_str(self) -> &'static str {
        use ErrorCode::*;
        match self {
            InvalidRequest => "invalid_request",
            NotFound => "not_found",
            ConsentRequired => "consent_required",
            VoiceInUse => "voice_in_use",
            JobNotReady => "job_not_ready",
            PayloadTooLarge => "payload_too_large",
            TextTooLong => "text_too_long",
            ReferenceInvalid => "reference_invalid",
            UnsplittableText => "unsplittable_text",
            UnsupportedLanguage => "unsupported_language",
            QueueFull => "queue_full",
            EngineCrashed => "engine_crashed",
            EngineOutputInvalid => "engine_output_invalid",
            MemoryBudgetExceeded => "memory_budget_exceeded",
            Cancelled => "cancelled",
            Internal => "internal",
        }
    }

    pub fn parse(s: &str) -> Option<ErrorCode> {
        ErrorCode::ALL.into_iter().find(|c| c.as_str() == s)
    }
}

impl fmt::Display for ErrorCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strings_match_serde_and_round_trip() {
        for code in ErrorCode::ALL {
            let json = serde_json_like(code);
            assert_eq!(json, code.as_str());
            assert_eq!(ErrorCode::parse(code.as_str()), Some(code));
        }
    }

    #[test]
    fn statuses_are_client_or_server_errors() {
        for code in ErrorCode::ALL {
            assert!((400..600).contains(&code.http_status()), "{code}");
        }
        assert_eq!(ErrorCode::QueueFull.http_status(), 429);
        assert_eq!(ErrorCode::MemoryBudgetExceeded.http_status(), 500);
    }

    /// serde's snake_case rename, checked without pulling serde_json into this crate.
    fn serde_json_like(code: ErrorCode) -> String {
        let debug = format!("{code:?}");
        let mut out = String::new();
        for (i, ch) in debug.chars().enumerate() {
            if ch.is_ascii_uppercase() {
                if i > 0 {
                    out.push('_');
                }
                out.push(ch.to_ascii_lowercase());
            } else {
                out.push(ch);
            }
        }
        out
    }
}
