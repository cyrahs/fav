//! Python's `mimetypes.guess_type`, as Python 3.12 answers it.
//!
//! Python origin: the standard library module. The table starts from Python 3.12's
//! built-in defaults and then reads the same system files Python reads (`/etc/mime.types`
//! and the rest of `mimetypes.knownfiles`, later files winning), so a name gets the type
//! Python would give it on the same machine. Only the image types of the defaults are
//! carried: the callers only ever ask whether a name is an image.

use std::collections::HashMap;
use std::sync::OnceLock;

use crate::pytext::py_split;

/// The image entries of Python 3.12's built-in table (`strict=True`).
const DEFAULT_IMAGE_TYPES: [(&str, &str); 23] = [
    (".avif", "image/avif"),
    (".bmp", "image/bmp"),
    (".gif", "image/gif"),
    (".heic", "image/heic"),
    (".heif", "image/heif"),
    (".ico", "image/vnd.microsoft.icon"),
    (".ief", "image/ief"),
    (".jpe", "image/jpeg"),
    (".jpeg", "image/jpeg"),
    (".jpg", "image/jpeg"),
    (".pbm", "image/x-portable-bitmap"),
    (".pgm", "image/x-portable-graymap"),
    (".png", "image/png"),
    (".pnm", "image/x-portable-anymap"),
    (".ppm", "image/x-portable-pixmap"),
    (".ras", "image/x-cmu-raster"),
    (".rgb", "image/x-rgb"),
    (".svg", "image/svg+xml"),
    (".tif", "image/tiff"),
    (".tiff", "image/tiff"),
    (".xbm", "image/x-xbitmap"),
    (".xpm", "image/x-xpixmap"),
    (".xwd", "image/x-xwindowdump"),
];

const SUFFIX_MAP: [(&str, &str); 6] = [
    (".svgz", ".svg.gz"),
    (".tgz", ".tar.gz"),
    (".taz", ".tar.gz"),
    (".tz", ".tar.gz"),
    (".tbz2", ".tar.bz2"),
    (".txz", ".tar.xz"),
];

const ENCODINGS: [&str; 5] = [".gz", ".Z", ".bz2", ".xz", ".br"];

/// `mimetypes.knownfiles`, in Python's order.
pub const KNOWN_FILES: [&str; 9] = [
    "/etc/mime.types",
    "/etc/httpd/mime.types",
    "/etc/httpd/conf/mime.types",
    "/etc/apache/mime.types",
    "/etc/apache2/mime.types",
    "/usr/local/etc/httpd/conf/mime.types",
    "/usr/local/lib/netscape/mime.types",
    "/usr/local/etc/httpd/conf/mime.types",
    "/usr/local/etc/mime.types",
];

/// `urllib.parse.uses_params`: the schemes whose path loses its `;params`.
const USES_PARAMS: [&str; 16] = [
    "", "ftp", "hdl", "prospero", "http", "imap", "https", "shttp", "rtsp", "rtsps", "rtspu", "sip", "sips", "mms", "sftp", "tel",
];

/// A `mimetypes.MimeTypes` table (strict types only).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MimeTypes {
    types: HashMap<String, String>,
}

impl MimeTypes {
    /// `MimeTypes()`: Python's built-in defaults, no system files.
    pub fn defaults() -> Self {
        Self {
            types: DEFAULT_IMAGE_TYPES
                .iter()
                .map(|(ext, kind)| ((*ext).to_owned(), (*kind).to_owned()))
                .collect(),
        }
    }

    /// The table `mimetypes.guess_type` uses: the defaults plus every known file that exists.
    pub fn system() -> &'static Self {
        static SYSTEM: OnceLock<MimeTypes> = OnceLock::new();
        SYSTEM.get_or_init(|| {
            let mut table = Self::defaults();
            for file in KNOWN_FILES {
                if let Ok(bytes) = std::fs::read(file) {
                    table.read(&String::from_utf8_lossy(&bytes));
                }
            }
            table
        })
    }

    /// `readfp`: a `mime.types` file; a later entry for an extension replaces the earlier one.
    pub fn read(&mut self, text: &str) {
        for line in text.split_inclusive('\n') {
            let mut words = py_split(line).take_while(|word| !word.starts_with('#'));
            let Some(kind) = words.next() else { continue };
            for suffix in words {
                self.types.insert(format!(".{suffix}"), kind.to_owned());
            }
        }
    }

    /// `guess_type(url)[0]`.
    pub fn guess_type(&self, url: &str) -> Option<String> {
        let path = match url_scheme_and_path(url) {
            Some((scheme, path)) if scheme == "data" => return guess_data_type(&path),
            Some((_, path)) => path,
            None => url.to_owned(),
        };
        let (base, ext) = splitext(&path);
        let (mut base, mut ext) = (base.to_owned(), ext.to_owned());
        while let Some((_, replacement)) = SUFFIX_MAP.iter().find(|(suffix, _)| *suffix == ext.to_lowercase()) {
            let joined = format!("{base}{replacement}");
            let (next_base, next_ext) = splitext(&joined);
            (base, ext) = (next_base.to_owned(), next_ext.to_owned());
        }
        if ENCODINGS.contains(&ext.as_str()) {
            ext = splitext(&base).1.to_owned();
        }
        self.types.get(&ext.to_lowercase()).cloned()
    }
}

/// `posixpath.splitext`: leading dots of the last component do not start an extension.
fn splitext(path: &str) -> (&str, &str) {
    let sep = path.rfind('/').map_or(0, |index| index + 1);
    let Some(dot) = path.rfind('.').filter(|dot| *dot >= sep) else {
        return (path, "");
    };
    if path[sep..dot].chars().all(|ch| ch == '.') {
        return (path, "");
    }
    (&path[..dot], &path[dot..])
}

/// The scheme and path `urllib.parse.urlparse` finds in `url`, when the scheme is longer
/// than one character (a single letter is a Windows drive to `guess_type`).
fn url_scheme_and_path(url: &str) -> Option<(String, String)> {
    let stripped: String = url
        .trim_start_matches(|ch: char| ch <= ' ')
        .chars()
        .filter(|ch| !matches!(ch, '\t' | '\r' | '\n'))
        .collect();
    let colon = stripped.find(':')?;
    let scheme = &stripped[..colon];
    let first = scheme.chars().next()?;
    if !first.is_ascii_alphabetic()
        || !scheme.chars().all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '+' | '-' | '.'))
        || scheme.len() <= 1
    {
        return None;
    }
    let scheme = scheme.to_ascii_lowercase();
    let mut rest = &stripped[colon + 1..];
    if rest.starts_with("//") {
        let end = rest[2..].find(['/', '?', '#']).map_or(rest.len(), |index| index + 2);
        rest = &rest[end..];
    }
    if let Some(index) = rest.find('#') {
        rest = &rest[..index];
    }
    if let Some(index) = rest.find('?') {
        rest = &rest[..index];
    }
    if USES_PARAMS.contains(&scheme.as_str()) && rest.contains(';') {
        let from = rest.rfind('/').unwrap_or(0);
        if let Some(index) = rest[from..].find(';') {
            rest = &rest[..from + index];
        }
    }
    Some((scheme, rest.to_owned()))
}

/// The `data:` branch of `guess_type`: the media type the URL declares.
fn guess_data_type(path: &str) -> Option<String> {
    let comma = path.find(',')?;
    let kind = match path[..comma].find(';') {
        Some(semi) => &path[..semi],
        None => &path[..comma],
    };
    Some(if kind.contains('=') || !kind.contains('/') { "text/plain".to_owned() } else { kind.to_owned() })
}

#[cfg(test)]
mod tests {
    use super::MimeTypes;

    #[test]
    fn a_system_file_adds_and_replaces_types() {
        let mut table = MimeTypes::defaults();
        assert_eq!(table.guess_type("a.webp"), None);
        table.read("# comment\nimage/webp\twebp  # trailing\nimage/x-png png\n\ntext/plain txt\n");
        assert_eq!(table.guess_type("a.WEBP").as_deref(), Some("image/webp"));
        assert_eq!(table.guess_type("a.png").as_deref(), Some("image/x-png"));
        assert_eq!(table.guess_type("a.txt").as_deref(), Some("text/plain"));
    }
}
