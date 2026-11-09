//! `fav`: one binary for the API, the worker, or both.
//!
//! Commands arrive with their work packages (`rust/docs/PLAN.md`): `worker` and
//! `trigger` in WP2, `api` and `serve` (the default) in WP3. Until then the
//! binary only reports its version and the Python baseline it ports.

use std::sync::LazyLock;

use clap::Parser;

static VERSION: LazyLock<String> = LazyLock::new(version);

#[derive(Debug, Parser)]
#[command(name = "fav", version = VERSION.as_str(), arg_required_else_help = true, about = "Crawls content sources on a schedule and archives their media")]
struct Cli {}

fn version() -> String {
    format!("{} (python baseline {})", env!("CARGO_PKG_VERSION"), fav_core::BASELINE_COMMIT)
}

fn main() {
    let Cli {} = Cli::parse();
}

#[cfg(test)]
mod tests {
    use clap::CommandFactory;

    use super::Cli;

    #[test]
    fn cli_definition_is_consistent() {
        Cli::command().debug_assert();
    }

    #[test]
    fn version_names_the_baseline() {
        let version = super::version();
        assert!(version.contains(fav_core::BASELINE_COMMIT), "{version}");
    }
}
