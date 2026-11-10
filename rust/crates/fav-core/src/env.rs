//! Bootstrap configuration from the environment and an optional `.env` file.
//!
//! Python origin: `src/core/env.py` (a pydantic-settings model) and the parts of
//! pydantic-settings and python-dotenv it relies on. Only what is needed to reach the
//! database and to protect the settings API comes from the environment; everything else
//! lives in `app_settings`. Variable names match case-insensitively and win over `.env`.

use std::collections::HashMap;

use crate::pyjson::{Dict, Value};
use crate::pytext::{is_py_space, py_strip};
use crate::validate::{self, ErrorItem, Errors, ValidationError, key};

pub const MAX_PORT: i64 = 65535;
/// `NOTICE` is fav's own level (see `logging`).
pub const LOG_LEVELS: [&str; 6] = ["DEBUG", "INFO", "NOTICE", "WARNING", "ERROR", "CRITICAL"];

/// The bootstrap values, validated.
#[derive(Clone, PartialEq, Eq)]
pub struct Env {
    pub postgres_dsn: String,
    pub api_token: String,
    /// Raise to DEBUG to make a run explain itself.
    pub log_level: String,
    pub api_bind: String,
    pub api_port: u16,
    /// Comma-separated; only needed by separate front ends such as the `Live2D` viewer.
    pub api_cors_origins: String,
    pub api_cors_allow_credentials: bool,
    /// Services that read a signed-in session from `GET /api/v2/credentials/{site}`:
    /// `name:sha256-of-token:site[,site...]` entries separated by whitespace or `;`.
    pub credential_clients: String,
}

/// Secrets stay out of debug output.
impl std::fmt::Debug for Env {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Env")
            .field("postgres_dsn", &"<redacted>")
            .field("api_token", &"<redacted>")
            .field("log_level", &self.log_level)
            .field("api_bind", &self.api_bind)
            .field("api_port", &self.api_port)
            .field("api_cors_origins", &self.api_cors_origins)
            .field("api_cors_allow_credentials", &self.api_cors_allow_credentials)
            .field("credential_clients", &self.credential_clients)
            .finish()
    }
}

/// The variables `Env` reads, in field order.
const NAMES: [&str; 8] = [
    "POSTGRES_DSN",
    "API_TOKEN",
    "LOG_LEVEL",
    "API_BIND",
    "API_PORT",
    "API_CORS_ORIGINS",
    "API_CORS_ALLOW_CREDENTIALS",
    "CREDENTIAL_CLIENTS",
];

impl Env {
    /// Read the process environment and `./.env`, like `Env()` at import time.
    pub fn from_process() -> Result<Self, ValidationError> {
        let dotenv = std::fs::read_to_string(".env").ok();
        Self::from_sources(std::env::vars(), dotenv.as_deref())
    }

    /// Read the given variables and `.env` text (`None`: no file), variables winning.
    pub fn from_sources(vars: impl IntoIterator<Item = (String, String)>, dotenv: Option<&str>) -> Result<Self, ValidationError> {
        let environ: Vec<(String, String)> = vars.into_iter().collect();
        let mut found: HashMap<&'static str, String> = HashMap::new();
        if let Some(text) = dotenv {
            for (name, value) in parse_dotenv(text, &environ) {
                if let (Some(alias), Some(value)) = (alias_of(&name), value) {
                    found.insert(alias, value);
                }
            }
        }
        for (name, value) in &environ {
            if let Some(alias) = alias_of(name) {
                found.insert(alias, value.clone());
            }
        }
        Self::validate(&found)
    }

    fn validate(found: &HashMap<&'static str, String>) -> Result<Self, ValidationError> {
        let mut errors: Errors = Vec::new();
        let input = Value::Dict(
            NAMES
                .iter()
                .filter_map(|name| found.get(name).map(|value| ((*name).to_owned(), Value::Str(value.clone()))))
                .collect::<Dict>(),
        );
        let mut text = |name: &'static str, default: Option<&str>, check: &dyn Fn(&str) -> Result<String, String>| -> Option<String> {
            let loc = key(&[], name);
            let raw = match (found.get(name), default) {
                (Some(raw), _) => raw.clone(),
                (None, Some(default)) => return Some(default.to_owned()),
                (None, None) => {
                    errors.push(ErrorItem {
                        kind: "missing",
                        loc,
                        msg: "Field required".to_owned(),
                        input: input.clone(),
                        ctx: None,
                    });
                    return None;
                }
            };
            check(&raw)
                .map_err(|message| errors.push(ErrorItem::value_error(&message, &loc, &Value::Str(raw.clone()))))
                .ok()
        };
        let postgres_dsn = text("POSTGRES_DSN", None, &|value| required(value, "POSTGRES_DSN cannot be empty"));
        let api_token = text("API_TOKEN", None, &|value| required(value, "API_TOKEN cannot be empty"));
        let log_level = text("LOG_LEVEL", Some("INFO"), &normalize_log_level);
        let api_bind = text("API_BIND", Some("0.0.0.0"), &|value| {
            let normalized = py_strip(value);
            Ok(if normalized.is_empty() { "0.0.0.0" } else { normalized }.to_owned())
        });
        let api_port = match found.get("API_PORT") {
            None => Some(8091),
            Some(raw) => api_port(raw).map_err(|more| errors.extend(more)).ok(),
        };
        let api_cors_origins = found.get("API_CORS_ORIGINS").cloned().unwrap_or_default();
        let api_cors_allow_credentials = match found.get("API_CORS_ALLOW_CREDENTIALS") {
            None => Some(false),
            Some(raw) => validate::boolean(&Value::Str(raw.clone()), &key(&[], "API_CORS_ALLOW_CREDENTIALS"))
                .map_err(|more| errors.extend(more))
                .ok(),
        };
        let credential_clients = found.get("CREDENTIAL_CLIENTS").cloned().unwrap_or_default();
        match (postgres_dsn, api_token, log_level, api_bind, api_port, api_cors_allow_credentials) {
            (Some(postgres_dsn), Some(api_token), Some(log_level), Some(api_bind), Some(api_port), Some(api_cors_allow_credentials))
                if errors.is_empty() =>
            {
                Ok(Self {
                    postgres_dsn,
                    api_token,
                    log_level,
                    api_bind,
                    api_port,
                    api_cors_origins,
                    api_cors_allow_credentials,
                    credential_clients,
                })
            }
            _ => Err(ValidationError { title: "Env", errors }),
        }
    }

    /// The CORS origins: split on commas, trimmed, trailing slash dropped, deduplicated.
    pub fn cors_origins(&self) -> Vec<String> {
        let mut origins: Vec<String> = Vec::new();
        for item in self.api_cors_origins.split(',') {
            let origin = py_strip(item).trim_end_matches('/');
            if origin.is_empty() || origins.iter().any(|seen| seen == origin) {
                continue;
            }
            origins.push(origin.to_owned());
        }
        origins
    }

    /// `env.model_dump()`, for comparing with Python.
    pub fn dump(&self) -> Value {
        let mut dict = Dict::new();
        dict.insert("postgres_dsn", Value::Str(self.postgres_dsn.clone()));
        dict.insert("api_token", Value::Str(self.api_token.clone()));
        dict.insert("log_level", Value::Str(self.log_level.clone()));
        dict.insert("api_bind", Value::Str(self.api_bind.clone()));
        dict.insert("api_port", Value::Int(i128::from(self.api_port)));
        dict.insert("api_cors_origins", Value::Str(self.api_cors_origins.clone()));
        dict.insert("api_cors_allow_credentials", Value::Bool(self.api_cors_allow_credentials));
        dict.insert("credential_clients", Value::Str(self.credential_clients.clone()));
        Value::Dict(dict)
    }
}

fn required(value: &str, message: &str) -> Result<String, String> {
    let normalized = py_strip(value);
    if normalized.is_empty() {
        Err(message.to_owned())
    } else {
        Ok(normalized.to_owned())
    }
}

/// Loud about a typo rather than quietly running at INFO.
fn normalize_log_level(value: &str) -> Result<String, String> {
    let normalized = py_strip(value).to_uppercase();
    let normalized = if normalized.is_empty() { "INFO".to_owned() } else { normalized };
    if LOG_LEVELS.contains(&normalized.as_str()) {
        Ok(normalized)
    } else {
        Err(format!("LOG_LEVEL must be one of: {}", LOG_LEVELS.join(", ")))
    }
}

fn api_port(raw: &str) -> Result<u16, Errors> {
    let raw_value = Value::Str(raw.to_owned());
    let loc = key(&[], "API_PORT");
    let port = validate::int(&raw_value, &loc)?;
    match u16::try_from(port) {
        Ok(port) if port > 0 => Ok(port),
        _ => Err(vec![ErrorItem::value_error(
            &format!("API_PORT must be between 1 and {MAX_PORT}"),
            &loc,
            &raw_value,
        )]),
    }
}

/// pydantic-settings matches variable names case-insensitively.
fn alias_of(name: &str) -> Option<&'static str> {
    NAMES.iter().copied().find(|alias| alias.eq_ignore_ascii_case(name))
}

/// Read a `.env` file like python-dotenv's `dotenv_values`: bindings in file order, a key
/// without `=` bound to `None`, `${NAME}` and `${NAME:-default}` expanded against the
/// process environment overlaid by the file's earlier bindings.
pub fn parse_dotenv(text: &str, environ: &[(String, String)]) -> Vec<(String, Option<String>)> {
    let mut resolved: Vec<(String, Option<String>)> = Vec::new();
    for (name, value) in (DotenvReader { text, pos: 0 }).bindings() {
        let value = value.map(|value| expand_variables(&value, environ, &resolved));
        resolved.push((name, value));
    }
    resolved
}

fn expand_variables(value: &str, environ: &[(String, String)], earlier: &[(String, Option<String>)]) -> String {
    let lookup = |name: &str| -> Option<Option<String>> {
        if let Some((_, value)) = earlier.iter().rev().find(|(key, _)| key == name) {
            return Some(value.clone());
        }
        environ.iter().find(|(key, _)| key == name).map(|(_, value)| Some(value.clone()))
    };
    let mut out = String::new();
    let mut rest = value;
    while let Some(start) = rest.find("${") {
        let after_open = &rest[start + 2..];
        let Some(close) = after_open.find('}') else { break };
        let body = &after_open[..close];
        let (name, default) = match body.split_once(":-") {
            Some((name, default)) if !name.contains(':') => (name, Some(default)),
            _ if body.contains(':') => {
                // `[^}:]*` must be followed by `:-` or `}`; anything else is literal.
                out.push_str(&rest[..start + 2]);
                rest = after_open;
                continue;
            }
            _ => (body, None),
        };
        out.push_str(&rest[..start]);
        let resolved = match lookup(name) {
            Some(value) => value.unwrap_or_default(),
            None => default.unwrap_or_default().to_owned(),
        };
        out.push_str(&resolved);
        rest = &after_open[close + 1..];
    }
    out.push_str(rest);
    out
}

/// python-dotenv's reader (`dotenv/parser.py`), byte offsets instead of character ones.
struct DotenvReader<'a> {
    text: &'a str,
    pos: usize,
}

fn is_inline_space(ch: char) -> bool {
    is_py_space(ch) && ch != '\r' && ch != '\n'
}

impl DotenvReader<'_> {
    fn rest(&self) -> &str {
        &self.text[self.pos..]
    }

    fn skip_while(&mut self, predicate: impl Fn(char) -> bool) -> &str {
        let start = self.pos;
        let length: usize = self.rest().chars().take_while(|ch| predicate(*ch)).map(char::len_utf8).sum();
        self.pos += length;
        &self.text[start..self.pos]
    }

    fn bindings(mut self) -> Vec<(String, Option<String>)> {
        let mut out = Vec::new();
        while self.pos < self.text.len() {
            let mark = self.pos;
            match self.binding() {
                Ok(Some(binding)) => out.push(binding),
                Ok(None) => {}
                Err(()) => {
                    // An unparsable statement is skipped up to the end of its line.
                    self.pos = mark;
                    self.skip_while(|ch| ch != '\r' && ch != '\n');
                    if self.rest().starts_with("\r\n") {
                        self.pos += 2;
                    } else if self.rest().starts_with(['\r', '\n']) {
                        self.pos += 1;
                    }
                }
            }
        }
        out
    }

    fn binding(&mut self) -> Result<Option<(String, Option<String>)>, ()> {
        self.skip_while(is_py_space);
        if self.pos >= self.text.len() {
            return Ok(None);
        }
        if let Some(after) = self.rest().strip_prefix("export")
            && after.starts_with(is_inline_space)
        {
            self.pos += "export".len();
            self.skip_while(is_inline_space);
        }
        let key = if self.rest().starts_with('#') {
            None
        } else if self.rest().starts_with('\'') {
            self.pos += 1;
            let key = self.skip_while(|ch| ch != '\'').to_owned();
            if key.is_empty() || !self.rest().starts_with('\'') {
                return Err(());
            }
            self.pos += 1;
            Some(key)
        } else {
            let key = self.skip_while(|ch| ch != '=' && ch != '#' && !is_py_space(ch)).to_owned();
            if key.is_empty() {
                return Err(());
            }
            Some(key)
        };
        self.skip_while(is_inline_space);
        let value = if self.rest().starts_with('=') {
            self.pos += 1;
            self.skip_while(is_inline_space);
            Some(self.value()?)
        } else {
            None
        };
        // An optional comment, then the end of the line.
        let before_comment = self.pos;
        self.skip_while(is_inline_space);
        if self.rest().starts_with('#') {
            self.skip_while(|ch| ch != '\r' && ch != '\n');
        } else {
            self.pos = before_comment;
        }
        self.skip_while(is_inline_space);
        if self.rest().starts_with("\r\n") {
            self.pos += 2;
        } else if self.rest().starts_with(['\r', '\n']) {
            self.pos += 1;
        } else if self.pos < self.text.len() {
            return Err(());
        }
        Ok(key.map(|key| (key, value)))
    }

    fn quoted(&mut self, quote: char) -> Result<String, ()> {
        self.pos += 1;
        let start = self.pos;
        let mut escaped = false;
        for (offset, ch) in self.rest().char_indices() {
            if ch == quote && !escaped {
                let raw = self.text[start..start + offset].to_owned();
                self.pos = start + offset + 1;
                return Ok(raw);
            }
            escaped = ch == '\\' && !escaped;
        }
        Err(())
    }

    fn value(&mut self) -> Result<String, ()> {
        match self.rest().chars().next() {
            Some('\'') => Ok(decode_escapes(&self.quoted('\'')?, "\\'")),
            Some('"') => Ok(decode_escapes(&self.quoted('"')?, "\\'\"abfnrtv")),
            None | Some('\r' | '\n') => Ok(String::new()),
            Some(_) => {
                let part = self.skip_while(|ch| ch != '\r' && ch != '\n');
                // `re.sub(r"\s+#.*", "", part).rstrip()`
                let mut cut = part.len();
                let chars: Vec<(usize, char)> = part.char_indices().collect();
                for (index, &(offset, ch)) in chars.iter().enumerate() {
                    if is_py_space(ch) {
                        let run_end = chars[index..].iter().find(|(_, next)| !is_py_space(*next));
                        if let Some(&(_, '#')) = run_end {
                            cut = offset;
                            break;
                        }
                    }
                }
                Ok(part[..cut].trim_end_matches(is_py_space).to_owned())
            }
        }
    }
}

/// Decode the backslash escapes python-dotenv decodes inside quotes.
fn decode_escapes(raw: &str, escapable: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut chars = raw.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '\\'
            && let Some(&next) = chars.peek()
            && escapable.contains(next)
        {
            chars.next();
            out.push(match next {
                'a' => '\u{7}',
                'b' => '\u{8}',
                'f' => '\u{c}',
                'n' => '\n',
                'r' => '\r',
                't' => '\t',
                'v' => '\u{b}',
                other => other,
            });
        } else {
            out.push(ch);
        }
    }
    out
}

#[cfg(test)]
mod tests;
