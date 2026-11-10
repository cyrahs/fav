use std::io::Write as _;

use aes_gcm::aead::{Aead, KeyInit, Payload};

use super::*;

fn descriptor() -> Azp2Descriptor {
    Azp2Descriptor {
        key: "11".repeat(32),
        identity: "22".repeat(32),
    }
}

fn gzip(content: &[u8]) -> Vec<u8> {
    let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    encoder.write_all(content).unwrap();
    encoder.finish().unwrap()
}

/// The test's `azp2_wrap`: reversed, xorshift-masked, AES-GCM.
pub(crate) fn azp2_wrap(content: &[u8], descriptor: &Azp2Descriptor, codec: u8) -> Vec<u8> {
    let seed: u32 = 0x1234_5678;
    let body = if codec == 1 { gzip(content) } else { content.to_vec() };
    let mut state = seed;
    let mut masked = vec![0_u8; body.len()];
    for (index, slot) in masked.iter_mut().enumerate() {
        state ^= state << 13;
        state ^= state >> 17;
        state ^= state << 5;
        *slot = body[body.len() - 1 - index].wrapping_add(state.to_le_bytes()[0]);
    }
    let mut plaintext = seed.to_le_bytes().to_vec();
    plaintext.push(codec);
    plaintext.extend_from_slice(&u32::try_from(content.len()).unwrap().to_le_bytes());
    plaintext.extend_from_slice(&masked);
    let nonce: Vec<u8> = (0..12).collect();
    let key = from_hex(&descriptor.key).unwrap();
    let sealed = Aes256Gcm::new_from_slice(&key)
        .unwrap()
        .encrypt(
            &aes_gcm::Nonce::<U12>::try_from(&nonce[..]).unwrap(),
            Payload {
                msg: &plaintext,
                aad: format!("azp2:{}", descriptor.identity).as_bytes(),
            },
        )
        .unwrap();
    [&b"AZP2"[..], &nonce, &sealed].concat()
}

// py: tests/test_azurlane_protected.py::test_decrypt_opens_both_codecs
#[test]
fn decrypt_opens_both_codecs() {
    let content = br#"{"Version":3,"FileReferences":{"Moc":"a.moc3"}}"#.repeat(3);
    for codec in [0, 1] {
        assert_eq!(decrypt(&azp2_wrap(&content, &descriptor(), codec), &descriptor()).unwrap(), content);
    }
}

// py: tests/test_azurlane_protected.py::test_decrypt_rejects_a_wrong_identity_and_non_envelopes
#[test]
fn decrypt_rejects_a_wrong_identity_and_non_envelopes() {
    let envelope = azp2_wrap(br#"{"a": 1}"#, &descriptor(), 0);
    let wrong = Azp2Descriptor {
        key: descriptor().key,
        identity: "33".repeat(32),
    };
    let error = decrypt(&envelope, &wrong).unwrap_err();
    assert_eq!(error, PyError::new(CLASS, "AZP2 envelope failed authentication: MAC check failed"));
    let error = decrypt(
        br#"{"not": "an envelope", "padding": "................................"}"#,
        &descriptor(),
    )
    .unwrap_err();
    assert_eq!(error, PyError::new(CLASS, "Invalid AZP2 envelope"));
}

// py: tests/test_azurlane_protected.py::test_data_descriptor_is_derived_from_the_path_below_data
#[test]
fn data_descriptor_is_derived_from_the_path_below_data() {
    let descriptor = data_descriptor("ships-CN.json").unwrap();
    assert_eq!(descriptor.key.len(), 64);
    assert_eq!(descriptor.identity.len(), 64);
    assert_ne!(descriptor, data_descriptor("ships-EN.json").unwrap());
    assert_eq!(
        data_url_descriptor("https://static.l2d.su/al/data/ships/CN/10117.json").unwrap(),
        Some(data_descriptor("ships/CN/10117.json").unwrap())
    );
    assert_eq!(data_url_descriptor("https://static.l2d.su/al/assets/x.json").unwrap(), None);
    assert_eq!(
        data_descriptor("../ships-CN.json").unwrap_err(),
        PyError::new(CLASS, "Invalid protected data path: '../ships-CN.json'")
    );
}

// py: tests/test_azurlane_protected.py::test_fragment_descriptor_reads_the_azp2_fragment
#[test]
fn fragment_descriptor_reads_the_azp2_fragment() {
    let url = format!(
        "https://static.l2d.su/al/assets/4306804565fc75b8/b0df.model3.json#azp2={}.{}",
        "ab".repeat(32),
        "cd".repeat(32)
    );
    assert_eq!(
        fragment_descriptor(&url).unwrap(),
        Some(Azp2Descriptor {
            key: "ab".repeat(32),
            identity: "cd".repeat(32)
        })
    );
    assert!(is_protected_url(&url).unwrap());
    assert_eq!(
        strip_fragment(&url),
        "https://static.l2d.su/al/assets/4306804565fc75b8/b0df.model3.json"
    );
    assert_eq!(
        fragment_descriptor("https://static.l2d.su/al/assets/a0722ee10fbfd588/fe86.webp").unwrap(),
        None
    );
    assert_eq!(
        fragment_descriptor("https://static.l2d.su/al/assets/x.json#azp2=nothex.nothex")
            .unwrap_err()
            .class,
        CLASS
    );
}

// py: tests/test_azurlane_protected.py::test_decrypt_for_url_opens_data_and_fragment_urls_and_passes_others_through
#[test]
fn decrypt_for_url_opens_data_and_fragment_urls_and_passes_others_through() {
    let data_url = "https://static.l2d.su/al/data/ships-CN.json";
    let payload = br#"{"ships": []}"#.to_vec();
    let wrapped = azp2_wrap(&payload, &data_descriptor("ships-CN.json").unwrap(), 0);
    assert_eq!(decrypt_for_url(wrapped, data_url).unwrap(), payload);
    // A data file still served in the clear is left alone.
    assert_eq!(decrypt_for_url(payload.clone(), data_url).unwrap(), payload);

    let asset_url = format!(
        "https://static.l2d.su/al/assets/a/b.json#azp2={}.{}",
        descriptor().key,
        descriptor().identity
    );
    assert_eq!(decrypt_for_url(azp2_wrap(&payload, &descriptor(), 0), &asset_url).unwrap(), payload);
    // A URL that names a descriptor must answer with an envelope; an error page is not the file.
    assert_eq!(
        decrypt_for_url(b"<!doctype html>".to_vec(), &asset_url).unwrap_err(),
        PyError::new(CLASS, "Expected an AZP2 envelope from https://static.l2d.su/al/assets/a/b.json")
    );

    assert_eq!(
        decrypt_for_url(b"RIFF....WEBP".to_vec(), "https://static.l2d.su/al/assets/a/b.webp").unwrap(),
        b"RIFF....WEBP"
    );
}

#[test]
fn gzip_decompress_reads_padded_members_and_reports_python_texts() {
    let mut two = gzip(b"hi");
    two.extend_from_slice(b"\0\0");
    two.extend_from_slice(&gzip(b" there"));
    assert_eq!(gzip_decompress(&two).unwrap(), b"hi there");
    assert_eq!(gzip_decompress(b"").unwrap(), b"");
    assert_eq!(gzip_decompress(&[b'x'; 20]).unwrap_err(), "Not a gzipped file (b'xx')");
    let mut junk = gzip(b"hi");
    junk.extend_from_slice(b"junk");
    assert_eq!(gzip_decompress(&junk).unwrap_err(), "Not a gzipped file (b'ju')");
    let whole = gzip(b"hello world");
    assert_eq!(gzip_decompress(&whole[..whole.len() - 9]).unwrap_err(), EOF_MESSAGE);
    let mut crc = whole.clone();
    let at = crc.len() - 8;
    crc[at] ^= 1;
    assert_eq!(gzip_decompress(&crc).unwrap_err(), "CRC check failed");
    let mut size = whole;
    let at = size.len() - 4;
    size[at] ^= 1;
    assert_eq!(gzip_decompress(&size).unwrap_err(), "Incorrect length of data produced");
}

#[test]
fn from_hex_matches_bytes_fromhex() {
    assert_eq!(from_hex("ab cd").unwrap(), [0xab, 0xcd]);
    assert_eq!(
        from_hex("zz").unwrap_err(),
        "non-hexadecimal number found in fromhex() arg at position 0"
    );
    assert_eq!(
        from_hex("abc").unwrap_err(),
        "non-hexadecimal number found in fromhex() arg at position 3"
    );
    assert_eq!(
        from_hex("a b").unwrap_err(),
        "non-hexadecimal number found in fromhex() arg at position 1"
    );
}

fn attempt<T>(result: Result<T, PyError>, render: impl FnOnce(T) -> fav_core::pyjson::Value) -> fav_core::pyjson::Value {
    use fav_core::pyjson::{Dict, Value};
    let mut out = Dict::new();
    match result {
        Ok(value) => out.insert("ok", render(value)),
        Err(error) => {
            let mut detail = Dict::new();
            detail.insert("class", Value::Str(error.class));
            detail.insert("message", Value::Str(error.message));
            out.insert("error", Value::Dict(detail));
        }
    }
    Value::Dict(out)
}

fn descriptor_value(descriptor: Option<Azp2Descriptor>) -> fav_core::pyjson::Value {
    use fav_core::pyjson::{Dict, Value};
    descriptor.map_or(Value::Null, |descriptor| {
        let mut out = Dict::new();
        out.insert("key", Value::Str(descriptor.key));
        out.insert("identity", Value::Str(descriptor.identity));
        Value::Dict(out)
    })
}

fn hex_text(bytes: &[u8]) -> fav_core::pyjson::Value {
    fav_core::pyjson::Value::Str(hex(bytes))
}

#[test]
fn the_envelope_matches_the_python_baseline() {
    use fav_core::golden;
    use fav_core::pyjson::Value;

    use crate::azurlane::l2d_sources::tests::golden::{assert_same, decode_hex};

    let document = golden::load("azurlane/protected.json");
    let [paths, urls, envelopes] = golden::cases(&document) else { panic!("three cases") };
    for case in golden::list(golden::get(paths, "data_descriptor")) {
        let path = golden::string(golden::get(case, "path"));
        let actual = attempt(data_descriptor(&path), |descriptor| descriptor_value(Some(descriptor)));
        assert_same(&actual, golden::get(case, "result"), &format!("data_descriptor({path:?})"));
    }
    for case in golden::list(golden::get(urls, "urls")) {
        let url = golden::string(golden::get(case, "url"));
        let check = |key: &str, actual: Value| assert_same(&actual, golden::get(case, key), &format!("{url}: {key}"));
        check("fragment_descriptor", attempt(fragment_descriptor(&url), descriptor_value));
        check("data_url_descriptor", attempt(data_url_descriptor(&url), descriptor_value));
        check("is_protected_url", attempt(is_protected_url(&url), Value::Bool));
        check("strip_fragment", Value::Str(strip_fragment(&url).to_owned()));
    }
    for case in golden::list(golden::get(envelopes, "decrypt_for_url")) {
        let name = golden::string(golden::get(case, "name"));
        let url = golden::string(golden::get(case, "url"));
        let content = decode_hex(&golden::string(golden::get(case, "content_hex")));
        let actual = attempt(decrypt_for_url(content, &url), |plain| hex_text(&plain));
        assert_same(&actual, golden::get(case, "result"), &name);
    }
}
