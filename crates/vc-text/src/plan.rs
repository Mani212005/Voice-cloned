use crate::normalize::normalize;

/// Sentence terminators: Latin `.?!`, the Devanagari danda and double danda, and the
/// ellipsis. A terminator only ends a sentence at the end of a word, so `3.5` stays whole.
const SENTENCE_END: &[char] = &['.', '?', '!', '\u{0964}', '\u{0965}', '\u{2026}'];
/// Clause punctuation where an over-long sentence may be cut.
const CLAUSE_END: &[char] = &[',', ';', ':', '\u{060C}', '\u{2013}', '\u{2014}'];
/// Closing quotes and brackets that may follow a terminator inside the same word.
const CLOSERS: &[char] = &['"', '\'', ')', ']', '\u{201D}', '\u{2019}', '\u{00BB}'];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PlanLimits {
    pub max_words: usize,
    pub max_chars: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Chunk {
    pub idx: usize,
    pub text: String,
    pub words: usize,
    pub chars: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChunkPlan {
    pub chunks: Vec<Chunk>,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PlanError {
    #[error("text is empty")]
    Empty,
    #[error("a single word is longer than {max_chars} characters")]
    Unsplittable { max_chars: usize },
    #[error("invalid limits")]
    InvalidLimits,
}

/// Normalize `text` and cut it into engine chunks within `limits`.
///
/// Sentences are packed greedily. A sentence over the limits is cut at the last clause
/// punctuation that fits, otherwise at the last word that fits. Joining the chunk texts
/// with single spaces gives the normalized text with line breaks turned into spaces.
pub fn plan(text: &str, limits: PlanLimits) -> Result<ChunkPlan, PlanError> {
    if limits.max_words == 0 || limits.max_chars == 0 {
        return Err(PlanError::InvalidLimits);
    }
    let normalized = normalize(text);
    let sentences = sentences(&normalized);
    if sentences.is_empty() {
        return Err(PlanError::Empty);
    }

    let mut chunks: Vec<Vec<&str>> = Vec::new();
    let mut current: Vec<&str> = Vec::new();
    for sentence in sentences {
        if fits(&current, &sentence, limits) {
            current.extend_from_slice(&sentence);
            continue;
        }
        if !current.is_empty() {
            chunks.push(std::mem::take(&mut current));
        }
        let mut rest: &[&str] = &sentence;
        while !fits(&[], rest, limits) {
            let cut = cut_point(rest, limits)?;
            chunks.push(rest[..cut].to_vec());
            rest = &rest[cut..];
        }
        current.extend_from_slice(rest);
    }
    if !current.is_empty() {
        chunks.push(current);
    }

    Ok(ChunkPlan {
        chunks: chunks
            .into_iter()
            .enumerate()
            .map(|(idx, words)| {
                let text = words.join(" ");
                Chunk {
                    idx,
                    chars: text.chars().count(),
                    words: words.len(),
                    text,
                }
            })
            .collect(),
    })
}

/// Split normalized text into sentences, each a list of words.
fn sentences(normalized: &str) -> Vec<Vec<&str>> {
    let mut out = Vec::new();
    for line in normalized.split('\n') {
        let mut current = Vec::new();
        for word in line.split(' ').filter(|w| !w.is_empty()) {
            current.push(word);
            if ends_with_any(word, SENTENCE_END) {
                out.push(std::mem::take(&mut current));
            }
        }
        if !current.is_empty() {
            out.push(current);
        }
    }
    out
}

fn ends_with_any(word: &str, set: &[char]) -> bool {
    word.trim_end_matches(CLOSERS)
        .chars()
        .next_back()
        .is_some_and(|c| set.contains(&c))
}

fn chars_of(words: &[&str]) -> usize {
    let letters: usize = words.iter().map(|w| w.chars().count()).sum();
    letters + words.len().saturating_sub(1)
}

fn fits(current: &[&str], extra: &[&str], limits: PlanLimits) -> bool {
    let words = current.len() + extra.len();
    let chars = match (current.is_empty(), extra.is_empty()) {
        (true, _) => chars_of(extra),
        (false, true) => chars_of(current),
        (false, false) => chars_of(current) + 1 + chars_of(extra),
    };
    words <= limits.max_words && chars <= limits.max_chars
}

/// Where to cut an over-long sentence: after the last clause punctuation inside the
/// longest prefix that fits, else after that prefix itself.
fn cut_point(words: &[&str], limits: PlanLimits) -> Result<usize, PlanError> {
    let mut longest = 0;
    while longest < words.len() && fits(&[], &words[..=longest], limits) {
        longest += 1;
    }
    if longest == 0 {
        return Err(PlanError::Unsplittable {
            max_chars: limits.max_chars,
        });
    }
    let clause = (1..=longest)
        .rev()
        .find(|&n| ends_with_any(words[n - 1], CLAUSE_END));
    Ok(clause.unwrap_or(longest))
}

#[cfg(test)]
mod tests {
    use super::*;

    const TURBO2: PlanLimits = PlanLimits {
        max_words: 19,
        max_chars: 200,
    };

    fn texts(p: &ChunkPlan) -> Vec<&str> {
        p.chunks.iter().map(|c| c.text.as_str()).collect()
    }

    #[test]
    fn packs_short_sentences_together() {
        let p = plan("Hi there. How are you? Fine!", TURBO2).unwrap();
        assert_eq!(texts(&p), ["Hi there. How are you? Fine!"]);
        assert_eq!(p.chunks[0].words, 6);
    }

    #[test]
    fn splits_at_sentence_boundary_when_the_next_would_overflow() {
        let a = "one two three four five six seven eight nine ten.";
        let b =
            "eleven twelve thirteen fourteen fifteen sixteen seventeen eighteen nineteen twenty.";
        let p = plan(&format!("{a} {b}"), TURBO2).unwrap();
        assert_eq!(texts(&p), [a, b]);
    }

    #[test]
    fn devanagari_danda_ends_a_sentence() {
        let limits = PlanLimits {
            max_words: 3,
            max_chars: 200,
        };
        let p = plan("मैं ठीक हूँ। आप कैसे हैं?", limits).unwrap();
        assert_eq!(texts(&p), ["मैं ठीक हूँ।", "आप कैसे हैं?"]);
    }

    #[test]
    fn long_sentence_is_cut_at_the_last_comma_that_fits() {
        let s = "w1 w2 w3 w4 w5, w6 w7 w8 w9 w10 w11 w12, w13 w14 w15 w16 w17 w18 w19 w20 w21 w22.";
        let p = plan(s, TURBO2).unwrap();
        assert_eq!(
            texts(&p),
            [
                "w1 w2 w3 w4 w5, w6 w7 w8 w9 w10 w11 w12,",
                "w13 w14 w15 w16 w17 w18 w19 w20 w21 w22."
            ]
        );
    }

    #[test]
    fn long_sentence_without_commas_is_cut_at_the_word_cap() {
        let words: Vec<String> = (1..=40).map(|i| format!("w{i}")).collect();
        let p = plan(&words.join(" "), TURBO2).unwrap();
        let counts: Vec<usize> = p.chunks.iter().map(|c| c.words).collect();
        assert_eq!(counts, [19, 19, 2]);
    }

    #[test]
    fn remainder_of_a_cut_sentence_packs_with_the_next() {
        let limits = PlanLimits {
            max_words: 4,
            max_chars: 200,
        };
        let p = plan("a b c d e f. g.", limits).unwrap();
        assert_eq!(texts(&p), ["a b c d", "e f. g."]);
    }

    #[test]
    fn decimal_point_does_not_end_a_sentence() {
        let limits = PlanLimits {
            max_words: 3,
            max_chars: 200,
        };
        let p = plan("It costs 3.5 rupees today.", limits).unwrap();
        assert_eq!(texts(&p), ["It costs 3.5", "rupees today."]);
    }

    #[test]
    fn newline_is_a_boundary_and_joins_as_space() {
        let limits = PlanLimits {
            max_words: 2,
            max_chars: 200,
        };
        let p = plan("hello world\nsecond line", limits).unwrap();
        assert_eq!(texts(&p), ["hello world", "second line"]);
    }

    #[test]
    fn overlong_word_is_unsplittable() {
        let limits = PlanLimits {
            max_words: 19,
            max_chars: 5,
        };
        assert_eq!(
            plan("ok abcdefgh", limits),
            Err(PlanError::Unsplittable { max_chars: 5 })
        );
    }

    #[test]
    fn empty_input_is_an_error() {
        assert_eq!(plan(" \n ", TURBO2), Err(PlanError::Empty));
    }

    #[test]
    fn indices_are_sequential() {
        let words: Vec<String> = (1..=100).map(|i| format!("w{i}.")).collect();
        let p = plan(&words.join(" "), TURBO2).unwrap();
        assert!(p.chunks.iter().enumerate().all(|(i, c)| c.idx == i));
    }
}
