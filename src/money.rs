//! Exact money. Fiat amounts are integer minor units (cents); token amounts are integer base units.
//! Nothing in the engine touches floating point when it moves money.

use serde::{Deserialize, Serialize};
use std::fmt;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub enum Ccy {
    USD,
    EUR,
}

impl Ccy {
    pub fn code(self) -> &'static str {
        match self {
            Ccy::USD => "USD",
            Ccy::EUR => "EUR",
        }
    }
    pub fn parse(s: &str) -> Option<Ccy> {
        match s.trim() {
            "USD" => Some(Ccy::USD),
            "EUR" => Some(Ccy::EUR),
            _ => None,
        }
    }
    /// Both supported currencies have two minor-unit digits (ISO 4217).
    pub fn scale(self) -> u32 {
        2
    }
}

impl fmt::Display for Ccy {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.code())
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum AmountError {
    Empty,
    NotANumber(String),
    TooManyDecimals(String),
    Negative(String),
    Overflow(String),
}

impl fmt::Display for AmountError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            AmountError::Empty => write!(f, "empty amount"),
            AmountError::NotANumber(s) => write!(f, "not a number: {s:?}"),
            AmountError::TooManyDecimals(s) => write!(f, "more decimals than the currency allows: {s:?}"),
            AmountError::Negative(s) => write!(f, "negative amount: {s:?}"),
            AmountError::Overflow(s) => write!(f, "amount too large: {s:?}"),
        }
    }
}

impl std::error::Error for AmountError {}

/// Parse a decimal string such as `"1234.5"` or `"1234.56"` into minor units, exactly.
/// More decimals than the currency's scale is an error, never a rounding.
pub fn parse_decimal(s: &str, ccy: Ccy) -> Result<i64, AmountError> {
    let t = s.trim();
    if t.is_empty() {
        return Err(AmountError::Empty);
    }
    if t.starts_with('-') {
        return Err(AmountError::Negative(t.to_string()));
    }
    let (int_part, frac_part) = match t.split_once('.') {
        Some((i, f)) => (i, f),
        None => (t, ""),
    };
    if int_part.is_empty() || !int_part.bytes().all(|b| b.is_ascii_digit()) || !frac_part.bytes().all(|b| b.is_ascii_digit()) {
        return Err(AmountError::NotANumber(t.to_string()));
    }
    let scale = ccy.scale() as usize;
    if frac_part.len() > scale {
        // Trailing zeros beyond the scale are harmless ("10.500"); anything else is not.
        if frac_part[scale..].bytes().any(|b| b != b'0') {
            return Err(AmountError::TooManyDecimals(t.to_string()));
        }
    }
    let mut frac: String = frac_part.chars().take(scale).collect();
    while frac.len() < scale {
        frac.push('0');
    }
    let whole: i64 = int_part.parse().map_err(|_| AmountError::Overflow(t.to_string()))?;
    let frac_v: i64 = if scale == 0 { 0 } else { frac.parse().map_err(|_| AmountError::NotANumber(t.to_string()))? };
    whole.checked_mul(10i64.pow(scale as u32)).and_then(|w| w.checked_add(frac_v)).ok_or_else(|| AmountError::Overflow(t.to_string()))
}

/// Format minor units as a plain decimal string with the currency's scale (`12345` → `"123.45"`).
pub fn format_decimal(minor: i64, ccy: Ccy) -> String {
    let scale = ccy.scale();
    let neg = minor < 0;
    let m = minor.unsigned_abs();
    let div = 10u64.pow(scale);
    let s = format!("{}.{:0width$}", m / div, m % div, width = scale as usize);
    if neg { format!("-{s}") } else { s }
}

/// Human formatting with thousands separators, for reports only.
pub fn pretty(minor: i64, ccy: Ccy) -> String {
    let raw = format_decimal(minor.abs(), ccy);
    let (i, f) = raw.split_once('.').unwrap_or((&raw, ""));
    let mut out = String::new();
    for (n, ch) in i.chars().enumerate() {
        if n > 0 && (i.len() - n) % 3 == 0 {
            out.push(',');
        }
        out.push(ch);
    }
    let sym = match ccy {
        Ccy::USD => "$",
        Ccy::EUR => "€",
    };
    format!("{}{sym}{out}.{f}", if minor < 0 { "-" } else { "" })
}

/// Convert fiat minor units into token base units for a token with `decimals` decimals, exactly.
pub fn minor_to_base_units(minor: i64, ccy: Ccy, decimals: u32) -> i128 {
    assert!(decimals >= ccy.scale(), "token decimals below the fiat scale would lose money");
    minor as i128 * 10i128.pow(decimals - ccy.scale())
}

/// Convert token base units back to fiat minor units. Returns `None` if the conversion is not exact
/// (a remainder below one cent), so a caller can never silently lose dust.
pub fn base_units_to_minor(base: i128, ccy: Ccy, decimals: u32) -> Option<i64> {
    let f = 10i128.pow(decimals - ccy.scale());
    if base % f != 0 {
        return None;
    }
    i64::try_from(base / f).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_exactly() {
        assert_eq!(parse_decimal("1234.56", Ccy::EUR), Ok(123456));
        assert_eq!(parse_decimal("1234.5", Ccy::EUR), Ok(123450));
        assert_eq!(parse_decimal("1234", Ccy::USD), Ok(123400));
        assert_eq!(parse_decimal("0.01", Ccy::USD), Ok(1));
        assert_eq!(parse_decimal("10.500", Ccy::USD), Ok(1050));
    }

    #[test]
    fn refuses_to_round() {
        assert!(matches!(parse_decimal("1.005", Ccy::USD), Err(AmountError::TooManyDecimals(_))));
        assert!(matches!(parse_decimal("-1.00", Ccy::USD), Err(AmountError::Negative(_))));
        assert!(matches!(parse_decimal("1,00", Ccy::EUR), Err(AmountError::NotANumber(_))));
        assert!(matches!(parse_decimal("", Ccy::EUR), Err(AmountError::Empty)));
        assert!(matches!(parse_decimal("99999999999999999999", Ccy::EUR), Err(AmountError::Overflow(_))));
    }

    #[test]
    fn formats_round_trip() {
        for m in [0, 1, 99, 100, 123456, 9_999_999_999] {
            assert_eq!(parse_decimal(&format_decimal(m, Ccy::USD), Ccy::USD), Ok(m));
        }
        assert_eq!(pretty(123456789, Ccy::EUR), "€1,234,567.89");
        assert_eq!(pretty(-5000, Ccy::USD), "-$50.00");
    }

    #[test]
    fn token_units_are_exact() {
        assert_eq!(minor_to_base_units(12345, Ccy::USD, 6), 123_450_000);
        assert_eq!(base_units_to_minor(123_450_000, Ccy::USD, 6), Some(12345));
        assert_eq!(base_units_to_minor(123_450_001, Ccy::USD, 6), None);
    }
}
