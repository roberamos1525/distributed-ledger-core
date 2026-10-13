//! Money primitives: ISO 4217 currencies, minor units and exact decimal parsing.
//!
//! All amounts are stored as signed 64-bit integers of the currency's minor
//! unit (cents for USD, yen for JPY, fils for BHD). Floating point is never
//! used; decimal strings are parsed digit by digit and rejected when they carry
//! more precision than the currency allows.

use std::collections::BTreeMap;
use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::error::{LedgerError, LedgerResult};

/// Highest number of minor-unit digits supported for any currency.
pub const MAX_MINOR_UNITS: u8 = 8;

/// A three-letter upper-case currency code (ISO 4217 alphabetic code).
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct CurrencyCode([u8; 3]);

impl CurrencyCode {
    /// Parses a code, requiring exactly three ASCII upper-case letters.
    pub fn new(raw: &str) -> LedgerResult<Self> {
        let bytes = raw.as_bytes();
        if bytes.len() != 3 || !bytes.iter().all(u8::is_ascii_uppercase) {
            return Err(LedgerError::validation(format!(
                "currency code {raw:?} must be three upper-case letters"
            )));
        }
        Ok(CurrencyCode([bytes[0], bytes[1], bytes[2]]))
    }

    pub fn as_str(&self) -> &str {
        // Construction guarantees ASCII upper-case letters.
        std::str::from_utf8(&self.0).unwrap_or("???")
    }
}

impl fmt::Debug for CurrencyCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "CurrencyCode({})", self.as_str())
    }
}

impl fmt::Display for CurrencyCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for CurrencyCode {
    type Err = LedgerError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        CurrencyCode::new(s)
    }
}

impl Serialize for CurrencyCode {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for CurrencyCode {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let raw = String::deserialize(d)?;
        CurrencyCode::new(&raw).map_err(serde::de::Error::custom)
    }
}

/// A currency together with the number of digits of its minor unit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Currency {
    pub code: CurrencyCode,
    pub minor_units: u8,
}

impl Currency {
    pub fn new(code: &str, minor_units: u8) -> LedgerResult<Self> {
        if minor_units > MAX_MINOR_UNITS {
            return Err(LedgerError::validation(format!(
                "currency {code} minor units must be <= {MAX_MINOR_UNITS}"
            )));
        }
        Ok(Currency {
            code: CurrencyCode::new(code)?,
            minor_units,
        })
    }

    /// Parses a decimal string (e.g. `"-12.50"`) into minor units.
    pub fn parse_amount(&self, raw: &str) -> LedgerResult<i64> {
        parse_decimal(raw, self.minor_units)
    }

    /// Formats minor units as a decimal string with exactly `minor_units` digits.
    pub fn format_amount(&self, minor: i64) -> String {
        format_decimal(minor, self.minor_units)
    }
}

/// ISO 4217 currencies known out of the box: (code, minor units).
///
/// This is intentionally a focused list of widely used currencies, including
/// zero-decimal (JPY, KRW) and three-decimal (BHD, KWD) examples. Deployments
/// can register additional currencies through configuration.
const ISO_4217: &[(&str, u8)] = &[
    ("AED", 2),
    ("AUD", 2),
    ("BHD", 3),
    ("BRL", 2),
    ("CAD", 2),
    ("CHF", 2),
    ("CLP", 0),
    ("CNY", 2),
    ("CZK", 2),
    ("DKK", 2),
    ("EUR", 2),
    ("GBP", 2),
    ("HKD", 2),
    ("HUF", 2),
    ("IDR", 2),
    ("ILS", 2),
    ("INR", 2),
    ("ISK", 0),
    ("JOD", 3),
    ("JPY", 0),
    ("KRW", 0),
    ("KWD", 3),
    ("MXN", 2),
    ("NOK", 2),
    ("NZD", 2),
    ("OMR", 3),
    ("PLN", 2),
    ("SAR", 2),
    ("SEK", 2),
    ("SGD", 2),
    ("THB", 2),
    ("TND", 3),
    ("TRY", 2),
    ("USD", 2),
    ("VND", 0),
    ("XAF", 0),
    ("ZAR", 2),
];

/// Registry of currencies accepted by the ledger.
#[derive(Debug, Clone)]
pub struct CurrencyRegistry {
    currencies: BTreeMap<CurrencyCode, Currency>,
}

impl Default for CurrencyRegistry {
    fn default() -> Self {
        Self::iso4217()
    }
}

impl CurrencyRegistry {
    /// Registry pre-populated with the built-in ISO 4217 subset.
    pub fn iso4217() -> Self {
        let currencies = ISO_4217
            .iter()
            .map(|(code, units)| {
                let c = Currency::new(code, *units).expect("built-in currency table is valid");
                (c.code, c)
            })
            .collect();
        CurrencyRegistry { currencies }
    }

    /// Registers (or overrides) a currency.
    pub fn register(&mut self, currency: Currency) {
        self.currencies.insert(currency.code, currency);
    }

    pub fn get(&self, code: CurrencyCode) -> Option<Currency> {
        self.currencies.get(&code).copied()
    }

    /// Looks up a currency by its code, failing with a validation error.
    pub fn resolve(&self, code: &str) -> LedgerResult<Currency> {
        let parsed = CurrencyCode::new(code)?;
        self.get(parsed)
            .ok_or_else(|| LedgerError::validation(format!("unsupported currency {code}")))
    }

    pub fn len(&self) -> usize {
        self.currencies.len()
    }

    pub fn is_empty(&self) -> bool {
        self.currencies.is_empty()
    }

    pub fn iter(&self) -> impl Iterator<Item = &Currency> {
        self.currencies.values()
    }
}

/// Parses a decimal string into an integer amount of minor units.
///
/// Accepts an optional leading sign, digits, and an optional fractional part
/// with at most `minor_units` digits. Thousands separators, exponents and
/// whitespace inside the number are rejected.
pub fn parse_decimal(raw: &str, minor_units: u8) -> LedgerResult<i64> {
    let s = raw.trim();
    if s.is_empty() {
        return Err(LedgerError::validation("amount must not be empty"));
    }
    let (negative, body) = match s.as_bytes()[0] {
        b'-' => (true, &s[1..]),
        b'+' => (false, &s[1..]),
        _ => (false, s),
    };
    let (int_part, frac_part) = match body.split_once('.') {
        Some((i, f)) => (i, Some(f)),
        None => (body, None),
    };
    if int_part.is_empty() && frac_part.is_none_or(str::is_empty) {
        return Err(LedgerError::validation(format!("invalid amount {raw:?}")));
    }
    let all_digits = |p: &str| p.bytes().all(|b| b.is_ascii_digit());
    if !all_digits(int_part) || !frac_part.is_none_or(all_digits) {
        return Err(LedgerError::validation(format!("invalid amount {raw:?}")));
    }
    let frac = frac_part.unwrap_or("");
    if frac.len() > minor_units as usize {
        return Err(LedgerError::validation(format!(
            "amount {raw:?} has more than {minor_units} decimal places"
        )));
    }
    let overflow = || LedgerError::validation(format!("amount {raw:?} is out of range"));
    let scale = 10i128.pow(minor_units as u32);
    let mut int_value: i128 = 0;
    for b in int_part.bytes() {
        int_value = int_value
            .checked_mul(10)
            .and_then(|v| v.checked_add((b - b'0') as i128))
            .ok_or_else(overflow)?;
        if int_value > i64::MAX as i128 {
            return Err(overflow());
        }
    }
    let mut frac_value: i128 = 0;
    for b in frac.bytes() {
        frac_value = frac_value * 10 + (b - b'0') as i128;
    }
    frac_value *= 10i128.pow((minor_units as usize - frac.len()) as u32);
    let magnitude = int_value
        .checked_mul(scale)
        .and_then(|v| v.checked_add(frac_value))
        .ok_or_else(overflow)?;
    let signed = if negative { -magnitude } else { magnitude };
    i64::try_from(signed).map_err(|_| overflow())
}

/// Formats an amount of minor units as a decimal string.
pub fn format_decimal(minor: i64, minor_units: u8) -> String {
    let negative = minor < 0;
    let magnitude = (minor as i128).unsigned_abs();
    if minor_units == 0 {
        return format!("{}{}", if negative { "-" } else { "" }, magnitude);
    }
    let scale = 10u128.pow(minor_units as u32);
    format!(
        "{}{}.{:0width$}",
        if negative { "-" } else { "" },
        magnitude / scale,
        magnitude % scale,
        width = minor_units as usize
    )
}

/// An amount in a specific currency.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Money {
    pub amount: i64,
    pub currency: CurrencyCode,
}

impl Money {
    pub fn new(amount: i64, currency: CurrencyCode) -> Self {
        Money { amount, currency }
    }

    pub fn checked_add(self, other: Money) -> LedgerResult<Money> {
        self.ensure_same_currency(other)?;
        self.amount
            .checked_add(other.amount)
            .map(|amount| Money::new(amount, self.currency))
            .ok_or(LedgerError::Overflow("money addition"))
    }

    pub fn checked_sub(self, other: Money) -> LedgerResult<Money> {
        self.ensure_same_currency(other)?;
        self.amount
            .checked_sub(other.amount)
            .map(|amount| Money::new(amount, self.currency))
            .ok_or(LedgerError::Overflow("money subtraction"))
    }

    pub fn checked_neg(self) -> LedgerResult<Money> {
        self.amount
            .checked_neg()
            .map(|amount| Money::new(amount, self.currency))
            .ok_or(LedgerError::Overflow("money negation"))
    }

    fn ensure_same_currency(&self, other: Money) -> LedgerResult<()> {
        if self.currency != other.currency {
            return Err(LedgerError::validation(format!(
                "cannot combine {} with {}",
                self.currency, other.currency
            )));
        }
        Ok(())
    }

    /// Renders the amount using the currency's minor units.
    pub fn display_with(&self, currency: &Currency) -> String {
        format!("{} {}", currency.format_amount(self.amount), self.currency)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn currency_codes_validate() {
        assert!(CurrencyCode::new("USD").is_ok());
        for bad in ["usd", "US", "USDX", "U$D", ""] {
            assert!(CurrencyCode::new(bad).is_err(), "{bad}");
        }
        let json = serde_json::to_string(&CurrencyCode::new("EUR").unwrap()).unwrap();
        assert_eq!(json, "\"EUR\"");
        assert!(serde_json::from_str::<CurrencyCode>("\"eur\"").is_err());
    }

    #[test]
    fn registry_resolves_known_currencies() {
        let reg = CurrencyRegistry::iso4217();
        assert_eq!(reg.resolve("USD").unwrap().minor_units, 2);
        assert_eq!(reg.resolve("JPY").unwrap().minor_units, 0);
        assert_eq!(reg.resolve("BHD").unwrap().minor_units, 3);
        assert!(reg.resolve("XXX").is_err());
        assert!(!reg.is_empty());
    }

    #[test]
    fn registry_accepts_custom_currency() {
        let mut reg = CurrencyRegistry::iso4217();
        let before = reg.len();
        reg.register(Currency::new("XTS", 4).unwrap());
        assert_eq!(reg.len(), before + 1);
        assert_eq!(reg.resolve("XTS").unwrap().minor_units, 4);
        assert!(Currency::new("XBT", 9).is_err());
    }

    #[test]
    fn parses_decimals_exactly() {
        assert_eq!(parse_decimal("12.34", 2).unwrap(), 1234);
        assert_eq!(parse_decimal("12.3", 2).unwrap(), 1230);
        assert_eq!(parse_decimal("12", 2).unwrap(), 1200);
        assert_eq!(parse_decimal("-0.01", 2).unwrap(), -1);
        assert_eq!(parse_decimal("+5", 0).unwrap(), 5);
        assert_eq!(parse_decimal(".5", 2).unwrap(), 50);
        assert_eq!(parse_decimal("7.", 2).unwrap(), 700);
        assert_eq!(parse_decimal(" 1.250 ", 3).unwrap(), 1250);
    }

    #[test]
    fn rejects_malformed_decimals() {
        for bad in [
            "", "-", ".", "1.2.3", "1,000", "1e3", "abc", "1.234", "--1", "1 000",
        ] {
            assert!(parse_decimal(bad, 2).is_err(), "{bad}");
        }
        assert!(parse_decimal("1.5", 0).is_err());
        assert!(parse_decimal("99999999999999999999", 2).is_err());
        assert!(parse_decimal("92233720368547758.08", 2).is_err());
        assert_eq!(parse_decimal("92233720368547758.07", 2).unwrap(), i64::MAX);
    }

    #[test]
    fn formats_decimals() {
        assert_eq!(format_decimal(1234, 2), "12.34");
        assert_eq!(format_decimal(-5, 2), "-0.05");
        assert_eq!(format_decimal(0, 3), "0.000");
        assert_eq!(format_decimal(1500, 0), "1500");
        assert_eq!(format_decimal(i64::MIN, 2), "-92233720368547758.08");
    }

    #[test]
    fn money_arithmetic_is_checked() {
        let usd = CurrencyCode::new("USD").unwrap();
        let eur = CurrencyCode::new("EUR").unwrap();
        let a = Money::new(100, usd);
        assert_eq!(a.checked_add(Money::new(50, usd)).unwrap().amount, 150);
        assert_eq!(a.checked_sub(Money::new(150, usd)).unwrap().amount, -50);
        assert!(a.checked_add(Money::new(1, eur)).is_err());
        assert!(
            Money::new(i64::MAX, usd)
                .checked_add(Money::new(1, usd))
                .is_err()
        );
        assert!(Money::new(i64::MIN, usd).checked_neg().is_err());
        let currency = Currency::new("USD", 2).unwrap();
        assert_eq!(a.display_with(&currency), "1.00 USD");
    }

    proptest! {
        #[test]
        fn format_parse_roundtrip(minor in any::<i64>().prop_filter("min", |v| *v != i64::MIN), units in 0u8..=8) {
            let text = format_decimal(minor, units);
            prop_assert_eq!(parse_decimal(&text, units).unwrap(), minor);
        }
    }
}
