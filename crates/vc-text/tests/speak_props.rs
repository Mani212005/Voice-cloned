//! Property tests for the spoken-form pass. The RNG seed is fixed so every run explores the
//! same cases; failures are reproducible without a regressions file.

#![allow(clippy::unwrap_used)]

use proptest::prelude::*;
use proptest::test_runner::{Config, RngSeed};
use vc_core::Language;
use vc_text::{PlanLimits, plan_speech, speakable};

fn config() -> Config {
    Config {
        cases: 2048,
        rng_seed: RngSeed::Fixed(0x5eed_5e4c),
        failure_persistence: None,
        ..Config::default()
    }
}

/// Tokens that exercise every rule: numbers, ranges, money, units, acronyms, dotted
/// letters, times, versions, mixed forms, symbols, punctuation and Devanagari.
fn token() -> impl Strategy<Value = String> {
    prop_oneof![
        "[a-zA-Z]{1,9}",
        "[A-Z]{1,6}s?",
        "-?[0-9]{1,16}",
        "[0-9]{1,3}(,[0-9]{3}){0,3}(\\.[0-9]{1,3})?",
        "[0-9]{1,4}[-\u{2013}][0-9]{1,4}%?",
        "[$€£₹][0-9]{1,6}(\\.[0-9]{1,3})?[KkMBmn]{0,2}",
        "[0-9]{1,4}(ms|GB|GHz|x|K|fps|th|st|nd|rd|s|G|FA)",
        "([A-Z]\\.){1,4}",
        "[0-9]{1,2}:[0-9]{2}",
        "[vV]?[0-9]{1,3}(\\.[0-9]{1,3}){1,3}",
        "[A-Z]{1,5}-[0-9]{1,3}|[a-z]{1,5}/[a-z]{1,5}|[a-z]{2,6}\\.(com|org|ai|io)",
        "[&+=@%#~/'\"(),.;:!?-]{1,3}",
        "[\u{0915}-\u{0939}\u{093E}-\u{094C}]{1,6}",
    ]
}

fn text() -> impl Strategy<Value = String> {
    prop::collection::vec(
        (token(), prop_oneof![Just(" "), Just("\n"), Just(", ")]),
        0..24,
    )
    .prop_map(|parts| parts.into_iter().map(|(t, sep)| t + sep).collect())
}

proptest! {
    #![proptest_config(config())]

    /// Rewriting spoken text again changes nothing, so the API (at accept) and the runner
    /// (at run) can never disagree, and a caller's own spelled-out text is kept.
    #[test]
    fn english_is_idempotent(t in text()) {
        let once = speakable(&t, Language::En);
        prop_assert_eq!(speakable(&once, Language::En), once);
    }

    #[test]
    fn hindi_is_passed_through(t in text()) {
        prop_assert_eq!(speakable(&t, Language::Hi), t);
    }

    /// Whatever the text, planning its spoken form either succeeds within the limits or
    /// fails cleanly.
    #[test]
    fn spoken_plans_respect_the_limits(t in text()) {
        let limits = PlanLimits::new(19, 200);
        if let Ok(p) = plan_speech(&t, Language::En, limits) {
            for c in &p.chunks {
                prop_assert!(c.words <= limits.max_words && c.chars <= limits.max_chars);
            }
        }
    }
}
