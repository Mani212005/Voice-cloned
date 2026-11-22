//! The turbo2 recipe's chunk caps keep measured over-budget text out of a single chunk.
//!
//! A chunk's peak RSS follows its audio length, not its word count. These English texts
//! were measured as one cold engine chunk with the owner's enrolled voice: the 19-word one
//! spoke 11.8 s and peaked at 1.272 GB, the 13-word one 8.6 s at 1.149 GB, both over the
//! 1.1 GB budget, and so did a 92-character comma list of long words (1.101 GB). The S3Gen
//! file with its flow decoder at Q8_0 adds a constant 32 MB to every peak, so English is
//! capped at 13 words and 85 characters. Hindi is given to the engine as written and
//! speaks up to 0.123 s per character: under the earlier Hindi caps (14 words, 200
//! characters) a chunk of long English words in a Hindi sentence peaked at 1.299 GB. Its
//! digits and acronym letters take far longer still, so they count as 5 and 3 characters,
//! and Hindi is capped at 12 words and 55 such characters. Planned with the recipe's
//! capabilities, no chunk of these texts may exceed those bounds.

#![allow(clippy::unwrap_used)]

use vc_backend::{Backend, CrispCliBackend, CrispOptions, RecipeManifest, RuntimePin};
use vc_core::Language;
use vc_text::{CharWeights, PlanLimits, plan_speech, weighted_chars};

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

/// 19 short words, 115 characters: under 15 words and 85 characters one chunk held 14 of them.
const SHORT_WORDS: &str = "Yes, no, maybe, sure, fine, okay, right, wait, stop, go, now, then, here, \
    there, done, again, later, never, always.";

/// The longest English chunk measured within the budget at the slowest speaking rate seen
/// (0.080 s per character); a 92-character one went over.
const MAX_SAFE_CHARS: usize = 85;
/// The word cap that keeps short-word chunks within the budget with the Q8_0 flow decoder.
const MAX_SAFE_WORDS: usize = 13;

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
    backend.capabilities().plan_limits_for(language)
}

#[test]
fn measured_over_budget_english_is_split_into_chunks_that_fit() {
    for text in [LONG_19, LONG_13, COMMA_LIST, SHORT_WORDS] {
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

/// Hindi and Hinglish text measured over the budget as one chunk with the Q8_0 flow
/// decoder; the published Q4_K file went over on the first two as well (1.267, 1.119 GB).
const HINDI_OVER_BUDGET: [&str; 6] = [
    // 14 words, 165 characters: 1.299 GB.
    "Presentation में recommendations, responsibilities, considerations, organizations, \
     investigations, evaluations, explanations, communications सब clearly mention करना।",
    // 14 words, 117 characters: 1.152 GB.
    "Customers को reliable notifications, consistent performance और transparent \
     communication चाहिए, बाकी सब secondary है।",
    // 13 words, 105 characters: 1.113 GB.
    "Unfortunately production deployment में configuration differences की वजह से \
     compatibility problems आ गईं।",
    // 14 words, 107 characters: 1.106 GB.
    "प्रधानमंत्री ने राष्ट्रीय स्वास्थ्य कार्यक्रम के अंतर्गत ग्रामीण क्षेत्रों में चिकित्सा सुविधाओं के \
     विस्तार की घोषणा की।",
    // Digits are read as Hindi number words, about 0.48 s each. 15 words, 70 characters:
    // 12.9 s, 1.278 GB.
    "इसकी कीमत 777999 रुपये है और 2013 से 1250 nodes पर 467 tasks चलते हैं।",
    // Acronyms are spelled, about 0.26 s a letter. 13 words, 60 characters: 8.1 s, 1.118 GB.
    "हमारे API, CPU, GPU, SSD, USB, NFC और VPN layers सब ठीक हैं।",
];

/// Within the budget at the slowest rate measured, 0.123 s per character (a comma list of
/// long English words in Hindi): about 6.8 s. A 59-character one spoke 7.2 s at 1.090 GB.
const MAX_SAFE_HINDI_CHARS: usize = 55;
/// 14 short Hinglish words spoke 7.2 s; 12 of them 5.1 s.
const MAX_SAFE_HINDI_WORDS: usize = 12;
/// What a digit and an acronym letter take to say, in characters at that slowest rate.
const HINDI_WEIGHTS: CharWeights = CharWeights {
    digit: 5,
    acronym_letter: 3,
};

#[test]
fn measured_over_budget_hindi_is_split_into_chunks_that_fit() {
    for text in HINDI_OVER_BUDGET {
        let text = format!("तो सुनो। {text}");
        let plan = plan_speech(&text, Language::Hi, limits(Language::Hi)).unwrap();
        for c in &plan.chunks {
            let length = weighted_chars(&c.text, HINDI_WEIGHTS);
            assert!(
                length <= MAX_SAFE_HINDI_CHARS && c.words <= MAX_SAFE_HINDI_WORDS,
                "chunk of {} words, {length} weighted characters is longer than any measured \
                 within the budget: {:?}",
                c.words,
                c.text
            );
        }
        assert!(plan.chunks.len() >= 3, "{:?}", plan.chunks);
    }
}
