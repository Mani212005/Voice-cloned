use crate::normalize::normalize;

/// Sentence terminators: Latin `.?!`, the Devanagari danda and double danda, and the
/// ellipsis. A terminator only ends a sentence at the end of a word, so `3.5` stays whole.
const SENTENCE_END: &[char] = &['.', '?', '!', '\u{0964}', '\u{0965}', '\u{2026}'];
/// Abbreviations (lowercase, without the final period) that do not end the first sentence.
const ABBREVIATIONS: &[&str] = &[
    "dr", "mr", "mrs", "ms", "prof", "st", "no", "vs", "etc", "e.g", "i.e",
];
/// Clause punctuation where an over-long sentence may be cut.
const CLAUSE_END: &[char] = &[',', ';', ':', '\u{060C}', '\u{2013}', '\u{2014}'];
/// Closing quotes and brackets that may follow a terminator inside the same word.
const CLOSERS: &[char] = &['"', '\'', ')', ']', '\u{201D}', '\u{2019}', '\u{00BB}'];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PlanLimits {
    pub max_words: usize,
    /// Cap on a chunk's length in characters, each weighted by [`CharWeights`].
    pub max_chars: usize,
    pub weights: CharWeights,
}

impl PlanLimits {
    /// Limits where every character counts once.
    pub const fn new(max_words: usize, max_chars: usize) -> Self {
        Self {
            max_words,
            max_chars,
            weights: CharWeights::PLAIN,
        }
    }
}

/// How many characters some characters count as against `max_chars`. Text the engine is
/// given as written can take much longer to say per character than words do: a digit is
/// read as a number word, a capital of an acronym as a letter name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CharWeights {
    /// Each digit, in any script.
    pub digit: usize,
    /// Each letter of a run of two or more capitals ("API", the "GPU" of "GPUs").
    pub acronym_letter: usize,
}

impl CharWeights {
    pub const PLAIN: Self = Self {
        digit: 1,
        acronym_letter: 1,
    };
}

/// The length of `text` against `max_chars`: its characters, with digits and acronym
/// letters counted as `weights` says.
pub fn weighted_chars(text: &str, weights: CharWeights) -> usize {
    let mut total = 0;
    let mut capitals = 0;
    let close = |capitals: usize| {
        if capitals >= 2 {
            capitals * weights.acronym_letter
        } else {
            capitals
        }
    };
    for c in text.chars() {
        if c.is_uppercase() {
            capitals += 1;
            continue;
        }
        total += close(capitals);
        capitals = 0;
        total += if c.is_numeric() { weights.digit } else { 1 };
    }
    total + close(capitals)
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
    #[error(
        "a single word is longer than {max_chars} characters (a digit or an acronym letter may count as several)"
    )]
    Unsplittable { max_chars: usize },
    #[error("invalid limits")]
    InvalidLimits,
}

/// Normalize `text` and cut it into engine chunks within `limits`.
///
/// The first sentence is a chunk of its own: time to first audio grows with the length of
/// the first chunk, so a short greeting is heard while the rest is still synthesizing.
/// An initial such as `J.` or an abbreviation such as `Dr.` does not end the first sentence.
/// The following sentences are packed greedily. A sentence over the limits is cut at the
/// last clause punctuation that fits, otherwise at the last word that fits. Joining the
/// chunk texts with single spaces gives the normalized text with line breaks turned into
/// spaces.
pub fn plan(text: &str, limits: PlanLimits) -> Result<ChunkPlan, PlanError> {
    if limits.max_words == 0
        || limits.max_chars == 0
        || limits.weights.digit == 0
        || limits.weights.acronym_letter == 0
    {
        return Err(PlanError::InvalidLimits);
    }
    let normalized = normalize(text);
    let sentences = sentences(&normalized);
    if sentences.is_empty() {
        return Err(PlanError::Empty);
    }

    let mut chunks: Vec<Vec<&str>> = Vec::new();
    let mut current: Vec<&str> = Vec::new();
    for (i, sentence) in sentences.into_iter().enumerate() {
        if fits(&current, &sentence, limits) {
            current.extend_from_slice(&sentence);
        } else {
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
        if i == 0 {
            chunks.push(std::mem::take(&mut current));
        }
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
            if ends_with_any(word, SENTENCE_END) && !(out.is_empty() && is_weak_end(word)) {
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

/// A word whose only sentence mark is a period after an initial or a known abbreviation.
fn is_weak_end(word: &str) -> bool {
    let Some(stem) = word.trim_end_matches(CLOSERS).strip_suffix('.') else {
        return false;
    };
    let stem = stem.trim_start_matches(|c: char| !c.is_alphanumeric());
    let mut chars = stem.chars();
    let initial = matches!((chars.next(), chars.next()), (Some(c), None) if c.is_alphabetic());
    initial || ABBREVIATIONS.contains(&stem.to_lowercase().as_str())
}

fn chars_of(words: &[&str], weights: CharWeights) -> usize {
    let letters: usize = words.iter().map(|w| weighted_chars(w, weights)).sum();
    letters + words.len().saturating_sub(1)
}

fn fits(current: &[&str], extra: &[&str], limits: PlanLimits) -> bool {
    let words = current.len() + extra.len();
    let w = limits.weights;
    let chars = match (current.is_empty(), extra.is_empty()) {
        (true, _) => chars_of(extra, w),
        (false, true) => chars_of(current, w),
        (false, false) => chars_of(current, w) + 1 + chars_of(extra, w),
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

    const TURBO2: PlanLimits = PlanLimits::new(19, 200);

    fn texts(p: &ChunkPlan) -> Vec<&str> {
        p.chunks.iter().map(|c| c.text.as_str()).collect()
    }

    #[test]
    fn the_first_sentence_is_a_chunk_of_its_own() {
        let p = plan("Hi there. How are you? Fine!", TURBO2).unwrap();
        assert_eq!(texts(&p), ["Hi there.", "How are you? Fine!"]);
        assert_eq!(p.chunks[0].words, 2);
        let p = plan(
            "Namaste! Abhi main busy hoon. Baad mein call karta hoon.",
            TURBO2,
        )
        .unwrap();
        assert_eq!(
            texts(&p),
            [
                "Namaste!",
                "Abhi main busy hoon. Baad mein call karta hoon."
            ]
        );
        let p = plan("नमस्ते। मैं अभी व्यस्त हूँ।", TURBO2).unwrap();
        assert_eq!(texts(&p), ["नमस्ते।", "मैं अभी व्यस्त हूँ।"]);
    }

    #[test]
    fn an_abbreviation_or_initial_does_not_end_the_first_sentence() {
        let p = plan("Dr. Smith will call you back. See you.", TURBO2).unwrap();
        assert_eq!(texts(&p), ["Dr. Smith will call you back.", "See you."]);
        let p = plan("J. K. Rowling wrote it. Bye.", TURBO2).unwrap();
        assert_eq!(texts(&p), ["J. K. Rowling wrote it.", "Bye."]);
        let p = plan("Hello, (Mrs.) Rao, e.g. this one! Bye.", TURBO2).unwrap();
        assert_eq!(texts(&p), ["Hello, (Mrs.) Rao, e.g. this one!", "Bye."]);
    }

    #[test]
    fn other_marks_and_plain_words_still_end_the_first_sentence() {
        let p = plan("Namaste! Abhi main busy hoon.", TURBO2).unwrap();
        assert_eq!(texts(&p), ["Namaste!", "Abhi main busy hoon."]);
        let p = plan("Hi, thanks for calling. Please leave a message.", TURBO2).unwrap();
        assert_eq!(
            texts(&p),
            ["Hi, thanks for calling.", "Please leave a message."]
        );
        let p = plan("Dr. Rao? Yes. Bye.", TURBO2).unwrap();
        assert_eq!(texts(&p), ["Dr. Rao?", "Yes. Bye."]);
        let p = plan("Thanks, bye. Dr. Smith called.", TURBO2).unwrap();
        assert_eq!(texts(&p), ["Thanks, bye.", "Dr. Smith called."]);
    }

    #[test]
    fn a_single_sentence_is_one_chunk() {
        let p = plan("Hi there", TURBO2).unwrap();
        assert_eq!(texts(&p), ["Hi there"]);
    }

    #[test]
    fn a_long_first_sentence_is_cut_and_its_remainder_does_not_pack_with_the_next() {
        let limits = PlanLimits::new(4, 200);
        let p = plan("aa bb cc dd ee ff. gg. hh.", limits).unwrap();
        assert_eq!(texts(&p), ["aa bb cc dd", "ee ff.", "gg. hh."]);
    }

    #[test]
    fn later_short_sentences_are_packed_together() {
        let p = plan("Hi. One two. Three four. Five!", TURBO2).unwrap();
        assert_eq!(texts(&p), ["Hi.", "One two. Three four. Five!"]);
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
        let limits = PlanLimits::new(3, 200);
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
        let limits = PlanLimits::new(4, 200);
        let p = plan("xx. aa bb cc dd ee ff. gg.", limits).unwrap();
        assert_eq!(texts(&p), ["xx.", "aa bb cc dd", "ee ff. gg."]);
    }

    #[test]
    fn decimal_point_does_not_end_a_sentence() {
        let limits = PlanLimits::new(3, 200);
        let p = plan("It costs 3.5 rupees today.", limits).unwrap();
        assert_eq!(texts(&p), ["It costs 3.5", "rupees today."]);
    }

    #[test]
    fn newline_is_a_boundary_and_joins_as_space() {
        let limits = PlanLimits::new(2, 200);
        let p = plan("hello world\nsecond line", limits).unwrap();
        assert_eq!(texts(&p), ["hello world", "second line"]);
    }

    #[test]
    fn overlong_word_is_unsplittable() {
        let limits = PlanLimits::new(19, 5);
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

    const HINDI: CharWeights = CharWeights {
        digit: 5,
        acronym_letter: 3,
    };

    #[test]
    fn digits_and_acronym_letters_weigh_more() {
        assert_eq!(weighted_chars("abc def", HINDI), 7);
        assert_eq!(weighted_chars("2013", HINDI), 20);
        assert_eq!(weighted_chars("\u{0968}\u{0966}", HINDI), 10);
        // A run of two or more capitals is an acronym; one capital is not.
        assert_eq!(weighted_chars("API,", HINDI), 10);
        assert_eq!(weighted_chars("GPUs", HINDI), 10);
        assert_eq!(weighted_chars("Users", HINDI), 5);
        assert_eq!(weighted_chars("GPT-4", HINDI), 9 + 1 + 5);
        assert_eq!(weighted_chars("2013 API", CharWeights::PLAIN), 8);
    }

    #[test]
    fn weighted_characters_count_against_the_cap() {
        let limits = PlanLimits {
            max_words: 12,
            max_chars: 30,
            weights: HINDI,
        };
        let p = plan("सुनो। 2013 में 1250 लोग थे और 2014 में 4670 लोग।", limits).unwrap();
        for c in &p.chunks {
            assert!(weighted_chars(&c.text, HINDI) <= 30, "{c:?}");
        }
        assert_eq!(p.chunks.len(), 5, "{p:?}");
        let zero = PlanLimits {
            weights: CharWeights {
                digit: 0,
                acronym_letter: 1,
            },
            ..limits
        };
        assert_eq!(plan("a b", zero), Err(PlanError::InvalidLimits));
    }
}
