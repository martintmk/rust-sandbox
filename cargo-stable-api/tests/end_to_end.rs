use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn run(package: &str) -> Output {
    let manifest = fixture_manifest();
    run_with_manifest(&manifest, ["--package", package])
}

fn run_with_manifest<const N: usize>(manifest: &Path, args: [&str; N]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_cargo-stable-api"))
        .arg("stable-api")
        .arg("--manifest-path")
        .arg(manifest.to_str().expect("UTF-8 fixture path"))
        .args(args)
        .output()
        .expect("run cargo-stable-api")
}

fn fixture_manifest() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("workspace")
        .join("Cargo.toml")
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

#[test]
fn accepts_stable_dependency_type() {
    let output = run("stable-app");

    assert!(output.status.success(), "{}", stderr(&output));
}

#[test]
fn rejects_unstable_dependency_type() {
    let output = run("unstable-app");
    let stderr = stderr(&output);

    assert_eq!(output.status.code(), Some(1), "{stderr}");
    assert!(
        stderr.contains("package `unstable-dep` v0.2.0 is pre-1.0 or a prerelease"),
        "{stderr}"
    );
}

#[test]
fn accepts_workspace_override() {
    let output = run("allowed-app");

    assert!(output.status.success(), "{}", stderr(&output));
}

#[test]
fn accepts_unstable_workspace_dependency_type() {
    let output = run("first-party-app");

    assert!(output.status.success(), "{}", stderr(&output));
}

#[test]
fn skips_unstable_root_package_by_default() {
    let manifest = unstable_manifest();
    let output = run_with_manifest(&manifest, []);
    let stderr = stderr(&output);

    assert!(output.status.success(), "{stderr}");
    assert!(
        stderr.contains("Skipping stable API validation for unstable-app v0.1.0"),
        "{stderr}"
    );
    assert!(stderr.contains("rerun with `--force`"), "{stderr}");
    assert!(!stderr.contains("Checking unstable-app"), "{stderr}");
}

#[test]
fn force_validates_unstable_root_package() {
    let manifest = unstable_manifest();
    let output = run_with_manifest(&manifest, ["--force"]);
    let stderr = stderr(&output);

    assert_eq!(output.status.code(), Some(1), "{stderr}");
    assert!(stderr.contains("Checking unstable-app v0.1.0"), "{stderr}");
    assert!(stderr.contains("unstable-dep"), "{stderr}");
}

fn unstable_manifest() -> PathBuf {
    fixture_manifest()
        .parent()
        .expect("fixture workspace path")
        .join("unstable-app")
        .join("Cargo.toml")
}
