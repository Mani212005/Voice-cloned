//! Spoken-form normalization: rewrite written English into the words a narrator says.
//!
//! The engine reads text literally and lowercases it first (the multilingual V3 tokenizer),
//! so "AI" reaches it as "ai", "CPU" as "cpu" and "467" as three digit tokens; all of them
//! came out garbled. This pass spells acronyms with letter names, says numbers, ranges,
//! money, percentages, times, ordinals, versions and units as words, and expands common
//! abbreviations, so callers can send ordinary written text.
//!
//! English only: Hindi and Hinglish text is passed through unchanged. The rules are
//! conservative and every one is a pure function of the token and its neighbour, so the
//! output is deterministic. Text that matches no rule is left exactly as written.

/// Letter names that survive the engine's lowercasing and read unambiguously.
const LETTER: [&str; 26] = [
    "ay",
    "bee",
    "see",
    "dee",
    "ee",
    "eff",
    "jee",
    "aitch",
    "eye",
    "jay",
    "kay",
    "el",
    "em",
    "en",
    "oh",
    "pee",
    "cue",
    "ar",
    "ess",
    "tee",
    "you",
    "vee",
    "double you",
    "ex",
    "why",
    "zee",
];
/// Plural of each letter name, for "CPUs" or "APIs".
const LETTER_PLURAL: [&str; 26] = [
    "ays",
    "bees",
    "sees",
    "dees",
    "ees",
    "effs",
    "jees",
    "aitches",
    "eyes",
    "jays",
    "kays",
    "els",
    "ems",
    "ens",
    "ohs",
    "pees",
    "cues",
    "ars",
    "esses",
    "tees",
    "yous",
    "vees",
    "double yous",
    "exes",
    "whys",
    "zees",
];

/// All-caps words that are said as words, not letter by letter.
const SAID_AS_WORDS: &[&str] = &[
    "AIDS", "ASAP", "CAPTCHA", "COVID", "DOS", "FAQ", "FIFA", "GIF", "IKEA", "JPEG", "LAN",
    "LASER", "LED", "LEGO", "NASA", "NATO", "OPEC", "PIN", "RADAR", "RAM", "ROM", "SCUBA", "SIM",
    "UNESCO", "UNICEF", "WAN", "YAML",
];

/// Whole-token replacements, matched case-sensitively after outer punctuation is removed.
const FIXED: &[(&str, &str)] = &[
    ("e.g.", "for example"),
    ("i.e.", "that is"),
    ("etc.", "et cetera"),
    ("vs.", "versus"),
    ("vs", "versus"),
    ("Mr.", "Mister"),
    ("Mrs.", "Missus"),
    ("Dr.", "Doctor"),
    ("approx.", "approximately"),
    ("a.m.", "ay em"),
    ("p.m.", "pee em"),
    ("OK", "okay"),
    ("iOS", "eye oh ess"),
    ("macOS", "mac oh ess"),
    ("iPadOS", "eye pad oh ess"),
    ("Wi-Fi", "why fye"),
    ("WiFi", "why fye"),
    ("&", "and"),
    ("+", "plus"),
    ("=", "equals"),
    ("@", "at"),
    ("%", "percent"),
    ("w/", "with"),
    ("24/7", "twenty-four seven"),
    ("km/h", "kilometers per hour"),
];

/// Unit symbols said after a number: (symbol, singular, plural). Case-sensitive.
const UNITS: &[(&str, &str, &str)] = &[
    ("ms", "millisecond", "milliseconds"),
    ("µs", "microsecond", "microseconds"),
    ("ns", "nanosecond", "nanoseconds"),
    ("sec", "second", "seconds"),
    ("min", "minute", "minutes"),
    ("hr", "hour", "hours"),
    ("hrs", "hours", "hours"),
    ("KB", "kilobyte", "kilobytes"),
    ("MB", "megabyte", "megabytes"),
    ("GB", "gigabyte", "gigabytes"),
    ("TB", "terabyte", "terabytes"),
    ("Kbps", "kilobit per second", "kilobits per second"),
    ("Mbps", "megabit per second", "megabits per second"),
    ("Gbps", "gigabit per second", "gigabits per second"),
    ("Hz", "hertz", "hertz"),
    ("kHz", "kilohertz", "kilohertz"),
    ("MHz", "megahertz", "megahertz"),
    ("GHz", "gigahertz", "gigahertz"),
    ("km", "kilometer", "kilometers"),
    ("cm", "centimeter", "centimeters"),
    ("mm", "millimeter", "millimeters"),
    ("kg", "kilogram", "kilograms"),
    ("mg", "milligram", "milligrams"),
    ("fps", "frame per second", "frames per second"),
    ("mph", "mile per hour", "miles per hour"),
    ("kph", "kilometer per hour", "kilometers per hour"),
    ("km/h", "kilometer per hour", "kilometers per hour"),
    ("px", "pixel", "pixels"),
    ("x", "times", "times"),
    ("K", "thousand", "thousand"),
    ("k", "thousand", "thousand"),
    ("M", "million", "million"),
    ("B", "billion", "billion"),
    ("bn", "billion", "billion"),
];

/// Currency symbols: (symbol, singular, plural, minor singular, minor plural).
const CURRENCIES: &[(char, &str, &str, &str, &str)] = &[
    ('$', "dollar", "dollars", "cent", "cents"),
    ('€', "euro", "euros", "cent", "cents"),
    ('£', "pound", "pounds", "penny", "pence"),
    ('₹', "rupee", "rupees", "paisa", "paise"),
];

/// Scale words that may follow an amount ("$5 million").
const SCALES: &[&str] = &["thousand", "million", "billion", "trillion"];

/// Punctuation that may wrap a token and is kept around the spoken form.
const LEADING: &[char] = &['"', '\'', '(', '[', '{', '\u{201C}', '\u{2018}', '\u{00AB}'];
const TRAILING: &[char] = &[
    '.', ',', ';', ':', '!', '?', '"', '\'', ')', ']', '}', '\u{201D}', '\u{2019}', '\u{00BB}',
    '\u{2026}',
];

/// Top-level domains read as "dot <name>".
const DOMAINS: &[&str] = &[
    "com", "org", "net", "io", "ai", "dev", "app", "co", "in", "edu", "gov",
];

const ONES: [&str; 20] = [
    "zero",
    "one",
    "two",
    "three",
    "four",
    "five",
    "six",
    "seven",
    "eight",
    "nine",
    "ten",
    "eleven",
    "twelve",
    "thirteen",
    "fourteen",
    "fifteen",
    "sixteen",
    "seventeen",
    "eighteen",
    "nineteen",
];
const TENS: [&str; 10] = [
    "", "", "twenty", "thirty", "forty", "fifty", "sixty", "seventy", "eighty", "ninety",
];

/// Rewrite `text` (already [`crate::normalize`]d or not) into its spoken form for an
/// English job. Lines are kept; within a line words are separated by single spaces.
pub fn speakable_en(text: &str) -> String {
    text.lines()
        .map(speak_line)
        .filter(|l| !l.is_empty())
        .collect::<Vec<_>>()
        .join("\n")
}

/// One whitespace-separated token with its wrapping punctuation split off.
#[derive(Debug, Clone)]
struct Tok<'a> {
    lead: &'a str,
    core: &'a str,
    trail: &'a str,
}

fn split_tok(raw: &str) -> Tok<'_> {
    let mut lead_len = raw.len() - raw.trim_start_matches(LEADING).len();
    // An apostrophe before digits belongs to the token ("'80s").
    if raw[..lead_len].ends_with('\'') && raw[lead_len..].starts_with(|c: char| c.is_ascii_digit())
    {
        lead_len -= 1;
    }
    let (lead, rest) = raw.split_at(lead_len);
    // Keep a final period that belongs to the token ("e.g.", "U.S.", "a.m.").
    let mut core = rest.trim_end_matches(TRAILING);
    let with_dot = &rest[..core.len() + usize::from(rest[core.len()..].starts_with('.'))];
    if with_dot.len() > core.len() && keeps_final_dot(with_dot) {
        core = with_dot;
    }
    let trail = &rest[core.len()..];
    Tok { lead, core, trail }
}

/// Whether a kept final period may also end the sentence ("etc.", "9 a.m.", "the U.S.").
fn keeps_sentence_dot(core: &str) -> bool {
    matches!(core, "etc." | "a.m." | "p.m.")
        || (core.ends_with('.') && dotted_letters(core).is_some())
}

/// Whether a token ending in '.' is an abbreviation whose dot is part of it.
fn keeps_final_dot(t: &str) -> bool {
    FIXED.iter().any(|(k, _)| *k == t) || dotted_letters(t).is_some()
}

fn speak_line(line: &str) -> String {
    let toks: Vec<Tok<'_>> = line.split_whitespace().map(split_tok).collect();
    let shouting = is_shouting(&toks);
    let mut out: Vec<String> = Vec::with_capacity(toks.len());
    let mut i = 0;
    while i < toks.len() {
        let t = &toks[i];
        let next = toks.get(i + 1);
        let (spoken, consumed_next) = speak_token(t.core, next, shouting);
        let trail = if consumed_next {
            next.map_or(t.trail, |n| n.trail)
        } else {
            t.trail
        };
        let last = if consumed_next { next.unwrap_or(t) } else { t };
        let after = toks.get(i + 1 + usize::from(consumed_next));
        let ends_sentence = after.is_none_or(|n| n.core.starts_with(char::is_uppercase));
        let mut s = String::with_capacity(t.lead.len() + spoken.len() + trail.len() + 1);
        s.push_str(t.lead);
        s.push_str(&spoken);
        if ends_sentence
            && keeps_sentence_dot(last.core)
            && !spoken.ends_with('.')
            && !trail.starts_with(['.', ',', ';', ':', '!', '?', '\u{2026}'])
        {
            s.push('.');
        }
        s.push_str(trail);
        if !s.is_empty() {
            out.push(s);
        }
        i += 1 + usize::from(consumed_next);
    }
    out.join(" ")
}

/// A line written mostly in capitals is emphasis, not a run of acronyms.
fn is_shouting(toks: &[Tok<'_>]) -> bool {
    let words: Vec<&str> = toks
        .iter()
        .map(|t| t.core)
        .filter(|c| c.chars().filter(|ch| ch.is_alphabetic()).count() >= 2)
        .collect();
    words.len() >= 3
        && words
            .iter()
            .all(|w| w.chars().all(|c| !c.is_alphabetic() || c.is_uppercase()))
}

/// The spoken form of one token core, and whether it also used the next token.
fn speak_token(core: &str, next: Option<&Tok<'_>>, shouting: bool) -> (String, bool) {
    if core.is_empty() {
        return (String::new(), false);
    }
    if let Some(s) = money(core, next) {
        return s;
    }
    if let Some(s) = number_with_next(core, next) {
        return s;
    }
    (word(core, shouting), false)
}

/// A token that is not money and not a number followed by a unit word.
fn word(core: &str, shouting: bool) -> String {
    if let Some((_, said)) = FIXED.iter().find(|(k, _)| *k == core) {
        return (*said).to_owned();
    }
    for apos in ["'s", "\u{2019}s"] {
        if let Some(stem) = core.strip_suffix(apos)
            && !stem.is_empty()
        {
            let said = word(stem, shouting);
            if said != stem {
                return format!("{said}{apos}");
            }
        }
    }
    if let Some(s) = number_like(core) {
        return s;
    }
    if let Some(s) = domain(core) {
        return s;
    }
    if let Some(letters) = dotted_letters(core) {
        return spell(&letters, false);
    }
    if !shouting && let Some(s) = acronym(core) {
        return s;
    }
    if let Some(s) = mixed(core, shouting) {
        return s;
    }
    if shouting && core.chars().all(|c| !c.is_alphabetic() || c.is_uppercase()) {
        return core.to_lowercase();
    }
    core.to_owned()
}

/// "$5", "$5.99", "$1.5M", "$5 million", "-$3": money in words.
fn money(core: &str, next: Option<&Tok<'_>>) -> Option<(String, bool)> {
    let (neg, body) = match core.strip_prefix('-') {
        Some(b) => (true, b),
        None => (false, core),
    };
    let sym = body.chars().next()?;
    let &(_, one, many, minor_one, minor_many) = CURRENCIES.iter().find(|c| c.0 == sym)?;
    let amount = &body[sym.len_utf8()..];
    let (num, suffix) = split_number_prefix(amount)?;
    let scale = match suffix {
        "" => None,
        "K" | "k" => Some("thousand"),
        "M" | "m" | "mn" => Some("million"),
        "B" | "bn" => Some("billion"),
        "T" | "tn" => Some("trillion"),
        _ => return None,
    };
    let next_scale = next.filter(|n| SCALES.contains(&n.core) && n.lead.is_empty());
    let mut words = Vec::new();
    if neg {
        words.push("minus".to_owned());
    }
    let scale = scale.or_else(|| next_scale.map(|n| n.core));
    if let Some(scale) = scale {
        words.push(number_words(num)?);
        words.push(scale.to_owned());
        words.push(many.to_owned());
        // "$5 million" used the next word; "$5M" did not.
        let consumed = suffix.is_empty() && next_scale.is_some();
        return Some((words.join(" "), consumed));
    }
    match num.split_once('.') {
        // "$5.99": dollars and cents.
        Some((int, f)) if f.len() == 2 => {
            let units = parse_int(int)?;
            words.push(cardinal(units));
            words.push(if units == 1 { one } else { many }.to_owned());
            let minor: u64 = f.parse().ok()?;
            if minor > 0 {
                words.push("and".to_owned());
                words.push(cardinal(minor));
                words.push(if minor == 1 { minor_one } else { minor_many }.to_owned());
            }
        }
        // "$5.5": a decimal amount.
        Some(_) => {
            words.push(number_words(num)?);
            words.push(many.to_owned());
        }
        None => {
            let units = parse_int(num)?;
            words.push(cardinal(units));
            words.push(if units == 1 { one } else { many }.to_owned());
        }
    }
    Some((words.join(" "), false))
}

/// A number token followed by a separate unit word ("2 GB", "40-80 ms", "1 hr").
fn number_with_next(core: &str, next: Option<&Tok<'_>>) -> Option<(String, bool)> {
    let n = next.filter(|n| n.lead.is_empty())?;
    let &(_, one, many) = UNITS
        .iter()
        // Single-letter scale suffixes only count when attached ("10K"), never as a word.
        .find(|u| u.0 == n.core && u.0.chars().count() > 1)?;
    if let Some((a, b)) = split_range(core) {
        return Some((
            format!("{} to {} {many}", number_words(a)?, number_words(b)?),
            true,
        ));
    }
    if !is_number(core) {
        return None;
    }
    let unit = if core == "1" { one } else { many };
    Some((format!("{} {unit}", number_words(core)?), true))
}

/// Numbers that stand alone or carry their unit in the token.
fn number_like(core: &str) -> Option<String> {
    if let Some((a, b)) = split_range(core) {
        return Some(format!("{} to {}", spoken_number(a)?, spoken_number(b)?));
    }
    if let Some(p) = core.strip_suffix('%') {
        if let Some((a, b)) = split_range(p) {
            return Some(format!(
                "{} to {} percent",
                number_words(a)?,
                number_words(b)?
            ));
        }
        return Some(format!("{} percent", number_words(p)?));
    }
    if let Some(s) = time_of_day(core) {
        return Some(s);
    }
    if let Some(s) = ordinal(core) {
        return Some(s);
    }
    if let Some(s) = decade(core) {
        return Some(s);
    }
    if let Some(s) = version(core) {
        return Some(s);
    }
    if is_number(core) {
        return spoken_number(core);
    }
    let (num, suffix) = split_number_prefix(core)?;
    if num.is_empty() || suffix.is_empty() {
        return None;
    }
    if let Some(&(_, one, many)) = UNITS.iter().find(|u| u.0 == suffix) {
        let unit = if num == "1" { one } else { many };
        return Some(format!("{} {unit}", number_words(num)?));
    }
    // "4G", "3D", "2FA": the number, then the letters.
    if suffix.len() <= 4 && suffix.chars().all(|c| c.is_ascii_uppercase()) {
        return Some(format!("{} {}", number_words(num)?, spell(suffix, false)));
    }
    None
}

/// "4" or "1,500" or "3.25" or "-5": a plain number.
fn is_number(s: &str) -> bool {
    let s = s.strip_prefix('-').unwrap_or(s);
    let (int, frac) = match s.split_once('.') {
        Some((i, f)) => (i, Some(f)),
        None => (s, None),
    };
    valid_int(int) && frac.is_none_or(|f| !f.is_empty() && f.bytes().all(|b| b.is_ascii_digit()))
}

/// Digits, optionally grouped by commas in threes ("1,500,000").
fn valid_int(s: &str) -> bool {
    if s.is_empty() || !s.bytes().all(|b| b.is_ascii_digit() || b == b',') {
        return false;
    }
    if !s.contains(',') {
        return true;
    }
    let mut groups = s.split(',');
    let first_ok = groups
        .next()
        .is_some_and(|g| (1..=3).contains(&g.len()) && g.bytes().all(|b| b.is_ascii_digit()));
    first_ok && groups.all(|g| g.len() == 3 && g.bytes().all(|b| b.is_ascii_digit()))
}

fn parse_int(s: &str) -> Option<u64> {
    if !valid_int(s) {
        return None;
    }
    s.replace(',', "").parse().ok()
}

/// The longest leading number ("1,500", "3.5", "-2") and the rest of the token.
fn split_number_prefix(s: &str) -> Option<(&str, &str)> {
    let start = usize::from(s.starts_with('-'));
    let bytes = s.as_bytes();
    let mut end = start;
    while end < bytes.len() {
        let b = bytes[end];
        let digit_follows = bytes.get(end + 1).is_some_and(u8::is_ascii_digit);
        if b.is_ascii_digit() || ((b == b',' || b == b'.') && end > start && digit_follows) {
            end += 1;
        } else {
            break;
        }
    }
    if end == start {
        return None;
    }
    let (num, rest) = s.split_at(end);
    is_number(num).then_some((num, rest))
}

/// "40-80" or "40–80" (en dash): two plain numbers.
fn split_range(s: &str) -> Option<(&str, &str)> {
    let (a, b) = s.split_once('\u{2013}').or_else(|| {
        s.get(1..)
            .and_then(|r| r.split_once('-').map(|(x, y)| (&s[..x.len() + 1], y)))
    })?;
    (is_number(a) && is_number(b) && !b.starts_with('-')).then_some((a, b))
}

/// A number as a narrator says it: years as years, everything else in words.
fn spoken_number(s: &str) -> Option<String> {
    if let Ok(n) = s.parse::<u64>()
        && s.len() == 4
        && (1900..=2099).contains(&n)
    {
        return Some(year(n));
    }
    number_words(s)
}

/// Cardinal words for a plain number token, with "point" for decimals and "minus".
fn number_words(s: &str) -> Option<String> {
    if !is_number(s) {
        return None;
    }
    let (neg, s) = match s.strip_prefix('-') {
        Some(r) => (true, r),
        None => (false, s),
    };
    let (int, frac) = match s.split_once('.') {
        Some((i, f)) => (i, Some(f)),
        None => (s, None),
    };
    let digits = int.replace(',', "");
    // Codes with a leading zero or very long digit runs are read digit by digit.
    let mut w = if (digits.len() > 1 && digits.starts_with('0') && !int.contains(','))
        || digits.len() > 15
    {
        digit_words(&digits)
    } else {
        cardinal(digits.parse().ok()?)
    };
    if let Some(f) = frac {
        w.push_str(" point ");
        w.push_str(&digit_words(f));
    }
    if neg {
        w.insert_str(0, "minus ");
    }
    Some(w)
}

fn digit_words(d: &str) -> String {
    d.bytes()
        .filter(u8::is_ascii_digit)
        .map(|b| ONES[usize::from(b - b'0')])
        .collect::<Vec<_>>()
        .join(" ")
}

/// American cardinal words: 467 is "four hundred sixty-seven".
pub(crate) fn cardinal(n: u64) -> String {
    const SCALE: [(u64, &str); 4] = [
        (1_000_000_000_000, "trillion"),
        (1_000_000_000, "billion"),
        (1_000_000, "million"),
        (1_000, "thousand"),
    ];
    if n == 0 {
        return ONES[0].to_owned();
    }
    let mut parts = Vec::new();
    let mut rest = n;
    for (value, name) in SCALE {
        if rest >= value {
            parts.push(format!("{} {name}", below_thousand(rest / value)));
            rest %= value;
        }
    }
    if rest > 0 {
        parts.push(below_thousand(rest));
    }
    parts.join(" ")
}

fn below_thousand(n: u64) -> String {
    let mut parts = Vec::new();
    let h = n / 100;
    if h > 0 {
        parts.push(format!("{} hundred", ONES[h as usize]));
    }
    let r = n % 100;
    if r > 0 {
        parts.push(below_hundred(r));
    }
    parts.join(" ")
}

fn below_hundred(n: u64) -> String {
    if n < 20 {
        return ONES[n as usize].to_owned();
    }
    let (t, o) = (TENS[(n / 10) as usize], n % 10);
    if o == 0 {
        t.to_owned()
    } else {
        format!("{t}-{}", ONES[o as usize])
    }
}

/// 2013 is "twenty thirteen", 2005 "two thousand five", 1905 "nineteen oh five".
fn year(n: u64) -> String {
    let (hi, lo) = (n / 100, n % 100);
    if (2000..2010).contains(&n) {
        return cardinal(n);
    }
    let first = below_hundred(hi);
    match lo {
        0 => format!("{first} hundred"),
        1..=9 => format!("{first} oh {}", ONES[lo as usize]),
        _ => format!("{first} {}", below_hundred(lo)),
    }
}

/// "1st", "22nd", "103rd", "4th": ordinal words.
fn ordinal(core: &str) -> Option<String> {
    let lower = core.to_ascii_lowercase();
    let num = ["st", "nd", "rd", "th"]
        .iter()
        .find_map(|s| lower.strip_suffix(s))?;
    let n = parse_int(num)?;
    let words = cardinal(n);
    Some(ordinal_of(&words))
}

/// Turn the last word of cardinal words into its ordinal.
fn ordinal_of(words: &str) -> String {
    const IRREGULAR: &[(&str, &str)] = &[
        ("one", "first"),
        ("two", "second"),
        ("three", "third"),
        ("five", "fifth"),
        ("eight", "eighth"),
        ("nine", "ninth"),
        ("twelve", "twelfth"),
    ];
    let split = words.rfind([' ', '-']).map_or(0, |i| i + 1);
    let (head, last) = words.split_at(split);
    let last = if let Some((_, o)) = IRREGULAR.iter().find(|(c, _)| *c == last) {
        (*o).to_owned()
    } else if let Some(stem) = last.strip_suffix('y') {
        format!("{stem}ieth")
    } else {
        format!("{last}th")
    };
    format!("{head}{last}")
}

/// "1990s" is "nineteen nineties"; "80s" and "'80s" are "eighties".
fn decade(core: &str) -> Option<String> {
    let d = core.strip_prefix('\'').unwrap_or(core).strip_suffix('s')?;
    let n: u64 = d.parse().ok()?;
    let words = match d.len() {
        2 if n.is_multiple_of(10) && n >= 10 => below_hundred(n),
        4 if n.is_multiple_of(10) && (1100..=2090).contains(&n) => year(n),
        _ => return None,
    };
    Some(plural_of(&words))
}

fn plural_of(words: &str) -> String {
    match words.strip_suffix('y') {
        Some(stem) => format!("{stem}ies"),
        None => format!("{words}s"),
    }
}

/// "10:30" is "ten thirty", "9:05" "nine oh five", "10:00" "ten o'clock".
fn time_of_day(core: &str) -> Option<String> {
    let (h, m) = core.split_once(':')?;
    if !(1..=2).contains(&h.len()) || m.len() != 2 {
        return None;
    }
    let (h, m): (u64, u64) = (h.parse().ok()?, m.parse().ok()?);
    if h > 23 || m > 59 {
        return None;
    }
    Some(match m {
        0 => format!("{} o'clock", cardinal(h)),
        1..=9 => format!("{} oh {}", cardinal(h), ONES[m as usize]),
        _ => format!("{} {}", cardinal(h), below_hundred(m)),
    })
}

/// "7.2.1" or "v7.2": dotted version numbers.
fn version(core: &str) -> Option<String> {
    let (prefix, rest) = match core.strip_prefix(['v', 'V']) {
        Some(r) => ("version ", r),
        None => ("", core),
    };
    let parts: Vec<&str> = rest.split('.').collect();
    let dotted = parts.len() >= 3;
    if !(dotted || (!prefix.is_empty() && !parts.is_empty())) {
        return None;
    }
    if !parts
        .iter()
        .all(|p| !p.is_empty() && p.len() <= 4 && p.bytes().all(|b| b.is_ascii_digit()))
    {
        return None;
    }
    let words: Option<Vec<String>> = parts.iter().map(|p| p.parse().ok().map(cardinal)).collect();
    Some(format!("{prefix}{}", words?.join(" point ")))
}

/// "signal.org" is "signal dot org".
fn domain(core: &str) -> Option<String> {
    let (name, tld) = core.rsplit_once('.')?;
    if !DOMAINS.contains(&tld)
        || name.is_empty()
        || !name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '.')
    {
        return None;
    }
    let spoken_tld = if tld.len() <= 2 && tld != "io" {
        spell(&tld.to_ascii_uppercase(), false)
    } else if tld == "io" {
        "eye oh".to_owned()
    } else {
        tld.to_owned()
    };
    Some(format!("{} dot {spoken_tld}", name.replace('.', " dot ")))
}

/// "U.S." or "A.I.": the capital letters, if the token is only dotted capitals.
fn dotted_letters(core: &str) -> Option<String> {
    let body = core.strip_suffix('.').unwrap_or(core);
    let parts: Vec<&str> = body.split('.').collect();
    if parts.len() < 2 {
        return None;
    }
    let mut letters = String::new();
    for p in parts {
        let mut cs = p.chars();
        match (cs.next(), cs.next()) {
            (Some(c), None) if c.is_ascii_uppercase() => letters.push(c),
            _ => return None,
        }
    }
    Some(letters)
}

/// "AI", "CPU", "CPUs", "HTTPS": capitals said letter by letter.
fn acronym(core: &str) -> Option<String> {
    let (body, plural) = match core.strip_suffix('s') {
        Some(b) if b.len() >= 2 && all_caps(b) => (b, true),
        _ => (core, false),
    };
    if !(2..=5).contains(&body.len()) || !all_caps(body) {
        return None;
    }
    if SAID_AS_WORDS.contains(&body) {
        let w = body.to_ascii_lowercase();
        return Some(if plural { format!("{w}s") } else { w });
    }
    Some(spell(body, plural))
}

fn all_caps(s: &str) -> bool {
    !s.is_empty() && s.bytes().all(|b| b.is_ascii_uppercase())
}

/// Letter names for `caps` (ASCII capitals), the last one plural if asked.
fn spell(caps: &str, plural: bool) -> String {
    let letters: Vec<usize> = caps
        .bytes()
        .filter(u8::is_ascii_uppercase)
        .map(|b| usize::from(b - b'A'))
        .collect();
    let n = letters.len();
    letters
        .iter()
        .enumerate()
        .map(|(i, &k)| {
            if plural && i + 1 == n {
                LETTER_PLURAL[k]
            } else {
                LETTER[k]
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// Tokens mixing an acronym with digits or other words: "GPT-4", "COVID-19", "AI-powered",
/// "MP3", "TCP/IP", "and/or". Each part is said on its own; a hyphenated word whose parts
/// all stay as written ("end-to-end") is left alone.
fn mixed(core: &str, shouting: bool) -> Option<String> {
    let has_letter = |p: &str| p.chars().any(|c| c.is_ascii_alphabetic());
    if let Some((a, b)) = core.split_once('/') {
        let ok = |p: &str| {
            !p.is_empty() && p.chars().all(|c| c.is_ascii_alphanumeric()) && has_letter(p)
        };
        if ok(a) && ok(b) {
            return Some(format!("{} {}", word(a, shouting), word(b, shouting)));
        }
        return None;
    }
    if core.contains('-') {
        let parts: Vec<&str> = core.split('-').collect();
        if parts.iter().any(|p| p.is_empty()) {
            return None;
        }
        let said: Vec<String> = parts.iter().map(|p| word(p, shouting)).collect();
        if said.iter().zip(&parts).all(|(s, p)| s == p) {
            return None;
        }
        return Some(said.join(" "));
    }
    // "MP3", "H264": capitals then digits.
    let split = core.find(|c: char| c.is_ascii_digit())?;
    let (a, b) = core.split_at(split);
    if (1..=4).contains(&a.len()) && all_caps(a) && b.bytes().all(|x| x.is_ascii_digit()) {
        return Some(format!("{} {}", spell(a, false), number_words(b)?));
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(t: &str) -> String {
        speakable_en(t)
    }

    #[test]
    fn the_garbled_first_pass_items() {
        assert_eq!(
            s("It does not use AI to scan your chats."),
            "It does not use ay eye to scan your chats."
        );
        assert_eq!(
            s("The server handled 467 CPU tasks."),
            "The server handled four hundred sixty-seven see pee you tasks."
        );
        assert_eq!(
            s("Calls connect in 40-80 milliseconds."),
            "Calls connect in forty to eighty milliseconds."
        );
        assert_eq!(s("in 40–80 ms"), "in forty to eighty milliseconds");
    }

    #[test]
    fn cardinals() {
        assert_eq!(cardinal(0), "zero");
        assert_eq!(cardinal(7), "seven");
        assert_eq!(cardinal(15), "fifteen");
        assert_eq!(cardinal(40), "forty");
        assert_eq!(cardinal(99), "ninety-nine");
        assert_eq!(cardinal(100), "one hundred");
        assert_eq!(cardinal(467), "four hundred sixty-seven");
        assert_eq!(cardinal(1_500), "one thousand five hundred");
        assert_eq!(cardinal(2_000_003), "two million three");
        assert_eq!(
            cardinal(1_234_567_890_123),
            "one trillion two hundred thirty-four billion five hundred sixty-seven million \
             eight hundred ninety thousand one hundred twenty-three"
        );
    }

    #[test]
    fn numbers_decimals_and_years() {
        assert_eq!(
            s("over 1.5 billion people"),
            "over one point five billion people"
        );
        assert_eq!(s("released in 2013."), "released in twenty thirteen.");
        assert_eq!(s("in 2005"), "in two thousand five");
        assert_eq!(s("in 1905"), "in nineteen oh five");
        assert_eq!(s("1,500 users"), "one thousand five hundred users");
        assert_eq!(s("-5 degrees"), "minus five degrees");
        assert_eq!(s("code 007"), "code zero zero seven");
        assert_eq!(s("2048 GB"), "two thousand forty-eight gigabytes");
        assert_eq!(
            s("1999 to 2001"),
            "nineteen ninety-nine to two thousand one"
        );
        assert_eq!(s("2023-2024"), "twenty twenty-three to twenty twenty-four");
    }

    #[test]
    fn units_money_and_percent() {
        assert_eq!(s("just 2 GB of memory"), "just two gigabytes of memory");
        assert_eq!(s("1 GB"), "one gigabyte");
        assert_eq!(s("a 3.5GHz chip"), "a three point five gigahertz chip");
        assert_eq!(s("10x faster"), "ten times faster");
        assert_eq!(s("on a 4G network"), "on a four jee network");
        assert_eq!(s("It costs $0 to use"), "It costs zero dollars to use");
        assert_eq!(
            s("$1 or $5.99"),
            "one dollar or five dollars and ninety-nine cents"
        );
        assert_eq!(s("$5 million."), "five million dollars.");
        assert_eq!(s("$1.5M raised"), "one point five million dollars raised");
        assert_eq!(s("10K users"), "ten thousand users");
        assert_eq!(s("about 50% of it"), "about fifty percent of it");
        assert_eq!(s("10-20% off"), "ten to twenty percent off");
        assert_eq!(s("₹500"), "five hundred rupees");
        assert_eq!(s("$5.5 each"), "five point five dollars each");
        assert_eq!(s("$5.50"), "five dollars and fifty cents");
    }

    #[test]
    fn times_ordinals_decades_versions() {
        assert_eq!(s("at 10:30 a.m."), "at ten thirty ay em.");
        assert_eq!(s("at 10:30 a.m. it ends"), "at ten thirty ay em it ends");
        assert_eq!(s("at 9:05"), "at nine oh five");
        assert_eq!(s("by 10:00,"), "by ten o'clock,");
        assert_eq!(
            s("the 1st, 2nd, 3rd and 21st"),
            "the first, second, third and twenty-first"
        );
        assert_eq!(s("the 12th and 40th"), "the twelfth and fortieth");
        assert_eq!(s("in the 1990s"), "in the nineteen nineties");
        assert_eq!(s("the '80s"), "the eighties");
        assert_eq!(s("Version 7.2 adds"), "Version seven point two adds");
        assert_eq!(s("v7.2.1"), "version seven point two point one");
        assert_eq!(s("update 7.2.1"), "update seven point two point one");
    }

    #[test]
    fn acronyms_and_words() {
        assert_eq!(
            s("runs on iOS and Android"),
            "runs on eye oh ess and Android"
        );
        assert_eq!(s("the CPUs and APIs"), "the see pee yous and ay pee eyes");
        assert_eq!(s("the U.S. and the UK."), "the you ess and the you kay.");
        assert_eq!(s("NASA and RAM"), "nasa and ram");
        assert_eq!(
            s("GPT-4 and COVID-19"),
            "jee pee tee four and covid nineteen"
        );
        assert_eq!(s("an MP3 file"), "an em pee three file");
        assert_eq!(s("TCP/IP and and/or"), "tee see pee eye pee and and or");
        assert_eq!(s("half is 1/2"), "half is 1/2");
        assert_eq!(s("AI-powered and AI's"), "ay eye powered and ay eye's");
        assert_eq!(s("an end-to-end test"), "an end-to-end test");
        assert_eq!(s("SIGNAL"), "SIGNAL");
        assert_eq!(s("I am A student"), "I am A student");
        assert_eq!(
            s("OK, e.g. this, i.e. that, etc."),
            "okay, for example this, that is that, et cetera."
        );
        assert_eq!(s("Dr. Smith vs. Mr. Rao"), "Doctor Smith versus Mister Rao");
        assert_eq!(s("Visit signal.org today"), "Visit signal dot org today");
        assert_eq!(s("cats & dogs"), "cats and dogs");
        assert_eq!(s("(AI)"), "(ay eye)");
        assert_eq!(s("\"CPU,\" she said"), "\"see pee you,\" she said");
    }

    #[test]
    fn sentence_final_abbreviations_keep_their_period() {
        assert_eq!(
            s("pears, etc. Then we left at 9 a.m. It was fine."),
            "pears, et cetera. Then we left at nine ay em. It was fine."
        );
        assert_eq!(s("Use U.S. rules."), "Use you ess rules.");
        assert_eq!(s("Use the U.S."), "Use the you ess.");
        assert_eq!(s("(pears, etc.)"), "(pears, et cetera.)");
        assert_eq!(s("pears, etc., and more"), "pears, et cetera, and more");
        assert_eq!(s("e.g. Paris"), "for example Paris");
        assert_eq!(s("Dr. Smith"), "Doctor Smith");
    }

    #[test]
    fn shouting_is_lowercased_not_spelled() {
        assert_eq!(s("THIS IS VERY IMPORTANT"), "this is very important");
    }

    #[test]
    fn plain_text_is_unchanged() {
        let t = "Signal is a free messaging app that keeps your conversations private.\nSecond line, end-to-end.";
        assert_eq!(s(t), t);
    }

    #[test]
    fn idempotent() {
        for t in [
            "It costs $0 and 467 CPU tasks in 40-80 ms on iOS, 50% in 2013.",
            "Version 7.2 at 10:30 a.m. on the 21st, GPT-4, the U.S.",
            "pears, etc. Then 9 a.m. It was the U.S. Army.",
        ] {
            let once = s(t);
            assert_eq!(s(&once), once, "{t}");
        }
    }
}
