use std::collections::BTreeSet;

use anyhow::{Context, Result};
use cargo_metadata::Metadata;
use serde::Deserialize;

const METADATA_KEY: &str = "cargo-stable-api";

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq)]
#[serde(default, deny_unknown_fields, rename_all = "kebab-case")]
pub(crate) struct Config {
    pub(crate) allowed_unstable_crates: BTreeSet<String>,
}

impl Config {
    pub(crate) fn from_metadata(metadata: &Metadata) -> Result<Self> {
        let Some(section) = metadata.workspace_metadata.get(METADATA_KEY) else {
            return Ok(Self::default());
        };

        serde_json::from_value(section.clone()).with_context(|| {
            format!(
                "invalid [workspace.metadata.{METADATA_KEY}] configuration in {}/Cargo.toml",
                metadata.workspace_root
            )
        })
    }

    pub(crate) fn allows(&self, package_name: &str) -> bool {
        self.allowed_unstable_crates.contains(package_name)
    }
}

#[cfg(test)]
mod tests {
    use super::Config;

    #[test]
    fn deserializes_kebab_case_override() {
        let config: Config = serde_json::from_value(serde_json::json!({
            "allowed-unstable-crates": ["preview-api", "other-preview"]
        }))
        .expect("valid config");

        assert!(config.allows("preview-api"));
        assert!(config.allows("other-preview"));
        assert!(!config.allows("unlisted"));
    }

    #[test]
    fn rejects_unknown_fields() {
        let result = serde_json::from_value::<Config>(serde_json::json!({
            "allowed-crates": ["preview-api"]
        }));

        result.expect_err("unknown field should be rejected");
    }
}
