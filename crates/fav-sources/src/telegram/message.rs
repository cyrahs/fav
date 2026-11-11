//! A Telegram message as `src/web/telegram.py` reads it through Telethon, and the checks
//! that decide what a message is saved as.
//!
//! The model keeps what Telethon's `Message` exposes and the source looks at: the text, the
//! album (`grouped_id`), the channel, whether it is a service message, and the media with
//! enough of the file to download it. Telethon's `msg.photo`, `msg.document`, `msg.video` and
//! `msg.sticker` are derived properties; [`TgMessage::document`] and the checks below derive
//! them the same way (a link preview's document counts, as in Telethon).

use fav_core::pytext::py_strip;

use crate::post_links;
use crate::pydecode::PyError;
use crate::telegram_queue::JobType;
use crate::wechat_article::ArticleLink;

/// One size of a photo (`PhotoSize` and its relatives), as `_get_thumb` sorts them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PhotoSize {
    /// `PhotoSizeEmpty`.
    Empty { kind: String },
    /// `PhotoSize`.
    Size { kind: String, size: i64 },
    /// `PhotoCachedSize`: the bytes come with the message.
    Cached { kind: String, bytes: Vec<u8> },
    /// `PhotoStrippedSize`: a tiny JPEG without its header.
    Stripped { kind: String, bytes: Vec<u8> },
    /// `PhotoSizeProgressive`.
    Progressive { kind: String, sizes: Vec<i64> },
    /// `PhotoPathSize`: an SVG outline, never downloaded.
    Path { kind: String },
    /// `VideoSize` (an animated photo's video).
    Video { kind: String, size: i64 },
}

/// A `types.Photo`, with what a download asks for.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Photo {
    pub id: i64,
    pub access_hash: i64,
    pub file_reference: Vec<u8>,
    pub dc_id: i32,
    pub sizes: Vec<PhotoSize>,
    pub video_sizes: Vec<PhotoSize>,
}

/// The document attributes the source and the download look at.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DocumentAttribute {
    /// `DocumentAttributeVideo`.
    Video { round_message: bool },
    /// `DocumentAttributeSticker`.
    Sticker,
    /// `DocumentAttributeCustomEmoji`.
    CustomEmoji,
    /// `DocumentAttributeFilename`.
    Filename(String),
    /// `DocumentAttributeAudio`.
    Audio {
        voice: bool,
        performer: Option<String>,
        title: Option<String>,
    },
    /// Any other attribute (image size, animated, has stickers).
    Other,
}

/// A `types.Document`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Document {
    pub id: i64,
    pub access_hash: i64,
    pub file_reference: Vec<u8>,
    pub dc_id: i32,
    pub size: i64,
    pub mime_type: String,
    pub attributes: Vec<DocumentAttribute>,
}

impl Document {
    /// A document of `mime_type` with `attributes`, as tests and fakes build them.
    pub fn of(mime_type: &str, attributes: Vec<DocumentAttribute>) -> Self {
        Self {
            mime_type: mime_type.to_owned(),
            attributes,
            ..Self::default()
        }
    }
}

/// A `MessageMediaWebPage`'s page: the URL any variant carries, and for a loaded
/// `types.WebPage` its photo and document.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WebPage {
    pub url: String,
    /// `isinstance(webpage, types.WebPage)`: a page Telegram has loaded.
    pub loaded: bool,
    pub photo: Option<Photo>,
    pub document: Option<Document>,
}

/// A message's media (`MessageMediaEmpty` is no media, as Telethon sets it).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Media {
    /// `MessageMediaPhoto`; `None` when the photo is not a `types.Photo`.
    Photo(Option<Photo>),
    /// `MessageMediaDocument`; `None` when the document is not a `types.Document`.
    Document(Option<Document>),
    /// `MessageMediaWebPage`.
    WebPage(WebPage),
    /// Any other media, by its Telethon class name (`MessageMediaGeo`, `MessageMediaPoll`).
    Other(String),
}

/// A Telethon `Message` or `MessageService`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TgMessage {
    pub id: i64,
    /// `msg.message`; empty for a service message.
    pub text: String,
    pub grouped_id: Option<i64>,
    /// `msg.peer_id.channel_id`.
    pub channel_id: Option<i64>,
    /// A `MessageService` (`msg.action is not None`).
    pub action: bool,
    pub media: Option<Media>,
    /// `msg.date`, seconds since the epoch.
    pub date: i64,
}

impl TgMessage {
    /// A plain message in a channel.
    pub fn new(id: i64, channel_id: i64) -> Self {
        Self {
            id,
            channel_id: Some(channel_id),
            ..Self::default()
        }
    }

    /// `msg.document`: the media's document, or a loaded link preview's.
    pub fn document(&self) -> Option<&Document> {
        match &self.media {
            Some(Media::Document(document)) => document.as_ref(),
            Some(Media::WebPage(page)) if page.loaded => page.document.as_ref(),
            _ => None,
        }
    }

    /// `msg.media.webpage`, for any page variant.
    fn webpage(&self) -> Option<&WebPage> {
        match &self.media {
            Some(Media::WebPage(page)) => Some(page),
            _ => None,
        }
    }

    /// `getattr(webpage, 'url', '') or ''`.
    pub fn webpage_url(&self) -> &str {
        self.webpage().map_or("", |page| page.url.as_str())
    }

    /// A truthy `grouped_id` (Python's `if grouped_id`).
    pub fn album(&self) -> Option<i64> {
        self.grouped_id.filter(|id| *id != 0)
    }
}

/// `msg.sticker`: Telethon's `_document_by_attribute(DocumentAttributeSticker)`.
fn is_sticker(message: &TgMessage) -> bool {
    message
        .document()
        .is_some_and(|document| document.attributes.iter().any(|attr| matches!(attr, DocumentAttribute::Sticker)))
}

/// `_is_video_message`: the message's document (a link preview's included) is a video.
pub fn is_video_message(message: &TgMessage) -> bool {
    message.document().is_some_and(|document| {
        document
            .attributes
            .iter()
            .any(|attr| matches!(attr, DocumentAttribute::Video { .. }))
    })
}

/// `_is_sticker_attribute`.
fn is_sticker_attribute(attr: &DocumentAttribute) -> bool {
    matches!(attr, DocumentAttribute::Sticker | DocumentAttribute::CustomEmoji)
}

/// `_is_image_message`: a photo, or an image document that is not a sticker. A link
/// preview's picture does not count.
pub fn is_image_message(message: &TgMessage) -> bool {
    if is_sticker(message) {
        return false;
    }
    match &message.media {
        Some(Media::Photo(_)) => true,
        Some(Media::Document(Some(document))) => {
            document.mime_type.starts_with("image/") && !document.attributes.iter().any(is_sticker_attribute)
        }
        _ => false,
    }
}

/// `_message_link`: the post a message links, from its text or its link preview.
pub fn message_link(message: &TgMessage) -> Result<Option<ArticleLink>, PyError> {
    post_links::find_link(&[&message.text, message.webpage_url()])
}

/// `_message_media_type`: what a message is queued as; `None` for one this channel does
/// not save. A picture or video of a type the channel has no route for is left alone on
/// purpose; a service message carries nothing either.
pub fn message_media_type(message: &TgMessage, media_types: &[JobType]) -> Result<Option<JobType>, PyError> {
    let video = is_video_message(message);
    if media_types.contains(&JobType::Video) && video {
        return Ok(Some(JobType::Video));
    }
    let image = is_image_message(message);
    if media_types.contains(&JobType::Image) && image {
        return Ok(Some(JobType::Image));
    }
    if video || image || message.action {
        return Ok(None);
    }
    if media_types.contains(&JobType::Link) && message_link(message)?.is_some() {
        return Ok(Some(JobType::Link));
    }
    if media_types.contains(&JobType::Unsupported) && message_link(message)?.is_none() {
        return Ok(Some(JobType::Unsupported));
    }
    Ok(None)
}

/// `_unsupported_reason`: what a message with nothing to save holds, for its report.
pub fn unsupported_reason(message: &TgMessage) -> String {
    let text = py_strip(&message.text);
    let url = post_links::first_url(&[text, message.webpage_url()]);
    let what = if is_sticker(message) {
        "a sticker".to_owned()
    } else if let Some(Media::Document(document)) = &message.media {
        let named = message.document().and_then(|document| {
            document.attributes.iter().find_map(|attr| match attr {
                DocumentAttribute::Filename(name) if !name.is_empty() => Some(name.clone()),
                _ => None,
            })
        });
        let mime = document.as_ref().map(|document| document.mime_type.clone()).unwrap_or_default();
        let name = named
            .or_else(|| (!mime.is_empty()).then_some(mime))
            .unwrap_or_else(|| "unknown type".to_owned());
        format!("a file ({name})")
    } else if let Some(Media::Other(class_name)) = &message.media {
        let kind = class_name.strip_prefix("MessageMedia").unwrap_or(class_name).to_lowercase();
        format!("a {} message", if kind.is_empty() { "media" } else { kind.as_str() })
    } else if matches!(message.media, Some(Media::Photo(_))) {
        "a photo message".to_owned()
    } else if !text.is_empty() {
        format!("a text message: {}", text.chars().take(100).collect::<String>())
    } else {
        "an empty message".to_owned()
    };
    post_links::unsupported_reason(&url, &what)
}
