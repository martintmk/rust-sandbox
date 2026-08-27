// Licensed under the MIT License.

//! Minimal binary crate used by the project checks.

#![cfg_attr(coverage_nightly, feature(coverage_attribute))]

#[cfg_attr(coverage_nightly, coverage(off))]
fn main() {
    println!("Hello, world!");
}
