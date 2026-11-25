//! The site's encrypted API channel, `dx150-edge-v1` (its front end's `edge-api-crypto.js`).
//!
//! In the site's CDN mode a signed-in request only counts when it goes through this channel:
//! the plain routes ignore the login, whether it comes as a cookie or as a bearer token. A
//! call is two POSTs to `/api/<scope>/request`:
//!
//! 1. `hello`: an ephemeral P-256 public key out; the server's key, a 32-byte salt and the
//!    session's `kid`, `sid`, `site` and clock back. ECDH, then HKDF-SHA256 with the info
//!    `dx150-edge-v1\n<scope>\n<site>\n<kid>\n<sid>\n<req|res>`, gives one AES-256-GCM key
//!    per direction.
//! 2. `call`: `{"request": {method, path, body, content_type}, "authorization": "Bearer …"}`
//!    sealed under the request key, nonce `0^8 || seq` (big-endian `u32`), additional data
//!    `<info>\n<req|res>\n<seq>\n<ts>`; the answer is the inner response (`status`,
//!    `headers`, base64 `body`) sealed the same way under the response key.

use base64::Engine as _;
use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use fav_runtime::source::JobError;
use ring::rand::SystemRandom;
use ring::{aead, agreement, hkdf};
use serde_json::{Value as Json, json};

pub(crate) const PROTOCOL: &str = "dx150-edge-v1";
const SALT_LEN: usize = 32;

fn channel_error(message: impl Into<String>) -> JobError {
    JobError::new("HuangguoError", message)
}

fn text<'a>(value: &'a Json, key: &str) -> Result<&'a str, JobError> {
    value
        .get(key)
        .and_then(Json::as_str)
        .ok_or_else(|| channel_error(format!("the encrypted channel's answer has no {key}")))
}

fn base64url(value: &str) -> Result<Vec<u8>, JobError> {
    URL_SAFE_NO_PAD
        .decode(value)
        .map_err(|error| channel_error(format!("the encrypted channel sent bad base64: {error}")))
}

/// `<info>\n<direction>`'s key, from the shared secret and the session's salt.
fn derive(shared: &[u8], salt: &[u8], info: &str, direction: &str) -> Result<aead::LessSafeKey, JobError> {
    let prk = hkdf::Salt::new(hkdf::HKDF_SHA256, salt).extract(shared);
    let info = format!("{info}\n{direction}");
    let parts = [info.as_bytes()];
    let okm = prk
        .expand(&parts, &aead::AES_256_GCM)
        .map_err(|_| channel_error("could not derive the encrypted channel's key"))?;
    Ok(aead::LessSafeKey::new(aead::UnboundKey::from(okm)))
}

/// The request and response keys of a session.
pub(crate) fn keys(shared: &[u8], salt: &[u8], info: &str) -> Result<(aead::LessSafeKey, aead::LessSafeKey), JobError> {
    Ok((derive(shared, salt, info, "req")?, derive(shared, salt, info, "res")?))
}

/// `dx150-edge-v1\n<scope>\n<site>\n<kid>\n<sid>`.
pub(crate) fn info(scope: &str, site: &str, kid: &str, sid: &str) -> String {
    [PROTOCOL, scope, site, kid, sid].join("\n")
}

pub(crate) fn nonce(sequence: u32) -> aead::Nonce {
    let mut bytes = [0_u8; aead::NONCE_LEN];
    bytes[8..].copy_from_slice(&sequence.to_be_bytes());
    aead::Nonce::assume_unique_for_key(bytes)
}

pub(crate) fn additional_data(info: &str, direction: &str, sequence: u32, timestamp: i64) -> String {
    format!("{info}\n{direction}\n{sequence}\n{timestamp}")
}

/// A fresh client key and the `hello` body that announces it.
pub(crate) fn hello() -> Result<(agreement::EphemeralPrivateKey, Json), JobError> {
    let private = agreement::EphemeralPrivateKey::generate(&agreement::ECDH_P256, &SystemRandom::new())
        .map_err(|_| channel_error("could not make a key for the encrypted channel"))?;
    let public = private
        .compute_public_key()
        .map_err(|_| channel_error("could not make a key for the encrypted channel"))?;
    let body = json!({"v": 1, "type": "hello", "client_pub": URL_SAFE_NO_PAD.encode(public.as_ref())});
    Ok((private, body))
}

/// The inner response of a call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Inner {
    pub status: u16,
    pub body: Vec<u8>,
}

/// One handshake's keys and identity.
pub(crate) struct Session {
    info: String,
    kid: String,
    sid: String,
    /// The server's clock at the handshake; a call made right after uses it as its `ts`.
    pub server_time: i64,
    request_key: aead::LessSafeKey,
    response_key: aead::LessSafeKey,
}

impl std::fmt::Debug for Session {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Session")
            .field("kid", &self.kid)
            .field("sid", &self.sid)
            .finish_non_exhaustive()
    }
}

impl Session {
    /// Finish the handshake from the server's `hello` answer.
    pub(crate) fn open(private: agreement::EphemeralPrivateKey, scope: &str, hello: &Json) -> Result<Self, JobError> {
        if hello.get("v").and_then(Json::as_i64) != Some(1) || hello.get("type").and_then(Json::as_str) != Some("hello") {
            return Err(channel_error("the encrypted channel's handshake answer is not a hello"));
        }
        let (kid, sid, site) = (text(hello, "kid")?, text(hello, "sid")?, text(hello, "site")?);
        let server_time = hello
            .get("server_time")
            .and_then(Json::as_i64)
            .ok_or_else(|| channel_error("the encrypted channel's answer has no server_time"))?;
        let salt = base64url(text(hello, "salt")?)?;
        if salt.len() != SALT_LEN {
            return Err(channel_error("the encrypted channel's salt is not 32 bytes"));
        }
        let server_pub = base64url(text(hello, "server_pub")?)?;
        let shared = agreement::agree_ephemeral(
            private,
            &agreement::UnparsedPublicKey::new(&agreement::ECDH_P256, &server_pub),
            <[u8]>::to_vec,
        )
        .map_err(|_| channel_error("the encrypted channel's server key is not a P-256 point"))?;
        let info = info(scope, site, kid, sid);
        let (request_key, response_key) = keys(&shared, &salt, &info)?;
        Ok(Self {
            info,
            kid: kid.to_owned(),
            sid: sid.to_owned(),
            server_time,
            request_key,
            response_key,
        })
    }

    /// The `call` body for `packet`.
    pub(crate) fn seal(&self, sequence: u32, timestamp: i64, packet: &Json) -> Result<Json, JobError> {
        let mut data = serde_json::to_vec(packet).map_err(|error| channel_error(error.to_string()))?;
        let aad = additional_data(&self.info, "req", sequence, timestamp);
        self.request_key
            .seal_in_place_append_tag(nonce(sequence), aead::Aad::from(aad.as_bytes()), &mut data)
            .map_err(|_| channel_error("could not encrypt the request"))?;
        Ok(json!({
            "v": 1, "type": "call", "kid": self.kid, "sid": self.sid, "seq": sequence, "ts": timestamp,
            "ciphertext": URL_SAFE_NO_PAD.encode(&data),
        }))
    }

    /// The inner response of a call's `result`.
    pub(crate) fn open_result(&self, sequence: u32, timestamp: i64, result: &Json) -> Result<Inner, JobError> {
        let same = result.get("v").and_then(Json::as_i64) == Some(1)
            && result.get("type").and_then(Json::as_str) == Some("result")
            && result.get("sid").and_then(Json::as_str) == Some(self.sid.as_str())
            && result.get("kid").and_then(Json::as_str) == Some(self.kid.as_str())
            && result.get("seq").and_then(Json::as_i64) == Some(i64::from(sequence))
            && result.get("ts").and_then(Json::as_i64) == Some(timestamp);
        if !same {
            return Err(channel_error("the encrypted channel's answer does not match the call"));
        }
        let mut data = base64url(text(result, "ciphertext")?)?;
        let aad = additional_data(&self.info, "res", sequence, timestamp);
        let plain = self
            .response_key
            .open_in_place(nonce(sequence), aead::Aad::from(aad.as_bytes()), &mut data)
            .map_err(|_| channel_error("could not decrypt the encrypted channel's answer"))?;
        let inner: Json = serde_json::from_slice(plain).map_err(|error| channel_error(error.to_string()))?;
        let status = inner
            .get("status")
            .and_then(Json::as_u64)
            .and_then(|status| u16::try_from(status).ok())
            .filter(|status| (200..=599).contains(status))
            .ok_or_else(|| channel_error("the encrypted channel's answer has no status"))?;
        let body = match inner.get("body").and_then(Json::as_str) {
            Some(body) => STANDARD
                .decode(body)
                .map_err(|error| channel_error(format!("the encrypted channel sent bad base64: {error}")))?,
            None => Vec::new(),
        };
        Ok(Inner { status, body })
    }
}
