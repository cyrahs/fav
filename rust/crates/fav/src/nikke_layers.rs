//! `fav nikke-layers`: the Nikke layer metadata commands
//! (`python script/nikke_layer_metadata.py <command>`), with the same arguments.
//!
//! The commands touch only the archive's files, so they need neither the environment
//! nor the database.

use std::io::Write;
use std::path::PathBuf;
use std::sync::Arc;

use clap::{Args, Subcommand};
use fav_core::pyjson::Value;
use fav_sources::PyError;
use fav_sources::nikke::layer_tool::{self, LayerTool, MergeArgs, MergeDirArgs, Outcome, RuntimeCaptureArgs, StripArgs, ValidateArgs};
use fav_sources::nikke::runtime::UnavailableCapturer;

const DEFAULT_ROOT: &str = "./collection/nikke";

/// Merge, validate, or strip NIKKE GameKee Live2D layer metadata.
#[derive(Debug, Subcommand)]
pub(crate) enum NikkeLayers {
    /// Merge a GameKee runtime layer capture into manifest.json and character.json.
    Merge {
        /// NIKKE collection root.
        #[arg(long, default_value = DEFAULT_ROOT)]
        nikke_root: PathBuf,
        /// Specific character directory containing manifest.json.
        #[arg(long)]
        character_root: Option<PathBuf>,
        /// Content id used to find the character directory.
        #[arg(long)]
        content_id: Option<i128>,
        /// Layer capture JSON file.
        #[arg(long)]
        capture: PathBuf,
        /// Write changes. Without this flag the command is a dry-run.
        #[arg(long)]
        write: bool,
        /// Directory for pre-write backups. Defaults under the NIKKE root.
        #[arg(long)]
        backup_dir: Option<PathBuf>,
    },
    /// Merge every capture JSON in a directory.
    MergeDir {
        /// NIKKE collection root.
        #[arg(long, default_value = DEFAULT_ROOT)]
        nikke_root: PathBuf,
        /// Directory containing layer capture JSON files.
        #[arg(long)]
        capture_dir: PathBuf,
        /// Glob pattern inside --capture-dir.
        #[arg(long, default_value = "*.json")]
        pattern: String,
        /// Write changes. Without this flag the command is a dry-run.
        #[arg(long)]
        write: bool,
        /// Directory for pre-write backups. Defaults under the NIKKE root.
        #[arg(long)]
        backup_dir: Option<PathBuf>,
    },
    /// Validate layer metadata quality gates.
    Validate {
        /// NIKKE collection root.
        #[arg(long, default_value = DEFAULT_ROOT)]
        nikke_root: PathBuf,
        /// Specific character directory to validate.
        #[arg(long)]
        character_root: Vec<PathBuf>,
        /// Content id to validate. Can be passed multiple times.
        #[arg(long)]
        content_id: Vec<i128>,
        /// Return exit code 0 even when validation errors are found.
        #[arg(long)]
        no_fail: bool,
    },
    /// Remove layer metadata fields for rollback.
    Strip {
        /// NIKKE collection root.
        #[arg(long, default_value = DEFAULT_ROOT)]
        nikke_root: PathBuf,
        /// Specific character directory containing manifest.json.
        #[arg(long)]
        character_root: Option<PathBuf>,
        /// Content id used to find the character directory.
        #[arg(long)]
        content_id: Option<i128>,
        /// Write changes. Without this flag the command is a dry-run.
        #[arg(long)]
        write: bool,
        /// Directory for pre-write backups. Defaults under the NIKKE root.
        #[arg(long)]
        backup_dir: Option<PathBuf>,
    },
    /// Capture missing, incomplete, or changed runtime layer metadata for multi-full NIKKE manifests.
    CaptureMissing(RuntimeCaptureOptions),
    /// Controlled historical backfill for missing, incomplete, or changed runtime layer metadata.
    BackfillRuntimeLayers {
        #[command(flatten)]
        options: RuntimeCaptureOptions,
        /// Capture even when a previous successful capture is reusable.
        #[arg(long)]
        force_refresh: bool,
    },
}

/// `add_runtime_capture_args`.
#[derive(Debug, Clone, Args)]
pub(crate) struct RuntimeCaptureOptions {
    /// NIKKE collection root.
    #[arg(long, default_value = DEFAULT_ROOT)]
    nikke_root: PathBuf,
    /// Specific character directory to process.
    #[arg(long)]
    character_root: Vec<PathBuf>,
    /// Content id to process. Can be passed multiple times.
    #[arg(long)]
    content_id: Vec<i128>,
    /// Write changes and launch Chromium. Without this flag the command is a dry-run.
    #[arg(long)]
    write: bool,
    /// Directory for pre-write backups. Defaults under the NIKKE root.
    #[arg(long)]
    backup_dir: Option<PathBuf>,
    /// Maximum number of captures/reuses to process. 0 means no limit.
    #[arg(long, default_value_t = 0, allow_negative_numbers = true)]
    limit: i64,
    /// Runtime capture timeout per page.
    #[arg(long, default_value_t = 60.0)]
    timeout_seconds: f64,
    /// Run Chromium headfully for debugging.
    #[arg(long)]
    headful: bool,
    /// Return exit code 0 even when captures or merges fail.
    #[arg(long)]
    no_fail: bool,
}

impl RuntimeCaptureOptions {
    fn into_args(self) -> RuntimeCaptureArgs {
        RuntimeCaptureArgs {
            nikke_root: self.nikke_root,
            character_root: self.character_root,
            content_id: self.content_id,
            write: self.write,
            backup_dir: self.backup_dir,
            limit: self.limit,
            timeout_seconds: self.timeout_seconds,
            headful: self.headful,
            no_fail: self.no_fail,
        }
    }
}

/// `main()`: run the command with `tool`, print its report, and return its exit status.
/// What the script raises goes to `err`: a `SystemExit`'s message, or the error.
pub(crate) async fn run(command: NikkeLayers, tool: &LayerTool, out: &mut dyn Write, err: &mut dyn Write) -> u8 {
    let outcome: Result<Outcome, PyError> = match command {
        NikkeLayers::Merge {
            nikke_root,
            character_root,
            content_id,
            capture,
            write,
            backup_dir,
        } => layer_tool::command_merge(&MergeArgs {
            nikke_root,
            character_root,
            content_id,
            capture,
            write,
            backup_dir,
        }),
        NikkeLayers::MergeDir {
            nikke_root,
            capture_dir,
            pattern,
            write,
            backup_dir,
        } => layer_tool::command_merge_dir(&MergeDirArgs {
            nikke_root,
            capture_dir,
            pattern,
            write,
            backup_dir,
        }),
        NikkeLayers::Validate {
            nikke_root,
            character_root,
            content_id,
            no_fail,
        } => layer_tool::command_validate(&ValidateArgs {
            nikke_root,
            character_root,
            content_id,
            no_fail,
        }),
        NikkeLayers::Strip {
            nikke_root,
            character_root,
            content_id,
            write,
            backup_dir,
        } => layer_tool::command_strip(&StripArgs {
            nikke_root,
            character_root,
            content_id,
            write,
            backup_dir,
        }),
        NikkeLayers::CaptureMissing(options) => tool.command_runtime_capture(&options.into_args(), false).await,
        NikkeLayers::BackfillRuntimeLayers { options, force_refresh } => {
            tool.command_runtime_capture(&options.into_args(), force_refresh).await
        }
    };
    // Nothing useful is left to do when stdout or stderr is gone.
    match outcome {
        Ok(Outcome { report, status }) => {
            let _ = out.write_all(fav_sources::nikke_layer_metadata::pretty_json(&Value::Dict(report)).as_bytes());
            status
        }
        Err(error) if error.class == "SystemExit" => {
            let _ = writeln!(err, "{}", error.message);
            1
        }
        Err(error) => {
            let _ = writeln!(err, "{}: {}", error.class, error.message);
            1
        }
    }
}

/// The production tool: runtime capture is WP17's, so it reports itself unavailable.
pub(crate) fn production_tool() -> LayerTool {
    LayerTool::new(Arc::new(UnavailableCapturer))
}

#[cfg(test)]
mod tests;
