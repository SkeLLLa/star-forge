//! Hand-rolled duration parser for config timing knobs: `<integer><unit>` where unit is
//! one of `ms|s|m|h` (e.g. `"500ms"`, `"60s"`, `"5m"`, `"1h"`). No derived formulas or
//! multipliers anywhere in this crate — every timing knob is one of these strings with a
//! single fixed default (see `config.rs`).

use std::time::Duration;

/// Parses `<integer><unit>` (unit `ms|s|m|h`), e.g. `"500ms"`, `"60s"`, `"5m"`, `"1h"`.
/// Rejects anything else (missing/unknown unit, empty/non-digit number, overflow) with a
/// message naming the offending string.
pub fn parse(s: &str) -> Result<Duration, String> {
    let split_at = s
        .find(|c: char| !c.is_ascii_digit())
        .ok_or_else(|| format!("invalid duration {s:?}: missing unit (expected ms|s|m|h)"))?;
    let (digits, unit) = s.split_at(split_at);
    if digits.is_empty() {
        return Err(format!("invalid duration {s:?}: missing number"));
    }
    let n: u64 = digits
        .parse()
        .map_err(|_| format!("invalid duration {s:?}: not a valid integer"))?;
    let ms_per_unit: u64 = match unit {
        "ms" => 1,
        "s" => 1_000,
        "m" => 60_000,
        "h" => 3_600_000,
        other => {
            return Err(format!(
                "invalid duration {s:?}: unknown unit {other:?} (expected ms|s|m|h)"
            ));
        }
    };
    let ms = n
        .checked_mul(ms_per_unit)
        .ok_or_else(|| format!("invalid duration {s:?}: overflow"))?;
    Ok(Duration::from_millis(ms))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_every_unit() {
        assert_eq!(parse("500ms").unwrap(), Duration::from_millis(500));
        assert_eq!(parse("60s").unwrap(), Duration::from_secs(60));
        assert_eq!(parse("5m").unwrap(), Duration::from_secs(5 * 60));
        assert_eq!(parse("1h").unwrap(), Duration::from_secs(3600));
    }

    #[test]
    fn parses_zero() {
        assert_eq!(parse("0s").unwrap(), Duration::ZERO);
    }

    #[test]
    fn rejects_missing_unit() {
        assert!(parse("60").is_err());
    }

    #[test]
    fn rejects_missing_number() {
        assert!(parse("s").is_err());
        assert!(parse("").is_err());
    }

    #[test]
    fn rejects_unknown_unit() {
        assert!(parse("60d").is_err());
        assert!(parse("60 s").is_err());
    }

    #[test]
    fn rejects_negative_or_float() {
        assert!(parse("-5s").is_err());
        assert!(parse("1.5s").is_err());
    }

    #[test]
    fn rejects_overflow() {
        assert!(parse("99999999999999999999h").is_err());
    }
}
