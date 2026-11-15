//! The languages v1 synthesizes. Hinglish (Hindi with English words, in either script) is
//! spoken as `hi`.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Language {
    En,
    Hi,
}

impl Language {
    pub const ALL: [Language; 2] = [Language::En, Language::Hi];

    pub const fn code(self) -> &'static str {
        match self {
            Language::En => "en",
            Language::Hi => "hi",
        }
    }
}

impl fmt::Display for Language {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.code())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("unsupported language")]
pub struct UnsupportedLanguage;

impl FromStr for Language {
    type Err = UnsupportedLanguage;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Language::ALL
            .into_iter()
            .find(|l| l.code() == s)
            .ok_or(UnsupportedLanguage)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_codes() {
        assert_eq!("en".parse::<Language>(), Ok(Language::En));
        assert_eq!("hi".parse::<Language>(), Ok(Language::Hi));
        assert!("EN".parse::<Language>().is_err());
        assert!("auto".parse::<Language>().is_err());
    }
}
