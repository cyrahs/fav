//! l2d.su's AZP2 envelope for Azur Lane data and text assets.
//!
//! Python origin: `src/tool/azurlane_protected.py`.
//!
//! Since 2026-10-31 l2d.su serves Azur Lane from static.l2d.su/al/ only, and every JSON file
//! there (the ship index, the per-ship details, model3/motion/physics files, spine part
//! manifests and atlases, painting indexes and meshes) is wrapped in an AZP2 envelope.
//! Images, moc3, skel and ogg files are served as they are.
//!
//! The envelope, as the site's own front end opens it (protectedCodec-*.js):
//!
//! ```text
//! b"AZP2" | nonce (12 bytes) | AES-256-GCM ciphertext + 16-byte tag
//! ```
//!
//! with the additional data `b"azp2:" + identity` (the identity as lowercase hex text). The
//! plaintext is `seed: u32 LE | codec: u8 | length: u32 LE | masked body`. The body is stored
//! reversed and masked with an xorshift32 stream seeded by `seed`; codec 0 is the raw file
//! and codec 1 a gzip member whose ISIZE trailer equals `length`.
//!
//! The 32-byte key and identity come from two places:
//!
//! - Asset references carry them in the URL fragment: `<path>#azp2=<key hex>.<identity hex>`.
//!   The fragment is never sent to the server, so a URL with it is still the URL to request.
//! - Files under /data/ have no fragment; their descriptor is derived from the path below
//!   /data/ (`ships-CN.json`, `ships/CN/10117.json`) with a key the front end ships in
//!   protectedData-*.js.
//!
//! Failures are [`PyError`]s of class `ProtectedResourceError` (a `ValueError`), with
//! Python's texts; a URL `urlsplit` refuses is Python's plain `ValueError`.

use std::fmt::Write as _;

use aes_gcm::aead::consts::U12;
use aes_gcm::aead::{Aead, KeyInit, Payload};
use aes_gcm::{Aes128Gcm, Aes256Gcm, AesGcm};
use fav_core::pyurl::urlsplit;
use fav_runtime::pystr::repr_str;
use hmac::{Hmac, Mac};
use sha2::{Digest, Sha256};

use crate::PyError;

pub const AZP2_MAGIC: &[u8; 4] = b"AZP2";
/// The data key l2d.su's front end embeds (protectedData-*.js); /data/ files are derived from it.
pub const AZP2_DATA_MASTER_KEY: [u8; 32] = [
    0x05, 0xb3, 0x41, 0xc7, 0xe4, 0x14, 0x9a, 0xf5, 0xc6, 0x5b, 0xee, 0xd3, 0xfe, 0x6e, 0xc4, 0x86, 0xc5, 0xc5, 0xc2, 0xb3, 0xec, 0xdf,
    0xe4, 0x15, 0x40, 0x90, 0x00, 0xfa, 0xc1, 0x57, 0x10, 0x34,
];
/// Limits the front end enforces: the envelope at most 64 MiB + 128 KiB, the plaintext at most 64 MiB.
pub const AZP2_MAX_ENVELOPE_SIZE: usize = 67_239_936;
pub const AZP2_MAX_PLAINTEXT_SIZE: u32 = 67_108_864;

const NONCE_END: usize = 16;
const MIN_ENVELOPE_SIZE: usize = 41;
/// `struct.Struct('<IBI').size`.
const HEADER_SIZE: usize = 9;
const CODEC_RAW: u8 = 0;
const CODEC_GZIP: u8 = 1;
const MIN_GZIP_SIZE: usize = 18;
const DATA_SEGMENT: &str = "/data/";
pub const CLASS: &str = "ProtectedResourceError";

fn protected_error(message: impl Into<String>) -> PyError {
    PyError::new(CLASS, message)
}

/// `Azp2Descriptor`: the key and identity as hex text.
#[derive(Clone, PartialEq, Eq)]
pub struct Azp2Descriptor {
    pub key: String,
    pub identity: String,
}

impl std::fmt::Debug for Azp2Descriptor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The key opens the asset; it is not a credential, but there is no reason to log it.
        f.debug_struct("Azp2Descriptor")
            .field("identity", &self.identity)
            .finish_non_exhaustive()
    }
}

fn hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        let _ = write!(out, "{byte:02x}");
    }
    out
}

/// `data_descriptor`: the descriptor of a /data/ file, from its path below /data/
/// (`ships-CN.json`).
pub fn data_descriptor(relative_path: &str) -> Result<Azp2Descriptor, PyError> {
    let path = fav_core::pytext::py_strip(relative_path);
    let invalid = path.is_empty()
        || path.starts_with('/')
        || path.contains('\\')
        || path.split('/').any(|segment| matches!(segment, "" | "." | ".."));
    if invalid {
        return Err(protected_error(format!("Invalid protected data path: {}", repr_str(relative_path))));
    }
    let mut mac = <Hmac<Sha256> as hmac::KeyInit>::new_from_slice(&AZP2_DATA_MASTER_KEY).expect("HMAC takes any key length");
    mac.update(format!("azp2:data:key:{path}").as_bytes());
    let key = hex(&mac.finalize().into_bytes());
    let identity = hex(&Sha256::digest(format!("azp2:data:identity:{path}").as_bytes()));
    Ok(Azp2Descriptor { key, identity })
}

fn value_error(error: &fav_core::pyurl::UrlError) -> PyError {
    PyError::new("ValueError", error.0.clone())
}

/// `data_url_descriptor`: the derived descriptor for a URL under a /data/ directory, or
/// `None` for any other URL.
pub fn data_url_descriptor(url: &str) -> Result<Option<Azp2Descriptor>, PyError> {
    let path = urlsplit(url, "").map_err(|error| value_error(&error))?.path;
    let Some(index) = path.rfind(DATA_SEGMENT) else {
        return Ok(None);
    };
    data_descriptor(&path[index + DATA_SEGMENT.len()..]).map(Some)
}

fn is_lower_hex_64(text: &str) -> bool {
    text.len() == 64 && text.bytes().all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
}

/// `fragment_descriptor`: the descriptor an asset URL carries in its `#azp2=` fragment, or
/// `None` without one.
pub fn fragment_descriptor(url: &str) -> Result<Option<Azp2Descriptor>, PyError> {
    let fragment = urlsplit(url, "").map_err(|error| value_error(&error))?.fragment;
    let Some(rest) = fragment.strip_prefix("azp2=") else {
        return Ok(None);
    };
    // `^azp2=([0-9a-f]{64})\.([0-9a-f]{64})$`: `$` also matches before one final newline,
    // which urlsplit has already removed.
    match rest.split_once('.') {
        Some((key, identity)) if is_lower_hex_64(key) && is_lower_hex_64(identity) => Ok(Some(Azp2Descriptor {
            key: key.to_owned(),
            identity: identity.to_owned(),
        })),
        _ => Err(protected_error(format!("Invalid AZP2 resource descriptor in {url}"))),
    }
}

/// `is_protected_url`.
pub fn is_protected_url(url: &str) -> Result<bool, PyError> {
    Ok(urlsplit(url, "")
        .map_err(|error| value_error(&error))?
        .fragment
        .starts_with("azp2="))
}

/// `strip_fragment`: everything before the first `#`.
pub fn strip_fragment(url: &str) -> &str {
    url.split_once('#').map_or(url, |(head, _)| head)
}

/// `is_azp2_envelope`.
pub fn is_azp2_envelope(content: &[u8]) -> bool {
    content.starts_with(AZP2_MAGIC)
}

/// `bytes.fromhex`: pairs of hex digits, ASCII whitespace allowed between pairs.
fn from_hex(text: &str) -> Result<Vec<u8>, String> {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len() / 2);
    let mut index = 0;
    let digit = |byte: u8| char::from(byte).to_digit(16).and_then(|value| u8::try_from(value).ok());
    let position = |index: usize| {
        // Python counts characters, not bytes.
        let characters = text.get(..index).map_or(index, |head| head.chars().count());
        format!("non-hexadecimal number found in fromhex() arg at position {characters}")
    };
    while index < bytes.len() {
        if matches!(bytes[index], b' ' | b'\t' | b'\n' | b'\r' | b'\x0b' | b'\x0c') {
            index += 1;
            continue;
        }
        let Some(high) = digit(bytes[index]) else {
            return Err(position(index));
        };
        let Some(low) = bytes.get(index + 1).copied().and_then(digit) else {
            return Err(position(index + 1));
        };
        out.push(high * 16 + low);
        index += 2;
    }
    Ok(out)
}

/// `AES.new(key, AES.MODE_GCM, nonce=...)`, `update(aad)`, `decrypt_and_verify(...)`, with
/// `PyCryptodome`'s `ValueError` texts.
fn open_gcm(key: &[u8], nonce: &[u8], aad: &[u8], sealed: &[u8]) -> Result<Vec<u8>, String> {
    let payload = Payload { msg: sealed, aad };
    let nonce = aes_gcm::Nonce::<U12>::try_from(nonce).map_err(|_| "Nonce cannot be empty".to_owned())?;
    let opened = match key.len() {
        16 => Aes128Gcm::new_from_slice(key)
            .map_err(|error| error.to_string())?
            .decrypt(&nonce, payload),
        24 => AesGcm::<aes::Aes192, U12>::new_from_slice(key)
            .map_err(|error| error.to_string())?
            .decrypt(&nonce, payload),
        32 => Aes256Gcm::new_from_slice(key)
            .map_err(|error| error.to_string())?
            .decrypt(&nonce, payload),
        other => return Err(format!("Incorrect AES key length ({other} bytes)")),
    };
    opened.map_err(|_| "MAC check failed".to_owned())
}

/// `decrypt`: open one AZP2 envelope and return the original file.
pub fn decrypt(content: &[u8], descriptor: &Azp2Descriptor) -> Result<Vec<u8>, PyError> {
    let size = content.len();
    if !(MIN_ENVELOPE_SIZE..=AZP2_MAX_ENVELOPE_SIZE).contains(&size) || !is_azp2_envelope(content) {
        return Err(protected_error("Invalid AZP2 envelope"));
    }
    let plaintext = from_hex(&descriptor.key)
        .and_then(|key| {
            open_gcm(
                &key,
                &content[AZP2_MAGIC.len()..NONCE_END],
                format!("azp2:{}", descriptor.identity).as_bytes(),
                &content[NONCE_END..],
            )
        })
        .map_err(|error| protected_error(format!("AZP2 envelope failed authentication: {error}")))?;

    if plaintext.len() < HEADER_SIZE {
        return Err(protected_error("Invalid AZP2 resource header"));
    }
    let seed = u32::from_le_bytes([plaintext[0], plaintext[1], plaintext[2], plaintext[3]]);
    let codec = plaintext[4];
    let length = u32::from_le_bytes([plaintext[5], plaintext[6], plaintext[7], plaintext[8]]);
    if length > AZP2_MAX_PLAINTEXT_SIZE {
        return Err(protected_error("Invalid AZP2 resource length"));
    }
    decode_body(unmask(&plaintext[HEADER_SIZE..], seed), codec, length)
}

fn decode_body(body: Vec<u8>, codec: u8, length: u32) -> Result<Vec<u8>, PyError> {
    let length_usize = usize::try_from(length).unwrap_or(usize::MAX);
    match codec {
        CODEC_RAW => {
            if body.len() != length_usize {
                return Err(protected_error("Invalid AZP2 resource length"));
            }
            Ok(body)
        }
        CODEC_GZIP => {
            let trailer = body
                .len()
                .checked_sub(4)
                .map(|start| u32::from_le_bytes([body[start], body[start + 1], body[start + 2], body[start + 3]]));
            if body.len() < MIN_GZIP_SIZE || trailer != Some(length) {
                return Err(protected_error("Invalid AZP2 compressed resource"));
            }
            let inflated = gzip_decompress(&body).map_err(|error| protected_error(format!("Invalid AZP2 compressed resource: {error}")))?;
            if inflated.len() != length_usize {
                return Err(protected_error("Invalid AZP2 compressed resource length"));
            }
            Ok(inflated)
        }
        other => Err(protected_error(format!("Unsupported AZP2 codec {other}"))),
    }
}

/// `decrypt_for_url`: open `content` when `url` names a protected resource; return anything
/// else unchanged.
///
/// A fragment descriptor wins; a /data/ URL falls back to the derived one. Only an actual
/// AZP2 envelope is opened, so a file the CDN still serves in the clear passes through.
pub fn decrypt_for_url(content: Vec<u8>, url: &str) -> Result<Vec<u8>, PyError> {
    Ok(open_for_url(&content, url)?.unwrap_or(content))
}

/// [`decrypt_for_url`], telling the two outcomes apart: `None` when `content` passes
/// through unchanged (Python's `content is response.content`).
pub fn open_for_url(content: &[u8], url: &str) -> Result<Option<Vec<u8>>, PyError> {
    let fragment = fragment_descriptor(url)?;
    let descriptor = match &fragment {
        Some(descriptor) => Some(descriptor.clone()),
        None => data_url_descriptor(url)?,
    };
    let Some(descriptor) = descriptor else {
        return Ok(None);
    };
    if !is_azp2_envelope(content) {
        if fragment.is_some() {
            return Err(protected_error(format!("Expected an AZP2 envelope from {}", strip_fragment(url))));
        }
        return Ok(None);
    }
    decrypt(content, &descriptor).map(Some)
}

/// `_unmask`: the body is stored reversed, each byte offset by the next xorshift32 state.
fn unmask(masked: &[u8], seed: u32) -> Vec<u8> {
    let mut output = vec![0_u8; masked.len()];
    let mut state = seed;
    let last = masked.len().wrapping_sub(1);
    for (index, value) in masked.iter().enumerate() {
        state ^= state << 13;
        state ^= state >> 17;
        state ^= state << 5;
        output[last - index] = value.wrapping_sub(state.to_le_bytes()[0]);
    }
    output
}

// ---------- Python 3.12's gzip.decompress ----------

const FHCRC: u8 = 2;
const FEXTRA: u8 = 4;
const FNAME: u8 = 8;
const FCOMMENT: u8 = 16;
const EOF_MESSAGE: &str = "Compressed file ended before the end-of-stream marker was reached";

/// `repr(bytes)`.
fn repr_bytes(bytes: &[u8]) -> String {
    let quote = if bytes.contains(&b'\'') && !bytes.contains(&b'"') {
        b'"'
    } else {
        b'\''
    };
    let mut out = String::from("b");
    out.push(char::from(quote));
    for &byte in bytes {
        match byte {
            b'\\' => out.push_str("\\\\"),
            b'\t' => out.push_str("\\t"),
            b'\n' => out.push_str("\\n"),
            b'\r' => out.push_str("\\r"),
            byte if byte == quote => {
                out.push('\\');
                out.push(char::from(byte));
            }
            0x20..=0x7e => out.push(char::from(byte)),
            byte => {
                let _ = write!(out, "\\x{byte:02x}");
            }
        }
    }
    out.push(char::from(quote));
    out
}

struct Reader<'a> {
    data: &'a [u8],
    position: usize,
}

impl Reader<'_> {
    fn read(&mut self, count: usize) -> &[u8] {
        let end = self.position.saturating_add(count).min(self.data.len());
        let chunk = &self.data[self.position..end];
        self.position = end;
        chunk
    }

    fn read_exact(&mut self, count: usize) -> Result<&[u8], String> {
        if self.data.len() - self.position < count {
            return Err(EOF_MESSAGE.to_owned());
        }
        Ok(self.read(count))
    }

    fn skip_zero_terminated(&mut self) {
        loop {
            match self.read(1) {
                [] | [0] => break,
                _ => {}
            }
        }
    }
}

/// `_read_gzip_header`: `false` at the end of the data.
fn read_gzip_header(reader: &mut Reader<'_>) -> Result<bool, String> {
    let magic = reader.read(2);
    if magic.is_empty() {
        return Ok(false);
    }
    if magic != b"\x1f\x8b" {
        return Err(format!("Not a gzipped file ({})", repr_bytes(magic)));
    }
    let head = reader.read_exact(8)?;
    let (method, flag) = (head[0], head[1]);
    if method != 8 {
        return Err("Unknown compression method".to_owned());
    }
    if flag & FEXTRA != 0 {
        let extra = reader.read_exact(2)?;
        let extra_len = usize::from(u16::from_le_bytes([extra[0], extra[1]]));
        reader.read_exact(extra_len)?;
    }
    if flag & FNAME != 0 {
        reader.skip_zero_terminated();
    }
    if flag & FCOMMENT != 0 {
        reader.skip_zero_terminated();
    }
    if flag & FHCRC != 0 {
        reader.read_exact(2)?;
    }
    Ok(true)
}

/// A raw deflate stream from the start of `data`: the output, and whether the end-of-stream
/// marker was reached with the number of input bytes it took.
fn inflate_raw(data: &[u8]) -> Result<(Vec<u8>, Option<usize>), String> {
    let mut inflater = flate2::Decompress::new(false);
    let mut output = Vec::with_capacity(data.len().saturating_mul(2).max(64));
    loop {
        if output.len() == output.capacity() {
            output.reserve(output.capacity().max(64));
        }
        let consumed = usize::try_from(inflater.total_in()).unwrap_or(usize::MAX);
        let before = (inflater.total_in(), inflater.total_out());
        let status = inflater
            .decompress_vec(&data[consumed..], &mut output, flate2::FlushDecompress::None)
            // zlib's own texts ("invalid block type", ...) are not miniz's: DIVERGENCES.md.
            .map_err(|error| format!("Error -3 while decompressing data: {error}"))?;
        let consumed = usize::try_from(inflater.total_in()).unwrap_or(usize::MAX);
        if status == flate2::Status::StreamEnd {
            return Ok((output, Some(consumed)));
        }
        // Output room left over means the inflater wanted more input than there is.
        let room = output.len() < output.capacity();
        let stuck = before == (inflater.total_in(), inflater.total_out());
        if room && (consumed == data.len() || stuck) {
            return Ok((output, None));
        }
    }
}

/// `gzip.decompress(data)` (Python 3.12): every member in turn, NUL padding between members
/// skipped, each member's CRC-32 and size checked.
pub(crate) fn gzip_decompress(data: &[u8]) -> Result<Vec<u8>, String> {
    let mut members = Vec::new();
    let mut data = data.to_vec();
    loop {
        let mut reader = Reader { data: &data, position: 0 };
        if !read_gzip_header(&mut reader)? {
            return Ok(members);
        }
        let start = reader.position;
        let (decompressed, end) = inflate_raw(&data[start..])?;
        let unused = end.map(|end| &data[start + end..]);
        let Some(unused) = unused.filter(|unused| unused.len() >= 8) else {
            return Err(EOF_MESSAGE.to_owned());
        };
        let crc = u32::from_le_bytes([unused[0], unused[1], unused[2], unused[3]]);
        let length = u32::from_le_bytes([unused[4], unused[5], unused[6], unused[7]]);
        let mut check = flate2::Crc::new();
        check.update(&decompressed);
        if crc != check.sum() {
            return Err("CRC check failed".to_owned());
        }
        #[allow(clippy::cast_possible_truncation)] // `len(decompressed) & 0xffffffff`
        if length != decompressed.len() as u32 {
            return Err("Incorrect length of data produced".to_owned());
        }
        members.extend_from_slice(&decompressed);
        let rest = &unused[8..];
        let padding = rest.iter().take_while(|byte| **byte == 0).count();
        data = rest[padding..].to_vec();
    }
}

#[cfg(test)]
mod tests;
