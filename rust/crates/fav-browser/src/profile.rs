//! Chromium profile housekeeping.
//!
//! Python origin: `PROFILE_SINGLETON_FILES` and `clear_stale_profile_locks` in
//! `src/web/rednote_browser.py`, and `profile_lock_name` in `src/web/rednote.py` (the
//! lock a RedNote run and a 登录态 login on its profile both take).

use std::io;
use std::path::{Component, Path, PathBuf};

/// Chromium writes these into the profile and stamps them with `hostname-pid`. Every pod
/// gets a new hostname, so after a SIGKILL the next one reads them as a lock held by
/// another machine and either refuses to start or hangs.
pub const PROFILE_SINGLETON_FILES: [&str; 3] = ["SingletonLock", "SingletonCookie", "SingletonSocket"];

/// `clear_stale_profile_locks(user_data_dir)`: drop the singleton files a killed Chromium
/// left behind, and name the ones that were there.
///
/// Safe only because the caller holds the advisory lock on this profile: nothing else
/// can be using it, so a lock file here is by definition stale. A broken symlink counts
/// (`SingletonLock` is one, pointing at `hostname-pid`).
pub fn clear_stale_profile_locks(user_data_dir: &Path) -> io::Result<Vec<String>> {
    let mut cleared = Vec::new();
    for name in PROFILE_SINGLETON_FILES {
        let path = user_data_dir.join(name);
        if std::fs::symlink_metadata(&path).is_ok() {
            match std::fs::remove_file(&path) {
                Ok(()) => {}
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => return Err(error),
            }
            cleared.push(name.to_owned());
        }
    }
    Ok(cleared)
}

/// `Path.expanduser().resolve()` (non-strict): `~` expanded from `HOME`, made absolute
/// against the working directory, symlinks of the part that exists resolved, `..` and
/// `.` folded.
pub fn resolve_path(path: &Path) -> PathBuf {
    let expanded = expand_user(path);
    let absolute = std::path::absolute(&expanded).unwrap_or(expanded);
    // `os.path.realpath(strict=False)`: component by component, each prefix that exists
    // replaced by its real path; one that does not is kept as written, and a later `..`
    // steps back out of it.
    let mut resolved = PathBuf::from("/");
    for component in absolute.components() {
        match component {
            Component::ParentDir => {
                resolved.pop();
            }
            Component::Normal(part) => {
                resolved.push(part);
                if let Ok(real) = std::fs::canonicalize(&resolved) {
                    resolved = real;
                }
            }
            Component::RootDir | Component::Prefix(_) | Component::CurDir => {}
        }
    }
    resolved
}

fn expand_user(path: &Path) -> PathBuf {
    let mut components = path.components();
    match components.next() {
        Some(Component::Normal(first)) if first == "~" => match std::env::var_os("HOME") {
            Some(home) if !home.is_empty() => {
                let mut expanded = PathBuf::from(home);
                expanded.extend(components);
                expanded
            }
            _ => path.to_path_buf(),
        },
        _ => path.to_path_buf(),
    }
}

/// `profile_lock_name(profile_path)`: the advisory lock on one Chromium profile, held by a
/// RedNote run and by a 登录态 login alike.
pub fn rednote_profile_lock_name(profile_path: &Path) -> String {
    format!("rednote-profile:{}", resolve_path(profile_path).display())
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::{PROFILE_SINGLETON_FILES, clear_stale_profile_locks, rednote_profile_lock_name, resolve_path};

    // py: tests/test_rednote.py::test_a_killed_chromium_does_not_lock_the_profile_forever
    #[test]
    fn a_killed_chromium_does_not_lock_the_profile_forever() {
        let profile = tempfile::tempdir().expect("a directory");
        // Chromium's SingletonLock is a symlink to `hostname-pid`, dangling once the pod is gone.
        std::os::unix::fs::symlink("old-pod-1234", profile.path().join("SingletonLock")).expect("a symlink");
        std::fs::write(profile.path().join("SingletonCookie"), "x").expect("a file");
        std::fs::write(profile.path().join("Preferences"), "{}").expect("a file");

        let cleared = clear_stale_profile_locks(profile.path()).expect("cleared");

        assert_eq!(cleared, ["SingletonLock", "SingletonCookie"]);
        for name in PROFILE_SINGLETON_FILES {
            assert!(
                std::fs::symlink_metadata(profile.path().join(name)).is_err(),
                "{name} is still there"
            );
        }
        // The profile itself is left alone.
        assert!(profile.path().join("Preferences").exists());
        assert_eq!(clear_stale_profile_locks(profile.path()).expect("cleared"), Vec::<String>::new());
    }

    #[test]
    fn a_path_is_resolved_like_pathlib_resolve() {
        let root = tempfile::tempdir().expect("a directory");
        let real = std::fs::canonicalize(root.path()).expect("canonical");
        std::os::unix::fs::symlink(&real, real.join("link")).expect("a symlink");

        assert_eq!(resolve_path(&real.join("link/missing/../profile")), real.join("profile"));
        assert_eq!(
            resolve_path(Path::new("/nonexistent-root/a/./b/../c")),
            Path::new("/nonexistent-root/a/c")
        );
        assert_eq!(
            rednote_profile_lock_name(&real.join("link/p")),
            format!("rednote-profile:{}", real.join("p").display())
        );
    }
}
