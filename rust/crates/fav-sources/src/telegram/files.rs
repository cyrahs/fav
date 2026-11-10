//! What Telethon's `message.download_media(file=path)` downloads and the name it writes.
//!
//! Python origin: Telethon 1.43's `DownloadMethods.download_media`, `_download_photo`,
//! `_download_document`, `_get_thumb`, `_get_kind_and_names`, `_get_proper_filename`,
//! `_download_cached_photo_size`, and `utils.get_extension` / `stripped_photo_to_jpg`.
//! `src/web/telegram.py` passes a path without an extension and keeps the suffix Telethon
//! chose, so the saved file is named the way Telethon would have named it.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use super::message::{Document, DocumentAttribute, Media, Photo, PhotoSize, TgMessage};

/// What to fetch for a message's media.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Location {
    /// `InputPhotoFileLocation` at `thumb_size`.
    Photo { photo: Photo, thumb_size: String },
    /// `InputDocumentFileLocation` (whole file).
    Document { document: Document },
    /// The bytes came with the message (`PhotoCachedSize`, or a stripped size made a JPEG).
    Inline(Vec<u8>),
}

/// A download Telethon would make: where to, and the size it expects (`file_size`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Plan {
    pub location: Location,
    pub path: PathBuf,
    pub size: Option<i64>,
}

/// `_get_thumb(thumbs, None)`: the largest size by Telethon's order. Empty and unknown
/// sizes sort first, video sizes last, and path sizes are never chosen.
pub fn largest_size(sizes: &[PhotoSize]) -> Option<&PhotoSize> {
    if sizes.is_empty() {
        return None;
    }
    let key = |size: &PhotoSize| -> (i32, i64) {
        match size {
            PhotoSize::Stripped { bytes, .. } | PhotoSize::Cached { bytes, .. } => (1, i64::try_from(bytes.len()).unwrap_or(i64::MAX)),
            PhotoSize::Size { size, .. } => (1, *size),
            PhotoSize::Progressive { sizes, .. } => (1, sizes.iter().copied().max().unwrap_or(0)),
            PhotoSize::Video { size, .. } => (2, *size),
            PhotoSize::Empty { .. } | PhotoSize::Path { .. } => (0, 0),
        }
    };
    let mut sorted: Vec<&PhotoSize> = sizes.iter().collect();
    // Python's `sorted` is stable, as `sort_by_key` is.
    sorted.sort_by_key(|size| key(size));
    sorted.retain(|size| !matches!(size, PhotoSize::Path { .. }));
    sorted.last().copied()
}

/// `_get_kind_and_names(attributes)`.
pub fn kind_and_names(attributes: &[DocumentAttribute]) -> (&'static str, Vec<String>) {
    let mut kind = "document";
    let mut names: Vec<String> = Vec::new();
    for attr in attributes {
        match attr {
            DocumentAttribute::Filename(name) => names.insert(0, name.clone()),
            DocumentAttribute::Audio { voice, performer, title } => {
                kind = "audio";
                let performer = performer.as_deref().filter(|text| !text.is_empty());
                let title = title.as_deref().filter(|text| !text.is_empty());
                match (performer, title) {
                    (Some(performer), Some(title)) => names.push(format!("{performer} - {title}")),
                    (Some(performer), None) => names.push(performer.to_owned()),
                    (None, Some(title)) => names.push(title.to_owned()),
                    (None, None) if *voice => kind = "voice",
                    (None, None) => {}
                }
            }
            _ => {}
        }
    }
    (kind, names)
}

/// The JPEG header `stripped_photo_to_jpg` puts in front of a stripped size.
const STRIPPED_HEADER: &[u8] = b"\xff\xd8\xff\xe0\x00\x10JFIF\x00\x01\x01\x00\x00\x01\x00\x01\x00\x00\xff\xdb\x00C\x00(\x1c\x1e#\x1e\x19(#!#-+(0<dA<77<{X]Id\x91\x80\x99\x96\x8f\x80\x8c\x8a\xa0\xb4\xe6\xc3\xa0\xaa\xda\xad\x8a\x8c\xc8\xff\xcb\xda\xee\xf5\xff\xff\xff\x9b\xc1\xff\xff\xff\xfa\xff\xe6\xfd\xff\xf8\xff\xdb\x00C\x01+--<5<vAAv\xf8\xa5\x8c\xa5\xf8\xf8\xf8\xf8\xf8\xf8\xf8\xf8\xf8\xf8\xf8\xf8\xf8\xf8\xf8\xf8\xf8\xf8\xf8\xf8\xf8\xf8\xf8\xf8\xf8\xf8\xf8\xf8\xf8\xf8\xf8\xf8\xf8\xf8\xf8\xf8\xf8\xf8\xf8\xf8\xf8\xf8\xf8\xf8\xf8\xf8\xf8\xf8\xf8\xf8\xff\xc0\x00\x11\x08\x00\x00\x00\x00\x03\x01\"\x00\x02\x11\x01\x03\x11\x01\xff\xc4\x00\x1f\x00\x00\x01\x05\x01\x01\x01\x01\x01\x01\x00\x00\x00\x00\x00\x00\x00\x00\x01\x02\x03\x04\x05\x06\x07\x08\t\n\x0b\xff\xc4\x00\xb5\x10\x00\x02\x01\x03\x03\x02\x04\x03\x05\x05\x04\x04\x00\x00\x01}\x01\x02\x03\x00\x04\x11\x05\x12!1A\x06\x13Qa\x07\"q\x142\x81\x91\xa1\x08#B\xb1\xc1\x15R\xd1\xf0$3br\x82\t\n\x16\x17\x18\x19\x1a%&'()*456789:CDEFGHIJSTUVWXYZcdefghijstuvwxyz\x83\x84\x85\x86\x87\x88\x89\x8a\x92\x93\x94\x95\x96\x97\x98\x99\x9a\xa2\xa3\xa4\xa5\xa6\xa7\xa8\xa9\xaa\xb2\xb3\xb4\xb5\xb6\xb7\xb8\xb9\xba\xc2\xc3\xc4\xc5\xc6\xc7\xc8\xc9\xca\xd2\xd3\xd4\xd5\xd6\xd7\xd8\xd9\xda\xe1\xe2\xe3\xe4\xe5\xe6\xe7\xe8\xe9\xea\xf1\xf2\xf3\xf4\xf5\xf6\xf7\xf8\xf9\xfa\xff\xc4\x00\x1f\x01\x00\x03\x01\x01\x01\x01\x01\x01\x01\x01\x01\x00\x00\x00\x00\x00\x00\x01\x02\x03\x04\x05\x06\x07\x08\t\n\x0b\xff\xc4\x00\xb5\x11\x00\x02\x01\x02\x04\x04\x03\x04\x07\x05\x04\x04\x00\x01\x02w\x00\x01\x02\x03\x11\x04\x05!1\x06\x12AQ\x07aq\x13\"2\x81\x08\x14B\x91\xa1\xb1\xc1\t#3R\xf0\x15br\xd1\n\x16$4\xe1%\xf1\x17\x18\x19\x1a&'()*56789:CDEFGHIJSTUVWXYZcdefghijstuvwxyz\x82\x83\x84\x85\x86\x87\x88\x89\x8a\x92\x93\x94\x95\x96\x97\x98\x99\x9a\xa2\xa3\xa4\xa5\xa6\xa7\xa8\xa9\xaa\xb2\xb3\xb4\xb5\xb6\xb7\xb8\xb9\xba\xc2\xc3\xc4\xc5\xc6\xc7\xc8\xc9\xca\xd2\xd3\xd4\xd5\xd6\xd7\xd8\xd9\xda\xe2\xe3\xe4\xe5\xe6\xe7\xe8\xe9\xea\xf2\xf3\xf4\xf5\xf6\xf7\xf8\xf9\xfa\xff\xda\x00\x0c\x03\x01\x00\x02\x11\x03\x11\x00?\x00";

/// `utils.stripped_photo_to_jpg`.
pub fn stripped_photo_to_jpg(stripped: &[u8]) -> Vec<u8> {
    if stripped.len() < 3 || stripped[0] != 1 {
        return stripped.to_vec();
    }
    let mut out = STRIPPED_HEADER.to_vec();
    out[164] = stripped[1];
    out[166] = stripped[2];
    out.extend_from_slice(&stripped[3..]);
    out.extend_from_slice(b"\xff\xd9");
    out
}

/// What `mimetypes.add_type` adds when Telethon's `utils` is imported, in its order.
pub const TELETHON_TYPES: [(&str, &str); 17] = [
    ("image/png", ".png"),
    ("image/jpeg", ".jpeg"),
    ("image/webp", ".webp"),
    ("image/gif", ".gif"),
    ("image/bmp", ".bmp"),
    ("image/x-tga", ".tga"),
    ("image/tiff", ".tiff"),
    ("image/vnd.adobe.photoshop", ".psd"),
    ("video/mp4", ".mp4"),
    ("video/quicktime", ".mov"),
    ("video/avi", ".avi"),
    ("audio/mpeg", ".mp3"),
    ("audio/m4a", ".m4a"),
    ("audio/aac", ".aac"),
    ("audio/ogg", ".ogg"),
    ("audio/flac", ".flac"),
    ("application/x-tgsticker", ".tgs"),
];

/// `mimetypes.guess_extension` over a table: the first extension added for a type.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Extensions {
    by_type: HashMap<String, Vec<String>>,
}

impl Extensions {
    /// `MimeTypes.add_type(type, ext)` for the inverse map.
    pub fn add(&mut self, kind: &str, ext: &str) {
        let list = self.by_type.entry(kind.to_lowercase()).or_default();
        if !list.iter().any(|known| known == ext) {
            list.push(ext.to_owned());
        }
    }

    /// `MimeTypes.readfp`: every `type ext ext ...` line, in order.
    pub fn read(&mut self, text: &str) {
        for line in text.split_inclusive('\n') {
            let mut words = fav_core::pytext::py_split(line).take_while(|word| !word.starts_with('#'));
            let Some(kind) = words.next() else { continue };
            for suffix in words {
                self.add(kind, &format!(".{suffix}"));
            }
        }
    }

    /// Python 3.12's built-in table (`_types_map_default`, strict), then Telethon's additions.
    pub fn defaults() -> Self {
        let mut table = Self::default();
        for (ext, kind) in super::mimetypes_defaults::DEFAULT_TYPES {
            table.add(kind, ext);
        }
        table
    }

    fn add_telethon(&mut self) {
        for (kind, ext) in TELETHON_TYPES {
            self.add(kind, ext);
        }
    }

    /// The defaults with Telethon's additions, without the system's files (tests).
    pub fn defaults_with_telethon() -> Self {
        let mut table = Self::defaults();
        table.add_telethon();
        table
    }

    /// The table the worker uses: the defaults, the system's `mime.types` files Python
    /// reads (`mimetypes.init`), then what Telethon adds on import.
    pub fn system() -> &'static Self {
        static SYSTEM: OnceLock<Extensions> = OnceLock::new();
        SYSTEM.get_or_init(|| {
            let mut table = Self::defaults();
            for file in fav_core::mimetypes::KNOWN_FILES {
                if let Ok(bytes) = std::fs::read(file) {
                    table.read(&String::from_utf8_lossy(&bytes));
                }
            }
            table.add_telethon();
            table
        })
    }

    /// `guess_extension(type)`.
    pub fn guess_extension(&self, kind: &str) -> Option<&str> {
        self.by_type
            .get(&kind.to_lowercase())
            .and_then(|list| list.first())
            .map(String::as_str)
    }

    /// `utils.get_extension(document)`.
    pub fn document_extension(&self, document: &Document) -> String {
        if document.mime_type == "application/octet-stream" {
            return String::new();
        }
        self.guess_extension(&document.mime_type).unwrap_or_default().to_owned()
    }
}

/// `os.path.splitext` on a name: the last dot not leading the name.
fn split_ext(name: &str) -> (&str, &str) {
    match name.rfind('.') {
        Some(dot) if !name[..dot].chars().all(|ch| ch == '.') => (&name[..dot], &name[dot..]),
        _ => (name, ""),
    }
}

/// `_get_proper_filename(file, kind, extension, possible_names=...)` for a path that names a
/// file: Telethon keeps an existing file's path as is, adds `extension` when the name has
/// none, and numbers the name ` (1)`, ` (2)`... while the result exists. `exists` is the
/// file system check (`os.path.isfile`).
pub fn proper_filename(file: &Path, extension: &str, is_file: impl Fn(&Path) -> bool, is_dir: impl Fn(&Path) -> bool) -> PathBuf {
    if is_file(file) || is_dir(file) {
        // A directory would get a generated name; the source never passes one.
        if is_file(file) {
            return file.to_path_buf();
        }
    }
    let directory = file.parent().map(Path::to_path_buf).unwrap_or_default();
    let name = file.file_name().map(|name| name.to_string_lossy().into_owned()).unwrap_or_default();
    let (stem, ext) = split_ext(&name);
    let ext = if ext.is_empty() { extension } else { ext };
    let result = directory.join(format!("{stem}{ext}"));
    if !is_file(&result) {
        return result;
    }
    let mut index = 1_u64;
    loop {
        let candidate = directory.join(format!("{stem} ({index}){ext}"));
        if !is_file(&candidate) {
            return candidate;
        }
        index += 1;
    }
}

fn photo_plan(photo: &Photo, file: &Path, exists: &dyn Fn(&Path) -> bool) -> Option<Plan> {
    let sizes: Vec<PhotoSize> = photo.sizes.iter().chain(photo.video_sizes.iter()).cloned().collect();
    let size = largest_size(&sizes)?;
    let extension = if matches!(size, PhotoSize::Video { .. }) { ".mp4" } else { ".jpg" };
    let path = proper_filename(file, extension, exists, |_| false);
    let (location, size) = match size {
        PhotoSize::Empty { .. } | PhotoSize::Path { .. } => return None,
        PhotoSize::Cached { bytes, .. } => (Location::Inline(bytes.clone()), None),
        PhotoSize::Stripped { bytes, .. } => (Location::Inline(stripped_photo_to_jpg(bytes)), None),
        PhotoSize::Progressive { kind, sizes } => (
            Location::Photo {
                photo: photo.clone(),
                thumb_size: kind.clone(),
            },
            sizes.iter().copied().max(),
        ),
        PhotoSize::Size { kind, size } | PhotoSize::Video { kind, size } => (
            Location::Photo {
                photo: photo.clone(),
                thumb_size: kind.clone(),
            },
            Some(*size),
        ),
    };
    Some(Plan { location, path, size })
}

fn document_plan(document: &Document, file: &Path, extensions: &Extensions, exists: &dyn Fn(&Path) -> bool) -> Plan {
    let path = proper_filename(file, &extensions.document_extension(document), exists, |_| false);
    Plan {
        location: Location::Document {
            document: document.clone(),
        },
        path,
        size: Some(document.size),
    }
}

/// `download_media(file=...)` up to the request: what would be fetched and where it would
/// be written; `None` where Telethon returns `None` without downloading anything.
pub fn plan(message: &TgMessage, file: &Path, extensions: &Extensions, exists: &dyn Fn(&Path) -> bool) -> Option<Plan> {
    match message.media.as_ref()? {
        Media::Photo(photo) => photo_plan(photo.as_ref()?, file, exists),
        Media::Document(document) => Some(document_plan(document.as_ref()?, file, extensions, exists)),
        Media::WebPage(page) if page.loaded => {
            // `media.webpage.document or media.webpage.photo`.
            if let Some(document) = &page.document {
                return Some(document_plan(document, file, extensions, exists));
            }
            photo_plan(page.photo.as_ref()?, file, exists)
        }
        Media::WebPage(_) | Media::Other(_) => None,
    }
}
