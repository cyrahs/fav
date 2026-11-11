//! The API's `data` field: AES-256-CBC with Jandan's fixed key and a zero IV, PKCS#7
//! padding, base64, then URL-quoted.
//!
//! Python origin: `decrypt_data_field` / `encrypt_data_field` and the PKCS#7 helpers in
//! `src/web/jandan.py`. The failures keep Python's classes and texts: `base64.b64decode`
//! is binascii's lenient decoder (characters outside the alphabet are skipped, decoding
//! stops at a completed pad), `PyCryptodome` refuses a ciphertext that is not whole blocks.

use aes::Aes256;
use aes::cipher::{Array, BlockCipherDecrypt as _, BlockCipherEncrypt as _, KeyInit as _};
use fav_core::pyurl::{quote, unquote};
use fav_runtime::source::JobError;

use crate::pybase64::{self, b64encode};
use crate::pydecode::PyError;
use crate::pyerr;

const BLOCK_SIZE: usize = 16;

fn cipher() -> Aes256 {
    let mut key = [0_u8; 32];
    key[..7].copy_from_slice(b"jD8sW=!");
    Aes256::new(&Array::from(key))
}

fn value_error(message: &str) -> JobError {
    JobError::new("ValueError", message)
}

/// `_pkcs7_unpad(data)`.
fn pkcs7_unpad(data: &[u8]) -> Result<&[u8], JobError> {
    if data.is_empty() || !data.len().is_multiple_of(BLOCK_SIZE) {
        return Err(value_error("invalid AES payload size"));
    }
    let pad = usize::from(data[data.len() - 1]);
    if !(1..=BLOCK_SIZE).contains(&pad) {
        return Err(value_error("invalid PKCS7 padding length"));
    }
    let (plain, padding) = data.split_at(data.len() - pad);
    if padding.iter().any(|&byte| usize::from(byte) != pad) {
        return Err(value_error("invalid PKCS7 padding bytes"));
    }
    Ok(plain)
}

/// `base64.b64decode(text)`: `binascii.a2b_base64` in its default, non-strict mode.
fn b64decode(text: &str) -> Result<Vec<u8>, JobError> {
    pybase64::b64decode(text, false).map_err(PyError::into_job_error)
}

/// `decrypt_data_field(data_value)`: the plaintext JSON of an API `data` field.
pub fn decrypt_data_field(data_value: &str) -> Result<String, JobError> {
    let mut raw_b64 = unquote(data_value);
    // Python pads by `len()`, which counts code points.
    let missing = (4 - raw_b64.chars().count() % 4) % 4;
    raw_b64.extend(std::iter::repeat_n('=', missing));
    let mut data = b64decode(&raw_b64)?;
    if data.len() % BLOCK_SIZE != 0 {
        return Err(value_error("Data must be padded to 16 byte boundary in CBC mode"));
    }
    let cipher = cipher();
    let mut previous = [0_u8; BLOCK_SIZE];
    for block in data.chunks_exact_mut(BLOCK_SIZE) {
        let mut current = [0_u8; BLOCK_SIZE];
        current.copy_from_slice(block);
        let mut array = Array::from(current);
        cipher.decrypt_block(&mut array);
        for (out, (plain, chain)) in block.iter_mut().zip(array.iter().zip(previous)) {
            *out = plain ^ chain;
        }
        previous = current;
    }
    pyerr::decode_utf8(pkcs7_unpad(&data)?)
}

/// `encrypt_data_field(plain_json)`: the `data` field for a request.
pub fn encrypt_data_field(plain_json: &str) -> String {
    let mut data = plain_json.as_bytes().to_vec();
    let pad = BLOCK_SIZE - data.len() % BLOCK_SIZE;
    data.extend(std::iter::repeat_n(u8::try_from(pad).unwrap_or(16), pad));
    let cipher = cipher();
    let mut previous = [0_u8; BLOCK_SIZE];
    for block in data.chunks_exact_mut(BLOCK_SIZE) {
        let mut current = [0_u8; BLOCK_SIZE];
        for (out, (plain, chain)) in current.iter_mut().zip(block.iter().zip(previous)) {
            *out = plain ^ chain;
        }
        let mut array = Array::from(current);
        cipher.encrypt_block(&mut array);
        block.copy_from_slice(&array);
        previous.copy_from_slice(&array);
    }
    quote(&b64encode(&data), "")
}

#[cfg(test)]
mod tests {
    use super::{b64decode, b64encode};

    #[test]
    fn base64_is_lenient_like_binascii() {
        assert_eq!(b64decode("aGVsbG8=").unwrap(), b"hello");
        assert_eq!(b64decode("aGV sb\nG8=").unwrap(), b"hello");
        assert_eq!(b64decode("aGVsbA==trailing").unwrap(), b"hell");
        assert_eq!(b64decode("aGVsbG8").unwrap_err().message(), "Incorrect padding");
        assert_eq!(
            b64decode("aGVsb").unwrap_err().message(),
            "Invalid base64-encoded string: number of data characters (5) cannot be 1 more than a multiple of 4"
        );
        assert_eq!(b64decode("é").unwrap_err().class_name(), "ValueError");
        assert_eq!(b64encode(b"hello"), "aGVsbG8=");
        assert_eq!(b64encode(b"hell"), "aGVsbA==");
        assert_eq!(b64encode(b"hel"), "aGVs");
    }
}
