//! `shutil.which` for the commands jobs declare in `required_commands`.

use std::path::{Path, PathBuf};

/// Python's `os.defpath`, used when `PATH` is unset.
const DEFAULT_PATH: &str = "/bin:/usr/bin";

/// The first executable file named `command` on `PATH`, like `shutil.which(command)`.
pub(crate) fn which(command: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH");
    which_in(command, path.as_deref().and_then(|path| path.to_str()).unwrap_or(DEFAULT_PATH))
}

fn which_in(command: &str, path: &str) -> Option<PathBuf> {
    if command.contains('/') {
        let candidate = PathBuf::from(command);
        return is_executable_file(&candidate).then_some(candidate);
    }
    path.split(':')
        .map(|dir| Path::new(if dir.is_empty() { "." } else { dir }).join(command))
        .find(|candidate| is_executable_file(candidate))
}

fn is_executable_file(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::metadata(path).is_ok_and(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
}

#[cfg(test)]
mod tests {
    use super::which_in;

    #[test]
    fn finds_executables_on_the_path_only() {
        let dir = tempfile::tempdir().expect("tempdir");
        let tool = dir.path().join("tool");
        std::fs::write(&tool, "#!/bin/sh\n").expect("write");
        let plain = dir.path().join("plain");
        std::fs::write(&plain, "").expect("write");
        {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(&tool, std::fs::Permissions::from_mode(0o755)).expect("chmod");
        }
        let path = format!("/nonexistent:{}", dir.path().display());
        assert_eq!(which_in("tool", &path), Some(tool.clone()));
        assert_eq!(which_in("plain", &path), None);
        assert_eq!(which_in("missing", &path), None);
        assert_eq!(which_in(tool.to_str().expect("utf-8"), ""), Some(tool));
    }
}
