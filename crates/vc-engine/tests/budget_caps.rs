//! The turbo2 recipe's chunk caps keep measured over-budget text out of a single chunk.
//!
//! A chunk's peak RSS follows its audio length, not its word count. These English texts
//! were measured as one cold engine chunk with the owner's enrolled voice: the 19-word one
//! spoke 11.8 s and peaked at 1.272 GB, the 13-word one 8.6 s at 1.149 GB, both over the
//! 1.1 GB budget, and so did a 92-character comma list of long words (1.101 GB). The worst
//! measured chunk within 85 characters and 15 words peaked at 1.044 GB. Planned with the
//! recipe's capabilities, no chunk of these texts may exceed those bounds.

#![allow(clippy::unwrap_used)]

use vc_backend::{Backend, CrispCliBackend, CrispOptions, RecipeManifest, RuntimePin};
use vc_core::Language;
use vc_text::{PlanLimits, plan_speech};

/// 19 long words, 195 characters: 11.8 s, 1.272 GB as one chunk.
const LONG_19: &str = "Encrypted conversations remain completely private because independent \
    researchers continuously verify every implementation, publishing detailed security \
    assessments, reviews, notes, explanations.";
/// Its first 13 words, 134 characters: 8.6 s, 1.149 GB as one chunk.
const LONG_13: &str = "Encrypted conversations remain completely private because independent \
    researchers continuously verify every implementation, publishing detailed";

/// 92 characters: 7.3 s, 1.101 GB as one chunk in vcd.
const COMMA_LIST: &str = "every implementation, publishing detailed security assessments, reviews, notes, \
    explanations.";

/// The longest English chunk measured within the budget at the slowest speaking rate seen
/// (0.080 s per character); a 92-character one went over.
const MAX_SAFE_CHARS: usize = 85;
const MAX_SAFE_WORDS: usize = 15;

fn limits(language: Language) -> PlanLimits {
    let backend = CrispCliBackend::new(
        RecipeManifest::turbo2(),
        &RuntimePin::crispasr(),
        CrispOptions {
            runtime_bin: "/unused".into(),
            model_dir: "/unused".into(),
            threads: None,
            nice: 10,
            extra_env: vec![],
        },
    );
    let caps = backend.capabilities();
    PlanLimits {
        max_words: caps.max_chunk_words_for(language),
        max_chars: caps.max_chunk_chars_for(language),
    }
}

#[test]
fn measured_over_budget_english_is_split_into_chunks_that_fit() {
    for text in [LONG_19, LONG_13, COMMA_LIST] {
        // The first sentence is its own chunk, so put a short one first, as in narration.
        let text = format!("Here is why. {text}");
        let plan = plan_speech(&text, Language::En, limits(Language::En)).unwrap();
        for c in &plan.chunks {
            assert!(
                c.chars <= MAX_SAFE_CHARS && c.words <= MAX_SAFE_WORDS,
                "chunk of {} words, {} characters is longer than any measured within the budget: {:?}",
                c.words,
                c.chars,
                c.text
            );
        }
        assert!(plan.chunks.len() >= 2, "{:?}", plan.chunks);
    }
}

#[test]
fn hindi_caps_are_unchanged() {
    let l = limits(Language::Hi);
    assert_eq!((l.max_words, l.max_chars), (14, 200));
}
