//! Filename sanitization and the `[uploader]title [id].ext` format.
//!
//! Python origin: `src/tool/filename.py`. Every file fav writes is named through these,
//! so existing files on the collection volume keep matching what a re-run produces.

use std::path::{Path, PathBuf};

use crate::pytext::py_strip;

/// Characters replaced by `_` (`[<>:"/\\|?*\n]`).
fn is_invalid(ch: char) -> bool {
    matches!(ch, '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*' | '\n')
}

/// Make `name` safe as a filename: invalid characters become `_`, surrounding whitespace
/// goes, then characters are dropped from the end until it fits in `max_bytes` of UTF-8.
pub fn sanitize(name: &str, max_bytes: usize) -> String {
    let replaced: String = name.chars().map(|ch| if is_invalid(ch) { '_' } else { ch }).collect();
    let mut base = py_strip(&replaced);
    while base.len() > max_bytes {
        let Some((cut, _)) = base.char_indices().next_back() else { break };
        base = &base[..cut];
    }
    base.to_owned()
}

/// `[uploader]title [media_id].ext`, or `title [media_id].ext` without an uploader.
///
/// `media_id` and `uploader` are cut to 100 bytes each and the title gets what is left of
/// `max_total_bytes` (the brackets, space, dot and extension are not counted).
pub fn format_media_filename(title: &str, media_id: &str, uploader: Option<&str>, ext: &str, max_total_bytes: i64) -> String {
    let media_id = sanitize(media_id, 100);
    let ext = ext.strip_prefix('.').unwrap_or(ext);
    let uploader = uploader
        .filter(|uploader| !uploader.is_empty())
        .map(|uploader| sanitize(uploader, 100));
    let used = byte_len(&media_id) + uploader.as_deref().map_or(0, byte_len);
    // A negative budget leaves Python's `while` loop cutting the title to nothing.
    let max_title_bytes = usize::try_from(max_total_bytes - used).unwrap_or(0);
    let title = sanitize(title, max_title_bytes);
    // An uploader that sanitizes to "" is falsy in Python, so it drops the brackets too.
    match uploader.filter(|uploader| !uploader.is_empty()) {
        Some(uploader) => format!("[{uploader}]{title} [{media_id}].{ext}"),
        None => format!("{title} [{media_id}].{ext}"),
    }
}

/// The same as [`format_media_filename`], named for video call sites.
pub fn format_video_filename(title: &str, video_id: &str, uploader: Option<&str>, ext: &str, max_total_bytes: i64) -> String {
    format_media_filename(title, video_id, uploader, ext, max_total_bytes)
}

/// Default for `max_total_bytes`.
pub const MAX_TOTAL_BYTES: i64 = 240;

fn byte_len(text: &str) -> i64 {
    i64::try_from(text.len()).unwrap_or(i64::MAX)
}

/// `path` if nothing exists there, otherwise the first free `stem (n).suffix`.
pub fn ensure_unique_path(path: &Path) -> PathBuf {
    if !path.exists() {
        return path.to_path_buf();
    }
    let name = path.file_name().map(|name| name.to_string_lossy().into_owned()).unwrap_or_default();
    let (stem, suffix) = split_suffix(&name);
    let mut counter = 1_u64;
    loop {
        let candidate = path.with_file_name(format!("{stem} ({counter}){suffix}"));
        if !candidate.exists() {
            return candidate;
        }
        counter += 1;
    }
}

/// `PurePath.stem` and `PurePath.suffix` as Python 3.12 splits a name.
fn split_suffix(name: &str) -> (&str, &str) {
    match name.rfind('.') {
        Some(index) if index > 0 && index < name.len() - 1 => name.split_at(index),
        _ => (name, ""),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::golden;

    #[test]
    fn sanitize_matches_python() {
        let document = golden::load("filename/sanitize.json");
        for case in golden::cases(&document) {
            let name = golden::string(golden::get(case, "name"));
            let max_bytes = usize::try_from(golden::int(golden::get(case, "max_bytes"))).expect("non-negative");
            assert_eq!(
                sanitize(&name, max_bytes),
                golden::string(golden::get(case, "result")),
                "{name:?} {max_bytes}"
            );
        }
    }

    #[test]
    fn format_media_filename_matches_python() {
        let document = golden::load("filename/format_media_filename.json");
        for case in golden::cases(&document) {
            let args = golden::get(case, "args");
            let uploader = args.get("uploader").and_then(|value| value.as_str());
            let max_total_bytes = args
                .get("max_total_bytes")
                .map_or(MAX_TOTAL_BYTES, |value| i64::try_from(golden::int(value)).expect("fits"));
            let result = format_media_filename(
                &golden::string(golden::get(args, "title")),
                &golden::string(golden::get(args, "media_id")),
                uploader,
                &golden::string(golden::get(args, "ext")),
                max_total_bytes,
            );
            assert_eq!(result, golden::string(golden::get(case, "result")), "{args:?}");
            let video = format_video_filename(
                &golden::string(golden::get(args, "title")),
                &golden::string(golden::get(args, "media_id")),
                uploader,
                &golden::string(golden::get(args, "ext")),
                max_total_bytes,
            );
            assert_eq!(video, result);
        }
    }

    #[test]
    fn suffix_split_follows_pure_path() {
        assert_eq!(split_suffix("a.tar.gz"), ("a.tar", ".gz"));
        assert_eq!(split_suffix(".bashrc"), (".bashrc", ""));
        assert_eq!(split_suffix("name."), ("name.", ""));
        assert_eq!(split_suffix("plain"), ("plain", ""));
    }

    #[test]
    fn ensure_unique_path_counts_up_from_one() {
        let directory = std::env::temp_dir().join(format!("fav-core-unique-{}", std::process::id()));
        std::fs::create_dir_all(&directory).expect("temp dir");
        let path = directory.join("clip.mp4");
        assert_eq!(ensure_unique_path(&path), path);
        std::fs::write(&path, b"").expect("write");
        assert_eq!(ensure_unique_path(&path), directory.join("clip (1).mp4"));
        std::fs::write(directory.join("clip (1).mp4"), b"").expect("write");
        assert_eq!(ensure_unique_path(&path), directory.join("clip (2).mp4"));
        std::fs::remove_dir_all(&directory).expect("cleanup");
    }
}
