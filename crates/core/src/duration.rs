//! Durations as people write them in TOML and JSON: a whole number and a unit (`90s`, `10m`, `2h`,
//! `7d`). Kept as milliseconds, written back in the largest unit that divides evenly.

use serde::{Deserialize, Serialize};
use std::fmt;

pub const SECOND: i64 = 1_000;
pub const MINUTE: i64 = 60 * SECOND;
pub const HOUR: i64 = 60 * MINUTE;
pub const DAY: i64 = 24 * HOUR;

/// A length of time, in milliseconds, always more than zero.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct Duration(i64);

impl Duration {
    pub const FORMS: &'static str = "a whole number and a unit: s, m, h or d, like 90s, 10m, 2h or 7d";

    pub fn from_ms(ms: i64) -> Option<Duration> {
        (ms > 0).then_some(Duration(ms))
    }

    pub fn ms(self) -> i64 {
        self.0
    }

    pub fn parse(text: &str) -> Result<Duration, String> {
        let text = text.trim();
        // The unit is the last character, which need not be one byte: `é` must be an error, not a panic.
        let cut = text.char_indices().next_back().map_or(0, |(at, _)| at);
        let (number, unit) = text.split_at(cut);
        let factor = match unit {
            "s" => SECOND,
            "m" => MINUTE,
            "h" => HOUR,
            "d" => DAY,
            _ => return Err(format!("`{text}` is not a duration")),
        };
        let number: i64 = number
            .parse()
            .map_err(|_| format!("`{text}` is not a duration"))?;
        if number <= 0 {
            return Err(format!("`{text}` must be more than zero"));
        }
        number
            .checked_mul(factor)
            .map(Duration)
            .ok_or_else(|| format!("`{text}` is too long"))
    }
}

impl fmt::Display for Duration {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let ms = self.0;
        for (unit, size) in [("d", DAY), ("h", HOUR), ("m", MINUTE), ("s", SECOND)] {
            if ms % size == 0 {
                return write!(f, "{}{unit}", ms / size);
            }
        }
        write!(f, "{ms}ms")
    }
}

impl TryFrom<String> for Duration {
    type Error = String;
    fn try_from(text: String) -> Result<Duration, String> {
        Duration::parse(&text)
    }
}

impl From<Duration> for String {
    fn from(duration: Duration) -> String {
        duration.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_and_writes_back() {
        assert_eq!(Duration::parse("90s").unwrap().ms(), 90 * SECOND);
        assert_eq!(Duration::parse("10m").unwrap().to_string(), "10m");
        assert_eq!(Duration::parse("120m").unwrap().to_string(), "2h");
        assert_eq!(Duration::parse("7d").unwrap().ms(), 7 * DAY);
        assert!(Duration::parse("0m").is_err());
        assert!(Duration::parse("-5m").is_err());
        assert!(Duration::parse("5").is_err());
        assert!(Duration::parse("5w").is_err());
        assert!(Duration::parse("5é").is_err());
        assert!(Duration::parse("").is_err());
        assert!(Duration::parse("99999999999999999d").is_err());
        assert_eq!(Duration::from_ms(0), None);
        let json = serde_json::to_value(Duration::parse("30m").unwrap()).unwrap();
        assert_eq!(json, "30m");
        let back: Duration = serde_json::from_value(json).unwrap();
        assert_eq!(back.ms(), 30 * MINUTE);
    }
}
