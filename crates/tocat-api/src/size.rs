//! size.rs: one grammar for every byte count tocat accepts.
//!
//! Shared rather than per-crate because `buffer-size` in the config, `size=` on
//! a pipe endpoint and `bytes=` on the `limit` plugin should not disagree about
//! what `10M` means. Suffixes are binary: `k` is 1024, not 1000, because the
//! things being sized are buffers and transfers.

use std::{fmt, str::FromStr};

use serde::{Deserialize, Serialize};

/// A byte count.
///
/// Accepts a plain number or a suffix: `65536`, `64k`, `1MiB`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct ByteSize(pub usize);

impl ByteSize {
    #[must_use]
    pub fn bytes(self) -> usize {
        self.0
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseSizeError(String);

impl fmt::Display for ParseSizeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for ParseSizeError {}

impl fmt::Display for ByteSize {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        const UNITS: [(usize, &str); 3] = [
            (1024 * 1024 * 1024, "GiB"),
            (1024 * 1024, "MiB"),
            (1024, "KiB"),
        ];

        for (scale, suffix) in UNITS {
            if self.0 >= scale && self.0.is_multiple_of(scale) {
                return write!(f, "{}{suffix}", self.0 / scale);
            }
        }

        write!(f, "{}", self.0)
    }
}

impl FromStr for ByteSize {
    type Err = ParseSizeError;

    fn from_str(raw: &str) -> Result<Self, Self::Err> {
        let trimmed = raw.trim();
        let digits = trimmed
            .trim_end_matches(|c: char| c.is_ascii_alphabetic())
            .trim_end();
        let suffix = trimmed[digits.len()..].trim().to_ascii_lowercase();

        let scale: usize = match suffix.as_str() {
            "" | "b" => 1,
            "k" | "kb" | "kib" => 1024,
            "m" | "mb" | "mib" => 1024 * 1024,
            "g" | "gb" | "gib" => 1024 * 1024 * 1024,
            other => {
                return Err(ParseSizeError(format!(
                    "unknown size suffix {other:?}; use k, m or g"
                )));
            }
        };

        let value: usize = digits
            .parse()
            .map_err(|_| ParseSizeError(format!("{digits:?} is not a number")))?;

        let bytes = value
            .checked_mul(scale)
            .ok_or_else(|| ParseSizeError(format!("{raw} overflows a size")))?;

        Ok(ByteSize(bytes))
    }
}

impl Serialize for ByteSize {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.to_string())
    }
}

impl<'de> Deserialize<'de> for ByteSize {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        use serde::de::Error as _;

        // Both `buffer-size = 262144` and `buffer-size = "256KiB"` are natural
        // things to write, so accept either.
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Raw {
            Bytes(usize),
            Text(String),
        }

        match Raw::deserialize(deserializer)? {
            Raw::Bytes(n) => Ok(ByteSize(n)),
            Raw::Text(s) => s.parse().map_err(D::Error::custom),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn suffixes_are_binary() {
        assert_eq!("64k".parse::<ByteSize>().unwrap(), ByteSize(65536));
        assert_eq!("1MiB".parse::<ByteSize>().unwrap(), ByteSize(1024 * 1024));
        assert_eq!("512".parse::<ByteSize>().unwrap(), ByteSize(512));
    }

    #[test]
    fn round_multiples_display_with_a_suffix() {
        assert_eq!(ByteSize(1024 * 1024).to_string(), "1MiB");
        assert_eq!(ByteSize(1500).to_string(), "1500");
    }

    #[test]
    fn nonsense_is_rejected() {
        assert!("10furlongs".parse::<ByteSize>().is_err());
        assert!("k".parse::<ByteSize>().is_err());
    }
}
