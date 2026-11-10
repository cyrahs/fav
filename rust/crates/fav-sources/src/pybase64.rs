//! `base64.b64decode(text, validate=...)`: `CPython` 3.12's `binascii.a2b_base64`.
//!
//! Non-strict (`validate=False`) skips characters outside the alphabet and stops at a
//! completed pad; strict (`validate=True`) refuses them, a leading pad, a pad where a quad
//! starts, data after the pad and data between pads. Both refuse a quad left incomplete.
//! The WeChat parsers only need whether decoding succeeds, so a failure is
//! `binascii.Error` with Python's text and a non-ASCII string the `ValueError` `str`
//! arguments get.
//!
//! Jandan's `crypto.rs` keeps an earlier non-strict copy of this (PLAN.md cleanup notes).

use crate::pydecode::PyError;

fn error(message: &str) -> PyError {
    PyError::new("Error", message)
}

fn table(byte: u8) -> Option<u8> {
    match byte {
        b'A'..=b'Z' => Some(byte - b'A'),
        b'a'..=b'z' => Some(byte - b'a' + 26),
        b'0'..=b'9' => Some(byte - b'0' + 52),
        b'+' => Some(62),
        b'/' => Some(63),
        _ => None,
    }
}

/// `base64.b64decode(text, validate=strict)` for a `str` argument.
pub(crate) fn b64decode(text: &str, strict: bool) -> Result<Vec<u8>, PyError> {
    if !text.is_ascii() {
        return Err(PyError::new("ValueError", "string argument should contain only ASCII characters"));
    }
    a2b_base64(text.as_bytes(), strict)
}

/// `binascii.a2b_base64(data, strict_mode=strict)`.
pub(crate) fn a2b_base64(data: &[u8], strict: bool) -> Result<Vec<u8>, PyError> {
    if strict && data.first() == Some(&b'=') {
        return Err(error("Leading padding not allowed"));
    }
    let mut out = Vec::with_capacity(data.len() / 4 * 3);
    let mut quad_pos = 0_u8;
    let mut left = 0_u8;
    let mut pads = 0_u8;
    let mut padding_started = false;
    for (index, &byte) in data.iter().enumerate() {
        if byte == b'=' {
            padding_started = true;
            if strict && quad_pos == 0 {
                return Err(error("Excess padding not allowed"));
            }
            if quad_pos >= 2 {
                pads += 1;
                if quad_pos + pads >= 4 {
                    if strict && index + 1 < data.len() {
                        return Err(error("Excess data after padding"));
                    }
                    return Ok(out);
                }
            }
            continue;
        }
        let Some(value) = table(byte) else {
            if strict {
                return Err(error("Only base64 data is allowed"));
            }
            continue;
        };
        if strict && padding_started {
            return Err(error("Discontinuous padding not allowed"));
        }
        pads = 0;
        match quad_pos {
            0 => {
                quad_pos = 1;
                left = value;
            }
            1 => {
                quad_pos = 2;
                out.push((left << 2) | (value >> 4));
                left = value & 0x0f;
            }
            2 => {
                quad_pos = 3;
                out.push((left << 4) | (value >> 2));
                left = value & 0x03;
            }
            _ => {
                quad_pos = 0;
                out.push((left << 6) | value);
                left = 0;
            }
        }
    }
    match quad_pos {
        0 => Ok(out),
        1 => {
            // CPython counts the characters from the bytes decoded so far.
            let characters = out.len() / 3 * 4 + 1;
            Err(error(&format!(
                "Invalid base64-encoded string: number of data characters ({characters}) cannot be 1 more than a multiple of 4"
            )))
        }
        _ => Err(error("Incorrect padding")),
    }
}

#[cfg(test)]
mod tests {
    use super::b64decode;

    #[test]
    fn decodes_like_binascii() {
        for (text, strict, expected) in [
            ("QUJD=", true, Err("Excess padding not allowed")),
            ("QUJD=", false, Ok(b"ABC".to_vec())),
            ("QUJDRA===", true, Err("Excess data after padding")),
            ("QUJDRA===", false, Ok(b"ABCD".to_vec())),
            ("QUJD=QUJD", true, Err("Excess padding not allowed")),
            ("QUJD=QUJD", false, Ok(b"ABCABC".to_vec())),
            ("QUJDRA==QQ==", true, Err("Excess data after padding")),
            ("QUJDRA==QQ==", false, Ok(b"ABCD".to_vec())),
            ("=QUJD", true, Err("Leading padding not allowed")),
            ("=QUJD", false, Ok(b"ABC".to_vec())),
            (
                "QUJDR",
                true,
                Err("Invalid base64-encoded string: number of data characters (5) cannot be 1 more than a multiple of 4"),
            ),
            ("QUJDRA=", false, Err("Incorrect padding")),
            ("QUJDRA", true, Err("Incorrect padding")),
            ("QU!JD", true, Err("Only base64 data is allowed")),
            ("QU!JD", false, Ok(b"ABC".to_vec())),
            ("QUJDRA=x=", true, Err("Discontinuous padding not allowed")),
        ] {
            let decoded = b64decode(text, strict);
            match expected {
                Ok(bytes) => assert_eq!(decoded.expect(text), bytes, "{text} {strict}"),
                Err(message) => assert_eq!(decoded.expect_err(text).message, message, "{text} {strict}"),
            }
        }
        assert_eq!(b64decode("中", false).expect_err("non-ASCII").class, "ValueError");
    }
}
