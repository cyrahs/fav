//! `pathlib.PurePosixPath` arithmetic: joining, `name`, `suffix`, `stem`.
//!
//! Python origin: `pathlib` in `CPython` 3.12. Paths fav stores (`local_path` columns,
//! notification payloads) are `str(Path)` values, so joins and suffixes have to come out
//! the way pathlib writes them: no trailing slash, `.` parts dropped, `//` collapsed.

use crate::validate::PyPath;

/// `str(PurePosixPath(base) / part)`: `part` replaces `base` when it is absolute.
pub fn join(base: &str, part: &str) -> String {
    if part.starts_with('/') {
        return PyPath::new(part).as_str().to_owned();
    }
    PyPath::new(&format!("{base}/{part}")).as_str().to_owned()
}

/// `PurePosixPath(path).name`: the last part, empty for `/`, `.` or the empty path.
pub fn name(path: &str) -> &str {
    path.rsplit('/').find(|part| !part.is_empty() && *part != ".").unwrap_or("")
}

/// `PurePosixPath(path).suffix`: from the last dot of the name, unless the dot starts or
/// ends it.
pub fn suffix(path: &str) -> &str {
    let name = name(path);
    match name.rfind('.') {
        Some(dot) if dot > 0 && dot < name.len() - 1 => &name[dot..],
        _ => "",
    }
}

/// `PurePosixPath(path).stem`: the name without its suffix.
pub fn stem(path: &str) -> &str {
    let name = name(path);
    let suffix = suffix(path);
    &name[..name.len() - suffix.len()]
}

#[cfg(test)]
mod tests {
    use super::{join, name, stem, suffix};
    use crate::golden;

    #[test]
    fn name_suffix_stem_and_join_match_pathlib() {
        let document = golden::load("pyweb/paths.json");
        let case = &golden::cases(&document)[0];
        for part in golden::list(golden::get(case, "parts")) {
            let path = golden::string(golden::get(part, "path"));
            assert_eq!(name(&path), golden::string(golden::get(part, "name")), "{path:?}");
            assert_eq!(suffix(&path), golden::string(golden::get(part, "suffix")), "{path:?}");
            assert_eq!(stem(&path), golden::string(golden::get(part, "stem")), "{path:?}");
        }
        for entry in golden::list(golden::get(case, "joins")) {
            let base = golden::string(golden::get(entry, "base"));
            let part = golden::string(golden::get(entry, "part"));
            assert_eq!(
                join(&base, &part),
                golden::string(golden::get(entry, "joined")),
                "{base:?} {part:?}"
            );
        }
    }
}
