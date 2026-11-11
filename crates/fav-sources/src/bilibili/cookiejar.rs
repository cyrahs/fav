//! The cookies a response's `Set-Cookie` headers set, as `http.cookiejar` reads them.
//!
//! Python origin: `bilibili_session._refresh` takes the renewed `SESSDATA`/`bili_jct` from
//! `response.cookies.jar`, the jar httpx fills through `CookieJar.extract_cookies` with
//! the default policy. This is that path for Netscape (`Set-Cookie`) headers: the ad hoc
//! attribute parser, the date parser, the normalisation and defaults, the policy's
//! `set_ok` checks, and the jar's (domain, path, name) order.
//!
//! The behaviour is Python 3.12's, including its quirks: a `Max-Age` without a value
//! raises inside the parser and so drops every cookie of the response, a negative
//! `Version` skips the domain check, a two-digit year is placed in the century closest
//! to the current year.

use std::fmt::Write as _;
use std::sync::LazyLock;

use fav_core::pyint::{self, IntError};
use fav_core::pytext::is_py_space;
use fav_core::pyurl;
use fav_runtime::site_sessions::StoredCookie;
use regex::Regex;

const MONTHS_LOWER: [&str; 12] = ["jan", "feb", "mar", "apr", "may", "jun", "jul", "aug", "sep", "oct", "nov", "dec"];
const KNOWN_ATTRS: [&str; 7] = ["expires", "domain", "path", "secure", "version", "port", "max-age"];
const VALUE_ATTRS: [&str; 8] = ["version", "expires", "max-age", "domain", "path", "port", "comment", "commenturl"];
const BOOLEAN_ATTRS: [&str; 2] = ["discard", "secure"];
/// `HTTP_PATH_SAFE`, on top of `quote`'s always-safe letters, digits and `_.-~`.
const HTTP_PATH_SAFE: &str = "%/;:@&=+$,!~*'()";
const DEFAULT_HTTP_PORT: &str = "80";
const MAX_YEAR: i128 = 9999;

static STRICT_DATE_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^(?-u:[SMTWF][a-z][a-z], (\d\d) ([JFMASOND][a-z][a-z]) (\d\d\d\d) (\d\d):(\d\d):(\d\d) GMT)$")
        .expect("strict date pattern")
});
static WEEKDAY_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^(?i-u:(?:Sun|Mon|Tue|Wed|Thu|Fri|Sat)[a-z]*,?\s*)").expect("weekday pattern"));
/// `LOOSE_HTTP_DATE_RE`, except that the timezone's `(?![APap][Mm]\b)` look-ahead (which
/// the regex crate has no syntax for) is checked on the match instead: see [`http2time`].
static LOOSE_HTTP_DATE_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(concat!(
        r"^(?-u:",
        r"(\d\d?)(?:\s+|[-/])(\w+)(?:\s+|[-/])(\d+)",
        r"(?:(?:\s+|:)(\d\d?):(\d\d)(?::(\d\d))?)?",
        r"\s*(?:([-+]?\d{2,4}|[A-Za-z]+)\s*)?",
        r"(?:\(\w+\)\s*)?",
        r")$",
    ))
    .expect("loose date pattern")
});
static TIMEZONE_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^(?-u:([-+])?(\d\d?):?(\d\d)?)$").expect("timezone pattern"));

/// An exception `make_cookies` catches: every Netscape cookie of the response is dropped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Unhandled;

/// One attribute value after `parse_ns_headers`: missing (`None`), text, or the number an
/// `expires` date became.
#[derive(Debug, Clone, PartialEq)]
enum Attr {
    Missing,
    Text(String),
    Number(f64),
}

/// The cookies the `Set-Cookie` headers of a response to `url` set in an empty jar, in the
/// jar's iteration order, as `bilibili_session._from_jar` lists them. `now` is
/// `time.time()`.
pub(crate) fn cookies_from_headers(url: &str, set_cookie: &[String], now: f64) -> Vec<StoredCookie> {
    #[allow(clippy::cast_possible_truncation)] // reason: `int(time.time())`; any real clock fits
    let now_int = now.floor() as i64;
    let request = RequestInfo::new(url);
    let Ok(attrs_set) = parse_ns_headers(set_cookie, now) else {
        return Vec::new();
    };
    let Ok(tuples) = normalized_cookie_tuples(attrs_set, now_int) else {
        return Vec::new();
    };
    let mut jar = Jar::default();
    for tuple in tuples {
        let Some(mut cookie) = cookie_from_tuple(tuple, &request, now_int) else {
            continue;
        };
        // `_process_rfc2109_cookies`: with RFC 2965 off, an RFC 2109 cookie is a Netscape one.
        if cookie.version == 1 {
            cookie.version = 0;
        }
        if set_ok(&cookie, &request) {
            jar.set(cookie);
        }
    }
    jar.into_stored()
}

// ---------- the request ----------

/// What the policy needs of the request URL.
#[derive(Debug)]
struct RequestInfo {
    /// `eff_request_host`'s effective host name: the lowercased host without its port,
    /// `.local` added when it has no dot.
    erhn: String,
    /// `request_path`: the escaped path, always starting with `/`.
    path: String,
    /// `request_port`: the port, `"80"` without one, `None` when it is not a number.
    port: Option<String>,
}

impl RequestInfo {
    fn new(url: &str) -> Self {
        let parts = pyurl::urlsplit(url, "").unwrap_or_default();
        let netloc = parts.netloc;
        let host = strip_port(&netloc).to_lowercase();
        let erhn = if host.contains('.') {
            host.clone()
        } else {
            format!("{host}.local")
        };
        let mut path = escape_path(&parts.path);
        if !path.starts_with('/') {
            path.insert(0, '/');
        }
        let port = match netloc.find(':') {
            Some(colon) => {
                let port = &netloc[colon + 1..];
                pyint::parse_int(port).ok().map(|_| port.to_owned())
            }
            None => Some(DEFAULT_HTTP_PORT.to_owned()),
        };
        Self { erhn, path, port }
    }
}

/// `cut_port_re.sub("", host, 1)`: drop a trailing `:digits`.
fn strip_port(netloc: &str) -> &str {
    match netloc.rfind(':') {
        Some(colon) if colon + 1 < netloc.len() && netloc[colon + 1..].bytes().all(|byte| byte.is_ascii_digit()) => &netloc[..colon],
        _ => netloc,
    }
}

/// `escape_path`: `quote(path, HTTP_PATH_SAFE)`, then every `%xx` escape uppercased.
fn escape_path(path: &str) -> String {
    let mut quoted = String::new();
    for byte in path.bytes() {
        if byte.is_ascii_alphanumeric() || b"_.-~".contains(&byte) || HTTP_PATH_SAFE.as_bytes().contains(&byte) {
            quoted.push(char::from(byte));
        } else {
            let _ = write!(quoted, "%{byte:02X}");
        }
    }
    let bytes = quoted.as_bytes();
    let mut out = String::with_capacity(quoted.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' && index + 2 < bytes.len() && bytes[index + 1].is_ascii_hexdigit() && bytes[index + 2].is_ascii_hexdigit() {
            out.push('%');
            out.push(char::from(bytes[index + 1].to_ascii_uppercase()));
            out.push(char::from(bytes[index + 2].to_ascii_uppercase()));
            index += 3;
        } else {
            out.push(char::from(bytes[index]));
            index += 1;
        }
    }
    out
}

// ---------- parse_ns_headers ----------

/// `strip_quotes`: one leading and one trailing `"` removed.
fn strip_quotes(text: &str) -> &str {
    let text = text.strip_prefix('"').unwrap_or(text);
    text.strip_suffix('"').unwrap_or(text)
}

fn py_strip(text: &str) -> &str {
    text.trim_matches(is_py_space)
}

/// `parse_ns_headers`: each header as `(name, value)` pairs, `version` added when absent,
/// `expires` turned into seconds since the epoch.
fn parse_ns_headers(headers: &[String], now: f64) -> Result<Vec<Vec<(String, Attr)>>, Unhandled> {
    let mut result = Vec::new();
    for header in headers {
        let mut pairs: Vec<(String, Attr)> = Vec::new();
        let mut version_set = false;
        for (index, param) in header.split(';').enumerate() {
            let param = py_strip(param);
            let (key, value) = match param.split_once('=') {
                Some((key, value)) => (py_strip(key), Some(py_strip(value))),
                None => (py_strip(param), None),
            };
            if key.is_empty() {
                if index == 0 {
                    break;
                }
                continue;
            }
            let mut key = key.to_owned();
            let mut attr = value.map_or(Attr::Missing, |value| Attr::Text(value.to_owned()));
            if index != 0 {
                let lower = key.to_lowercase();
                if KNOWN_ATTRS.contains(&lower.as_str()) {
                    key = lower;
                }
                if key == "version" {
                    if let Some(value) = value {
                        attr = Attr::Text(strip_quotes(value).to_owned());
                    }
                    version_set = true;
                } else if key == "expires"
                    && let Some(value) = value
                {
                    attr = http2time(strip_quotes(value), now)?.map_or(Attr::Missing, Attr::Number);
                }
            }
            pairs.push((key, attr));
        }
        if !pairs.is_empty() {
            if !version_set {
                pairs.push(("version".to_owned(), Attr::Text("0".to_owned())));
            }
            result.push(pairs);
        }
    }
    Ok(result)
}

// ---------- dates ----------

/// The UTC calendar year of a Unix time (`time.localtime(time.time())[0]` in a UTC process).
fn current_year(now: f64) -> i128 {
    #[allow(clippy::cast_possible_truncation)] // reason: a clock reading in seconds
    let days = (now / 86_400.0).floor() as i64;
    civil_from_days(i128::from(days)).0
}

/// Days since 1970-01-01 of a proleptic Gregorian date (`calendar.timegm`'s day count).
fn days_from_civil(year: i128, month: i128, day: i128) -> i128 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = year.div_euclid(400);
    let year_of_era = year - era * 400;
    let month_index = (month + 9) % 12;
    let day_of_year = (153 * month_index + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

/// The `(year, month, day)` of a day count since 1970-01-01.
pub(crate) fn civil_from_days(days: i128) -> (i128, i128, i128) {
    let shifted = days + 719_468;
    let era = shifted.div_euclid(146_097);
    let day_of_era = shifted - era * 146_097;
    let year_of_era = (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_index = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_index + 2) / 5 + 1;
    let month = if month_index < 10 { month_index + 3 } else { month_index - 9 };
    let year = year_of_era + era * 400 + i128::from(month <= 2);
    (year, month, day)
}

/// `_timegm`: seconds since the epoch of a UTC time, `None` outside the ranges it accepts.
fn timegm(year: i128, month: i128, day: i128, hour: i128, minute: i128, second: f64) -> Option<f64> {
    let valid = year >= 1970
        && (1..=12).contains(&month)
        && (1..=31).contains(&day)
        && (0..=24).contains(&hour)
        && (0..=59).contains(&minute)
        && (0.0..=61.0).contains(&second);
    if !valid {
        return None;
    }
    let days = days_from_civil(year, month, 1) + day - 1;
    let minutes = (days * 24 + hour) * 60 + minute;
    #[allow(clippy::cast_precision_loss)] // reason: years up to 9999 fit an f64 exactly
    Some(minutes as f64 * 60.0 + second)
}

/// `offset_from_tz_string`: seconds east of UTC, `None` for a zone it does not know.
fn offset_from_tz_string(tz: &str) -> Option<i128> {
    if matches!(tz, "GMT" | "UTC" | "UT" | "Z") {
        return Some(0);
    }
    let captures = TIMEZONE_RE.captures(tz)?;
    let hours: i128 = captures.get(2)?.as_str().parse().ok()?;
    let minutes: i128 = captures.get(3).map_or(Some(0), |minutes| minutes.as_str().parse().ok())?;
    let offset = hours * 3600 + minutes * 60;
    Some(if captures.get(1).is_some_and(|sign| sign.as_str() == "-") {
        -offset
    } else {
        offset
    })
}

/// `http2time`: seconds since the epoch of a cookie date, `None` when it cannot be read.
/// A strict-format date with an unknown month raises in Python ([`Unhandled`]).
fn http2time(text: &str, now: f64) -> Result<Option<f64>, Unhandled> {
    if let Some(captures) = STRICT_DATE_RE.captures(text) {
        let group = |index: usize| captures.get(index).map_or("", |group| group.as_str());
        let month = MONTHS_LOWER
            .iter()
            .position(|month| *month == group(2).to_ascii_lowercase())
            .ok_or(Unhandled)?;
        let number = |index: usize| group(index).parse::<i128>().unwrap_or_default();
        #[allow(clippy::cast_precision_loss)] // reason: two digits
        let second = number(6) as f64;
        return Ok(timegm(
            number(3),
            i128::try_from(month).unwrap_or_default() + 1,
            number(1),
            number(4),
            number(5),
            second,
        ));
    }
    let text = text.trim_start_matches(is_py_space);
    let text = WEEKDAY_RE.replace(text, "");
    let Some(captures) = LOOSE_HTTP_DATE_RE.captures(&text) else {
        return Ok(None);
    };
    let group = |index: usize| captures.get(index).map(|group| group.as_str());
    if let Some(tz) = group(7) {
        let bytes = tz.as_bytes();
        // `(?![APap][Mm]\b)`: the zone runs to the end of its letters, so the look-ahead
        // refuses exactly a two-letter AM or PM, and no other reading of the text exists.
        if bytes.len() == 2 && b"APap".contains(&bytes[0]) && b"Mm".contains(&bytes[1]) {
            return Ok(None);
        }
    }
    Ok(str2time(
        group(1).unwrap_or_default(),
        group(2).unwrap_or_default(),
        group(3).unwrap_or_default(),
        [group(4), group(5), group(6)],
        group(7),
        now,
    ))
}

/// `_str2time`.
fn str2time(day: &str, month: &str, year: &str, clock: [Option<&str>; 3], tz: Option<&str>, now: f64) -> Option<f64> {
    // `int(yr)` of `\d+`: anything past the year 9999 is refused, however long.
    let mut year = match pyint::parse_int(year) {
        Ok(year) if year <= MAX_YEAR => year,
        _ => return None,
    };
    let month = match MONTHS_LOWER.iter().position(|name| *name == month.to_lowercase()) {
        Some(index) => i128::try_from(index).unwrap_or_default() + 1,
        None => match pyint::parse_int(month) {
            Ok(number) if (1..=12).contains(&number) => number,
            _ => return None,
        },
    };
    let number = |text: Option<&str>| text.map_or(0, |text| pyint::parse_int(text).unwrap_or_default());
    let day = number(Some(day));
    let [hour, minute, second] = clock.map(number);
    if year < 1000 {
        // The century that puts the year closest to the current one.
        let current = current_year(now);
        let mut gap = current % 100;
        let given = year;
        year = year + current - gap;
        gap -= given;
        if gap.abs() > 50 {
            year += if gap > 0 { 100 } else { -100 };
        }
    }
    #[allow(clippy::cast_precision_loss)] // reason: two digits
    let seconds = timegm(year, month, day, hour, minute, second as f64)?;
    let offset = offset_from_tz_string(&tz.unwrap_or("UTC").to_uppercase())?;
    #[allow(clippy::cast_precision_loss)] // reason: at most a few days of seconds
    Some(seconds - offset as f64)
}

// ---------- normalisation ----------

/// `(name, value, standard, rest)`, keeping only the standard attributes the jar uses.
#[derive(Debug)]
struct CookieTuple {
    name: String,
    value: Option<String>,
    version: Option<String>,
    expires: Option<f64>,
    domain: Option<String>,
    path: Option<String>,
    /// `None`: no Port attribute; `Some(None)`: `Port` without a value.
    #[allow(clippy::option_option)] // reason: the attribute can be absent, bare or valued, as in Python
    port: Option<Option<String>>,
}

/// `_normalized_cookie_tuples`. A `Max-Age` without a value is `int(None)`, a `TypeError`
/// nothing in the loop catches ([`Unhandled`]).
fn normalized_cookie_tuples(attrs_set: Vec<Vec<(String, Attr)>>, now: i64) -> Result<Vec<CookieTuple>, Unhandled> {
    let mut tuples = Vec::new();
    'cookies: for attrs in attrs_set {
        let mut attrs = attrs.into_iter();
        let Some((name, value)) = attrs.next() else { continue };
        let value = match value {
            Attr::Text(text) => Some(text),
            _ => None,
        };
        let mut tuple = CookieTuple {
            name,
            value,
            version: None,
            expires: None,
            domain: None,
            path: None,
            port: None,
        };
        // Which standard attributes are set: only the first value of each counts.
        let mut standard: Vec<String> = Vec::new();
        let mut max_age_set = false;
        for (key, attr) in attrs {
            let lower = key.to_lowercase();
            let mut key = if VALUE_ATTRS.contains(&lower.as_str()) || BOOLEAN_ATTRS.contains(&lower.as_str()) {
                lower
            } else {
                key
            };
            let mut attr = attr;
            let is_boolean = BOOLEAN_ATTRS.contains(&key.as_str());
            if is_boolean && attr == Attr::Missing {
                // Present without a value, like `secure`: true.
                attr = Attr::Text(String::new());
            }
            if standard.contains(&key) {
                continue;
            }
            if key == "domain" {
                match &attr {
                    Attr::Missing => continue 'cookies,
                    Attr::Text(text) => attr = Attr::Text(text.to_lowercase()),
                    Attr::Number(_) => {}
                }
            }
            if key == "expires" && (max_age_set || attr == Attr::Missing) {
                // Max-Age wins over Expires; a missing or unreadable date is a session cookie.
                continue;
            }
            if key == "max-age" {
                max_age_set = true;
                let seconds = match &attr {
                    Attr::Missing => return Err(Unhandled),
                    Attr::Text(text) => match pyint::parse_int(text) {
                        Ok(seconds) => seconds,
                        Err(IntError::Invalid) => continue 'cookies,
                        Err(IntError::Overflow { negative }) => {
                            if negative {
                                i128::MIN / 2
                            } else {
                                i128::MAX / 2
                            }
                        }
                    },
                    Attr::Number(number) => {
                        #[allow(clippy::cast_possible_truncation)] // reason: `int(float)`
                        let seconds = *number as i128;
                        seconds
                    }
                };
                "expires".clone_into(&mut key);
                #[allow(clippy::cast_precision_loss)] // reason: seconds since the epoch as Python's float() of an int
                let expires = (i128::from(now) + seconds) as f64;
                attr = Attr::Number(expires);
            }
            if VALUE_ATTRS.contains(&key.as_str()) || is_boolean {
                if attr == Attr::Missing && !matches!(key.as_str(), "port" | "comment" | "commenturl") {
                    continue 'cookies;
                }
                match key.as_str() {
                    "version" => tuple.version = text_of(&attr),
                    "expires" => {
                        tuple.expires = match attr {
                            Attr::Number(number) => Some(number),
                            _ => None,
                        };
                    }
                    "domain" => tuple.domain = text_of(&attr),
                    "path" => tuple.path = text_of(&attr),
                    "port" => tuple.port = Some(text_of(&attr)),
                    _ => {}
                }
                standard.push(key);
            }
        }
        tuples.push(tuple);
    }
    Ok(tuples)
}

fn text_of(attr: &Attr) -> Option<String> {
    match attr {
        Attr::Text(text) => Some(text.clone()),
        _ => None,
    }
}

// ---------- the cookie ----------

#[derive(Debug, Clone)]
struct Cookie {
    version: i128,
    name: String,
    value: Option<String>,
    port: Option<String>,
    port_specified: bool,
    domain: String,
    domain_specified: bool,
    path: String,
    expires: Option<f64>,
}

/// `_cookie_from_cookie_tuple`: defaults filled in; `None` for an invalid version or a
/// cookie that has already expired (a request to delete it).
fn cookie_from_tuple(tuple: CookieTuple, request: &RequestInfo, now: i64) -> Option<Cookie> {
    let version = match tuple.version.as_deref().map(pyint::parse_int) {
        None => 0,
        Some(Ok(version)) => version,
        Some(Err(IntError::Overflow { negative })) => {
            if negative {
                i128::MIN
            } else {
                i128::MAX
            }
        }
        Some(Err(IntError::Invalid)) => return None,
    };
    let path = match tuple.path {
        Some(path) if !path.is_empty() => escape_path(&path),
        _ => {
            let mut path = request.path.clone();
            if let Some(slash) = path.rfind('/') {
                path.truncate(if version == 0 { slash } else { slash + 1 });
            }
            if path.is_empty() {
                "/".clone_into(&mut path);
            }
            path
        }
    };
    let domain_specified = tuple.domain.is_some();
    let domain = match tuple.domain {
        None => request.erhn.clone(),
        Some(domain) if domain.starts_with('.') => domain,
        Some(domain) => format!(".{domain}"),
    };
    let (port, port_specified) = match tuple.port {
        None => (None, false),
        Some(None) => (request.port.clone(), false),
        Some(Some(port)) => (Some(port.chars().filter(|ch| !is_py_space(*ch)).collect()), true),
    };
    #[allow(clippy::cast_precision_loss)] // reason: `expires <= self._now` compares an int with a float
    if tuple.expires.is_some_and(|expires| expires <= now as f64) {
        return None;
    }
    Some(Cookie {
        version,
        name: tuple.name,
        value: tuple.value,
        port,
        port_specified,
        domain,
        domain_specified,
        path,
        expires: tuple.expires,
    })
}

/// `DefaultCookiePolicy().set_ok`, with its default (liberal, Netscape-only) settings.
fn set_ok(cookie: &Cookie, request: &RequestInfo) -> bool {
    // `set_ok_version`: RFC 2965 is off. (Python 3.12 lets a negative version through.)
    if cookie.version > 0 {
        return false;
    }
    if cookie.domain_specified {
        let domain = cookie.domain.as_str();
        let undotted = domain.strip_prefix('.').unwrap_or(domain);
        #[allow(clippy::case_sensitive_file_extension_comparisons)] // reason: a host name, compared as Python does
        let local = request.erhn.ends_with(".local");
        if !undotted.contains('.') && !local {
            return false;
        }
        if cookie.version == 0 {
            let erhn = request.erhn.as_str();
            let matches = erhn.ends_with(domain) || erhn.ends_with(&format!("{undotted}.local"));
            if !matches && !erhn.starts_with('.') && !format!(".{erhn}").ends_with(domain) {
                return false;
            }
        }
    }
    if cookie.port_specified {
        let request_port = request.port.as_deref().unwrap_or(DEFAULT_HTTP_PORT);
        let ports = cookie.port.as_deref().unwrap_or_default();
        for port in ports.split(',') {
            if pyint::parse_int(port) == Err(IntError::Invalid) {
                return false;
            }
            if port == request_port {
                return true;
            }
        }
        return false;
    }
    true
}

// ---------- the jar ----------

/// One domain's cookies: by path, then by name.
type Paths = Vec<(String, Vec<(String, Cookie)>)>;

/// `CookieJar._cookies`: domain, then path, then name, each in insertion order.
#[derive(Debug, Default)]
struct Jar {
    domains: Vec<(String, Paths)>,
}

impl Jar {
    fn set(&mut self, cookie: Cookie) {
        let domain_index = if let Some(index) = self.domains.iter().position(|(domain, _)| *domain == cookie.domain) {
            index
        } else {
            self.domains.push((cookie.domain.clone(), Vec::new()));
            self.domains.len() - 1
        };
        let paths = &mut self.domains[domain_index].1;
        let path_index = if let Some(index) = paths.iter().position(|(path, _)| *path == cookie.path) {
            index
        } else {
            paths.push((cookie.path.clone(), Vec::new()));
            paths.len() - 1
        };
        let names = &mut paths[path_index].1;
        match names.iter_mut().find(|(name, _)| *name == cookie.name) {
            Some(slot) => slot.1 = cookie,
            None => names.push((cookie.name.clone(), cookie)),
        }
    }

    /// `_from_jar`.
    fn into_stored(self) -> Vec<StoredCookie> {
        self.domains
            .into_iter()
            .flat_map(|(_, paths)| paths)
            .flat_map(|(_, names)| names)
            .map(|(_, cookie)| StoredCookie {
                name: cookie.name,
                value: cookie.value.unwrap_or_default(),
                domain: cookie.domain,
                path: if cookie.path.is_empty() { "/".to_owned() } else { cookie.path },
                expires: cookie.expires.filter(|expires| *expires != 0.0),
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use fav_core::golden;
    use fav_core::pyjson::Value;

    use super::{civil_from_days, cookies_from_headers, days_from_civil, escape_path, http2time};

    fn float(value: &Value) -> Option<f64> {
        match value {
            Value::Float(float) => Some(*float),
            #[allow(clippy::cast_precision_loss)] // reason: test values are small
            Value::Int(int) => Some(*int as f64),
            _ => None,
        }
    }

    #[test]
    fn set_cookie_headers_become_the_cookies_python_keeps() {
        let document = golden::load("bilibili/set_cookie.json");
        for case in golden::cases(&document) {
            let url = golden::string(golden::get(case, "url"));
            let now = float(golden::get(case, "now")).expect("now");
            let headers: Vec<String> = golden::list(golden::get(case, "set_cookie")).iter().map(golden::string).collect();
            let cookies = cookies_from_headers(&url, &headers, now);
            let expected = golden::list(golden::get(case, "cookies"));
            assert_eq!(cookies.len(), expected.len(), "{headers:?}: {cookies:?}");
            for (cookie, want) in cookies.iter().zip(expected) {
                assert_eq!(cookie.name, golden::string(golden::get(want, "name")), "{headers:?}");
                assert_eq!(cookie.value, golden::string(golden::get(want, "value")), "{}", cookie.name);
                assert_eq!(cookie.domain, golden::string(golden::get(want, "domain")), "{}", cookie.name);
                assert_eq!(cookie.path, golden::string(golden::get(want, "path")), "{}", cookie.name);
                assert_eq!(cookie.expires, float(golden::get(want, "expires")), "{}", cookie.name);
            }
        }
    }

    #[test]
    fn calendar_arithmetic_round_trips() {
        assert_eq!(days_from_civil(1970, 1, 1), 0);
        assert_eq!(days_from_civil(2031, 1, 1), 22_280);
        for days in [-1_000_000, -1, 0, 59, 60, 22_280, 2_932_896] {
            let (year, month, day) = civil_from_days(days);
            assert_eq!(days_from_civil(year, month, day), days);
        }
    }

    #[test]
    fn dates_python_reads() {
        let now = 1_800_000_000.0;
        assert_eq!(http2time("Wed, 01 Jan 2031 00:00:00 GMT", now), Ok(Some(1_924_992_000.0)));
        assert_eq!(http2time("01 Jan 2031 08:00:00 +0800", now), Ok(Some(1_924_992_000.0)));
        assert_eq!(http2time("Wed, 01 Jxx 2031 00:00:00 GMT", now), Err(super::Unhandled));
        assert_eq!(http2time("01 Jan 2031 00:00 am", now), Ok(None));
        assert_eq!(http2time("01 Jan 2031 00:00 AMT", now), Ok(None));
    }

    #[test]
    fn paths_are_escaped_with_uppercase_hex() {
        assert_eq!(escape_path("/a%7eb/c d"), "/a%7Eb/c%20d");
        assert_eq!(escape_path("/ü"), "/%C3%BC");
    }
}
