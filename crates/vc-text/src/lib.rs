//! Text pipeline: spoken-form normalization, whitespace normalization and the chunk planner.
//!
//! The engine is run once per chunk. The first sentence is a chunk of its own, so audio
//! starts sooner. A chunk is at most `max_words` words and `max_chars` characters (for the
//! turbo2 recipe 15 and 85 in English, 14 and 200 in Hindi, measured to stay within the
//! 1.1 GB engine budget). Chunks are cut at sentence boundaries first, then at clause punctuation,
//! then between words. English text is first rewritten into its spoken form ([`speakable`]),
//! so the caps apply to the words the engine actually says. Pure functions only.

mod normalize;
mod plan;
mod speak;

pub use normalize::normalize;
pub use plan::{Chunk, ChunkPlan, PlanError, PlanLimits, plan};
use vc_core::Language;

/// The text the engine is given for `language`: English is rewritten into its spoken form
/// (acronyms, numbers, ranges, money, units, abbreviations); Hindi and Hinglish are passed
/// through unchanged.
pub fn speakable(text: &str, language: Language) -> String {
    match language {
        Language::En => speak::speakable_en(&normalize(text)),
        Language::Hi => text.to_owned(),
    }
}

/// Plan the chunks of a job: [`speakable`], then [`plan`]. The API (at accept) and the
/// runner (at run) both call this, so they always agree.
pub fn plan_speech(
    text: &str,
    language: Language,
    limits: PlanLimits,
) -> Result<ChunkPlan, PlanError> {
    plan(&speakable(text, language), limits)
}
