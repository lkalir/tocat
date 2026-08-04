use std::{fmt, str::FromStr, time::Duration};

use serde::{Deserialize, Serialize};

/// A duration.
///
/// Accepts a plain number (seconds) or a suffix: `1`, `1s`, `1ms`, `1m1us`.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Interval(Duration);

impl Interval {
    #[must_use]
    pub fn duration(self) -> Duration {
        self.0
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseIntervalError(String);

impl fmt::Display for ParseIntervalError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for ParseIntervalError {}

const MICROS: u128 = 1000;
const MILLIS: u128 = 1000 * MICROS;
const SECONDS: u128 = 1000 * MILLIS;
const MINUTES: u128 = 60 * SECONDS;
const HOURS: u128 = 60 * MINUTES;
const DAYS: u128 = 24 * HOURS;
const WEEKS: u128 = 7 * DAYS;

impl fmt::Display for Interval {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        const UNITS: [(u128, &str); 7] = [
            (WEEKS, "w"),
            (DAYS, "d"),
            (HOURS, "h"),
            (MINUTES, "m"),
            (SECONDS, "s"),
            (MILLIS, "ms"),
            (MICROS, "us"),
        ];

        let nanos = self.0.as_nanos();

        let (mut res, rem) =
            UNITS
                .iter()
                .fold((String::new(), nanos), |(mut s, rem), (scale, suffix)| {
                    let count = rem / scale;
                    if count > 0 {
                        s.push_str(&format!("{count}{suffix}"));
                    }

                    (s, rem % scale)
                });

        if rem > 0 {
            res.push_str(&format!("{rem}ns"));
        }

        if res.is_empty() {
            write!(f, "0s")
        } else {
            write!(f, "{res}")
        }
    }
}

impl FromStr for Interval {
    type Err = ParseIntervalError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let mut rest = s.trim();
        let mut total_nanos = 0u128;
        let mut seen_units = [false; 8];

        while !rest.is_empty() {
            let digit_len = rest
                .chars()
                .take_while(|c| c.is_ascii_digit())
                .map(|c| c.len_utf8())
                .sum::<usize>();

            if digit_len == 0 {
                return Err(ParseIntervalError("Invalid interval".to_string()));
            }

            let (num_str, tail) = rest.split_at(digit_len);
            let count: u128 = num_str
                .parse()
                .map_err(|_| ParseIntervalError("Failed to parse duration".to_string()))?;

            let unit_len = tail
                .chars()
                .take_while(|c| c.is_ascii_alphabetic())
                .map(|c| c.len_utf8())
                .sum::<usize>();

            let (unit_str, next_rest) = if unit_len == 0 {
                ("s", tail)
            } else {
                tail.split_at(unit_len)
            };

            let (multiplier, idx) = match unit_str.to_lowercase().as_str() {
                "ns" => (1, 0),
                "us" => (MICROS, 1),
                "ms" => (MILLIS, 2),
                "s" => (SECONDS, 3),
                "m" => (MINUTES, 4),
                "h" => (HOURS, 5),
                "d" => (DAYS, 6),
                "w" => (WEEKS, 7),
                unit => return Err(ParseIntervalError(format!("Invalid unit: {unit}"))),
            };

            if seen_units[idx] {
                return Err(ParseIntervalError("Duplicate units".to_string()));
            }

            seen_units[idx] = true;

            total_nanos = total_nanos
                .checked_add(
                    count
                        .checked_mul(multiplier)
                        .ok_or_else(|| ParseIntervalError("Duration overflow".to_string()))?,
                )
                .ok_or_else(|| ParseIntervalError("Duration overflow".to_string()))?;

            rest = next_rest;
        }

        let max = Duration::MAX.as_nanos();

        if total_nanos > max {
            return Err(ParseIntervalError("Duration too long".to_string()));
        }

        Ok(Interval(Duration::from_nanos_u128(total_nanos)))
    }
}

impl Serialize for Interval {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.to_string())
    }
}

impl<'de> Deserialize<'de> for Interval {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        use serde::de::Error as _;

        // Both `interval = 10` and `interval = "10s"` are natural
        // things to write, so accept either.
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Raw {
            Secs(u64),
            Text(String),
        }

        match Raw::deserialize(deserializer)? {
            Raw::Secs(s) => Ok(Interval(Duration::from_secs(s))),
            Raw::Text(s) => s.parse().map_err(D::Error::custom),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    #[test]
    fn test_display_single_units() {
        assert_eq!(Interval(Duration::from_nanos(500)).to_string(), "500ns");
        assert_eq!(Interval(Duration::from_micros(5)).to_string(), "5us");
        assert_eq!(Interval(Duration::from_millis(10)).to_string(), "10ms");
        assert_eq!(Interval(Duration::from_secs(45)).to_string(), "45s");
        assert_eq!(Interval(Duration::from_secs(120)).to_string(), "2m");
        assert_eq!(Interval(Duration::from_secs(3600)).to_string(), "1h");
        assert_eq!(Interval(Duration::from_secs(86400)).to_string(), "1d");
        assert_eq!(Interval(Duration::from_secs(604800)).to_string(), "1w");
    }

    #[test]
    fn test_display_combined_units() {
        // 1 week + 2 days + 3 hours + 4 mins + 5 secs + 6 ms + 7 us + 8 ns
        let nanos = (7 * 86400 + 2 * 86400 + 3 * 3600 + 4 * 60 + 5) * 1_000_000_000u128
            + 6 * 1_000_000
            + 7 * 1_000
            + 8;

        let interval = Interval(Duration::from_nanos(nanos as u64));
        assert_eq!(interval.to_string(), "1w2d3h4m5s6ms7us8ns");
    }

    #[test]
    fn test_display_zero() {
        assert_eq!(Interval(Duration::ZERO).to_string(), "0s");
    }

    #[test]
    fn test_from_str_basic() {
        assert_eq!("".parse::<Interval>(), Ok(Interval(Duration::ZERO)));
        assert_eq!("0".parse::<Interval>(), Ok(Interval(Duration::ZERO)));
        assert_eq!(
            "10".parse::<Interval>(),
            Ok(Interval(Duration::from_secs(10)))
        );
        assert_eq!(
            "500ns".parse::<Interval>(),
            Ok(Interval(Duration::from_nanos(500)))
        );
        assert_eq!(
            "5us".parse::<Interval>(),
            Ok(Interval(Duration::from_micros(5)))
        );
        assert_eq!(
            "10ms".parse::<Interval>(),
            Ok(Interval(Duration::from_millis(10)))
        );
        assert_eq!(
            "45s".parse::<Interval>(),
            Ok(Interval(Duration::from_secs(45)))
        );
        assert_eq!(
            "2m".parse::<Interval>(),
            Ok(Interval(Duration::from_secs(120)))
        );
        assert_eq!(
            "1h".parse::<Interval>(),
            Ok(Interval(Duration::from_secs(3600)))
        );
        assert_eq!(
            "1d".parse::<Interval>(),
            Ok(Interval(Duration::from_secs(86400)))
        );
        assert_eq!(
            "1w".parse::<Interval>(),
            Ok(Interval(Duration::from_secs(604800)))
        );
    }

    #[test]
    fn test_from_str_combined() {
        let expected_secs = (7 * 86400) + (2 * 86400) + (3 * 3600) + (4 * 60) + 5;
        let expected_nanos = (6 * 1_000_000) + (7 * 1_000) + 8;

        let parsed: Interval = "1w2d3h4m5s6ms7us8ns".parse().unwrap();
        assert_eq!(
            parsed,
            Interval(Duration::new(expected_secs, expected_nanos as u32))
        );
    }

    #[test]
    fn test_round_trip() {
        let original_durations = vec![
            Duration::ZERO,
            Duration::from_nanos(1),
            Duration::from_micros(999),
            Duration::from_millis(123456789),
            Duration::from_secs(31536000), // ~1 year in seconds
            Duration::new(123456, 789012345),
        ];

        for dur in original_durations {
            let interval = Interval(dur);
            let formatted = interval.to_string();
            let parsed: Interval = formatted.parse().expect("Failed to parse formatted string");
            assert_eq!(interval, parsed, "Round trip failed for {formatted}");
        }
    }

    #[test]
    fn test_invalid_formats() {
        assert!("abc".parse::<Interval>().is_err());
        assert!("s".parse::<Interval>().is_err()); // Missing quantity
        assert!("-10s".parse::<Interval>().is_err()); // Negative duration
        assert!("10x".parse::<Interval>().is_err()); // Invalid unit
        assert!("10s5".parse::<Interval>().is_err()); // Trailing unitless number
        assert!("10.5s".parse::<Interval>().is_err()); // Decimals not supported
    }

    #[test]
    fn test_overflow_protection() {
        // u128 overflow during scalar multiplication
        assert!(
            "9999999999999999999999999999999999999w"
                .parse::<Interval>()
                .is_err()
        );
        // Value exceeding Duration::MAX (~u64 MAX seconds)
        assert!("18446744073709551616s".parse::<Interval>().is_err());
    }
}
