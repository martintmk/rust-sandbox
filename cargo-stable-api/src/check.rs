// Licensed under the MIT License.

use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};
use cargo_check_external_types::error::ValidationError;
use cargo_check_external_types::visitor::Visitor;
use cargo_metadata::{CargoOpt, Metadata, Package, Target, TargetKind};
use rustdoc_types::{Crate, FORMAT_VERSION};
use serde::Deserialize;

use crate::config::Config;
use crate::policy::Policy;
use crate::{StableApiArgs, cargo_path, format_command_failure};

const RUSTDOC_TOOLCHAIN: &str = "nightly-2026-03-20";

pub(crate) fn run(args: &StableApiArgs) -> Result<bool> {
    let metadata = load_metadata(args)?;
    let config = Config::from_metadata(&metadata)?;
    if should_skip_unstable_root(&metadata, args) {
        return Ok(true);
    }
    let packages = select_packages(&metadata, args)?;

    if packages.is_empty() {
        eprintln!("No library packages selected.");
        return Ok(true);
    }

    let mut violation_count = 0;
    for package in packages {
        eprintln!("Checking {} v{}", package.name, package.version);
        violation_count += check_package(&metadata, package, &config, args)?;
    }

    if violation_count == 0 {
        eprintln!("All checked public APIs use stable or explicitly allowed dependencies.");
        Ok(true)
    } else {
        eprintln!(
            "Found {violation_count} public API reference{} to unstable dependencies.",
            if violation_count == 1 { "" } else { "s" }
        );
        Ok(false)
    }
}

fn should_skip_unstable_root(metadata: &Metadata, args: &StableApiArgs) -> bool {
    if args.force || args.workspace || !args.packages.is_empty() {
        return false;
    }

    let Some(root_package) = metadata.root_package() else {
        return false;
    };
    let unstable = root_package.version.major == 0 || !root_package.version.pre.is_empty();
    if unstable {
        eprintln!(
            "Skipping stable API validation for {} v{}: the package is not stable yet.",
            root_package.name, root_package.version
        );
        eprintln!(
            "Stable API validation is intended for stable crates. \
             To validate this crate explicitly, rerun with `--force`."
        );
    }
    unstable
}

fn load_metadata(args: &StableApiArgs) -> Result<Metadata> {
    let mut command = cargo_metadata::MetadataCommand::new();
    if let Some(manifest_path) = &args.manifest_path {
        command.manifest_path(manifest_path);
    }
    if args.all_features {
        command.features(CargoOpt::AllFeatures);
    } else {
        if args.no_default_features {
            command.features(CargoOpt::NoDefaultFeatures);
        }
        if !args.features.is_empty() {
            command.features(CargoOpt::SomeFeatures(args.features.clone()));
        }
    }
    if let Some(target) = &args.target {
        command.other_options(vec!["--filter-platform".into(), target.clone()]);
    }

    command.exec().context("failed to read Cargo metadata")
}

fn select_packages<'a>(metadata: &'a Metadata, args: &StableApiArgs) -> Result<Vec<&'a Package>> {
    let workspace_packages = metadata.workspace_packages();
    let selected = if !args.packages.is_empty() {
        let mut selected = Vec::new();
        for spec in &args.packages {
            let matches = workspace_packages
                .iter()
                .copied()
                .filter(|package| package.name == *spec || format!("{}@{}", package.name, package.version) == *spec)
                .collect::<Vec<_>>();
            match matches.as_slice() {
                [] => bail!("package `{spec}` is not a member of this workspace"),
                [package] => selected.push(*package),
                _ => bail!("package specification `{spec}` is ambiguous"),
            }
        }
        selected
    } else if args.workspace {
        workspace_packages
    } else if let Some(root_package) = metadata.root_package() {
        vec![root_package]
    } else {
        metadata.workspace_default_packages()
    };

    let explicitly_selected = !args.packages.is_empty();
    let mut seen = HashSet::new();
    let mut libraries = Vec::new();
    for package in selected {
        if library_target(package).is_some() {
            if seen.insert(&package.id) {
                libraries.push(package);
            }
        } else if explicitly_selected {
            bail!("package `{}` does not define a library target", package.name);
        }
    }
    libraries.sort_by(|left, right| left.name.cmp(&right.name));
    Ok(libraries)
}

fn library_target(package: &Package) -> Option<&Target> {
    package.targets.iter().find(|target| target.kind.contains(&TargetKind::Lib))
}

fn check_package(metadata: &Metadata, package: &Package, config: &Config, args: &StableApiArgs) -> Result<usize> {
    let target = library_target(package).expect("selected packages have library targets");
    let policy = Policy::for_package(metadata, package, config);
    let rustdoc = generate_rustdoc_json(metadata, package, target, args)?;
    let errors = Visitor::new(policy.external_types_config(), rustdoc)
        .context("failed to initialize public API visitor")?
        .visit_all()
        .context("failed to inspect public API")?;

    let mut violations = 0;
    for error in errors.iter() {
        match error {
            ValidationError::UnapprovedExternalTypeRef { .. } => {
                print_violation(error, &policy);
                violations += 1;
            }
            ValidationError::UnusedApprovalPattern { .. } => {}
            _ => print_warning(error),
        }
    }
    Ok(violations)
}

#[derive(Deserialize)]
struct RustdocFormatVersion {
    format_version: u32,
}

fn generate_rustdoc_json(metadata: &Metadata, package: &Package, target: &Target, args: &StableApiArgs) -> Result<Crate> {
    let mut command = Command::new(cargo_path());
    command
        .current_dir(&metadata.workspace_root)
        .env("RUSTUP_TOOLCHAIN", RUSTDOC_TOOLCHAIN)
        .env_remove("RUSTC_WRAPPER")
        .env_remove("RUSTC_WORKSPACE_WRAPPER")
        .arg("rustdoc")
        .arg("--manifest-path")
        .arg(&package.manifest_path)
        .arg("--lib");
    if args.all_features {
        command.arg("--all-features");
    }
    if args.no_default_features {
        command.arg("--no-default-features");
    }
    if !args.features.is_empty() {
        command.arg("--features").arg(args.features.join(","));
    }
    if let Some(build_target) = &args.target {
        command.arg("--target").arg(build_target);
    }
    command.args([
        "--",
        "--document-private-items",
        "-Z",
        "unstable-options",
        "--output-format",
        "json",
    ]);

    let output = command
        .output()
        .with_context(|| format!("failed to run rustdoc for `{}`", package.name))?;
    format_command_failure("rustdoc", &output)?;

    let output_path = rustdoc_output_path(metadata, target, args.target.as_deref());
    let json = fs::read_to_string(&output_path).with_context(|| format!("failed to read rustdoc JSON at {}", output_path.display()))?;
    let version: RustdocFormatVersion = serde_json::from_str(&json).context("rustdoc JSON has no format_version")?;
    if version.format_version != FORMAT_VERSION {
        bail!(
            "rustdoc produced JSON format version {}, but this tool requires version {}. \
             Run it with a compatible nightly toolchain (for example, \
             `cargo +{RUSTDOC_TOOLCHAIN} stable-api`)",
            version.format_version,
            FORMAT_VERSION
        );
    }

    serde_json::from_str(&json).context("failed to parse rustdoc JSON")
}

fn rustdoc_output_path(metadata: &Metadata, target: &Target, build_target: Option<&str>) -> PathBuf {
    let mut output = PathBuf::from(metadata.target_directory.as_std_path());
    if let Some(build_target) = build_target {
        output.push(build_target);
    }
    output.push("doc");
    output.push(format!("{}.json", target.name.replace('-', "_")));
    output
}

fn print_violation(error: &ValidationError, policy: &Policy) {
    let ValidationError::UnapprovedExternalTypeRef {
        type_name,
        what,
        in_what_type,
        location,
        ..
    } = error
    else {
        return;
    };

    eprintln!("error: unstable dependency type `{type_name}` is exposed in the public API");
    print_location(location.as_ref());
    eprintln!("  = in {what} `{in_what_type}`");
    let unstable_packages = policy.unstable_packages(type_name);
    if unstable_packages.is_empty() {
        eprintln!("  = the external crate could not be matched to a stable resolved package");
    } else {
        for package in unstable_packages {
            eprintln!("  = package `{}` v{} is pre-1.0 or a prerelease", package.name, package.version);
        }
        if unstable_packages.len() == 1 {
            eprintln!("  = allow intentionally in the workspace Cargo.toml:");
            eprintln!(
                "      [workspace.metadata.cargo-stable-api]\n      \
                 allowed-unstable-crates = [\"{}\"]",
                unstable_packages[0].name
            );
        }
    }
    eprintln!();
}

fn print_warning(error: &ValidationError) {
    eprintln!("warning: {error}");
    print_location(error.location());
    let subtext = error.subtext();
    if !subtext.is_empty() {
        eprintln!("  = {subtext}");
    }
    eprintln!();
}

fn print_location(location: Option<&rustdoc_types::Span>) {
    if let Some(location) = location {
        eprintln!(
            "  --> {}:{}:{}",
            display_path(&location.filename),
            location.begin.0,
            location.begin.1
        );
    }
}

fn display_path(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

#[cfg(test)]
mod tests {
    #[cfg(not(miri))]
    use cargo_metadata::MetadataCommand;

    use super::rustdoc_output_path;

    #[test]
    #[cfg(not(miri))]
    fn rustdoc_output_uses_target_subdirectory() {
        let metadata = MetadataCommand::new().no_deps().exec().expect("workspace metadata");
        let package = metadata
            .packages
            .iter()
            .find(|package| package.name == "cargo-stable-api")
            .expect("tool package");
        let target = package
            .targets
            .iter()
            .find(|target| target.name == "cargo-stable-api")
            .expect("binary target");

        let output = rustdoc_output_path(&metadata, target, Some("example-target"));

        assert!(output.ends_with("example-target/doc/cargo_stable_api.json"));
    }
}
