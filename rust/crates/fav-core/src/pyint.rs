//! `int(text)`: Python's parsing of a base-10 integer from a string.
//!
//! Python accepts surrounding whitespace, a sign, any Unicode decimal digit (category
//! `Nd`, so `'٣'` is 3, read with [`crate::pytext::py_decimal_value`]) and single
//! underscores between digits. Its integers are unbounded;
//! this model keeps 128 bits and says when a number does not fit.

use crate::pytext::{py_decimal_value as decimal_value, py_strip};

/// Why `int(text)` gave no `i128`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IntError {
    /// Python raises `ValueError: invalid literal for int() with base 10: ...`.
    Invalid,
    /// A valid integer outside `i128`, which Python would have returned.
    Overflow { negative: bool },
}

/// `int(text)` in base 10.
pub fn parse_int(text: &str) -> Result<i128, IntError> {
    let stripped = py_strip(text);
    let (negative, digits) = match stripped.chars().next() {
        Some('-') => (true, &stripped[1..]),
        Some('+') => (false, &stripped[1..]),
        _ => (false, stripped),
    };
    if digits.is_empty() || digits.starts_with('_') || digits.ends_with('_') || digits.contains("__") {
        return Err(IntError::Invalid);
    }
    let mut value: i128 = 0;
    let mut overflow = false;
    for ch in digits.chars() {
        if ch == '_' {
            continue;
        }
        let digit = decimal_value(ch).ok_or(IntError::Invalid)?;
        match value.checked_mul(10).and_then(|tens| tens.checked_add(i128::from(digit))) {
            Some(next) => value = next,
            None => overflow = true,
        }
    }
    if overflow {
        return Err(IntError::Overflow { negative });
    }
    Ok(if negative { -value } else { value })
}

/// `float(text)` for a string that is all decimal digits, possibly padded with
/// whitespace: the digits' value, as Python's correctly rounded conversion gives it.
pub fn decimal_digits_to_float(text: &str) -> Option<f64> {
    let digits: String = py_strip(text)
        .chars()
        .map(|ch| decimal_value(ch).and_then(|value| char::from_digit(value, 10)))
        .collect::<Option<String>>()?;
    if digits.is_empty() {
        return None;
    }
    digits.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::{IntError, decimal_digits_to_float, parse_int};

    #[test]
    fn int_reads_like_python() {
        assert_eq!(parse_int(" 42 "), Ok(42));
        assert_eq!(parse_int("+60"), Ok(60));
        assert_eq!(parse_int("-1"), Ok(-1));
        assert_eq!(parse_int("1_0"), Ok(10));
        assert_eq!(parse_int("007"), Ok(7));
        assert_eq!(parse_int("٣"), Ok(3));
        for bad in ["", "-", "_1", "1_", "1__0", "1.5", "²", "0x10", "1 2", "--1"] {
            assert_eq!(parse_int(bad), Err(IntError::Invalid), "{bad:?}");
        }
        assert_eq!(parse_int(&"9".repeat(50)), Err(IntError::Overflow { negative: false }));
    }

    #[test]
    fn digit_strings_become_floats() {
        assert_eq!(decimal_digits_to_float("१८००९००००००"), Some(18_009_000_000.0));
        assert_eq!(decimal_digits_to_float("²"), None);
    }
}
