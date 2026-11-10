//! Guards on the Rust image (`rust/Dockerfile`), ported from Python's guards on its own
//! image and on how the two downloaders are declared.
//!
//! Python origin: `tests/test_dependencies.py` and
//! `tests/test_nikke_layer_metadata_script.py::test_dockerfile_copies_layer_metadata_script_and_runs_playwright_smoke`.
//! The Rust image takes yt-dlp and gallery-dl from the Python app's `pyproject.toml` and
//! `uv.lock` (exported as constraints), so the parts worth pinning are the same silent
//! failures: a dropped extra, a downloader that leaves the lockfile, a second copy of a
//! tool on `PATH`, and an image that ships without the Chromium the browser code launches.

use std::path::PathBuf;

fn repo_file(relative: &str) -> String {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../..").join(relative);
    std::fs::read_to_string(&path).unwrap_or_else(|error| panic!("{}: {error}", path.display()))
}

fn dockerfile() -> String {
    repo_file("rust/Dockerfile")
}

/// `project.dependencies` of the Python app, as written in `pyproject.toml`.
fn dependencies() -> Vec<String> {
    let pyproject = repo_file("pyproject.toml");
    let start = pyproject.find("\ndependencies = [").expect("a dependencies list") + 1;
    let end = start + pyproject[start..].find("\n]").expect("the end of the list");
    pyproject[start..end]
        .lines()
        .skip(1)
        .map(str::trim)
        .filter(|line| line.starts_with('"'))
        .map(|line| line.trim_end_matches(',').trim_matches('"').to_owned())
        .collect()
}

/// `dependency.split('[')[0].split('>')[0].split('=')[0].strip()`.
fn name_of(dependency: &str) -> &str {
    dependency
        .split('[')
        .next()
        .unwrap_or_default()
        .split('>')
        .next()
        .unwrap_or_default()
        .split('=')
        .next()
        .unwrap_or_default()
        .trim()
}

fn requirement(name: &str) -> String {
    dependencies()
        .into_iter()
        .find(|dependency| name_of(dependency) == name)
        .unwrap_or_else(|| panic!("{name} is not declared in pyproject.toml"))
}

/// Plain `yt-dlp` declares no dependencies at all: dropping the extra installs cleanly and
/// silently leaves out pycryptodomex, websockets, certifi, requests/urllib3 and yt-dlp-ejs,
/// which nothing misses until a download needs one (encrypted HLS, in Hanime1's case). The
/// Rust image installs the same requirement.
// py: tests/test_dependencies.py::test_yt_dlp_keeps_its_default_extra
#[test]
fn yt_dlp_keeps_its_default_extra() {
    assert!(requirement("yt-dlp").contains("[default]"));
    assert!(dockerfile().contains("'yt-dlp[default]' gallery-dl"));
}

/// If either downloader were reinstated as an image-level binary, its version would leave
/// the lockfile and Dependabot could no longer upgrade it: the Rust image installs both
/// from the Python app's `uv.lock`, exported as constraints.
// py: tests/test_dependencies.py::test_both_downloaders_are_uv_dependencies
#[test]
fn both_downloaders_are_uv_dependencies() {
    let declared: Vec<String> = dependencies().iter().map(|dependency| name_of(dependency).to_owned()).collect();
    assert!(declared.iter().any(|name| name == "yt-dlp"), "{declared:?}");
    assert!(declared.iter().any(|name| name == "gallery-dl"), "{declared:?}");

    let dockerfile = dockerfile();
    assert!(dockerfile.contains("COPY pyproject.toml uv.lock ./"));
    assert!(dockerfile.contains("uv export --frozen --no-dev"));
    assert!(dockerfile.contains("--constraint constraints.txt"));
}

/// Two sources of truth for one tool: whichever comes first on `PATH` wins, and the
/// lockfile would no longer describe what runs. ffmpeg is bundled by neither Python
/// package and still comes from apt.
// py: tests/test_dependencies.py::test_the_image_does_not_also_install_a_yt_dlp_binary
#[test]
fn the_image_does_not_also_install_a_yt_dlp_binary() {
    let dockerfile = dockerfile();

    assert!(!dockerfile.contains("yt-dlp_linux"));
    assert!(!dockerfile.contains("/usr/local/bin/yt-dlp"));
    assert!(dockerfile.contains("ffmpeg"));
}

/// The image installs Playwright's own Chromium build and proves it starts, and carries the
/// layer-metadata tool: Python copies `script/` and byte-compiles it; here the tool is the
/// binary's `nikke-layers` command, which the build runs.
// py: tests/test_nikke_layer_metadata_script.py::test_dockerfile_copies_layer_metadata_script_and_runs_playwright_smoke
#[test]
fn dockerfile_ships_the_layer_metadata_tool_and_runs_a_chromium_smoke() {
    let dockerfile = dockerfile();

    assert!(dockerfile.contains("-m playwright install --with-deps chromium"));
    assert!(dockerfile.contains("PLAYWRIGHT_BROWSERS_PATH=/ms-playwright"));
    assert!(dockerfile.contains("--dump-dom 'data:text/html,<main>ok</main>' | grep -q '<main>ok</main>'"));
    assert!(dockerfile.contains("COPY --from=rust-builder /out/fav /usr/local/bin/fav"));
    assert!(dockerfile.contains("fav nikke-layers --help"));
}

/// The image's Chromium is the revision the browser code launches: `uv.lock` pins the
/// Playwright whose installer the Dockerfile runs, `fav_browser::chromium` names that
/// Playwright and its Chromium revision, and the Dockerfile's launch check uses the same
/// revision. A Playwright bump in `uv.lock` alone fails here instead of at the first login.
#[test]
fn the_image_chromium_is_the_revision_fav_launches() {
    let lock = repo_file("uv.lock");
    let start = lock.find("\nname = \"playwright\"\n").expect("playwright in uv.lock");
    let version = lock[start..]
        .lines()
        .find_map(|line| line.strip_prefix("version = \""))
        .and_then(|rest| rest.strip_suffix('"'))
        .expect("playwright's version");
    assert_eq!(version, fav_browser::chromium::PLAYWRIGHT_VERSION);
    let path = format!("/ms-playwright/chromium-{}/", fav_browser::chromium::CHROMIUM_REVISION);
    assert!(dockerfile().contains(&path), "the Dockerfile checks {path}");
}
