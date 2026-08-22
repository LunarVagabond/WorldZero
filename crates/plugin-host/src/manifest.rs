//! `plugin.toml` — the plugin manifest declaring what host API version a
//! plugin targets (docs/PROPOSAL.md, "Plugin Manifest & Capability
//! Declaration"). Read and checked *before* a plugin is instantiated
//! (#37's acceptance criteria), not after.
//!
//! The full manifest story in the proposal — per-plugin optional hooks,
//! gated host-function capability groups (`economy`, `combat`, ...) — is
//! richer than this v0 slice needs: the fixed `plugin` WIT world (#38)
//! has exactly four hooks and every plugin must export all of them (a
//! WIT world's exports aren't individually optional), and the v0 `host`
//! interface has only two ungated functions, no capability groups yet.
//! `capabilities` is still parsed and carried here so a real capability
//! system has a stable place to land later without another manifest
//! format change — just not enforced against anything yet.

use std::path::Path;

use common::{Error, Result};
use serde::Deserialize;

/// The `worldzero:plugin` WIT package version this build implements
/// (`wit/plugin.wit`) — a plugin manifest declaring a different
/// `host_api_version` is refused at load time rather than instantiated
/// against an interface it didn't actually target.
pub const HOST_API_VERSION: &str = "0.1.0";

#[derive(Debug, Clone, Deserialize)]
pub struct PluginManifest {
    pub plugin: PluginDeclaration,
}

#[derive(Debug, Clone, Deserialize)]
pub struct PluginDeclaration {
    pub name: String,
    pub host_api_version: String,
    /// Declared, not yet enforced — see module docs.
    #[serde(default)]
    pub capabilities: Vec<String>,
}

impl PluginManifest {
    pub fn from_toml(input: &str) -> Result<Self> {
        toml::from_str(input)
            .map_err(|e| Error::wrap("plugin-host", "failed to parse plugin.toml", e))
    }

    pub fn from_file(path: &Path) -> Result<Self> {
        let contents = std::fs::read_to_string(path).map_err(|e| {
            Error::wrap(
                "plugin-host",
                format!("failed to read {}", path.display()),
                e,
            )
        })?;
        Self::from_toml(&contents)
    }

    /// Refuses a manifest declaring a `host_api_version` this build
    /// doesn't implement — a plugin built against a future or unrelated
    /// interface version should fail clearly here, not fail obscurely
    /// during instantiation (or worse, silently link against the wrong
    /// interface shape).
    pub fn check_compatible(&self) -> Result<()> {
        if self.plugin.host_api_version != HOST_API_VERSION {
            return Err(Error::new(
                "plugin-host",
                format!(
                    "plugin {:?} targets host_api_version {:?}, this build implements {HOST_API_VERSION:?}",
                    self.plugin.name, self.plugin.host_api_version
                ),
            ));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_minimal_manifest() {
        let manifest = PluginManifest::from_toml(
            r#"
[plugin]
name = "example-plugin"
host_api_version = "0.1.0"
"#,
        )
        .unwrap();

        assert_eq!(manifest.plugin.name, "example-plugin");
        assert!(manifest.plugin.capabilities.is_empty());
    }

    #[test]
    fn parses_declared_capabilities() {
        let manifest = PluginManifest::from_toml(
            r#"
[plugin]
name = "example-plugin"
host_api_version = "0.1.0"
capabilities = ["economy", "combat"]
"#,
        )
        .unwrap();

        assert_eq!(manifest.plugin.capabilities, vec!["economy", "combat"]);
    }

    #[test]
    fn matching_host_api_version_is_compatible() {
        let manifest = PluginManifest::from_toml(
            r#"
[plugin]
name = "example-plugin"
host_api_version = "0.1.0"
"#,
        )
        .unwrap();
        assert!(manifest.check_compatible().is_ok());
    }

    #[test]
    fn mismatched_host_api_version_is_rejected() {
        let manifest = PluginManifest::from_toml(
            r#"
[plugin]
name = "example-plugin"
host_api_version = "99.0.0"
"#,
        )
        .unwrap();
        let err = manifest.check_compatible().unwrap_err();
        assert!(err.to_string().contains("99.0.0"), "{err}");
    }
}
