// Licensed under the MIT License.

//! # cargo-stable-api
//!
//! `cargo-stable-api` checks that library crates do not expose types from
//! unstable third-party dependencies in their public API. A dependency version
//! is stable when it is at least `1.0.0` and is not a pre-release. Project
//! members are treated as first-party crates.
//!
//! The checker uses Rust documentation data, so it must run with a nightly
//! compiler
//! compatible with its `rustdoc-types` version:
//!
//! ```console
//! cargo install --path cargo-stable-api
//! cargo +nightly-2026-03-20 stable-api --workspace
//! ```
//!
//! Check one package with `-p` or use `--manifest-path` to select another
//! project:
//!
//! ```console
//! cargo +nightly-2026-03-20 stable-api -p my-library
//! ```
//!
//! By default, running the command from a pre-1.0 or pre-release crate skips
//! validation because the crate's own API is not stable yet. Use `--force` to
//! validate it explicitly:
//!
//! ```console
//! cargo +nightly-2026-03-20 stable-api --force
//! ```
//!
//! ## Allowing an unstable dependency
//!
//! Intentional exceptions are package names in the project `Cargo.toml`:
//!
//! ```toml
//! [workspace.metadata.cargo-stable-api]
//! allowed-unstable-crates = ["experimental-dependency"]
//! ```
//!
//! The exception applies to every project package and every resolved version
//! of the named dependency.
