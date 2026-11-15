use unicode_normalization::UnicodeNormalization;

/// NFC-normalize, turn control characters into spaces, collapse runs of horizontal
/// whitespace to one space, trim every line and drop empty lines. Lines are joined with
/// `\n`, which the planner treats as a sentence boundary.
pub fn normalize(text: &str) -> String {
    let nfc: String = text.nfc().collect();
    let mut out = String::with_capacity(nfc.len());
    for line in nfc.lines() {
        let mut words = line
            .split(|c: char| c.is_whitespace() || (c.is_control() && c != '\n'))
            .filter(|w| !w.is_empty())
            .peekable();
        if words.peek().is_none() {
            continue;
        }
        if !out.is_empty() {
            out.push('\n');
        }
        let mut first = true;
        for w in words {
            if !first {
                out.push(' ');
            }
            out.push_str(w);
            first = false;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn collapses_whitespace_and_blank_lines() {
        assert_eq!(
            normalize("  Hello \t world  \r\n\n\n  next\u{0007}line "),
            "Hello world\nnext line"
        );
    }

    #[test]
    fn composes_devanagari_to_nfc() {
        // "क़" written as क + nukta composes to the precomposed form only where Unicode
        // allows; NFC must at least be idempotent and stable.
        let decomposed = "\u{0915}\u{093C}";
        let once = normalize(decomposed);
        assert_eq!(normalize(&once), once);
    }

    #[test]
    fn empty_and_whitespace_only_give_empty() {
        assert_eq!(normalize(""), "");
        assert_eq!(normalize(" \n\t \r\n"), "");
    }
}
