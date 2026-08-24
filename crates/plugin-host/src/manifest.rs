//! `plugin.toml` — the plugin manifest declaring what host API version a
//! plugin targets (docs/PROPOSAL.md, "Plugin Manifest & Capability
//! Declaration"). Read and checked *before* a plugin is instantiated
//! (#37's acceptance criteria), not after.
//!
//! `capabilities` (#153) is real enforcement now, not just a parsed
//! field — see `KNOWN_CAPABILITIES` below for the defined set and
//! `runtime::CapabilityGatedCallbacks` for how a declared capability
//! actually gates the host functions it covers. Per-plugin optional
//! hooks (the other half of the proposal's richer manifest story) are
//! still not built — the fixed `plugin` WIT world (#38) has every plugin
//! export all fifteen hooks unconditionally (a WIT world's exports
//! aren't individually optional in v0).

use std::path::Path;

use common::{Error, Result};
use serde::Deserialize;

/// The `worldzero:plugin` WIT package version this build implements
/// (`wit/plugin.wit`) — a plugin manifest declaring a different
/// `host_api_version` is refused at load time rather than instantiated
/// against an interface it didn't actually target.
pub const HOST_API_VERSION: &str = "0.7.0";

/// `message_type` values below this are core-reserved (auth, chat, world
/// — see docs/specs/Networking_Spec.md's catalog); a plugin declaring one
/// below the floor is refused at load time (#95).
pub const PLUGIN_MESSAGE_TYPE_FLOOR: u16 = 1000;

/// Grants `spawn-npc` (#153) — the ticket's own worked example names
/// this exact grouping, so it's kept literally rather than re-derived.
pub const CAPABILITY_SPAWNING: &str = "spawning";
/// Grants `move-entity`.
pub const CAPABILITY_MOVEMENT: &str = "movement";
/// Grants `apply-stat-delta`/`report-death`/`report-respawn` — grouped
/// together since all three exist for the same combat-outcome purpose
/// (docs/PROPOSAL.md's "Combat" hook group).
pub const CAPABILITY_COMBAT: &str = "combat";
/// Grants `grant-item`/`remove-item`/`modify-currency`.
pub const CAPABILITY_ECONOMY: &str = "economy";

/// Every capability name this build recognizes — a manifest declaring
/// anything outside this set is refused at load time (`check_capabilities`
/// below), the same "fail loudly on a plugin-authoring mistake" discipline
/// `check_message_types`/`check_chat_commands` already apply: a typo'd
/// capability name should never silently grant nothing.
pub const KNOWN_CAPABILITIES: &[&str] = &[
    CAPABILITY_SPAWNING,
    CAPABILITY_MOVEMENT,
    CAPABILITY_COMBAT,
    CAPABILITY_ECONOMY,
];

#[derive(Debug, Clone, Deserialize)]
pub struct PluginManifest {
    pub plugin: PluginDeclaration,
}

#[derive(Debug, Clone, Deserialize)]
pub struct PluginDeclaration {
    pub name: String,
    pub host_api_version: String,
    /// Which named capability groups this plugin may call host functions
    /// from (#153) — `KNOWN_CAPABILITIES` above. Strict default: an empty
    /// list (the pre-#153 default in every existing manifest) grants
    /// *none* of the gated capabilities, not all of them — a plugin only
    /// gets a gated host function if it explicitly declares the
    /// capability that covers it. A handful of host functions
    /// (`send-message`, `caller-role`, `plugin-state-get`/`-set`) are
    /// ungated regardless of `capabilities` — see
    /// `runtime::CapabilityGatedCallbacks` for why those specifically.
    #[serde(default)]
    pub capabilities: Vec<String>,
    /// `message_type` values (docs/specs/Networking_Spec.md) this plugin
    /// wants gateway-routed messages for, delivered via the `on-message`
    /// hook. Each must be `>= PLUGIN_MESSAGE_TYPE_FLOOR` and appear at
    /// most once — checked by `check_message_types` (#95). Cross-plugin
    /// collision checking doesn't exist yet: the server only ever loads
    /// one plugin today, so there's no second declared set to check
    /// against (docs/specs/Networking_Spec.md notes this as deferred).
    #[serde(default)]
    pub message_types: Vec<u16>,
    /// Chat command names (without the leading `/`) this plugin wants
    /// routed to its `on-chat-command` hook instead of published as an
    /// ordinary chat message (#57). Checked by `check_chat_commands` for
    /// emptiness, a stray leading `/`, and duplicates — the same kind of
    /// plugin-authoring mistakes `check_message_types` catches for
    /// `message_types`.
    #[serde(default)]
    pub chat_commands: Vec<String>,
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
        self.check_message_types()?;
        self.check_chat_commands()?;
        self.check_capabilities()
    }

    /// Refuses a manifest declaring a capability name outside
    /// `KNOWN_CAPABILITIES`, or the same one twice — same "fail loudly on
    /// a typo, don't silently grant nothing" discipline as
    /// `check_message_types`/`check_chat_commands` (#153).
    fn check_capabilities(&self) -> Result<()> {
        let mut seen = std::collections::HashSet::new();
        for capability in &self.plugin.capabilities {
            if !KNOWN_CAPABILITIES.contains(&capability.as_str()) {
                return Err(Error::new(
                    "plugin-host",
                    format!(
                        "plugin {:?} declares unknown capability {capability:?} — known capabilities: {KNOWN_CAPABILITIES:?}",
                        self.plugin.name
                    ),
                ));
            }
            if !seen.insert(capability.as_str()) {
                return Err(Error::new(
                    "plugin-host",
                    format!(
                        "plugin {:?} declares capability {capability:?} more than once",
                        self.plugin.name
                    ),
                ));
            }
        }
        Ok(())
    }

    /// Refuses a manifest declaring a `message_types` entry below the
    /// core-reserved floor, or the same value twice — both are plugin
    /// authoring mistakes that should fail clearly at load time rather
    /// than silently colliding with core dispatch or shadowing themselves
    /// later (#95).
    fn check_message_types(&self) -> Result<()> {
        let mut seen = std::collections::HashSet::new();
        for message_type in &self.plugin.message_types {
            if *message_type < PLUGIN_MESSAGE_TYPE_FLOOR {
                return Err(Error::new(
                    "plugin-host",
                    format!(
                        "plugin {:?} declares message_type {message_type}, below the core-reserved floor of {PLUGIN_MESSAGE_TYPE_FLOOR}",
                        self.plugin.name
                    ),
                ));
            }
            if !seen.insert(*message_type) {
                return Err(Error::new(
                    "plugin-host",
                    format!(
                        "plugin {:?} declares message_type {message_type} more than once",
                        self.plugin.name
                    ),
                ));
            }
        }
        Ok(())
    }

    /// Refuses a manifest declaring an empty command name, a name with a
    /// leading `/` (the slash is protocol punctuation, not part of the
    /// name — a plugin declaring `"/roll"` almost certainly meant
    /// `"roll"`), or the same command name twice.
    fn check_chat_commands(&self) -> Result<()> {
        let mut seen = std::collections::HashSet::new();
        for command in &self.plugin.chat_commands {
            if command.is_empty() {
                return Err(Error::new(
                    "plugin-host",
                    format!(
                        "plugin {:?} declares an empty chat_commands entry",
                        self.plugin.name
                    ),
                ));
            }
            if let Some(stripped) = command.strip_prefix('/') {
                return Err(Error::new(
                    "plugin-host",
                    format!(
                        "plugin {:?} declares chat_commands entry {command:?} with a leading '/' — did you mean {stripped:?}?",
                        self.plugin.name
                    ),
                ));
            }
            if !seen.insert(command.as_str()) {
                return Err(Error::new(
                    "plugin-host",
                    format!(
                        "plugin {:?} declares chat_commands entry {command:?} more than once",
                        self.plugin.name
                    ),
                ));
            }
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
host_api_version = "0.7.0"
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

    #[test]
    fn parses_declared_message_types() {
        let manifest = PluginManifest::from_toml(
            r#"
[plugin]
name = "example-plugin"
host_api_version = "0.7.0"
message_types = [1000, 1001]
"#,
        )
        .unwrap();

        assert_eq!(manifest.plugin.message_types, vec![1000, 1001]);
        assert!(manifest.check_compatible().is_ok());
    }

    #[test]
    fn message_type_below_the_floor_is_rejected() {
        let manifest = PluginManifest::from_toml(
            r#"
[plugin]
name = "example-plugin"
host_api_version = "0.7.0"
message_types = [200]
"#,
        )
        .unwrap();

        let err = manifest.check_compatible().unwrap_err();
        assert!(err.to_string().contains("200"), "{err}");
    }

    #[test]
    fn duplicate_message_type_is_rejected() {
        let manifest = PluginManifest::from_toml(
            r#"
[plugin]
name = "example-plugin"
host_api_version = "0.7.0"
message_types = [1000, 1000]
"#,
        )
        .unwrap();

        let err = manifest.check_compatible().unwrap_err();
        assert!(err.to_string().contains("1000"), "{err}");
    }

    #[test]
    fn parses_declared_chat_commands() {
        let manifest = PluginManifest::from_toml(
            r#"
[plugin]
name = "example-plugin"
host_api_version = "0.7.0"
chat_commands = ["roll", "whisper"]
"#,
        )
        .unwrap();

        assert_eq!(manifest.plugin.chat_commands, vec!["roll", "whisper"]);
        assert!(manifest.check_compatible().is_ok());
    }

    #[test]
    fn empty_chat_command_is_rejected() {
        let manifest = PluginManifest::from_toml(
            r#"
[plugin]
name = "example-plugin"
host_api_version = "0.7.0"
chat_commands = [""]
"#,
        )
        .unwrap();

        let err = manifest.check_compatible().unwrap_err();
        assert!(err.to_string().contains("empty"), "{err}");
    }

    #[test]
    fn chat_command_with_leading_slash_is_rejected() {
        let manifest = PluginManifest::from_toml(
            r#"
[plugin]
name = "example-plugin"
host_api_version = "0.7.0"
chat_commands = ["/roll"]
"#,
        )
        .unwrap();

        let err = manifest.check_compatible().unwrap_err();
        assert!(err.to_string().contains("/roll"), "{err}");
    }

    #[test]
    fn duplicate_chat_command_is_rejected() {
        let manifest = PluginManifest::from_toml(
            r#"
[plugin]
name = "example-plugin"
host_api_version = "0.7.0"
chat_commands = ["roll", "roll"]
"#,
        )
        .unwrap();

        let err = manifest.check_compatible().unwrap_err();
        assert!(err.to_string().contains("roll"), "{err}");
    }

    #[test]
    fn known_capabilities_are_accepted() {
        let manifest = PluginManifest::from_toml(
            r#"
[plugin]
name = "example-plugin"
host_api_version = "0.7.0"
capabilities = ["spawning", "movement", "combat", "economy"]
"#,
        )
        .unwrap();

        assert!(manifest.check_compatible().is_ok());
    }

    #[test]
    fn an_unknown_capability_is_rejected() {
        let manifest = PluginManifest::from_toml(
            r#"
[plugin]
name = "example-plugin"
host_api_version = "0.7.0"
capabilities = ["telekinesis"]
"#,
        )
        .unwrap();

        let err = manifest.check_compatible().unwrap_err();
        assert!(err.to_string().contains("telekinesis"), "{err}");
    }

    #[test]
    fn a_duplicate_capability_is_rejected() {
        let manifest = PluginManifest::from_toml(
            r#"
[plugin]
name = "example-plugin"
host_api_version = "0.7.0"
capabilities = ["economy", "economy"]
"#,
        )
        .unwrap();

        let err = manifest.check_compatible().unwrap_err();
        assert!(err.to_string().contains("economy"), "{err}");
    }

    #[test]
    fn no_declared_capabilities_is_the_strict_default() {
        let manifest = PluginManifest::from_toml(
            r#"
[plugin]
name = "example-plugin"
host_api_version = "0.7.0"
"#,
        )
        .unwrap();

        assert!(manifest.plugin.capabilities.is_empty());
        assert!(manifest.check_compatible().is_ok());
    }
}
