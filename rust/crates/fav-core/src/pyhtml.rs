//! Python's `html.unescape` and the tokenizer of `html.parser.HTMLParser`.
//!
//! Python origin: `html/__init__.py` and `html/parser.py` of `CPython` 3.12 (the version
//! production runs, 3.12.13 when this was written; the 2025 security releases changed how
//! comments, `</` and raw-text elements are tokenized, and this follows the new rules).
//! Sources that parse pages with an `HTMLParser` subclass (Stella Sora) or unescape text
//! with `html.unescape` (Zhihu, Hanime1, WeChat, NGA) go through here, so the tags, attribute
//! values and text they see are the ones Python saw. The `pyhtml` goldens replay a corpus of
//! awkward markup through both.
//!
//! Only what fav's parsers use is reproduced: `convert_charrefs=True`, `scripting=False`,
//! and `feed()` without `close()` (none of the Python parsers calls `close()`, so a construct
//! left open at the end of the document is never reported, exactly as in Python).

use std::borrow::Cow;

mod entities;

use crate::pytext::py_strip;

// ---------- html.unescape ----------

/// `html.unescape(s)`: every named and numeric character reference replaced, by HTML5's
/// rules for valid and invalid ones alike.
pub fn unescape(text: &str) -> Cow<'_, str> {
    if !text.contains('&') {
        return Cow::Borrowed(text);
    }
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(amp) = rest.find('&') {
        out.push_str(&rest[..amp]);
        let after = &rest[amp + 1..];
        if let Some(len) = charref_len(after) {
            replace_charref(&after[..len], &mut out);
            rest = &after[len..];
        } else {
            out.push('&');
            rest = after;
        }
    }
    out.push_str(rest);
    Cow::Owned(out)
}

/// The length of what `&(#[0-9]+;?|#[xX][0-9a-fA-F]+;?|[^\t\n\f <&#;]{1,32};?)` matches
/// after the `&`, if it does.
fn charref_len(after: &str) -> Option<usize> {
    let bytes = after.as_bytes();
    let with_semicolon = |end: usize| if bytes.get(end) == Some(&b';') { end + 1 } else { end };
    if bytes.first() == Some(&b'#') {
        let digits = bytes[1..].iter().take_while(|b| b.is_ascii_digit()).count();
        if digits > 0 {
            return Some(with_semicolon(1 + digits));
        }
        if matches!(bytes.get(1), Some(b'x' | b'X')) {
            let hex = bytes[2..].iter().take_while(|b| b.is_ascii_hexdigit()).count();
            if hex > 0 {
                return Some(with_semicolon(2 + hex));
            }
        }
        // `#` is excluded from the named alternative.
        return None;
    }
    let mut end = 0;
    for (count, ch) in after.chars().enumerate() {
        if count == 32 || matches!(ch, '\t' | '\n' | '\x0c' | ' ' | '<' | '&' | '#' | ';') {
            break;
        }
        end += ch.len_utf8();
    }
    if end == 0 { None } else { Some(with_semicolon(end)) }
}

/// `_replace_charref`: what one matched reference (without its `&`) becomes.
fn replace_charref(reference: &str, out: &mut String) {
    if let Some(number) = reference.strip_prefix('#') {
        let (digits, radix) = match number.strip_prefix(['x', 'X']) {
            Some(hex) => (hex, 16),
            None => (number, 10),
        };
        let digits = digits.trim_end_matches(';');
        // Python's int is unbounded; anything past the last code point behaves the same.
        let value = digits
            .chars()
            .try_fold(0_u32, |acc, ch| acc.checked_mul(radix)?.checked_add(ch.to_digit(radix)?))
            .unwrap_or(u32::MAX);
        if let Some(replacement) = invalid_charref(value) {
            out.push_str(replacement);
        } else if (0xD800..=0xDFFF).contains(&value) || value > 0x0010_FFFF {
            out.push('\u{fffd}');
        } else if is_invalid_codepoint(value) {
            // Dropped.
        } else if let Some(ch) = char::from_u32(value) {
            out.push(ch);
        }
        return;
    }
    if let Some(value) = entity(reference) {
        out.push_str(value);
        return;
    }
    // The longest prefix (at least two characters) that names an entity wins.
    let boundaries: Vec<usize> = reference.char_indices().map(|(index, _)| index).collect();
    for count in (2..boundaries.len()).rev() {
        let cut = boundaries[count];
        if let Some(value) = entity(&reference[..cut]) {
            out.push_str(value);
            out.push_str(&reference[cut..]);
            return;
        }
    }
    out.push('&');
    out.push_str(reference);
}

fn entity(name: &str) -> Option<&'static str> {
    entities::ENTITIES
        .binary_search_by(|(candidate, _)| candidate.as_bytes().cmp(name.as_bytes()))
        .ok()
        .map(|index| entities::ENTITIES[index].1)
}

/// `_invalid_charrefs`: the C1 code points browsers map to Windows-1252, NUL and CR.
fn invalid_charref(value: u32) -> Option<&'static str> {
    Some(match value {
        0x00 => "\u{fffd}",
        0x0d => "\r",
        0x80 => "\u{20ac}",
        0x81 => "\u{81}",
        0x82 => "\u{201a}",
        0x83 => "\u{192}",
        0x84 => "\u{201e}",
        0x85 => "\u{2026}",
        0x86 => "\u{2020}",
        0x87 => "\u{2021}",
        0x88 => "\u{2c6}",
        0x89 => "\u{2030}",
        0x8a => "\u{160}",
        0x8b => "\u{2039}",
        0x8c => "\u{152}",
        0x8d => "\u{8d}",
        0x8e => "\u{17d}",
        0x8f => "\u{8f}",
        0x90 => "\u{90}",
        0x91 => "\u{2018}",
        0x92 => "\u{2019}",
        0x93 => "\u{201c}",
        0x94 => "\u{201d}",
        0x95 => "\u{2022}",
        0x96 => "\u{2013}",
        0x97 => "\u{2014}",
        0x98 => "\u{2dc}",
        0x99 => "\u{2122}",
        0x9a => "\u{161}",
        0x9b => "\u{203a}",
        0x9c => "\u{153}",
        0x9d => "\u{9d}",
        0x9e => "\u{17e}",
        0x9f => "\u{178}",
        _ => return None,
    })
}

/// `_invalid_codepoints`: references to these produce nothing.
fn is_invalid_codepoint(value: u32) -> bool {
    matches!(value, 0x1..=0x8 | 0xb | 0xe..=0x1f | 0x7f..=0x9f | 0xfdd0..=0xfdef) || (value & 0xfffe == 0xfffe && value <= 0x0010_ffff)
}

// ---------- html.parser.HTMLParser ----------

/// One attribute as `HTMLParser` reports it: the lowercased name and the value, `None` for
/// a bare attribute, unescaped otherwise.
pub type Attribute = (String, Option<String>);

/// The callbacks of an `HTMLParser` subclass. Comments, declarations and processing
/// instructions are consumed but not reported: no fav parser overrides their handlers.
pub trait HtmlHandler {
    /// `handle_starttag(tag, attrs)`, with the tag name lowercased.
    fn start_tag(&mut self, tag: &str, attrs: &[Attribute]);

    /// `handle_endtag(tag)`, with the tag name lowercased.
    fn end_tag(&mut self, tag: &str);

    /// `handle_data(data)`, character references already converted.
    fn data(&mut self, data: &str);

    /// `handle_startendtag`: a self-closing `<tag/>`. Python's default calls both.
    fn start_end_tag(&mut self, tag: &str, attrs: &[Attribute]) {
        self.start_tag(tag, attrs);
        self.end_tag(tag);
    }
}

/// Elements whose content is raw text up to their end tag (`CDATA_CONTENT_ELEMENTS`).
const CDATA_CONTENT_ELEMENTS: [&str; 6] = ["script", "style", "xmp", "iframe", "noembed", "noframes"];
/// Elements whose content is text with character references (`RCDATA_CONTENT_ELEMENTS`).
const RCDATA_CONTENT_ELEMENTS: [&str; 2] = ["textarea", "title"];

/// `parser = Subclass(); parser.feed(html)`: tokenize `html` into the handler's callbacks.
///
/// Like a single `feed()` without `close()`, whatever is still incomplete at the end of
/// the text (an unterminated tag or comment, a raw-text element without its end tag, or
/// trailing text that could end in a cut-off character reference) is never reported.
pub fn feed(html: &str, handler: &mut impl HtmlHandler) {
    Tokenizer {
        raw: html,
        cdata_elem: None,
        escapable: true,
    }
    .go_ahead(handler);
}

struct Tokenizer<'a> {
    raw: &'a str,
    /// The raw-text or RCDATA element being read, lowercased.
    cdata_elem: Option<String>,
    escapable: bool,
}

/// `[\t\n\r\f ]`
fn is_html_space(ch: char) -> bool {
    matches!(ch, '\t' | '\n' | '\r' | '\x0c' | ' ')
}

impl Tokenizer<'_> {
    fn emit_data(&self, text: &str, handler: &mut impl HtmlHandler) {
        if self.escapable {
            handler.data(&unescape(text));
        } else {
            handler.data(text);
        }
    }

    /// `goahead(0)`.
    fn go_ahead(&mut self, handler: &mut impl HtmlHandler) {
        let raw = self.raw;
        let n = raw.len();
        let mut i = 0;
        while i < n {
            let j = if self.cdata_elem.is_none() {
                if let Some(offset) = raw[i..].find('<') {
                    i + offset
                } else {
                    // Text that may end in a reference cut in half waits for more input,
                    // which never comes.
                    let window = char_index_from_end(raw, 34).max(i);
                    if let Some(amp) = raw[window..].rfind('&').map(|offset| window + offset)
                        && !raw[amp..].contains(['\t', '\n', '\r', '\x0c', ' ', ';'])
                    {
                        break;
                    }
                    n
                }
            } else {
                match self.find_cdata_end(i) {
                    Some(j) => j,
                    None => break,
                }
            };
            if i < j {
                self.emit_data(&raw[i..j], handler);
            }
            i = j;
            if i == n {
                break;
            }
            // `interesting` only ever stops at `<` with convert_charrefs on.
            let rest = &raw[i..];
            let k = if starts_tag_open(rest) {
                self.parse_starttag(i, handler)
            } else if rest.starts_with("</") {
                self.parse_endtag(i, handler)
            } else if rest.starts_with("<!--") {
                parse_comment(raw, i)
            } else if rest.starts_with("<?") {
                find_from(raw, ">", i + 2).map(|j| j + 1)
            } else if rest.starts_with("<!") {
                parse_html_declaration(raw, i)
            } else if i + 1 < n {
                handler.data("<");
                Some(i + 1)
            } else {
                None
            };
            match k {
                Some(k) => i = k,
                None => break,
            }
        }
    }

    /// `interesting.search`: `</elem` (ASCII case-insensitive) followed by `[\t\n\r\f />]`.
    fn find_cdata_end(&self, from: usize) -> Option<usize> {
        let elem = self.cdata_elem.as_deref()?;
        if elem == "plaintext" {
            // `\Z`: everything to the end is text.
            return Some(self.raw.len());
        }
        let bytes = self.raw.as_bytes();
        let mut at = from;
        while let Some(offset) = self.raw[at..].find("</") {
            let start = at + offset;
            let name_end = start + 2 + elem.len();
            if bytes.len() > name_end
                && bytes[start + 2..name_end].eq_ignore_ascii_case(elem.as_bytes())
                && matches!(bytes[name_end], b'\t' | b'\n' | b'\r' | b'\x0c' | b' ' | b'/' | b'>')
            {
                return Some(start);
            }
            at = start + 2;
        }
        None
    }

    /// `parse_starttag(i)`: the index after the tag, or `None` while it is incomplete.
    fn parse_starttag(&mut self, i: usize, handler: &mut impl HtmlHandler) -> Option<usize> {
        let raw = self.raw;
        let endpos = locate_tag_end(raw, i + 1);
        if raw.as_bytes()[endpos - 1] != b'>' {
            return None;
        }
        let (name_end, mut k) = tag_find(raw, i + 1);
        let tag = raw[i + 1..name_end].to_lowercase();
        let mut attrs: Vec<Attribute> = Vec::new();
        while k < endpos {
            let Some(found) = attr_find(raw, k, AttrTail::Tolerant) else {
                break;
            };
            let name = raw[found.name.0..found.name.1].to_lowercase();
            let value = found.value.map(|(start, end)| {
                let mut value = &raw[start..end];
                if value.len() >= 2 && (value.starts_with('\'') && value.ends_with('\'') || value.starts_with('"') && value.ends_with('"'))
                {
                    value = &value[1..value.len() - 1];
                } else if value.len() == 1 && matches!(value, "'" | "\"") {
                    // `attrvalue[:1] == "'" == attrvalue[-1:]` holds for a lone quote too.
                    value = "";
                }
                unescape(value).into_owned()
            });
            attrs.push((name, value));
            k = found.end;
        }
        let end = py_strip(raw.get(k..endpos).unwrap_or(""));
        if end != ">" && end != "/>" {
            handler.data(&raw[i..endpos]);
            return Some(endpos);
        }
        if end.ends_with("/>") {
            handler.start_end_tag(&tag, &attrs);
        } else {
            handler.start_tag(&tag, &attrs);
            if CDATA_CONTENT_ELEMENTS.contains(&tag.as_str()) || tag == "plaintext" {
                self.cdata_elem = Some(tag);
                self.escapable = false;
            } else if RCDATA_CONTENT_ELEMENTS.contains(&tag.as_str()) {
                self.cdata_elem = Some(tag);
                self.escapable = true;
            }
        }
        Some(endpos)
    }

    /// `parse_endtag(i)`.
    fn parse_endtag(&mut self, i: usize, handler: &mut impl HtmlHandler) -> Option<usize> {
        let raw = self.raw;
        find_from(raw, ">", i + 2)?;
        if !raw.as_bytes().get(i + 2).is_some_and(u8::is_ascii_alphabetic) {
            // `</>` is ignored; anything else not starting with a letter is a bogus comment.
            if raw[i + 2..].starts_with('>') {
                return Some(i + 3);
            }
            return find_from(raw, ">", i + 2).map(|pos| pos + 1);
        }
        let j = locate_tag_end(raw, i + 2);
        if raw.as_bytes()[j - 1] != b'>' {
            return None;
        }
        let (name_end, _) = tag_find(raw, i + 2);
        handler.end_tag(&raw[i + 2..name_end].to_lowercase());
        self.cdata_elem = None;
        self.escapable = true;
        Some(j)
    }
}

/// `starttagopen.match`: `<` followed by an ASCII letter.
fn starts_tag_open(text: &str) -> bool {
    let bytes = text.as_bytes();
    bytes.len() >= 2 && bytes[0] == b'<' && bytes[1].is_ascii_alphabetic()
}

fn find_from(text: &str, needle: &str, from: usize) -> Option<usize> {
    text.get(from..)?.find(needle).map(|offset| from + offset)
}

/// The byte index of the character `count` characters before the end (`n - count` in
/// Python's character indexing), or 0.
fn char_index_from_end(text: &str, count: usize) -> usize {
    text.char_indices().rev().nth(count - 1).map_or(0, |(index, _)| index)
}

/// `parse_comment(i)`: `--!?>` searched after `<!--`, else `-?>` right after it.
fn parse_comment(raw: &str, i: usize) -> Option<usize> {
    let body = i + 4;
    let mut at = body;
    while let Some(dashes) = find_from(raw, "--", at) {
        let after = &raw[dashes + 2..];
        if after.starts_with('>') {
            return Some(dashes + 3);
        }
        if after.starts_with("!>") {
            return Some(dashes + 4);
        }
        at = dashes + 1;
    }
    let rest = &raw[body..];
    if rest.starts_with('>') {
        Some(body + 1)
    } else if rest.starts_with("->") {
        Some(body + 2)
    } else {
        None
    }
}

/// `parse_html_declaration(i)` for `<!` that is not a comment.
fn parse_html_declaration(raw: &str, i: usize) -> Option<usize> {
    let rest = &raw[i..];
    if rest.starts_with("<![CDATA[") {
        return find_from(raw, "]]>", i + 9).map(|j| j + 3);
    }
    if rest.len() >= 9 && rest.as_bytes()[..9].eq_ignore_ascii_case(b"<!doctype") {
        return find_from(raw, ">", i + 9).map(|j| j + 1);
    }
    if rest.starts_with("<![") {
        return find_from(raw, ">", i + 3).map(|j| j + 1);
    }
    // `parse_bogus_comment`.
    find_from(raw, ">", i + 2).map(|pos| pos + 1)
}

/// `tagfind_tolerant.match(raw, at)`: `([a-zA-Z][^\t\n\r\f />]*)(?:[\t\n\r\f ]|/(?!>))*`.
/// Returns the end of the name and the end of the match.
fn tag_find(raw: &str, at: usize) -> (usize, usize) {
    let name_end = at
        + raw[at..]
            .find(|ch: char| is_html_space(ch) || ch == '/' || ch == '>')
            .unwrap_or(raw.len() - at);
    let mut end = name_end;
    let bytes = raw.as_bytes();
    while end < bytes.len() {
        match bytes[end] {
            b'\t' | b'\n' | b'\r' | b'\x0c' | b' ' => end += 1,
            b'/' if bytes.get(end + 1) != Some(&b'>') => end += 1,
            _ => break,
        }
    }
    (name_end, end)
}

/// What follows an attribute: `(?:[\t\n\r\f ]|/(?!>))*` in `attrfind_tolerant`,
/// `[\t\n\r\f /]*` in `locatetagend`.
#[derive(Clone, Copy, PartialEq, Eq)]
enum AttrTail {
    Tolerant,
    Locate,
}

struct AttrMatch {
    name: (usize, usize),
    /// Group 3: the raw value, quotes included; `None` when there is no `=` part.
    value: Option<(usize, usize)>,
    end: usize,
}

/// One attribute of `attrfind_tolerant` / `locatetagend`, matched at `at`:
///
/// ```text
/// (?<=['"\t\n\r\f /])[^\t\n\r\f />][^\t\n\r\f /=>]*
/// ([\t\n\r\f ]*=[\t\n\r\f ]*('[^']*'|"[^"]*"|(?!['"])[^>\t\n\r\f ]*))?
/// <tail>
/// ```
///
/// including the backtracking Python's engine does when no value alternative matches
/// after the spaces following `=`: it gives back one space and matches an empty bare value.
fn attr_find(raw: &str, at: usize, tail: AttrTail) -> Option<AttrMatch> {
    let previous = raw[..at].chars().next_back()?;
    if !matches!(previous, '\'' | '"' | '\t' | '\n' | '\r' | '\x0c' | ' ' | '/') {
        return None;
    }
    let first = raw[at..].chars().next()?;
    if is_html_space(first) || first == '/' || first == '>' {
        return None;
    }
    let after_first = at + first.len_utf8();
    let name_end = after_first
        + raw[after_first..]
            .find(|ch: char| is_html_space(ch) || matches!(ch, '/' | '=' | '>'))
            .unwrap_or(raw.len() - after_first);
    let bytes = raw.as_bytes();
    let skip_spaces = |mut pos: usize| {
        while pos < bytes.len() && matches!(bytes[pos], b'\t' | b'\n' | b'\r' | b'\x0c' | b' ') {
            pos += 1;
        }
        pos
    };
    let mut value = None;
    let mut end = name_end;
    let before_equals = skip_spaces(name_end);
    if bytes.get(before_equals) == Some(&b'=') {
        let value_start = skip_spaces(before_equals + 1);
        let quoted = |quote: u8| -> Option<usize> {
            if bytes.get(value_start) == Some(&quote) {
                find_from(raw, if quote == b'\'' { "'" } else { "\"" }, value_start + 1).map(|close| close + 1)
            } else {
                None
            }
        };
        if let Some(value_end) = quoted(b'\'').or_else(|| quoted(b'"')) {
            value = Some((value_start, value_end));
            end = value_end;
        } else if !matches!(bytes.get(value_start), Some(b'\'' | b'"')) {
            let value_end = value_start
                + raw[value_start..]
                    .find(|ch: char| is_html_space(ch) || ch == '>')
                    .unwrap_or(raw.len() - value_start);
            value = Some((value_start, value_end));
            end = value_end;
        } else if value_start > before_equals + 1 {
            // An unclosed quote after spaces: one space is given back and the bare value
            // alternative matches nothing in front of it.
            value = Some((value_start - 1, value_start - 1));
            end = value_start - 1;
        }
    }
    while end < bytes.len() {
        match (bytes[end], tail) {
            (b'\t' | b'\n' | b'\r' | b'\x0c' | b' ', _) | (b'/', AttrTail::Locate) => end += 1,
            (b'/', AttrTail::Tolerant) if bytes.get(end + 1) != Some(&b'>') => end += 1,
            _ => break,
        }
    }
    Some(AttrMatch {
        name: (at, name_end),
        value,
        end,
    })
}

/// `locatetagend.match(raw, at).end()`: the tag name, attributes as `attr_find` reads them
/// and an optional `>`.
fn locate_tag_end(raw: &str, at: usize) -> usize {
    let bytes = raw.as_bytes();
    let mut end = at
        + raw[at..]
            .find(|ch: char| is_html_space(ch) || ch == '/' || ch == '>')
            .unwrap_or(raw.len() - at);
    while end < bytes.len() && matches!(bytes[end], b'\t' | b'\n' | b'\r' | b'\x0c' | b' ' | b'/') {
        end += 1;
    }
    while let Some(found) = attr_find(raw, end, AttrTail::Locate) {
        end = found.end;
    }
    if bytes.get(end) == Some(&b'>') { end + 1 } else { end }
}

#[cfg(test)]
mod tests;
