//! Exact fixed-point money amounts.
//!
//! Every amount is stored as a whole number of 1/10 000 units (`1.5` is
//! `15000`), so additions and subtractions are exact integer operations and
//! nothing is ever rounded. Floats are never involved, not even while parsing.

use std::fmt;
use std::str::FromStr;

/// Number of digits kept after the decimal point.
const DECIMALS: usize = 4;
/// Units per whole currency unit: 10^DECIMALS.
const SCALE: u64 = 10_000;

/// A signed amount of money with exactly four decimal places.
///
/// Arithmetic is only available through `checked_*` methods, so an overflow
/// can never wrap around or panic; callers must handle it.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Amount(i64);

impl Amount {
    pub const ZERO: Amount = Amount(0);

    /// Builds an amount from a raw count of 1/10 000 units.
    pub const fn from_units(units: i64) -> Self {
        Amount(units)
    }

    pub fn checked_add(self, other: Amount) -> Option<Amount> {
        self.0.checked_add(other.0).map(Amount)
    }

    pub fn checked_sub(self, other: Amount) -> Option<Amount> {
        self.0.checked_sub(other.0).map(Amount)
    }
}

/// Why a string could not be read as an [`Amount`].
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ParseAmountError {
    #[error("amount is empty")]
    Empty,
    #[error("amount is not a plain decimal number")]
    Invalid,
    #[error("amount has more than {DECIMALS} significant decimal places")]
    TooPrecise,
    #[error("amount is too large")]
    Overflow,
}

impl FromStr for Amount {
    type Err = ParseAmountError;

    /// Accepts plain decimals such as `1`, `1.5`, `.5`, `5.` and `-0.25`,
    /// with surrounding whitespace.
    ///
    /// More than four decimals are accepted only when the extra digits are
    /// zeros (`1.50000`), because then no value is lost. Anything that would
    /// need rounding (`1.23456`) is refused rather than silently changed.
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let s = s.trim();
        if s.is_empty() {
            return Err(ParseAmountError::Empty);
        }

        let (negative, digits) = match s.strip_prefix('-') {
            Some(rest) => (true, rest),
            None => (false, s),
        };
        let (int_part, frac_part) = digits.split_once('.').unwrap_or((digits, ""));

        let all_digits = |part: &str| part.bytes().all(|b| b.is_ascii_digit());
        if (int_part.is_empty() && frac_part.is_empty())
            || !all_digits(int_part)
            || !all_digits(frac_part)
        {
            return Err(ParseAmountError::Invalid);
        }

        // Only ASCII digits remain, so splitting at a byte index is safe.
        let (kept, extra) = frac_part.split_at(frac_part.len().min(DECIMALS));
        if extra.bytes().any(|b| b != b'0') {
            return Err(ParseAmountError::TooPrecise);
        }

        // Read "<int><kept><padding zeros>" as one integer number of units.
        let padding = std::iter::repeat(b'0').take(DECIMALS - kept.len());
        let units = int_part
            .bytes()
            .chain(kept.bytes())
            .chain(padding)
            .try_fold(0i64, |acc, digit| {
                acc.checked_mul(10)?.checked_add(i64::from(digit - b'0'))
            })
            .ok_or(ParseAmountError::Overflow)?;

        Ok(Amount(if negative { -units } else { units }))
    }
}

impl fmt::Display for Amount {
    /// Always prints exactly four decimals, e.g. `1.5000` or `-0.2500`.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let sign = if self.0 < 0 { "-" } else { "" };
        // `unsigned_abs` cannot overflow, even for `i64::MIN`.
        let abs = self.0.unsigned_abs();
        write!(
            f,
            "{sign}{}.{:0width$}",
            abs / SCALE,
            abs % SCALE,
            width = DECIMALS
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(s: &str) -> Result<Amount, ParseAmountError> {
        s.parse()
    }

    #[test]
    fn parses_plain_decimals() {
        assert_eq!(parse("1"), Ok(Amount(10_000)));
        assert_eq!(parse("1.0"), Ok(Amount(10_000)));
        assert_eq!(parse("1.5"), Ok(Amount(15_000)));
        assert_eq!(parse("0.0001"), Ok(Amount(1)));
        assert_eq!(parse("123.4567"), Ok(Amount(1_234_567)));
        assert_eq!(parse("007.5"), Ok(Amount(75_000)));
    }

    #[test]
    fn trims_surrounding_whitespace() {
        assert_eq!(parse("  2.5 "), Ok(Amount(25_000)));
        assert_eq!(parse("\t3\t"), Ok(Amount(30_000)));
    }

    #[test]
    fn accepts_shorthand_forms() {
        assert_eq!(parse(".5"), Ok(Amount(5_000)));
        assert_eq!(parse("5."), Ok(Amount(50_000)));
    }

    #[test]
    fn accepts_extra_decimals_only_when_they_are_zeros() {
        assert_eq!(parse("1.50000"), Ok(Amount(15_000)));
        assert_eq!(parse("1.000000000"), Ok(Amount(10_000)));
        assert_eq!(parse("1.23456"), Err(ParseAmountError::TooPrecise));
        assert_eq!(parse("0.00001"), Err(ParseAmountError::TooPrecise));
    }

    #[test]
    fn parses_negative_amounts() {
        assert_eq!(parse("-0.25"), Ok(Amount(-2_500)));
        assert_eq!(parse("-3"), Ok(Amount(-30_000)));
    }

    #[test]
    fn rejects_malformed_input() {
        assert_eq!(parse(""), Err(ParseAmountError::Empty));
        assert_eq!(parse("   "), Err(ParseAmountError::Empty));
        for bad in [
            ".", "-", "-.", "+1", "1e3", "abc", "1.2.3", "1,5", "- 1", "1 000", "0x10", "١",
        ] {
            assert_eq!(parse(bad), Err(ParseAmountError::Invalid), "input {bad:?}");
        }
    }

    #[test]
    fn rejects_values_that_do_not_fit() {
        assert_eq!(parse("922337203685477.5807"), Ok(Amount(i64::MAX)));
        assert_eq!(
            parse("922337203685477.5808"),
            Err(ParseAmountError::Overflow)
        );
        assert_eq!(
            parse("99999999999999999999"),
            Err(ParseAmountError::Overflow)
        );
    }

    #[test]
    fn always_displays_four_decimals() {
        assert_eq!(Amount(0).to_string(), "0.0000");
        assert_eq!(Amount(1).to_string(), "0.0001");
        assert_eq!(Amount(15_000).to_string(), "1.5000");
        assert_eq!(Amount(1_234_567).to_string(), "123.4567");
    }

    #[test]
    fn displays_small_negatives_with_their_sign() {
        assert_eq!(Amount(-5_000).to_string(), "-0.5000");
        assert_eq!(Amount(-1).to_string(), "-0.0001");
        assert_eq!(Amount(-15_000).to_string(), "-1.5000");
    }

    #[test]
    fn displays_extreme_values_without_panicking() {
        assert_eq!(Amount(i64::MAX).to_string(), "922337203685477.5807");
        assert_eq!(Amount(i64::MIN).to_string(), "-922337203685477.5808");
    }

    #[test]
    fn display_then_parse_round_trips() {
        for units in [0, 1, -1, 5_000, -5_000, 15_000, 1_234_567, i64::MAX] {
            let amount = Amount(units);
            assert_eq!(parse(&amount.to_string()), Ok(amount));
        }
    }

    #[test]
    fn checked_arithmetic_reports_overflow() {
        assert_eq!(Amount(1).checked_add(Amount(2)), Some(Amount(3)));
        assert_eq!(Amount(1).checked_sub(Amount(2)), Some(Amount(-1)));
        assert_eq!(Amount(i64::MAX).checked_add(Amount(1)), None);
        assert_eq!(Amount(i64::MIN).checked_sub(Amount(1)), None);
    }
}
