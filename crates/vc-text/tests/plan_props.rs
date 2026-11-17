//! Property tests for the chunk planner. The RNG seed is fixed so every run explores the
//! same cases; failures are reproducible without a regressions file.

#![allow(clippy::unwrap_used)]

use proptest::prelude::*;
use proptest::test_runner::{Config, RngSeed};
use vc_text::{PlanError, PlanLimits, normalize, plan};

fn config() -> Config {
    Config {
        cases: 512,
        rng_seed: RngSeed::Fixed(0x5eed_0019),
        failure_persistence: None,
        ..Config::default()
    }
}

/// Words drawn from Latin, Devanagari and punctuation-heavy pieces, so sentences,
/// clauses and mixed scripts (Hinglish) all occur.
fn word() -> impl Strategy<Value = String> {
    prop_oneof![
        "[a-zA-Z]{1,9}",
        "[\u{0915}-\u{0939}\u{093E}-\u{094C}]{1,6}",
        "[a-z]{1,6}[.,?!;:]",
        "[\u{0915}-\u{0939}]{1,5}\u{0964}",
        "[0-9]{1,3}\\.[0-9]{1,2}",
    ]
}

fn separator() -> impl Strategy<Value = &'static str> {
    prop_oneof![
        Just(" "),
        Just("  "),
        Just("\t"),
        Just("\n"),
        Just(" \n\n ")
    ]
}

fn text() -> impl Strategy<Value = String> {
    prop::collection::vec((word(), separator()), 1..120).prop_map(|parts| {
        parts
            .into_iter()
            .map(|(w, s)| format!("{w}{s}"))
            .collect::<String>()
    })
}

fn limits() -> impl Strategy<Value = PlanLimits> {
    (1usize..30, 12usize..250).prop_map(|(max_words, max_chars)| PlanLimits {
        max_words,
        max_chars,
    })
}

proptest! {
    #![proptest_config(config())]

    #[test]
    fn chunks_respect_limits_and_reassemble_the_input(t in text(), l in limits()) {
        match plan(&t, l) {
            Ok(p) => {
                prop_assert!(!p.chunks.is_empty());
                for (i, c) in p.chunks.iter().enumerate() {
                    prop_assert_eq!(c.idx, i);
                    prop_assert!(c.words >= 1 && c.words <= l.max_words);
                    prop_assert!(c.chars <= l.max_chars);
                    prop_assert_eq!(c.words, c.text.split(' ').count());
                    prop_assert_eq!(c.chars, c.text.chars().count());
                    prop_assert_eq!(c.text.trim(), c.text.as_str());
                }
                let joined: Vec<&str> = p.chunks.iter().map(|c| c.text.as_str()).collect();
                prop_assert_eq!(joined.join(" "), normalize(&t).replace('\n', " "));
            }
            Err(PlanError::Unsplittable { max_chars }) => {
                prop_assert_eq!(max_chars, l.max_chars);
                let longest = normalize(&t)
                    .split_whitespace()
                    .map(|w| w.chars().count())
                    .max()
                    .unwrap_or(0);
                prop_assert!(longest > l.max_chars);
            }
            Err(e) => prop_assert!(false, "unexpected error {e:?}"),
        }
    }

    #[test]
    fn the_first_chunk_never_reaches_past_the_first_sentence(t in text(), l in limits()) {
        if let Ok(p) = plan(&t, l) {
            let normalized = normalize(&t);
            let first_line = normalized.split('\n').find(|l| !l.trim().is_empty()).unwrap();
            let words: Vec<&str> = p.chunks[0].text.split(' ').collect();
            // Within the first line, and no sentence ends before its last word.
            prop_assert!(first_line.trim_start().starts_with(p.chunks[0].text.as_str()));
            let ends = ['.', '?', '!', '\u{0964}'];
            for w in &words[..words.len() - 1] {
                prop_assert!(!w.ends_with(ends), "first chunk {:?} spans sentences", p.chunks[0].text);
            }
        }
    }

    #[test]
    fn planning_is_deterministic(t in text(), l in limits()) {
        prop_assert_eq!(plan(&t, l), plan(&t, l));
    }

    #[test]
    fn normalization_is_idempotent(t in text()) {
        let once = normalize(&t);
        prop_assert_eq!(normalize(&once), once);
    }

    #[test]
    fn a_chunk_ends_at_a_boundary_unless_forced(t in text()) {
        // With a generous char cap, a chunk that does not end at a sentence or clause mark
        // or a line end must have been cut at the word cap, so it is exactly full.
        let l = PlanLimits { max_words: 19, max_chars: 10_000 };
        let p = plan(&t, l).unwrap();
        let normalized = normalize(&t);
        let line_ends: std::collections::HashSet<&str> =
            normalized.lines().filter_map(|line| line.rsplit(' ').next()).collect();
        let marks = ['.', '?', '!', '\u{0964}', ',', ';', ':'];
        for c in &p.chunks[..p.chunks.len() - 1] {
            let last = c.text.rsplit(' ').next().unwrap();
            prop_assert!(
                last.ends_with(marks) || line_ends.contains(last) || c.words == l.max_words,
                "chunk {:?} ends mid-clause", c.text
            );
        }
    }
}
