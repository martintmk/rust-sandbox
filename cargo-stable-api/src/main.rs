// Licensed under the MIT License.

//! Cargo command for checking public interface dependency stability.

#![cfg_attr(coverage_nightly, feature(coverage_attribute))]

mod check;
mod config;
mod policy;

use std::path::PathBuf;
use std::process::ExitCode;

use anyhow::Result;
use clap::{Args, Parser, Subcommand};

#[derive(Debug, Parser)]
#[command(author, version, about, bin_name = "cargo")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Check public interfaces for types from unstable third-party crates.
    StableApi(StableApiArgs),
}

#[derive(Clone, Debug, Args)]
#[expect(clippy::struct_excessive_bools, reason = "CLI flags are independently represented by clap")]
struct StableApiArgs {
    /// Path to a package or Cargo manifest.
    #[arg(long)]
    manifest_path: Option<PathBuf>,

    /// Check a package. May be specified more than once.
    #[arg(short = 'p', long = "package", value_name = "SPEC", conflicts_with = "workspace")]
    packages: Vec<String>,

    /// Check every library package in the project.
    #[arg(long)]
    workspace: bool,

    /// Activate all available features.
    #[arg(long, conflicts_with_all = ["no_default_features", "features"])]
    all_features: bool,

    /// Do not activate the default feature.
    #[arg(long, conflicts_with = "all_features")]
    no_default_features: bool,

    /// Space- or comma-separated list of features to activate.
    #[arg(
        long,
        value_delimiter = ',',
        num_args = 1..,
        conflicts_with = "all_features"
    )]
    features: Vec<String>,

    /// Build for the target triple.
    #[arg(long)]
    target: Option<String>,

    /// Validate a pre-1.0 or preview package instead of skipping it.
    #[arg(long)]
    force: bool,
}

#[cfg_attr(coverage_nightly, coverage(off))]
fn main() -> ExitCode {
    let Cli {
        command: Command::StableApi(args),
    } = Cli::parse();

    match check::run(&args) {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => ExitCode::FAILURE,
        Err(error) => {
            eprintln!("error: {error:#}");
            ExitCode::from(2)
        }
    }
}

#[cfg_attr(coverage_nightly, coverage(off))]
fn format_command_failure(operation: &str, output: &std::process::Output) -> Result<()> {
    if output.status.success() {
        return Ok(());
    }

    anyhow::bail!(
        "{operation} failed with status {status}\nstdout:\n{stdout}\nstderr:\n{stderr}",
        status = output.status,
        stdout = String::from_utf8_lossy(&output.stdout),
        stderr = String::from_utf8_lossy(&output.stderr),
    )
}
