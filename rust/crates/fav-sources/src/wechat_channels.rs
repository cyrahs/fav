//! 视频号 (WeChat Channels) videos forwarded to the agent's WeChat account.
//!
//! Python origin: `src/web/wechat_channels.py`.
//!
//! Forwarding a 视频号 video sends a type-51 message whose `<finderFeed>` names the feed:
//! `objectId` (an unsigned 64-bit number), the `objectNonceId` WeChat needs to open it, the
//! author's nickname, the caption, and per media item its `mediaType` (4 for a video), size,
//! duration and cover. The video address in it carries no token or key, so it cannot be
//! downloaded as it is.
//!
//! TikHub's `fetch_video_detail` looks the feed up by `objectId` with WeChat accounts of its
//! own and answers with a signed CDN address and a `decode_key`. The file behind that
//! address is the original MP4 with only its first [`ENCRYPTED_BYTES`] bytes encrypted, by XOR with
//! an ISAAC-64 keystream seeded with the `decode_key`, each 64-bit output written
//! big-endian, starting from the top of the generator's first block as its `rand()` hands
//! them out. Every lookup returns a fresh address and key, so the two must come from the
//! same answer.
//!
//! The address's `basedata` lists the encodings on offer (`xWT111`, `xWT112`, `xWT113`, ...),
//! and without an `X-snsvideoflag` the CDN sends the first one listed, a 576p one.
//! [`best_quality`] asks for the lowest numbered `xWT` spec, which is the best (720p for a
//! 1080p upload, about what WeChat itself plays); the original upload is not among them.

use std::sync::LazyLock;

use fav_core::pyjson::{Dict, Value};
use fav_core::pytext::{py_isdigit, py_strip};

use crate::pybase64::b64decode;
use crate::pydecode::{PyError, py_int};
use crate::pyre::compile;
use crate::pyval::{py_str, split, str_or_empty};
use crate::wechat_article::tag;

static FEED: LazyLock<regex::Regex> = LazyLock::new(|| compile(r"<finderFeed>.*?</finderFeed>", false, true));
static MEDIA: LazyLock<regex::Regex> = LazyLock::new(|| compile(r"<media>.*?</media>", false, true));
const VIDEO_MEDIA: &str = "4";
static OBJECT_ID: LazyLock<regex::Regex> = LazyLock::new(|| compile(r"^(?:\d{1,20})$", false, false));
static NONCE_ID: LazyLock<regex::Regex> = LazyLock::new(|| compile(r"^(?:[A-Za-z0-9_-]{1,200})$", false, false));
/// TikHub takes only a numeric nonce; older shares carry a longer composite one.
static NUMERIC_NONCE: LazyLock<regex::Regex> = LazyLock::new(|| compile(r"^(?:\d{1,32})$", false, false));
static DECODE_KEY: LazyLock<regex::Regex> = LazyLock::new(|| compile(r"^(?:\d{1,20})$", false, false));
/// `rb'xWT(\d{3})'`: a bytes pattern, so ASCII digits only.
static SPEC: LazyLock<regex::bytes::Regex> = LazyLock::new(|| regex::bytes::Regex::new(r"(?-u)xWT([0-9]{3})").expect("pattern"));
/// How much of a video file WeChat encrypts.
pub const ENCRYPTED_BYTES: usize = 131_072;
const GOLDEN_RATIO: u64 = 0x9E37_79B9_7F4A_7C13;
const WORDS: usize = 256;

/// `ChannelVideo`. The numbers are Python integers, which `int()` gives unbounded; past
/// 128 bits they are refused (D63).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ChannelVideo {
    pub object_id: String,
    pub nonce_id: String,
    pub author: String,
    pub description: String,
    pub duration: i128,
    pub width: i128,
    pub height: i128,
    pub cover_url: String,
}

/// `_number`: `int(text) if text.isdigit() else 0`, so a superscript digit is `int()`'s
/// `ValueError`.
fn number(text: &str) -> Result<i128, PyError> {
    if py_isdigit(text) {
        py_int(&Value::Str(text.to_owned()))
    } else {
        Ok(0)
    }
}

/// `int(text)` for a string of `\d` digits, which `int()` reads whatever their script.
fn digits_value(text: &str) -> Option<i128> {
    fav_core::pytext::py_int_decimal(text)
}

fn is_web_scheme(url: &str) -> Result<bool, PyError> {
    Ok(matches!(split(url)?.scheme.as_str(), "http" | "https"))
}

/// `parse_channel_video`: the 视频号 video a message shares, or `None` for anything else:
/// image feeds and lives included.
pub fn parse_channel_video(content: &str) -> Result<Option<ChannelVideo>, PyError> {
    let Some(feed) = FEED.find(content) else {
        return Ok(None);
    };
    let xml = feed.as_str();
    // The feed's own ids come first; a quoted feed nested further down has its own.
    let object_id = tag(xml, "objectId");
    let nonce_id = tag(xml, "objectNonceId");
    let in_range = OBJECT_ID.is_match(&object_id) && digits_value(&object_id).is_some_and(|id| id < 1 << 64);
    if !in_range || !NONCE_ID.is_match(&nonce_id) {
        return Ok(None);
    }
    if !matches!(tag(xml, "liveId").as_str(), "" | "0") {
        return Ok(None);
    }
    let Some(media) = MEDIA.find(xml) else {
        return Ok(None);
    };
    let item = media.as_str();
    if tag(item, "mediaType") != VIDEO_MEDIA {
        return Ok(None);
    }
    let mut cover = tag(item, "coverUrl");
    if cover.is_empty() {
        cover = tag(item, "thumbUrl");
    }
    Ok(Some(ChannelVideo {
        object_id,
        nonce_id,
        author: tag(xml, "nickname"),
        description: tag(xml, "desc"),
        duration: number(&tag(item, "videoPlayDuration"))?,
        width: number(&tag(item, "width"))?,
        height: number(&tag(item, "height"))?,
        cover_url: if is_web_scheme(&cover)? { cover } else { String::new() },
    }))
}

/// `VideoSource`: where TikHub says the video file is, and the key that decrypts its start.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VideoSource {
    pub url: String,
    pub decode_key: u64,
}

/// `tikhub_lookup`: the body of a `fetch_video_detail` request for this video.
pub fn tikhub_lookup(video: &ChannelVideo) -> Dict {
    let mut body = Dict::new();
    body.insert("object_id", Value::Str(video.object_id.clone()));
    body.insert("raw", Value::Bool(false));
    if NUMERIC_NONCE.is_match(&video.nonce_id) {
        body.insert("object_nonce_id", Value::Str(video.nonce_id.clone()));
    }
    body
}

/// `parse_video_source`: the file address and decode key in a `fetch_video_detail` answer,
/// or `None` without them.
pub fn parse_video_source(payload: &Value) -> Result<Option<VideoSource>, PyError> {
    let mut data = match payload {
        Value::Dict(payload) => payload.get("data"),
        _ => None,
    };
    // The parsed video is `data`; some answers wrap it once more.
    if let Some(Value::Dict(outer)) = data
        && !outer.contains_key("media")
        && let Some(inner @ Value::Dict(_)) = outer.get("data")
    {
        data = Some(inner);
    }
    let mut media = match data {
        Some(Value::Dict(data)) => data.get("media"),
        _ => None,
    };
    if let Some(Value::List(items)) = media {
        media = items.first();
    }
    let Some(Value::Dict(media)) = media else {
        return Ok(None);
    };
    let mut url = str_or_empty(media.get("full_url"));
    if url.is_empty()
        && let Some(base) = media.get("url").filter(|base| base.is_truthy())
    {
        url = format!("{}{}", py_str(base), str_or_empty(media.get("url_token")));
    }
    let key = str_or_empty(media.get("decode_key"));
    let key = py_strip(&key);
    if !is_web_scheme(&url)? || !DECODE_KEY.is_match(key) {
        return Ok(None);
    }
    let Some(decode_key) = digits_value(key).and_then(|key| u64::try_from(key).ok()) else {
        return Ok(None);
    };
    Ok(Some(VideoSource {
        url: best_quality(&url)?,
        decode_key,
    }))
}

/// `urllib.parse.parse_qsl(query)`: `&`-separated pairs, `+` read as a space and escapes
/// decoded; a pair without `=` or with an empty value is left out.
fn parse_qsl(query: &str) -> Vec<(String, String)> {
    let decode = |text: &str| fav_core::pyurl::unquote(&text.replace('+', " "));
    query
        .split('&')
        .filter_map(|pair| pair.split_once('='))
        .filter(|(_, value)| !value.is_empty())
        .map(|(name, value)| (decode(name), decode(value)))
        .collect()
}

/// `best_quality`: `url` asking for the best encoding its `basedata` offers; as it is when
/// it names none.
pub fn best_quality(url: &str) -> Result<String, PyError> {
    let query = parse_qsl(&split(url)?.query);
    if query.iter().any(|(name, _)| name.to_lowercase() == "x-snsvideoflag") {
        return Ok(url.to_owned());
    }
    let basedata = query
        .iter()
        .find(|(name, _)| name == "basedata")
        .map(|(_, value)| value.as_str())
        .unwrap_or_default();
    // parse_qsl turns a bare + into a space; the value may use either base64 alphabet.
    let mut basedata = basedata.replace([' ', '-'], "+").replace('_', "/");
    let missing = (4 - basedata.chars().count() % 4) % 4;
    basedata.extend(std::iter::repeat_n('=', missing));
    let Ok(raw) = b64decode(&basedata, false) else {
        return Ok(url.to_owned());
    };
    let best = SPEC
        .captures_iter(&raw)
        .map(|found| found.get(1).map_or(&[][..], |spec| spec.as_bytes()).to_vec())
        .fold(None::<(u32, Vec<u8>)>, |best, spec| {
            let value = std::str::from_utf8(&spec)
                .ok()
                .and_then(|text| text.parse::<u32>().ok())
                .unwrap_or(u32::MAX);
            match best {
                Some((lowest, _)) if lowest <= value => best,
                _ => Some((value, spec)),
            }
        });
    Ok(match best {
        Some((_, spec)) => format!("{url}&X-snsvideoflag=xWT{}", String::from_utf8_lossy(&spec)),
        None => url.to_owned(),
    })
}

// ---------- ISAAC-64 ----------

#[allow(clippy::many_single_char_names, reason = "ISAAC's own names")]
fn mix(x: &mut [u64; 8]) {
    let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut h] = *x;
    a = a.wrapping_sub(e);
    f ^= h >> 9;
    h = h.wrapping_add(a);
    b = b.wrapping_sub(f);
    g ^= a << 9;
    a = a.wrapping_add(b);
    c = c.wrapping_sub(g);
    h ^= b >> 23;
    b = b.wrapping_add(c);
    d = d.wrapping_sub(h);
    a ^= c << 15;
    c = c.wrapping_add(d);
    e = e.wrapping_sub(a);
    b ^= d >> 14;
    d = d.wrapping_add(e);
    f = f.wrapping_sub(b);
    c ^= e << 20;
    e = e.wrapping_add(f);
    g = g.wrapping_sub(c);
    d ^= f >> 17;
    f = f.wrapping_add(g);
    h = h.wrapping_sub(d);
    e ^= g << 14;
    g = g.wrapping_add(h);
    *x = [a, b, c, d, e, f, g, h];
}

/// Bob Jenkins' ISAAC-64, seeded with one 64-bit word and zeros.
struct Isaac64 {
    results: [u64; WORDS],
    memory: [u64; WORDS],
    a: u64,
    b: u64,
    c: u64,
}

impl Isaac64 {
    fn new(seed: u64) -> Self {
        let mut results = [0_u64; WORDS];
        results[0] = seed;
        let mut generator = Self {
            results,
            memory: [0; WORDS],
            a: 0,
            b: 0,
            c: 0,
        };
        let mut x = [GOLDEN_RATIO; 8];
        for _ in 0..4 {
            mix(&mut x);
        }
        for pass in 0..2 {
            for i in (0..WORDS).step_by(8) {
                let source = if pass == 0 { &generator.results } else { &generator.memory };
                for j in 0..8 {
                    x[j] = x[j].wrapping_add(source[i + j]);
                }
                mix(&mut x);
                generator.memory[i..i + 8].copy_from_slice(&x);
            }
        }
        generator.refill();
        generator
    }

    fn refill(&mut self) {
        self.c = self.c.wrapping_add(1);
        let mut a = self.a;
        let mut b = self.b.wrapping_add(self.c);
        for i in 0..WORDS {
            a = match i & 3 {
                0 => !(a ^ (a << 21)),
                1 => a ^ (a >> 5),
                2 => a ^ (a << 12),
                _ => a ^ (a >> 33),
            };
            a = a.wrapping_add(self.memory[(i + 128) & 255]);
            let x = self.memory[i];
            let y = self.memory[usize::try_from((x >> 3) & 255).unwrap_or_default()]
                .wrapping_add(a)
                .wrapping_add(b);
            self.memory[i] = y;
            b = self.memory[usize::try_from((y >> 11) & 255).unwrap_or_default()].wrapping_add(x);
            self.results[i] = b;
        }
        self.a = a;
        self.b = b;
    }
}

/// `keystream`: the bytes WeChat XORs over the start of a video encrypted with
/// `decode_key`, `size` of them.
pub fn keystream(decode_key: u64, size: usize) -> Vec<u8> {
    let mut generator = Isaac64::new(decode_key);
    let mut out = Vec::with_capacity(size.next_multiple_of(WORDS * 8));
    loop {
        // Each block in the order `rand()` hands it out: from the top down.
        for word in generator.results.iter().rev() {
            out.extend_from_slice(&word.to_be_bytes());
        }
        if out.len() >= size {
            out.truncate(size);
            return out;
        }
        generator.refill();
    }
}

/// `decrypt_head`: `head`, the first bytes of a video file, with WeChat's encryption
/// undone.
pub fn decrypt_head(head: &[u8], decode_key: u64) -> Vec<u8> {
    let mut out = head.to_vec();
    decrypt_in_place(&mut out, decode_key);
    out
}

/// [`decrypt_head`] over a buffer.
pub(crate) fn decrypt_in_place(head: &mut [u8], decode_key: u64) {
    let stream = keystream(decode_key, head.len().min(ENCRYPTED_BYTES));
    for (byte, key) in head.iter_mut().zip(stream) {
        *byte ^= key;
    }
}

/// `str.splitlines()[0]`: the text before Python's first line boundary.
fn first_line(text: &str) -> &str {
    let boundary = |ch: char| {
        matches!(
            ch,
            '\n' | '\r' | '\u{0b}' | '\u{0c}' | '\u{1c}' | '\u{1d}' | '\u{1e}' | '\u{85}' | '\u{2028}' | '\u{2029}'
        )
    };
    text.find(boundary).map_or(text, |end| &text[..end])
}

/// `video_title`: a name for the video: its caption's first line, else its author.
pub fn video_title(video: &ChannelVideo) -> String {
    let caption = if py_strip(&video.description).is_empty() {
        ""
    } else {
        py_strip(first_line(&video.description))
    };
    [caption, video.author.as_str(), video.object_id.as_str()]
        .into_iter()
        .find(|part| !part.is_empty())
        .unwrap_or_default()
        .to_owned()
}
