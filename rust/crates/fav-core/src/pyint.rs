//! `int(text)`: Python's parsing of a base-10 integer from a string.
//!
//! Python accepts surrounding whitespace, a sign, any Unicode decimal digit (category
//! `Nd`, so `'٣'` is 3) and single underscores between digits. Its integers are unbounded;
//! this model keeps 128 bits and says when a number does not fit.

use crate::pytext::py_strip;

/// The code points of the `0` of every run of ten Unicode decimal digits (Python 3.12,
/// Unicode 15.0): each `Nd` character is `zero + value` for one of these.
const DECIMAL_ZEROS: [u32; 68] = [
    0x30, 0x660, 0x6f0, 0x7c0, 0x966, 0x9e6, 0xa66, 0xae6, 0xb66, 0xbe6, 0xc66, 0xce6, 0xd66, 0xde6, 0xe50, 0xed0, 0xf20, 0x1040, 0x1090,
    0x17e0, 0x1810, 0x1946, 0x19d0, 0x1a80, 0x1a90, 0x1b50, 0x1bb0, 0x1c40, 0x1c50, 0xa620, 0xa8d0, 0xa900, 0xa9d0, 0xa9f0, 0xaa50, 0xabf0,
    0xff10, 0x104a0, 0x10d30, 0x11066, 0x110f0, 0x11136, 0x111d0, 0x112f0, 0x11450, 0x114d0, 0x11650, 0x116c0, 0x11730, 0x118e0, 0x11950,
    0x11c50, 0x11d50, 0x11da0, 0x11f50, 0x16a60, 0x16ac0, 0x16b50, 0x1d7ce, 0x1d7d8, 0x1d7e2, 0x1d7ec, 0x1d7f6, 0x1e140, 0x1e2f0, 0x1e4f0,
    0x1e950, 0x1fbf0,
];

/// `unicodedata.decimal(ch)`: the value of a decimal digit, `None` for anything else
/// (superscripts and circled digits are digits to `str.isdigit()` but not decimals).
pub fn decimal_value(ch: char) -> Option<u32> {
    let code = u32::from(ch);
    let index = DECIMAL_ZEROS.partition_point(|&zero| zero <= code);
    let zero = *DECIMAL_ZEROS.get(index.checked_sub(1)?)?;
    let value = code - zero;
    (value < 10).then_some(value)
}

/// `str.isdecimal()`.
pub fn py_isdecimal(text: &str) -> bool {
    !text.is_empty() && text.chars().all(|ch| decimal_value(ch).is_some())
}

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
    use super::{IntError, decimal_digits_to_float, decimal_value, parse_int, py_isdecimal};

    #[test]
    fn decimals_are_category_nd_only() {
        assert_eq!(decimal_value('7'), Some(7));
        assert_eq!(decimal_value('٣'), Some(3));
        assert_eq!(decimal_value('९'), Some(9));
        assert_eq!(decimal_value('\u{1d7d8}'), Some(0));
        assert_eq!(decimal_value('²'), None);
        assert_eq!(decimal_value('a'), None);
        assert!(py_isdecimal("१८००"));
        assert!(!py_isdecimal(""));
    }

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
