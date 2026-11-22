//! Speaking rate of a job: 1.0 is the engine's own pace, 1.2 is 20 % faster.
//!
//! Kept as an exact count of thousandths, so a job row compares and round-trips exactly.

use std::fmt;

use serde::{Serialize, Serializer};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Rate(u16);

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("rate must be a number from {min} to {max}", min = Rate::MIN.as_f64(), max = Rate::MAX.as_f64())]
pub struct InvalidRate;

impl Rate {
    /// The engine's own pace; audio is passed through untouched.
    pub const NATIVE: Rate = Rate(1000);
    pub const MIN: Rate = Rate(750);
    pub const MAX: Rate = Rate(1500);

    /// The rate nearest to `x` in thousandths, if it is within [`Rate::MIN`]-[`Rate::MAX`].
    pub fn from_f64(x: f64) -> Result<Rate, InvalidRate> {
        if !x.is_finite() {
            return Err(InvalidRate);
        }
        let milli = (x * 1000.0).round();
        if milli < f64::from(Self::MIN.0) || milli > f64::from(Self::MAX.0) {
            return Err(InvalidRate);
        }
        Ok(Rate(milli as u16))
    }

    pub fn from_milli(milli: u16) -> Result<Rate, InvalidRate> {
        if (Self::MIN.0..=Self::MAX.0).contains(&milli) {
            Ok(Rate(milli))
        } else {
            Err(InvalidRate)
        }
    }

    pub const fn milli(self) -> u16 {
        self.0
    }

    pub fn as_f64(self) -> f64 {
        f64::from(self.0) / 1000.0
    }

    pub const fn is_native(self) -> bool {
        self.0 == Self::NATIVE.0
    }
}

impl Default for Rate {
    fn default() -> Self {
        Self::NATIVE
    }
}

impl fmt::Display for Rate {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.as_f64())
    }
}

impl Serialize for Rate {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_f64(self.as_f64())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_within_range_to_thousandths() {
        assert_eq!(Rate::from_f64(1.0), Ok(Rate::NATIVE));
        assert_eq!(Rate::from_f64(1.2).map(Rate::milli), Ok(1200));
        assert_eq!(Rate::from_f64(1.2345).map(Rate::milli), Ok(1235));
        assert_eq!(Rate::from_f64(0.75).map(Rate::milli), Ok(750));
        assert_eq!(Rate::from_f64(1.5).map(Rate::milli), Ok(1500));
        for bad in [0.74, 1.51, 0.0, -1.2, f64::NAN, f64::INFINITY] {
            assert_eq!(Rate::from_f64(bad), Err(InvalidRate), "{bad}");
        }
        assert!(Rate::from_milli(749).is_err());
        assert_eq!(Rate::from_milli(1200).map(Rate::as_f64), Ok(1.2));
        assert!(Rate::default().is_native());
        assert_eq!(
            Rate::from_milli(1200).unwrap_or_default().to_string(),
            "1.2"
        );
    }
}
