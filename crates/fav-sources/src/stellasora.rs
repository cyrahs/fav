//! Stella Sora Wiki scraper: character galleries and disc art from the `MediaWiki` site.
//!
//! Python origin: `src/web/stellasora.py`. The Characters and List of Discs pages give the
//! file titles, each character's gallery page adds its CGs, Memory Snapshots, Awakened and
//! Skin sprites, the `MediaWiki` API resolves titles to image URLs, and each file is
//! downloaded once into `<path>/<character>/` or `<path>/disc/`. Pages are read with the
//! same `HTMLParser` tokenizer Python uses (`fav_core::pyhtml`), so the titles found, their
//! order and the directories they land in match. Dedupe is the file system itself: a file
//! already under the target directory is skipped, one found under `disc/` that now belongs
//! to a character is moved there.

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use fav_core::filename::sanitize;
use fav_core::logging::{self, Logger};
use fav_core::pycasefold::casefold;
use fav_core::pyhtml::{self, Attribute, HtmlHandler};
use fav_core::pyjson::{Dict, Value};
use fav_core::pytext::py_strip;
use fav_core::pyurl::{UrlError, unquote, urljoin, urlsplit};
use fav_core::{pyjson, pypath};
use fav_net::httpx::{HttpError, StreamError as DownloadError};
use fav_net::reqwest;
use fav_net::retry::{Sleeper, tokio_sleeper};
use fav_runtime::BoxFuture;
use fav_runtime::notifications::{NewNotification, Outbox};
use fav_runtime::pystr::str_value;
use fav_runtime::source::{JobError, Registry, Source};
use futures_util::StreamExt;

use crate::SourceContext;
use crate::notifier::{HttpJobError as _, OutboxNotifier, SharedNotifier};

pub const JOB_KEY: &str = "stellasora";
pub const BASE_URL: &str = "https://stellasora.miraheze.org";
const API_PATH: &str = "/w/api.php";
const CHARACTERS_PATH: &str = "/wiki/Characters";
const LIST_OF_DISCS_PATH: &str = "/wiki/List_of_Discs";
const CHARACTERS_NAME_COL_INDEX: usize = 2;
const USER_AGENT: &str = "fav/0.1 (stellasora)";
/// Galleries fetched at once (`asyncio.Semaphore(5)`).
const GALLERY_CONCURRENCY: usize = 5;
const RESOLVE_BATCH: usize = 50;
const DOWNLOAD_ATTEMPTS: u32 = 3;

fn log() -> Logger {
    logging::get("stellasora")
}

/// A wiki character page entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CharacterPage {
    pub name: String,
    pub url: String,
}

/// A resolved `MediaWiki` file entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WikiFile {
    pub title: String,
    pub url: String,
    pub description_url: Option<String>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DownloadStats {
    pub skipped: u64,
    pub moved: u64,
    pub downloaded: u64,
}

fn value_error(error: &UrlError) -> JobError {
    JobError::new("ValueError", error.to_string())
}

// ---------- titles and destinations ----------

/// `normalize_wiki_file_title`: a stable `File:Name_With_Underscores.ext` form for dedupe.
pub fn normalize_wiki_file_title(title: &str) -> Result<String, UrlError> {
    let mut t = py_strip(title).to_owned();
    if t.is_empty() {
        return Ok(String::new());
    }
    // Drop the fragment, then the query.
    t = t
        .split('#')
        .next()
        .unwrap_or_default()
        .split('?')
        .next()
        .unwrap_or_default()
        .to_owned();
    if t.starts_with("http://") || t.starts_with("https://") {
        let path = urlsplit(&t, "")?.path;
        if path.starts_with("/wiki/") {
            t = path;
        }
    }
    if let Some(rest) = t.strip_prefix("/wiki/") {
        t = rest.to_owned();
    }
    let t = unquote(&t).replace(' ', "_");
    // `t.lower().startswith('file:')`: only ASCII letters lowercase into `file:`.
    if t.len() >= 5 && t.as_bytes()[..5].eq_ignore_ascii_case(b"file:") {
        return Ok(format!("File:{}", &t[5..]));
    }
    Ok(format!("File:{t}"))
}

/// `dedupe_wiki_file_titles`: normalized, first occurrence wins, compared casefolded.
pub fn dedupe_wiki_file_titles(titles: &[String]) -> Result<Vec<String>, UrlError> {
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for raw in titles {
        let normalized = normalize_wiki_file_title(raw)?;
        if normalized.is_empty() {
            continue;
        }
        if seen.insert(casefold(&normalized)) {
            out.push(normalized);
        }
    }
    Ok(out)
}

/// A map from normalized title to its directory that keeps insertion order, as a Python
/// dict does (the download loop walks it in that order).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DestinationMap {
    order: Vec<String>,
    dirs: HashMap<String, String>,
}

impl DestinationMap {
    /// `setdefault(title, dir)`.
    fn set_default(&mut self, title: String, dir: &str) {
        if !self.dirs.contains_key(&title) {
            self.dirs.insert(title.clone(), dir.to_owned());
            self.order.push(title);
        }
    }

    pub fn get(&self, title: &str) -> Option<&str> {
        self.dirs.get(title).map(String::as_str)
    }

    pub fn len(&self) -> usize {
        self.order.len()
    }

    pub fn is_empty(&self) -> bool {
        self.order.is_empty()
    }

    /// `(title, dir)` pairs in insertion order.
    pub fn iter(&self) -> impl Iterator<Item = (&str, &str)> {
        self.order.iter().map(|title| (title.as_str(), self.dirs[title].as_str()))
    }
}

/// `build_download_destination_map`: character files under `<base>/<character>/`, discs
/// under `<base>/disc/`; a disc file a character also lists goes to the first such
/// character. `character_titles` is in the characters' page order.
pub fn build_download_destination_map(
    base_path: &str,
    disc_titles: &[String],
    character_titles: &[(String, Vec<String>)],
) -> Result<DestinationMap, UrlError> {
    let mut map = DestinationMap::default();
    for (character_name, titles) in character_titles {
        let character_dir = pypath::join(base_path, &sanitize(character_name, 120));
        for raw in titles {
            let normalized = normalize_wiki_file_title(raw)?;
            if !normalized.is_empty() {
                map.set_default(normalized, &character_dir);
            }
        }
    }
    let disc_dir = pypath::join(base_path, "disc");
    for raw in disc_titles {
        let normalized = normalize_wiki_file_title(raw)?;
        if !normalized.is_empty() {
            map.set_default(normalized, &disc_dir);
        }
    }
    Ok(map)
}

/// `_safe_local_filename_from_wiki_title`: the file name without `File:`, its stem cut so
/// the whole stays within 200 bytes, the extension kept.
pub fn safe_local_filename_from_wiki_title(title: &str) -> Result<String, UrlError> {
    let normalized = normalize_wiki_file_title(title)?;
    let name = normalized.strip_prefix("File:").unwrap_or(&normalized);
    let suffix = pypath::suffix(name);
    let stem = if suffix.is_empty() { name } else { pypath::stem(name) };
    let max_stem_bytes = 200_usize.saturating_sub(suffix.len());
    Ok(format!("{}{suffix}", sanitize(stem, max_stem_bytes)))
}

// ---------- page parsers ----------

fn attr<'a>(attrs: &'a [Attribute], name: &str) -> Option<&'a Option<String>> {
    // `dict(attrs)`: the last duplicate wins.
    attrs.iter().rev().find(|(key, _)| key == name).map(|(_, value)| value)
}

/// `dict(attrs).get(name)` as text, `None` for a missing or bare attribute.
fn attr_text<'a>(attrs: &'a [Attribute], name: &str) -> Option<&'a str> {
    attr(attrs, name).and_then(Option::as_deref)
}

#[allow(clippy::struct_excessive_bools)] // The parser state flags of Python's `_CharactersTableParser`.
struct CharactersTable<'a> {
    base_url: &'a str,
    in_table: bool,
    in_row: bool,
    td_index: usize,
    capture_anchor: bool,
    in_anchor: bool,
    current_href: Option<String>,
    text_parts: Vec<String>,
    characters: Vec<CharacterPage>,
    error: Option<UrlError>,
}

impl HtmlHandler for CharactersTable<'_> {
    fn start_tag(&mut self, tag: &str, attrs: &[Attribute]) {
        if tag == "table" && attr_text(attrs, "id") == Some("trekker-table") {
            self.in_table = true;
            return;
        }
        if !self.in_table {
            return;
        }
        if tag == "tr" {
            self.in_row = true;
            self.td_index = 0;
            return;
        }
        if tag == "td" && self.in_row {
            self.td_index += 1;
            self.capture_anchor = false;
            self.in_anchor = false;
            self.current_href = None;
            self.text_parts.clear();
            return;
        }
        if tag == "a"
            && self.in_row
            && self.td_index == CHARACTERS_NAME_COL_INDEX
            && let Some(href) = attr_text(attrs, "href")
            && href.starts_with("/wiki/")
        {
            self.capture_anchor = true;
            self.in_anchor = true;
            self.current_href = Some(href.to_owned());
        }
    }

    fn end_tag(&mut self, tag: &str) {
        if tag == "table" && self.in_table {
            self.in_table = false;
            return;
        }
        if !self.in_table {
            return;
        }
        if tag == "tr" {
            self.in_row = false;
            return;
        }
        if tag == "a" && self.in_anchor {
            if self.capture_anchor
                && let Some(href) = self.current_href.as_deref().filter(|href| !href.is_empty())
            {
                let joined = self.text_parts.concat();
                let name = py_strip(&pyhtml::unescape(&joined)).to_owned();
                if !name.is_empty() {
                    match urljoin(self.base_url, href) {
                        Ok(url) => self.characters.push(CharacterPage { name, url }),
                        Err(error) => {
                            self.error.get_or_insert(error);
                        }
                    }
                }
            }
            self.capture_anchor = false;
            self.in_anchor = false;
            self.current_href = None;
            self.text_parts.clear();
        }
    }

    fn data(&mut self, data: &str) {
        if self.in_anchor && self.capture_anchor {
            self.text_parts.push(data.to_owned());
        }
    }
}

/// `parse_characters_page`: the character links in the name column of `#trekker-table`.
pub fn parse_characters_page(html: &str, base_url: &str) -> Result<Vec<CharacterPage>, UrlError> {
    let mut parser = CharactersTable {
        base_url,
        in_table: false,
        in_row: false,
        td_index: 0,
        capture_anchor: false,
        in_anchor: false,
        current_href: None,
        text_parts: Vec::new(),
        characters: Vec::new(),
        error: None,
    };
    pyhtml::feed(html, &mut parser);
    match parser.error {
        Some(error) => Err(error),
        None => Ok(parser.characters),
    }
}

struct InfoboxImages<'a> {
    base_url: &'a str,
    depth: i64,
    /// `{title: url}` in first-insertion order; a repeated title keeps its place.
    images: Vec<(String, String)>,
    error: Option<UrlError>,
}

impl HtmlHandler for InfoboxImages<'_> {
    fn start_tag(&mut self, tag: &str, attrs: &[Attribute]) {
        if tag == "aside" {
            let classes = attr_text(attrs, "class").unwrap_or_default();
            if fav_core::pytext::py_split(classes).any(|class| class == "portable-infobox") {
                self.depth += 1;
            }
            return;
        }
        if self.depth <= 0 || tag != "a" {
            return;
        }
        let (Some(href), Some(title)) = (attr_text(attrs, "href"), attr_text(attrs, "title")) else {
            return;
        };
        if href.is_empty() || title.is_empty() {
            return;
        }
        if !attr_text(attrs, "class").unwrap_or_default().contains("image-thumbnail") {
            return;
        }
        match urljoin(self.base_url, href) {
            Ok(url) => match self.images.iter_mut().find(|(key, _)| key == title) {
                Some(entry) => entry.1 = url,
                None => self.images.push((title.to_owned(), url)),
            },
            Err(error) => {
                self.error.get_or_insert(error);
            }
        }
    }

    fn end_tag(&mut self, tag: &str) {
        if tag == "aside" && self.depth > 0 {
            self.depth -= 1;
        }
    }

    fn data(&mut self, _data: &str) {}
}

/// `parse_character_infobox_images`: `{title: url}` of the thumbnails in the infobox
/// (typically "Profile Image" and "Artwork").
pub fn parse_character_infobox_images(html: &str, base_url: &str) -> Result<Vec<(String, String)>, UrlError> {
    let mut parser = InfoboxImages {
        base_url,
        depth: 0,
        images: Vec::new(),
        error: None,
    };
    pyhtml::feed(html, &mut parser);
    match parser.error {
        Some(error) => Err(error),
        None => Ok(parser.images),
    }
}

/// The `titles` property shared by the section parsers: order kept, repeats dropped.
fn unique(titles: Vec<String>) -> Vec<String> {
    let mut seen = HashSet::new();
    titles.into_iter().filter(|title| seen.insert(title.clone())).collect()
}

/// `href` of a `/wiki/File:` link as a title, percent-decoded.
fn file_title(attrs: &[Attribute]) -> Option<String> {
    let href = attr_text(attrs, "href")?;
    href.starts_with("/wiki/File:").then(|| unquote(&href["/wiki/".len()..]))
}

struct GallerySection<'a> {
    section_id: &'a str,
    in_section: bool,
    started: bool,
    titles: Vec<String>,
}

impl HtmlHandler for GallerySection<'_> {
    fn start_tag(&mut self, tag: &str, attrs: &[Attribute]) {
        if tag == "h2" {
            let h_id = attr_text(attrs, "id").unwrap_or_default();
            if !self.started && h_id == self.section_id {
                self.in_section = true;
                self.started = true;
                return;
            }
            if self.started && self.in_section && !h_id.is_empty() && h_id != self.section_id {
                self.in_section = false;
                return;
            }
        }
        if !self.in_section || tag != "a" {
            return;
        }
        if let Some(title) = file_title(attrs) {
            self.titles.push(title);
        }
    }

    fn end_tag(&mut self, _tag: &str) {}

    fn data(&mut self, _data: &str) {}
}

fn gallery_section_titles(html: &str, section_id: &str) -> Vec<String> {
    let mut parser = GallerySection {
        section_id,
        in_section: false,
        started: false,
        titles: Vec::new(),
    };
    pyhtml::feed(html, &mut parser);
    unique(parser.titles)
}

/// `parse_gallery_cg_file_titles`: the File titles of the `CGs` section.
pub fn parse_gallery_cg_file_titles(html: &str) -> Vec<String> {
    gallery_section_titles(html, "CGs")
}

/// `parse_gallery_images_file_titles`: the File titles of the `Images` section.
pub fn parse_gallery_images_file_titles(html: &str) -> Vec<String> {
    gallery_section_titles(html, "Images")
}

/// `parse_gallery_memory_snapshot_file_titles`: the `Images` titles naming a Memory Snapshot.
pub fn parse_gallery_memory_snapshot_file_titles(html: &str) -> Vec<String> {
    parse_gallery_images_file_titles(html)
        .into_iter()
        .filter(|title| title.to_lowercase().replace(' ', "_").contains("memory_snapshot"))
        .collect()
}

/// How a sprite subsection is chosen.
enum Subsection<'a> {
    /// `_SpritesSubsectionFilesParser`: the first `h3` with this id, until another id.
    Exact { id: &'a str, started: bool },
    /// `_SpritesSubsectionPrefixFilesParser`: every `h3` whose id starts with this
    /// (casefolded).
    Prefix(String),
}

struct SpritesSubsection<'a> {
    which: Subsection<'a>,
    in_sprites: bool,
    sprites_started: bool,
    in_subsection: bool,
    titles: Vec<String>,
}

impl SpritesSubsection<'_> {
    fn update_sprites_state(&mut self, h_id: &str) {
        if !self.sprites_started {
            if h_id == "Sprites" {
                self.in_sprites = true;
                self.sprites_started = true;
            }
            return;
        }
        if self.in_sprites && !h_id.is_empty() && h_id != "Sprites" {
            self.in_sprites = false;
            self.in_subsection = false;
        }
    }

    fn update_subsection_state(&mut self, h_id: &str) {
        match &mut self.which {
            Subsection::Exact { id, started } => {
                if !*started {
                    if h_id == *id {
                        self.in_subsection = true;
                        *started = true;
                    }
                    return;
                }
                if self.in_subsection && !h_id.is_empty() && h_id != *id {
                    self.in_subsection = false;
                }
            }
            Subsection::Prefix(prefix) => {
                self.in_subsection = !h_id.is_empty() && casefold(h_id).starts_with(prefix.as_str());
            }
        }
    }
}

impl HtmlHandler for SpritesSubsection<'_> {
    fn start_tag(&mut self, tag: &str, attrs: &[Attribute]) {
        let h_id = || attr_text(attrs, "id").unwrap_or_default();
        if tag == "h2" {
            self.update_sprites_state(h_id());
        }
        if !self.in_sprites {
            return;
        }
        if tag == "h3" {
            self.update_subsection_state(h_id());
        }
        if !self.in_subsection {
            return;
        }
        if tag == "a"
            && let Some(title) = file_title(attrs)
        {
            self.titles.push(title);
        }
    }

    fn end_tag(&mut self, _tag: &str) {}

    fn data(&mut self, _data: &str) {}
}

fn sprites_titles(html: &str, which: Subsection<'_>) -> Vec<String> {
    let mut parser = SpritesSubsection {
        which,
        in_sprites: false,
        sprites_started: false,
        in_subsection: false,
        titles: Vec::new(),
    };
    pyhtml::feed(html, &mut parser);
    unique(parser.titles)
}

/// `parse_gallery_awakened_file_titles`: `Sprites` > `Awakened`.
pub fn parse_gallery_awakened_file_titles(html: &str) -> Vec<String> {
    sprites_titles(
        html,
        Subsection::Exact {
            id: "Awakened",
            started: false,
        },
    )
}

/// `parse_gallery_skin_file_titles`: `Sprites` > every `Skin*` subsection.
pub fn parse_gallery_skin_file_titles(html: &str) -> Vec<String> {
    sprites_titles(html, Subsection::Prefix(casefold("Skin")))
}

struct TableFiles<'a> {
    table_id: &'a str,
    in_table: bool,
    titles: Vec<String>,
}

impl HtmlHandler for TableFiles<'_> {
    fn start_tag(&mut self, tag: &str, attrs: &[Attribute]) {
        if tag == "table" && attr_text(attrs, "id") == Some(self.table_id) {
            self.in_table = true;
            return;
        }
        if !self.in_table || tag != "a" {
            return;
        }
        if let Some(title) = file_title(attrs) {
            self.titles.push(title);
        }
    }

    fn end_tag(&mut self, tag: &str) {
        if tag == "table" && self.in_table {
            self.in_table = false;
        }
    }

    fn data(&mut self, _data: &str) {}
}

/// `parse_list_of_discs_page_file_titles`: the `File:Disc_*` links of `#disc-table`, in
/// order, repeats kept.
pub fn parse_list_of_discs_page_file_titles(html: &str) -> Result<Vec<String>, UrlError> {
    let mut parser = TableFiles {
        table_id: "disc-table",
        in_table: false,
        titles: Vec::new(),
    };
    pyhtml::feed(html, &mut parser);
    let mut out = Vec::new();
    for title in parser.titles {
        if casefold(&normalize_wiki_file_title(&title)?).starts_with("file:disc_") {
            out.push(title);
        }
    }
    Ok(out)
}

/// All target titles of one gallery page, in Python's order: CGs, Memory Snapshots,
/// Awakened, Skins.
pub fn gallery_target_titles(html: &str) -> Vec<String> {
    let mut titles = parse_gallery_cg_file_titles(html);
    titles.extend(parse_gallery_memory_snapshot_file_titles(html));
    titles.extend(parse_gallery_awakened_file_titles(html));
    titles.extend(parse_gallery_skin_file_titles(html));
    titles
}

// ---------- MediaWiki API payload ----------

/// The Python type name `type(value).__name__` would print, for error messages.
fn py_type_name(value: &Value) -> &'static str {
    match value {
        Value::Null => "NoneType",
        Value::Bool(_) => "bool",
        Value::Int(_) => "int",
        Value::Float(_) => "float",
        Value::Str(_) => "str",
        Value::List(_) => "list",
        Value::Dict(_) => "dict",
    }
}

fn attribute_error(value: &Value) -> JobError {
    JobError::new("AttributeError", format!("'{}' object has no attribute 'get'", py_type_name(value)))
}

/// `value.get(key, default)` on something that must be a dict.
fn py_get<'a>(value: &'a Value, key: &str) -> Result<Option<&'a Value>, JobError> {
    match value {
        Value::Dict(dict) => Ok(dict.get(key)),
        other => Err(attribute_error(other)),
    }
}

/// `for item in value`: what iterating a JSON value yields in Python.
fn py_iter(value: &Value) -> Result<Vec<Value>, JobError> {
    match value {
        Value::List(items) => Ok(items.clone()),
        Value::Dict(dict) => Ok(dict.iter().map(|(key, _)| Value::Str(fav_runtime::pystr::str_key(key))).collect()),
        Value::Str(text) => Ok(text.chars().map(|ch| Value::Str(ch.to_string())).collect()),
        other => Err(JobError::new(
            "TypeError",
            format!("'{}' object is not iterable", py_type_name(other)),
        )),
    }
}

/// `value[0]` for the truthy `imageinfo` value.
fn py_first(value: &Value) -> Result<Value, JobError> {
    match value {
        Value::List(items) => Ok(items.first().cloned().unwrap_or(Value::Null)),
        Value::Str(text) => Ok(Value::Str(text.chars().next().map(String::from).unwrap_or_default())),
        // `imageinfo[0]` looks up the int 0, which no JSON object key equals.
        Value::Dict(_) => Err(JobError::new("KeyError", "0")),
        other => Err(JobError::new(
            "TypeError",
            format!("'{}' object is not subscriptable", py_type_name(other)),
        )),
    }
}

/// The pages of one `action=query&prop=imageinfo` answer as `WikiFile`s.
pub fn parse_imageinfo_payload(payload: &Value) -> Result<Vec<WikiFile>, JobError> {
    let empty_dict = Value::Dict(Dict::new());
    let empty_list = Value::List(Vec::new());
    let query = py_get(payload, "query")?.unwrap_or(&empty_dict);
    let pages = py_get(query, "pages")?.unwrap_or(&empty_list);
    let mut results = Vec::new();
    for page in py_iter(pages)? {
        let title = py_get(&page, "title")?
            .filter(|value| value.is_truthy())
            .map(str_value)
            .unwrap_or_default();
        if title.is_empty() {
            continue;
        }
        let imageinfo = py_get(&page, "imageinfo")?.filter(|value| value.is_truthy());
        let Some(imageinfo) = imageinfo else {
            log().debug(format_args!("No imageinfo for {title}"));
            continue;
        };
        let first = py_first(imageinfo)?;
        let info = if first.is_truthy() { first } else { empty_dict.clone() };
        let Some(url) = py_get(&info, "url")?.filter(|value| value.is_truthy()) else {
            continue;
        };
        let description_url = py_get(&info, "descriptionurl")?.filter(|value| value.is_truthy()).map(str_value);
        results.push(WikiFile {
            title,
            url: str_value(url),
            description_url,
        });
    }
    Ok(results)
}

// ---------- the crawler ----------

/// Seconds to wait after failed download attempt `attempt`: `min(10, 2 ** attempt)`.
fn download_backoff_seconds(attempt: u32) -> f64 {
    f64::from(10_u32.min(2_u32.saturating_pow(attempt)))
}

/// `StellaSora`: one run's client, settings and notification seam.
pub struct StellaSora {
    client: reqwest::Client,
    base_url: String,
    /// `cfg.path` as `str(Path)`.
    path: String,
    notifier: SharedNotifier,
    /// `asyncio.sleep`: the download backoff goes through it.
    sleeper: Sleeper,
}

impl std::fmt::Debug for StellaSora {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StellaSora")
            .field("base_url", &self.base_url)
            .field("path", &self.path)
            .finish_non_exhaustive()
    }
}

/// `httpx.AsyncClient(timeout=60, follow_redirects=True, limits=10, User-Agent)`.
pub fn build_client() -> Result<reqwest::Client, JobError> {
    fav_net::client_builder()
        .connect_timeout(Duration::from_mins(1))
        .read_timeout(Duration::from_mins(1))
        .redirect(reqwest::redirect::Policy::limited(20))
        .pool_max_idle_per_host(10)
        .user_agent(USER_AGENT)
        .build()
        .map_err(|error| JobError::new("RuntimeError", format!("cannot build the HTTP client: {error}")))
}

impl StellaSora {
    pub fn new(client: reqwest::Client, base_url: &str, path: &str, notifier: SharedNotifier) -> Self {
        Self {
            client,
            base_url: base_url.trim_end_matches('/').to_owned(),
            path: path.to_owned(),
            notifier,
            sleeper: tokio_sleeper(),
        }
    }

    /// Sleep through `sleeper` (tests record the waits instead of taking them).
    #[must_use]
    pub fn with_sleeper(mut self, sleeper: Sleeper) -> Self {
        self.sleeper = sleeper;
        self
    }

    /// `client.get(url)` with `base_url` set: httpx appends a URL without a scheme and host
    /// to the base path, its leading slashes dropped.
    fn absolute(&self, url: &str) -> String {
        match urlsplit(url, "") {
            Ok(parts) if !parts.scheme.is_empty() && !parts.netloc.is_empty() => url.to_owned(),
            Ok(parts) => {
                let query = if parts.query.is_empty() {
                    String::new()
                } else {
                    format!("?{}", parts.query)
                };
                format!("{}/{}{query}", self.base_url, parts.path.trim_start_matches('/'))
            }
            Err(_) => url.to_owned(),
        }
    }

    async fn get_html(&self, url: &str) -> Result<String, HttpError> {
        let response = self
            .client
            .get(self.absolute(url))
            .send()
            .await
            .map_err(|error| HttpError::transport(&error))?;
        fav_net::httpx::raise_for_status(&response)?;
        response.text().await.map_err(|error| HttpError::transport(&error))
    }

    pub async fn list_characters(&self) -> Result<Vec<CharacterPage>, JobError> {
        let html = self.get_html(CHARACTERS_PATH).await.map_err(HttpError::into_job_error)?;
        let characters = parse_characters_page(&html, &self.base_url).map_err(|error| value_error(&error))?;
        log().info(format_args!("Found {} characters", characters.len()));
        Ok(characters)
    }

    pub async fn list_disc_image_files(&self) -> Result<Vec<String>, JobError> {
        let html = self.get_html(LIST_OF_DISCS_PATH).await.map_err(HttpError::into_job_error)?;
        parse_list_of_discs_page_file_titles(&html).map_err(|error| value_error(&error))
    }

    async fn list_character_target_files(&self, character: &CharacterPage) -> Result<Vec<String>, HttpError> {
        let html = self.get_html(&format!("{}/gallery", character.url)).await?;
        Ok(gallery_target_titles(&html))
    }

    /// `_list_character_target_files_map`: every character's gallery, five at a time, in
    /// page order; a gallery that fails over HTTP counts as empty.
    async fn character_target_files(&self) -> Result<Vec<(String, Vec<String>)>, JobError> {
        let characters = self.list_characters().await?;
        // Owned items keep the future free of a borrow the `Send` check cannot name.
        let per_character: Vec<Vec<String>> = futures_util::stream::iter(characters.clone())
            .map(|character| async move {
                match self.list_character_target_files(&character).await {
                    Ok(titles) => titles,
                    Err(error) => {
                        log().warning(format_args!("Failed to fetch gallery for {}: {error}", character.name));
                        Vec::new()
                    }
                }
            })
            .buffered(GALLERY_CONCURRENCY)
            .collect()
            .await;
        Ok(characters.into_iter().map(|character| character.name).zip(per_character).collect())
    }

    /// `build_target_destination_map`.
    pub async fn build_target_destination_map(&self) -> Result<DestinationMap, JobError> {
        let disc_files = self.list_disc_image_files().await?;
        let character_map = self.character_target_files().await?;
        build_download_destination_map(&self.path, &disc_files, &character_map).map_err(|error| value_error(&error))
    }

    /// `resolve_files`: titles to image URLs through the API, 50 per request.
    pub async fn resolve_files(&self, file_titles: &[String]) -> Result<Vec<WikiFile>, JobError> {
        let mut results = Vec::new();
        for batch in file_titles.chunks(RESOLVE_BATCH) {
            let titles = batch.join("|");
            let params = [
                ("action", "query"),
                ("format", "json"),
                ("formatversion", "2"),
                ("prop", "imageinfo"),
                ("iiprop", "url"),
                ("titles", titles.as_str()),
                ("redirects", "1"),
            ];
            let response = self
                .client
                .get(self.absolute(API_PATH))
                .query(&params)
                .send()
                .await
                .map_err(|error| HttpError::transport(&error).into_job_error())?;
            fav_net::httpx::raise_for_status(&response).map_err(HttpError::into_job_error)?;
            let text = response
                .text()
                .await
                .map_err(|error| HttpError::transport(&error).into_job_error())?;
            let payload = pyjson::loads(&text).map_err(|error| JobError::new("JSONDecodeError", error.python_message(&text)))?;
            results.extend(parse_imageinfo_payload(&payload)?);
        }
        Ok(results)
    }

    /// `_download_file`: stream to `<name>.part`, then rename; three attempts.
    async fn download_file(&self, url: &str, dst_path: &str) -> Result<(), JobError> {
        let dst = Path::new(dst_path);
        if let Some(parent) = dst.parent().filter(|parent| !parent.as_os_str().is_empty()) {
            tokio::fs::create_dir_all(parent).await.map_err(|error| {
                JobError::new(
                    fav_net::download::io_class_name(&error),
                    fav_net::download::py_os_error_text(&error),
                )
            })?;
        }
        let part = format!("{dst_path}.part");
        let name = pypath::name(dst_path);
        for attempt in 1..=DOWNLOAD_ATTEMPTS {
            if Path::new(&part).exists() {
                tokio::fs::remove_file(&part).await.map_err(|error| {
                    JobError::new(
                        fav_net::download::io_class_name(&error),
                        fav_net::download::py_os_error_text(&error),
                    )
                })?;
            }
            let outcome: Result<(), DownloadError> = async {
                let response = self
                    .client
                    .get(self.absolute(url))
                    .send()
                    .await
                    .map_err(|error| DownloadError::Http(HttpError::transport(&error)))?;
                fav_net::httpx::raise_for_status(&response).map_err(DownloadError::Http)?;
                fav_net::httpx::stream_body_to(response, Path::new(&part)).await
            }
            .await;
            match outcome {
                Ok(()) => {
                    return tokio::fs::rename(&part, dst_path).await.map_err(|error| {
                        JobError::new(
                            fav_net::download::io_class_name(&error),
                            fav_net::download::py_os_error_text(&error),
                        )
                    });
                }
                Err(error) => {
                    log().warning(format_args!("Download failed ({attempt}/{DOWNLOAD_ATTEMPTS}) {name}: {error}"));
                    (self.sleeper)(download_backoff_seconds(attempt)).await;
                }
            }
        }
        Err(JobError::new(
            "RuntimeError",
            format!("Failed to download after {DOWNLOAD_ATTEMPTS} attempts: {name}"),
        ))
    }

    /// `_notify_relpath`: the saved path relative to the collection root, else as is.
    fn notify_relpath(&self, saved_path: &str) -> String {
        let resolve = |path: &str| std::fs::canonicalize(path).or_else(|_| std::path::absolute(path));
        if let (Ok(saved), Ok(base)) = (resolve(saved_path), resolve(&self.path))
            && let Ok(relative) = saved.strip_prefix(&base)
        {
            let relative = relative.to_string_lossy().into_owned();
            return if relative.is_empty() { ".".to_owned() } else { relative };
        }
        saved_path.to_owned()
    }

    /// `_notify_download`: one `download_completed` notification; a failure is logged.
    pub async fn notify_download(&self, title: &str, image_url: &str, saved_path: &str, link_url: &str) {
        let mut payload = Dict::new();
        payload.insert("saved_path", Value::from(saved_path));
        payload.insert("image_path", Value::from(saved_path));
        let notification = NewNotification {
            kind: "download_completed".to_owned(),
            source: JOB_KEY.to_owned(),
            header: "Stella Sora".to_owned(),
            title: title.strip_prefix("File:").unwrap_or(title).to_owned(),
            body: self.notify_relpath(saved_path),
            link_url: link_url.to_owned(),
            image_url: image_url.to_owned(),
            payload: Some(payload),
            ..NewNotification::default()
        };
        if let Err(error) = self.notifier.enqueue(notification).await {
            log().warning(format_args!(
                "Failed to enqueue stellasora download notification for {title}: {error}"
            ));
        }
    }

    fn disc_dir(&self) -> String {
        pypath::join(&self.path, "disc")
    }

    /// `_handle_existing_file`: `(needs_download, moved, skipped)`.
    pub async fn handle_existing_file(&self, title: &str, dst_dir: &str, index: &mut ExistingIndex) -> Result<(bool, u64, u64), JobError> {
        let local_name = safe_local_filename_from_wiki_title(title).map_err(|error| value_error(&error))?;
        let dst_path = pypath::join(dst_dir, &local_name);
        if Path::new(&dst_path).exists() {
            return Ok((false, 0, 1));
        }
        let key = casefold(&local_name);
        let existing = index.paths(&key);
        if existing.is_empty() {
            return Ok((true, 0, 0));
        }
        let disc_dir = self.disc_dir();
        if dst_dir != disc_dir
            && let Some(source) = existing.iter().find(|path| parent(path) == disc_dir)
        {
            let (from, dir, dst) = (source.clone(), dst_dir.to_owned(), dst_path.clone());
            let io_error = |error: std::io::Error| {
                JobError::new(
                    fav_net::download::io_class_name(&error),
                    fav_net::download::py_os_error_text(&error),
                )
            };
            // A move across file systems copies the whole file: off the async threads.
            tokio::task::spawn_blocking(move || {
                std::fs::create_dir_all(&dir)?;
                fav_net::download::move_file(Path::new(&from), Path::new(&dst))
            })
            .await
            .map_err(|error| JobError::new("RuntimeError", error.to_string()))?
            .map_err(io_error)?;
            // The file is no longer at its old location: the index follows it there. Python
            // keeps the old path too, so a later title with the same case-folded name going to
            // another folder tries to move it again and fails the run (D53).
            index.replace(&key, source, dst_path);
            return Ok((false, 1, 0));
        }
        Ok((false, 0, 1))
    }

    /// `download_targets`: the whole run.
    pub async fn download_targets(&self) -> Result<(), JobError> {
        let root = Path::new(&self.path);
        if !root.exists() {
            log().warning(format_args!(
                "Stellasora path does not exist: {} (skipping; create the directory to enable downloads)",
                self.path
            ));
            return Ok(());
        }
        if !root.is_dir() {
            log().warning(format_args!("Stellasora path is not a directory: {} (skipping)", self.path));
            return Ok(());
        }

        let title_to_dir = self.build_target_destination_map().await?;
        // The walk covers the whole collection tree: off the async threads.
        let root = self.path.clone();
        let mut index = tokio::task::spawn_blocking(move || ExistingIndex::scan(&root))
            .await
            .map_err(|error| JobError::new("RuntimeError", error.to_string()))?;
        let mut stats = DownloadStats::default();

        let mut to_download = Vec::new();
        for (title, dst_dir) in title_to_dir.iter() {
            let (needs_download, moved, skipped) = self.handle_existing_file(title, dst_dir, &mut index).await?;
            stats.moved += moved;
            stats.skipped += skipped;
            if needs_download {
                to_download.push(title.to_owned());
            }
        }

        let resolved = self.resolve_files(&to_download).await?;
        // API titles normalized; only the ones that map to a destination are kept, the
        // last answer for a title winning but keeping the first one's place.
        let mut resolved_by_key: Vec<(String, WikiFile)> = Vec::new();
        for item in resolved {
            let key = normalize_wiki_file_title(&item.title).map_err(|error| value_error(&error))?;
            if key.is_empty() || title_to_dir.get(&key).is_none() {
                continue;
            }
            match resolved_by_key.iter_mut().find(|(existing, _)| *existing == key) {
                Some(entry) => entry.1 = item,
                None => resolved_by_key.push((key, item)),
            }
        }
        let missing = to_download
            .iter()
            .filter(|title| !resolved_by_key.iter().any(|(key, _)| key == *title))
            .count();
        if missing > 0 {
            log().warning(format_args!("Missing {missing} files from API resolve"));
        }

        self.download_resolved_files(&resolved_by_key, &title_to_dir, &mut index, &mut stats)
            .await?;

        log().info(format_args!(
            "Download summary: targets={} resolved={} to_download={} downloaded={} moved={} skipped={}",
            title_to_dir.len(),
            resolved_by_key.len(),
            to_download.len(),
            stats.downloaded,
            stats.moved,
            stats.skipped
        ));
        Ok(())
    }

    /// `_download_resolved_files`.
    pub async fn download_resolved_files(
        &self,
        resolved_by_key: &[(String, WikiFile)],
        title_to_dir: &DestinationMap,
        index: &mut ExistingIndex,
        stats: &mut DownloadStats,
    ) -> Result<(), JobError> {
        for (title, item) in resolved_by_key {
            let dst_dir = title_to_dir
                .get(title)
                .ok_or_else(|| JobError::new("KeyError", fav_runtime::pystr::repr_str(title)))?
                .to_owned();
            let local_name = safe_local_filename_from_wiki_title(title).map_err(|error| value_error(&error))?;
            let dst_path = pypath::join(&dst_dir, &local_name);
            let (needs_download, moved, skipped) = self.handle_existing_file(title, &dst_dir, index).await?;
            stats.moved += moved;
            stats.skipped += skipped;
            if !needs_download {
                continue;
            }
            self.download_file(&item.url, &dst_path).await?;
            stats.downloaded += 1;
            index.push(casefold(&local_name), dst_path.clone());
            self.notify_download(title, &item.url, &dst_path, item.description_url.as_deref().unwrap_or_default())
                .await;
        }
        Ok(())
    }
}

fn parent(path: &str) -> String {
    match path.rfind('/') {
        Some(0) => "/".to_owned(),
        Some(slash) => path[..slash].to_owned(),
        None => ".".to_owned(),
    }
}

/// `_index_existing_files`: every file under the collection root by casefolded name,
/// `.part` files left out.
#[derive(Debug, Default)]
pub struct ExistingIndex {
    by_name: HashMap<String, Vec<String>>,
}

impl ExistingIndex {
    /// `self.path.rglob('*')` filtered to files, in pathlib's walk order: symlinked
    /// directories are not entered, symlinks to files count as files.
    pub fn scan(root: &str) -> Self {
        let mut index = Self::default();
        if !Path::new(root).exists() {
            return index;
        }
        let mut directories = vec![root.to_owned()];
        let mut walk = vec![root.to_owned()];
        // `_iterate_directories`: the root, then every directory `Path.walk()` reports, in
        // its order (each directory's subdirectories as a group, depth first).
        while let Some(dir) = walk.pop() {
            let mut subdirs = Vec::new();
            if let Ok(entries) = std::fs::read_dir(&dir) {
                for entry in entries.flatten() {
                    let is_dir = entry.file_type().is_ok_and(|kind| kind.is_dir());
                    if is_dir {
                        subdirs.push(pypath::join(&dir, &entry.file_name().to_string_lossy()));
                    }
                }
            }
            directories.extend(subdirs.iter().cloned());
            walk.extend(subdirs.into_iter().rev());
        }
        for dir in directories {
            let Ok(entries) = std::fs::read_dir(&dir) else { continue };
            for entry in entries.flatten() {
                let name = entry.file_name().to_string_lossy().into_owned();
                let path = pypath::join(&dir, &name);
                #[allow(clippy::case_sensitive_file_extension_comparisons)] // Python's `endswith(".part")` is case-sensitive.
                let partial = name.ends_with(".part");
                if !Path::new(&path).is_file() || partial {
                    continue;
                }
                index.push(casefold(&name), path);
            }
        }
        let duplicates = index.by_name.values().filter(|paths| paths.len() > 1).count();
        if duplicates > 0 {
            log().warning(format_args!(
                "Found {duplicates} duplicate filenames under {root} (same name in multiple locations)"
            ));
        }
        index
    }

    fn paths(&self, key: &str) -> Vec<String> {
        self.by_name.get(key).cloned().unwrap_or_default()
    }

    fn push(&mut self, key: String, path: String) {
        self.by_name.entry(key).or_default().push(path);
    }

    /// `old`, a file just moved to `new`, is listed under `key` as `new` (D53).
    fn replace(&mut self, key: &str, old: &str, new: String) {
        let paths = self.by_name.entry(key.to_owned()).or_default();
        paths.retain(|path| path != old);
        paths.push(new);
    }
}

// ---------- the job ----------

/// The `stellasora` job: settings and client are read when the run starts.
struct StellaSoraJob {
    ctx: SourceContext,
}

impl Source for StellaSoraJob {
    fn update(&mut self) -> BoxFuture<'_, Result<(), JobError>> {
        Box::pin(async move {
            let settings = self
                .ctx
                .settings
                .load()
                .await
                .map_err(|error| JobError::new("RuntimeError", error.to_string()))?;
            let notifier: SharedNotifier = Arc::new(OutboxNotifier(Outbox::new(self.ctx.db.clone(), Arc::clone(&self.ctx.settings))));
            let source = StellaSora::new(build_client()?, BASE_URL, settings.web.stellasora.path.as_str(), notifier);
            source.download_targets().await
        })
    }
}

/// Register the `stellasora` job.
pub fn register(registry: &mut Registry, ctx: &SourceContext) {
    let ctx = ctx.clone();
    registry.per_run(JOB_KEY, move || Box::new(StellaSoraJob { ctx: ctx.clone() }) as Box<dyn Source>);
}

#[cfg(test)]
mod tests;
