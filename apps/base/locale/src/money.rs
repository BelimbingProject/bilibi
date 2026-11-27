//! Money as pages, documents and exports for people show it.
use rust_decimal::{Decimal, RoundingStrategy};

/// Show an amount with exactly `places` decimals and comma thousands groups.
///
/// Rounds half away from zero, so `-1234.565` with 2 places shows `-1,234.57`.
///
/// Shared helper: `let shown = bilibi_locale::money(total, currency.minor_units);`
pub fn money(amount: Decimal, places: u32) -> String {
    let mut fixed = amount.round_dp_with_strategy(places, RoundingStrategy::MidpointAwayFromZero);
    fixed.rescale(places);
    let digits = fixed.abs().to_string();
    let (whole, fraction) = digits.split_once('.').unwrap_or((&digits, ""));
    let mut out = String::with_capacity(digits.len() + whole.len() / 3 + 1);
    if fixed.is_sign_negative() && !fixed.is_zero() {
        out.push('-');
    }
    for (i, digit) in whole.char_indices() {
        if i > 0 && (whole.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(digit);
    }
    if !fraction.is_empty() {
        out.push('.');
        out.push_str(fraction);
    }
    out
}
