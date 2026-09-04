//! `<config_dir>/game.yaml` (#271) — the one file naming this game and
//! declaring this deployment's default for each optional-system toggle
//! that already has a real env-var mechanism elsewhere
//! (`WZ_SERVICE_CHAT_ENABLED`, `WZ_CHAT_PERSISTENCE_ENABLED`,
//! `WZ_SERVICE_METRICS_ENABLED`). Required, like `stats.schema.yaml` —
//! copied by `make quickstart` from `game.example.yaml`, and `server`
//! panics at startup if it's missing, same discipline as every other
//! required config file.
//!
//! **`game_name`** is metadata only today — logged once at startup
//! (`worldzero server starting` — search `main.rs`'s own call site) —
//! not consumed by any wire protocol or client-facing behavior. A real
//! client-visible use (e.g. echoing it somewhere in the auth/realm
//! handshake) is a natural follow-up, not built here.
//!
//! **`systems`** only ever supplies a *default*: an explicitly-set env
//! var always wins over whatever this file declares, the same "an
//! operator's runtime override beats any checked-in default" discipline
//! `common::config::ServicesConfig::from_env` already has — see
//! `ServicesConfig::from_env_with_defaults`/`chat::persistence_enabled`,
//! the two call sites this file's parsed values actually feed. A field
//! this struct doesn't declare (or the whole file being absent-but-not-
//! required in some future relaxation) falls back to that same
//! `true`/`true`/`false` default every deployment already had before
//! this file existed — nothing about adding `game.yaml` changes default
//! behavior for a deployment that doesn't customize it.
//!
//! Deliberately does **not** cover `party.schema.yaml`/
//! `guild.schema.yaml`/`crafting.schema.yaml`/`currency.schema.yaml`'s
//! systems — those have no enable/disable mechanism of their own today
//! (they're always-on, data-driven by their schema file's presence, not
//! gated behind a toggle); adding one is a bigger, separate ask than
//! this file's initial scope.

use std::path::Path;

use common::{Error, Result};
use serde::Deserialize;

const SUPPORTED_SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Clone, Deserialize)]
pub struct GameManifest {
    pub schema_version: u32,
    pub game_name: String,
    #[serde(default)]
    pub systems: Systems,
}

/// Every field's default matches the corresponding env var's own
/// hardcoded default (`ServicesConfig::default`, `WZ_CHAT_PERSISTENCE_ENABLED`'s
/// `false`) — declaring `systems` at all, or any field under it, is
/// optional, and omitting one is never distinguishable on the wire from
/// "this deployment wants the ordinary default."
#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(default)]
pub struct Systems {
    pub chat: bool,
    pub chat_persistence: bool,
    pub metrics: bool,
}

impl Default for Systems {
    fn default() -> Self {
        Self {
            chat: true,
            chat_persistence: false,
            metrics: true,
        }
    }
}

impl GameManifest {
    pub fn from_yaml(input: &str) -> Result<Self> {
        let manifest: Self = serde_yaml::from_str(input)
            .map_err(|e| Error::wrap("server", "failed to parse game manifest", e))?;

        if manifest.schema_version != SUPPORTED_SCHEMA_VERSION {
            return Err(Error::new(
                "server",
                format!(
                    "game.yaml: unsupported schema_version {} (this build understands {SUPPORTED_SCHEMA_VERSION})",
                    manifest.schema_version
                ),
            ));
        }
        if manifest.game_name.trim().is_empty() {
            return Err(Error::new(
                "server",
                "game.yaml: game_name must not be empty",
            ));
        }

        Ok(manifest)
    }

    pub fn from_file(path: &Path) -> Result<Self> {
        let contents = std::fs::read_to_string(path)
            .map_err(|e| Error::wrap("server", format!("failed to read {}", path.display()), e))?;
        Self::from_yaml(&contents)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_minimal_manifest_with_default_systems() {
        let manifest = GameManifest::from_yaml(
            r#"
schema_version: 1
game_name: "My Game"
"#,
        )
        .unwrap();
        assert_eq!(manifest.game_name, "My Game");
        assert!(manifest.systems.chat);
        assert!(!manifest.systems.chat_persistence);
        assert!(manifest.systems.metrics);
    }

    #[test]
    fn parses_explicitly_declared_systems() {
        let manifest = GameManifest::from_yaml(
            r#"
schema_version: 1
game_name: "My Game"
systems:
  chat: false
  chat_persistence: true
  metrics: false
"#,
        )
        .unwrap();
        assert!(!manifest.systems.chat);
        assert!(manifest.systems.chat_persistence);
        assert!(!manifest.systems.metrics);
    }

    #[test]
    fn an_unsupported_schema_version_is_rejected() {
        let err = GameManifest::from_yaml(
            r#"
schema_version: 99
game_name: "My Game"
"#,
        )
        .unwrap_err();
        assert!(
            err.to_string().contains("unsupported schema_version 99"),
            "{err}"
        );
    }

    #[test]
    fn an_empty_game_name_is_rejected() {
        let err = GameManifest::from_yaml(
            r#"
schema_version: 1
game_name: "   "
"#,
        )
        .unwrap_err();
        assert!(err.to_string().contains("game_name"), "{err}");
    }

    #[test]
    fn a_missing_game_name_is_a_parse_error() {
        let err = GameManifest::from_yaml("schema_version: 1\n").unwrap_err();
        assert!(err.to_string().contains("failed to parse"), "{err}");
    }
}
