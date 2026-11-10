//! New-character and new-skin detection against the previous `character.json`.
//!
//! Python origin: `src/tool/content_discovery.py`. WP9 (NIKKE) ports the same module as
//! `fav-sources/src/content_discovery.rs` in parallel; until both have merged this is BD2's
//! private copy (`rust/docs/PLAN.md`, cleanup notes).

use std::path::Path;

use fav_core::pycasefold::casefold;
use fav_core::pyjson::{Dict, Value};
use fav_core::pytext::py_strip;

use crate::pydecode::json_loads;

/// `CharacterSnapshot`: whether a previous snapshot exists, and its object if it parsed.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct CharacterSnapshot {
    pub(crate) exists: bool,
    pub(crate) character: Option<Dict>,
}

/// The directories directly under `base_dir`, by name (`base_dir.glob('*/...')`).
fn child_dirs(base_dir: &Path) -> Vec<std::path::PathBuf> {
    let Ok(entries) = std::fs::read_dir(base_dir) else {
        return Vec::new();
    };
    entries
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.is_dir())
        .collect()
}

/// `has_character_snapshot(base_dir)`: some `*/character.json` exists.
pub(crate) fn has_character_snapshot(base_dir: &Path) -> bool {
    child_dirs(base_dir).iter().any(|dir| dir.join("character.json").exists())
}

/// `load_character_snapshot(base_dir=..., current_root=..., content_id=...)`: the snapshot
/// under the current title directory, else under an earlier `<content_id> - *` one.
pub(crate) fn load_character_snapshot(base_dir: &Path, current_root: &Path, content_id: i128) -> CharacterSnapshot {
    let prefix = format!("{content_id} - ");
    let mut earlier: Vec<std::path::PathBuf> = child_dirs(base_dir)
        .into_iter()
        .filter(|dir| dir.file_name().is_some_and(|name| name.to_string_lossy().starts_with(&prefix)))
        .map(|dir| dir.join("character.json"))
        .filter(|path| path.exists())
        .collect();
    earlier.sort();
    let current = current_root.join("character.json");
    let mut visited = std::collections::HashSet::new();
    for path in std::iter::once(current).chain(earlier) {
        if !path.is_file() || !visited.insert(path.clone()) {
            continue;
        }
        let parsed = std::fs::read(&path)
            .ok()
            .and_then(|bytes| String::from_utf8(bytes).ok())
            .and_then(|text| json_loads(&text).ok());
        return CharacterSnapshot {
            exists: true,
            character: match parsed {
                Some(Value::Dict(dict)) => Some(dict),
                _ => None,
            },
        };
    }
    CharacterSnapshot {
        exists: false,
        character: None,
    }
}

fn normalized_text(value: Option<&Value>) -> String {
    match value {
        Some(Value::Str(text)) => casefold(py_strip(text)),
        _ => String::new(),
    }
}

fn display_text(value: Option<&Value>) -> String {
    match value {
        Some(Value::Str(text)) => py_strip(text).to_owned(),
        _ => String::new(),
    }
}

/// `collection_item_names(items, identity_fields=..., display_fields=..., fallback_label=...)`:
/// stable identity to display name, in source order, the first of an identity kept.
pub(crate) fn collection_item_names(
    items: Option<&Value>,
    identity_fields: &[&str],
    display_fields: &[&str],
    fallback_label: &str,
) -> Vec<(String, String)> {
    let Some(Value::List(items)) = items else {
        return Vec::new();
    };
    let mut named: Vec<(String, String)> = Vec::new();
    for (index, item) in items.iter().enumerate() {
        let Value::Dict(item) = item else { continue };
        let identity = identity_fields
            .iter()
            .map(|field| normalized_text(item.get(field)))
            .find(|text| !text.is_empty())
            .unwrap_or_default();
        let display = display_fields
            .iter()
            .map(|field| display_text(item.get(field)))
            .find(|text| !text.is_empty())
            .unwrap_or_default();
        let key = if identity.is_empty() { format!("index:{index}") } else { identity };
        if named.iter().any(|(existing, _)| *existing == key) {
            continue;
        }
        let name = if display.is_empty() {
            format!("{fallback_label} {}", index + 1)
        } else {
            display
        };
        named.push((key, name));
    }
    named
}

/// `new_collection_item_names(previous, current)`.
pub(crate) fn new_collection_item_names(previous: &[(String, String)], current: &[(String, String)]) -> Vec<String> {
    current
        .iter()
        .filter(|(identity, _)| !previous.iter().any(|(known, _)| known == identity))
        .map(|(_, name)| name.clone())
        .collect()
}
