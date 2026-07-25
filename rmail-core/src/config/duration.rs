//! Human-friendly duration parsing (`"5m"`, `"30s"`, `"1h"`, `"3d"`, `"250ms"`).

use std::fmt;
use std::time::Duration;

use serde::de::{Deserialize, Deserializer, Error as _};

/// A [`Duration`] deserialized from a compact human string such as `"5m"`.
///
/// Supported unit suffixes: `ms`, `s`, `m` (minutes), `h`, `d`. The numeric
/// part must be a non-negative integer. Parsing is exact and deterministic —
/// there is no locale or fuzzy handling.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HumanDuration(Duration);

impl HumanDuration {
    /// Construct from a raw [`Duration`].
    #[must_use]
    pub const fn new(duration: Duration) -> Self {
        Self(duration)
    }

    /// The underlying [`Duration`].
    #[must_use]
    pub const fn as_duration(self) -> Duration {
        self.0
    }
}

impl From<HumanDuration> for Duration {
    fn from(value: HumanDuration) -> Self {
        value.0
    }
}

impl fmt::Display for HumanDuration {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}ms", self.0.as_millis())
    }
}

/// Parse a compact duration string (`"5m"`, `"250ms"`, ...) into a [`Duration`].
///
/// # Errors
///
/// Returns a human-readable message if the string is empty, lacks a unit, has a
/// non-integer value, uses an unknown unit, or overflows.
pub fn parse_human_duration(input: &str) -> Result<Duration, String> {
    let s = input.trim();
    if s.is_empty() {
        return Err("empty duration string".to_owned());
    }

    let split = s
        .find(|c: char| !c.is_ascii_digit())
        .ok_or_else(|| format!("duration {s:?} is missing a unit (use ms, s, m, h, d)"))?;
    let (value_part, unit_part) = s.split_at(split);
    let value: u64 = value_part
        .parse()
        .map_err(|_| format!("duration {s:?} has a non-integer value"))?;
    let unit = unit_part.trim();

    let secs_multiplier: u64 = match unit {
        "ms" => {
            return Ok(Duration::from_millis(value));
        }
        "s" => 1,
        "m" => 60,
        "h" => 3_600,
        "d" => 86_400,
        other => {
            return Err(format!(
                "duration {s:?} has unknown unit {other:?} (use ms, s, m, h, d)"
            ));
        }
    };

    value
        .checked_mul(secs_multiplier)
        .map(Duration::from_secs)
        .ok_or_else(|| format!("duration {s:?} overflows"))
}

impl<'de> Deserialize<'de> for HumanDuration {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let raw = String::deserialize(deserializer)?;
        parse_human_duration(&raw)
            .map(HumanDuration)
            .map_err(D::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_each_unit() {
        assert_eq!(
            parse_human_duration("250ms").unwrap(),
            Duration::from_millis(250)
        );
        assert_eq!(
            parse_human_duration("30s").unwrap(),
            Duration::from_secs(30)
        );
        assert_eq!(
            parse_human_duration("5m").unwrap(),
            Duration::from_secs(300)
        );
        assert_eq!(
            parse_human_duration("1h").unwrap(),
            Duration::from_secs(3_600)
        );
        assert_eq!(
            parse_human_duration("3d").unwrap(),
            Duration::from_secs(259_200)
        );
    }

    #[test]
    fn rejects_bad_input() {
        assert!(parse_human_duration("").is_err());
        assert!(parse_human_duration("5x").is_err());
        assert!(parse_human_duration("abc").is_err());
        assert!(parse_human_duration("m").is_err());
    }
}
