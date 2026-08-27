# cargo-stable-api ![License: MIT](https://img.shields.io/badge/license-MIT-blue) [![cargo-stable-api on crates.io](https://img.shields.io/crates/v/cargo-stable-api)](https://crates.io/crates/cargo-stable-api) [![cargo-stable-api on docs.rs](https://docs.rs/cargo-stable-api/badge.svg)](https://docs.rs/cargo-stable-api) [![Source Code Repository](https://img.shields.io/badge/Code-On%20GitHub-blue?logo=GitHub)](https://github.com/martintmk/rust-sandbox)

## cargo-stable-api

`cargo-stable-api` checks that library crates do not expose types from
unstable third-party dependencies in their public API. A dependency version
is stable when it is at least `1.0.0` and is not a prerelease. Workspace
members are treated as first-party crates.

The checker uses rustdoc JSON, so it must run with a nightly toolchain
compatible with its `rustdoc-types` version:

```console
cargo install --path cargo-stable-api
cargo +nightly-2026-03-20 stable-api --workspace
```

Check one package with `-p` or use `--manifest-path` to select another
workspace:

```console
cargo +nightly-2026-03-20 stable-api -p my-library
```

By default, running the command from a pre-1.0 or prerelease crate skips
validation because the crate’s own API is not stable yet. Use `--force` to
validate it explicitly:

```console
cargo +nightly-2026-03-20 stable-api --force
```

### Allowing an unstable dependency

Intentional exceptions are package names in the workspace `Cargo.toml`:

```toml
[workspace.metadata.cargo-stable-api]
allowed-unstable-crates = ["experimental-dependency"]
```

The exception applies to every workspace package and every resolved version
of the named dependency.
