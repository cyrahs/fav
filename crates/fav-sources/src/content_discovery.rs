//! Detecting newly archived characters and their cosmetic items
//! (`src/tool/content_discovery.py`). NIKKE (WP9) uses it; BD2 (WP10) shares it.

use std::path::{Path, PathBuf};

use fav_core::pycasefold::casefold;
use fav_core::pyjson::{Dict, Value};
use fav_core::pytext::py_strip;

/// A character's previous `character.json`: `exists` with `character: None` when the file
/// was there but unreadable or not an object.
#[derive(Debug, Clone, PartialEq)]
pub struct CharacterSnapshot {
    pub exists: bool,
    pub character: Option<Dict>,
}

/// The directories directly under `base_dir`, sorted by name.
fn child_dirs(base_dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(base_dir) else {
        return Vec::new();
    };
    let mut dirs: Vec<PathBuf> = entries
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.is_dir())
        .collect();
    dirs.sort();
    dirs
}

/// `has_character_snapshot`: whether any `*/character.json` exists under `base_dir`.
pub fn has_character_snapshot(base_dir: &Path) -> bool {
    child_dirs(base_dir)
        .iter()
        .any(|dir| dir.join("character.json").symlink_metadata().is_ok())
}

/// `load_character_snapshot`: the current directory's `character.json`, else one under an
/// earlier title (`<content_id> - */character.json`).
pub fn load_character_snapshot(base_dir: &Path, current_root: &Path, content_id: i128) -> CharacterSnapshot {
    let prefix = format!("{content_id} - ");
    let mut candidates = vec![current_root.join("character.json")];
    candidates.extend(
        child_dirs(base_dir)
            .into_iter()
            .filter(|dir| dir.file_name().is_some_and(|name| name.to_string_lossy().starts_with(&prefix)))
            .map(|dir| dir.join("character.json"))
            .filter(|path| path.symlink_metadata().is_ok()),
    );
    let mut visited: Vec<PathBuf> = Vec::new();
    for path in candidates {
        if visited.contains(&path) || !path.is_file() {
            continue;
        }
        visited.push(path.clone());
        let parsed = std::fs::read(&path)
            .ok()
            .and_then(|bytes| String::from_utf8(bytes).ok())
            .and_then(|text| fav_core::pyjson::loads(&text).ok());
        return CharacterSnapshot {
            exists: true,
            character: match parsed {
                Some(Value::Dict(character)) => Some(character),
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

/// `collection_item_names`: stable identity to display name, in the items' order (the
/// first item with an identity wins).
pub fn collection_item_names(
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
        if !named.iter().any(|(existing, _)| *existing == key) {
            let name = if display.is_empty() {
                format!("{fallback_label} {}", index + 1)
            } else {
                display
            };
            named.push((key, name));
        }
    }
    named
}

/// `new_collection_item_names`: display names whose identity the previous snapshot lacked.
pub fn new_collection_item_names(previous: &[(String, String)], current: &[(String, String)]) -> Vec<String> {
    current
        .iter()
        .filter(|(identity, _)| !previous.iter().any(|(known, _)| known == identity))
        .map(|(_, name)| name.clone())
        .collect()
}
