//! Stickers (表情) forwarded to the agent's WeChat account.
//!
//! Python origin: `src/web/wechat_emoji.py`.
//!
//! A sticker is a type-47 message whose `<emoji>` element names the file twice on WeChat's
//! CDN. `cdnurl` serves it as it is, with no login, and `md5` hashes those bytes.
//! `encrypturl` serves the same file encrypted with AES-128-CBC, the 16 bytes of `aeskey`
//! (hex) being both key and IV. Store stickers can leave `cdnurl` empty, so the encrypted
//! copy is the fallback. Both addresses have `*#*` where the scheme's `:` belongs.
//!
//! A large sticker (several MB, which WeChat only loads when it is tapped) comes as an app
//! message of `<type>8</type>` instead. Its `<emojiinfo>` is a base64 protobuf with the
//! same three things: field 1 the md5, field 2 the plain address, field 5 the encrypted one
//! and field 6 its key.

use std::collections::HashMap;
use std::fmt::Write as _;
use std::sync::LazyLock;

use aes::Aes128;
use aes::cipher::{Array, BlockCipherDecrypt as _, KeyInit as _};
use fav_core::pyhtml::unescape;
use fav_core::pytext::{py_split, py_strip};
use md5::{Digest as _, Md5};

use crate::pybase64::b64decode;
use crate::pydecode::PyError;
use crate::pyre::compile;

static EMOJI: LazyLock<regex::Regex> = LazyLock::new(|| compile(r"<emoji\s([^>]*)>", false, true));
static ATTRIBUTE: LazyLock<regex::Regex> = LazyLock::new(|| compile(r#"([A-Za-z0-9_]+)="([^"]*)""#, false, false));
static MD5_HEX: LazyLock<regex::Regex> = LazyLock::new(|| compile(r"^(?:[0-9a-f]{32})$", false, false));
static AES_KEY: LazyLock<regex::Regex> = LazyLock::new(|| compile(r"^(?:[0-9a-fA-F]{32})$", false, false));
static APP_TYPE: LazyLock<regex::Regex> = LazyLock::new(|| compile(r"<appmsg[\s>].*?<type>\s*(\d+)\s*</type>", false, true));
static EMOJI_INFO: LazyLock<regex::Regex> = LazyLock::new(|| compile(r"<emojiinfo>\s*([A-Za-z0-9+/=\s]+?)\s*</emojiinfo>", false, false));
const LARGE_STICKER_APP_TYPE: &str = "8";
/// Protobuf's length-delimited wire type.
const LENGTH_DELIMITED: u8 = 2;
/// The `<emojiinfo>` fields this reads, by protobuf field number.
const INFO_MD5: u64 = 1;
const INFO_URL: u64 = 2;
const INFO_ENCRYPTED_URL: u64 = 5;
const INFO_AES_KEY: u64 = 6;
const BLOCK_SIZE: usize = 16;

/// `Sticker`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Sticker {
    pub md5: String,
    pub cdn_url: String,
    pub encrypted_url: String,
    pub aes_key: String,
}

/// `_url`: an address with its entities undone and `*#*` read as `:`; empty when it is not
/// a web address.
fn url(value: &str) -> String {
    let unescaped = unescape(value).replace("*#*", ":");
    let url = py_strip(&unescaped);
    if url.starts_with("http://") || url.starts_with("https://") {
        url.to_owned()
    } else {
        String::new()
    }
}

/// `_sticker`: the sticker, or `None` when it names no file it can be fetched from.
fn sticker(md5: &str, cdn_url: &str, encrypted_url: &str, aes_key: &str) -> Option<Sticker> {
    let md5 = py_strip(md5).to_lowercase();
    let aes_key = py_strip(aes_key);
    let sticker = Sticker {
        md5: if MD5_HEX.is_match(&md5) { md5 } else { String::new() },
        cdn_url: url(cdn_url),
        encrypted_url: url(encrypted_url),
        aes_key: if AES_KEY.is_match(aes_key) {
            aes_key.to_owned()
        } else {
            String::new()
        },
    };
    if sticker.cdn_url.is_empty() && (sticker.encrypted_url.is_empty() || sticker.aes_key.is_empty()) {
        return None;
    }
    Some(sticker)
}

/// A varint's value; `None` past 64 bits, where Python's unbounded integer is a field
/// number no reader asks for and a length past the data's end.
struct Varint {
    value: Option<u64>,
    wire_type: u8,
}

/// `_varint`: the varint at `index` and the index after it; `None` when the data ends
/// first (Python's `IndexError`).
fn varint(data: &[u8], mut index: usize) -> Option<(Varint, usize)> {
    let wire_type = data.get(index)? & 7;
    let mut value: u128 = 0;
    let mut big = false;
    let mut shift = 0_u32;
    loop {
        let byte = *data.get(index)?;
        index += 1;
        let bits = u128::from(byte & 0x7F);
        if shift < 64 {
            value |= bits << shift;
        } else if bits != 0 {
            big = true;
        }
        shift = shift.saturating_add(7);
        if byte & 0x80 == 0 {
            let value = if big { None } else { u64::try_from(value).ok() };
            return Some((Varint { value, wire_type }, index));
        }
    }
}

/// `_protobuf_strings`: the first value of each length-delimited field that is text; stops
/// at anything it cannot read.
fn protobuf_strings(data: &[u8]) -> HashMap<u64, String> {
    let mut fields: HashMap<u64, String> = HashMap::new();
    let mut index = 0;
    while index < data.len() {
        let Some((key, next)) = varint(data, index) else { break };
        index = next;
        match key.wire_type {
            0 => {
                let Some((_, next)) = varint(data, index) else { break };
                index = next;
            }
            LENGTH_DELIMITED => {
                let Some((length, next)) = varint(data, index) else { break };
                index = next;
                // `data[index : index + length]`: a length past the end takes what is left,
                // and the loop ends after it.
                let end = length
                    .value
                    .and_then(|length| usize::try_from(length).ok())
                    .map_or(data.len(), |length| index.saturating_add(length).min(data.len()));
                let value = &data[index.min(data.len())..end];
                index = match length.value.and_then(|length| usize::try_from(length).ok()) {
                    Some(length) => index.saturating_add(length),
                    None => usize::MAX,
                };
                if let (Some(number), Ok(text)) = (key.value.map(|key| key >> 3), std::str::from_utf8(value)) {
                    fields.entry(number).or_insert_with(|| text.to_owned());
                }
            }
            _ => break,
        }
    }
    fields
}

fn field(fields: &HashMap<u64, String>, number: u64) -> &str {
    fields.get(&number).map_or("", String::as_str)
}

/// `_large_sticker`: the sticker an app message of type 8 names in its `<emojiinfo>`.
fn large_sticker(content: &str) -> Option<Sticker> {
    let app_type = APP_TYPE.captures(content)?;
    let info = EMOJI_INFO.captures(content)?;
    if &app_type[1] != LARGE_STICKER_APP_TYPE {
        return None;
    }
    let joined: String = py_split(&info[1]).collect();
    let data = b64decode(&joined, true).ok()?;
    let fields = protobuf_strings(&data);
    sticker(
        field(&fields, INFO_MD5),
        field(&fields, INFO_URL),
        field(&fields, INFO_ENCRYPTED_URL),
        field(&fields, INFO_AES_KEY),
    )
}

/// `parse_sticker`: the sticker a message carries, or `None` when it names no file to fetch.
pub fn parse_sticker(content: &str) -> Option<Sticker> {
    let Some(found) = EMOJI.captures(content) else {
        return large_sticker(content);
    };
    let mut attributes: HashMap<String, &str> = HashMap::new();
    for attribute in ATTRIBUTE.captures_iter(&found[1]) {
        let (_, [key, value]) = attribute.extract();
        attributes.insert(key.to_lowercase(), value);
    }
    let get = |name: &str| attributes.get(name).copied().unwrap_or("");
    sticker(get("md5"), get("cdnurl"), get("encrypturl"), get("aeskey"))
}

fn value_error(message: &str) -> PyError {
    PyError::new("ValueError", message)
}

/// `bytes.fromhex(aes_key)` for the 32 hex digits `parse_sticker` keeps.
fn key_bytes(aes_key: &str) -> Result<[u8; 16], PyError> {
    let mut key = [0_u8; 16];
    let digits = aes_key.as_bytes();
    if digits.len() != 32 {
        return Err(value_error("the sticker key is not 16 bytes of hex"));
    }
    for (slot, pair) in key.iter_mut().zip(digits.chunks_exact(2)) {
        let pair = std::str::from_utf8(pair).map_err(|_| value_error("non-hexadecimal number found in fromhex() arg"))?;
        *slot = u8::from_str_radix(pair, 16).map_err(|_| value_error("non-hexadecimal number found in fromhex() arg"))?;
    }
    Ok(key)
}

/// `decrypt_sticker`: the file behind `encrypturl`. Fails with `ValueError` for bytes the
/// key does not fit, in `PyCryptodome`'s words.
pub fn decrypt_sticker(content: &[u8], aes_key: &str) -> Result<Vec<u8>, PyError> {
    let key = key_bytes(aes_key)?;
    if !content.len().is_multiple_of(BLOCK_SIZE) {
        return Err(value_error("Data must be padded to 16 byte boundary in CBC mode"));
    }
    let cipher = Aes128::new(&Array::from(key));
    let mut data = content.to_vec();
    let mut previous = key;
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
    unpad(data)
}

/// `Crypto.Util.Padding.unpad(data, 16)` (PKCS#7).
fn unpad(mut data: Vec<u8>) -> Result<Vec<u8>, PyError> {
    let Some(&last) = data.last() else {
        return Err(value_error("Zero-length input cannot be unpadded"));
    };
    if !data.len().is_multiple_of(BLOCK_SIZE) {
        return Err(value_error("Input data is not padded"));
    }
    let padding = usize::from(last);
    if padding < 1 || padding > BLOCK_SIZE.min(data.len()) {
        return Err(value_error("Padding is incorrect."));
    }
    if data[data.len() - padding..].iter().any(|&byte| byte != last) {
        return Err(value_error("PKCS#7 padding is incorrect."));
    }
    data.truncate(data.len() - padding);
    Ok(data)
}

/// The lowercase hex MD5 of `content`.
pub(crate) fn md5_hex(content: &[u8]) -> String {
    Md5::digest(content).iter().fold(String::with_capacity(32), |mut out, byte| {
        let _ = write!(out, "{byte:02x}");
        out
    })
}

/// `matches`: whether `content` is the file the message names; `true` when it names no
/// hash.
pub fn matches(sticker: &Sticker, content: &[u8]) -> bool {
    !content.is_empty() && (sticker.md5.is_empty() || md5_hex(content) == sticker.md5)
}
