//! The Spine version of a skeleton file.
//!
//! Python origin: `src/api/spine.py`. Both binary `.skel` files and exported `.json`
//! skeletons carry the Spine version string within their first bytes; reading the same
//! 97-byte window as game-view's client-side probe keeps the two detections aligned.
//!
//! Python memoises the answer per path, modification time and size (`lru_cache`); the
//! answer is the same without the cache, so the file is read on each call.

use std::io::Read as _;
use std::path::Path;
use std::sync::LazyLock;

use regex::Regex;

/// `_SPINE_VERSION_PATTERN`; `\d` is Unicode's decimal digits in both languages.
static SPINE_VERSION_PATTERN: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"4\.(\d+)\.\d+").expect("valid pattern"));
/// `_SPINE_PROBE_BYTES`.
const SPINE_PROBE_BYTES: u64 = 97;

/// `spine_version_from_file`: the first `4.x.y` in the file's first 97 bytes, decoded as
/// UTF-8 with replacement characters; `None` when the file cannot be read.
pub(crate) fn spine_version_from_file(path: &Path) -> Option<String> {
    let mut sample = Vec::new();
    std::fs::File::open(path)
        .and_then(|file| file.take(SPINE_PROBE_BYTES).read_to_end(&mut sample))
        .ok()?;
    let text = String::from_utf8_lossy(&sample);
    SPINE_VERSION_PATTERN.find(&text).map(|found| found.as_str().to_owned())
}

#[cfg(test)]
mod tests {
    use super::spine_version_from_file;

    #[test]
    fn reads_the_version_from_the_first_97_bytes() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let cases: [(&str, Vec<u8>, Option<&str>); 5] = [
            ("skel", b"hash 4.1.24 skel".to_vec(), Some("4.1.24")),
            ("json", br#"{"skeleton":{"spine":"4.2.11"}}"#.to_vec(), Some("4.2.11")),
            ("late", [vec![b' '; 93], b"4.1.24".to_vec()].concat(), None),
            ("binary", [vec![0xff, 0xfe, 0x00], b"x4.0.64y".to_vec()].concat(), Some("4.0.64")),
            ("three", b"3.8.99".to_vec(), None),
        ];
        for (name, body, expected) in cases {
            let path = tmp.path().join(name);
            std::fs::write(&path, body).expect("write");
            assert_eq!(spine_version_from_file(&path).as_deref(), expected, "{name}");
        }
        assert_eq!(spine_version_from_file(&tmp.path().join("missing")), None);
    }
}
