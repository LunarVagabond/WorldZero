//! `chat.yaml` — dev-configured system channel categories
//! (docs/specs/Chat_Spec.md, "chat.yaml: dev-configured system channels").

use common::{Error, Result};
use serde::Deserialize;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ChannelScope {
    Global,
    Zone,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SystemChannelDeclaration {
    pub category: String,
    pub scope: ChannelScope,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SystemChannelConfig {
    pub schema_version: u32,
    pub system_channels: Vec<SystemChannelDeclaration>,
}

impl SystemChannelConfig {
    pub fn from_yaml(input: &str) -> Result<Self> {
        serde_yaml::from_str(input).map_err(|e| Error::wrap("chat", "failed to parse chat.yaml", e))
    }

    pub fn from_file(path: &std::path::Path) -> Result<Self> {
        let contents = std::fs::read_to_string(path)
            .map_err(|e| Error::wrap("chat", format!("failed to read {}", path.display()), e))?;
        Self::from_yaml(&contents)
    }

    /// Reads `chat.yaml` from the dev's config directory
    /// (`common::config::config_dir` — `WZ_CONFIG_DIR` or `./config`).
    pub fn from_config_dir() -> Result<Self> {
        Self::from_file(&common::config::config_dir().join("chat.yaml"))
    }

    /// Ensures every declared system channel exists — one channel for a
    /// `global`-scope category, one per `zone_id` for a `zone`-scope
    /// category. Idempotent, per [`crate::store::ChannelStore::ensure_zone_channel`].
    pub async fn ensure_channels(
        &self,
        store: &crate::store::ChannelStore,
        zone_ids: &[String],
    ) -> Result<Vec<common::id::ChannelId>> {
        let mut ids = Vec::new();

        for declared in &self.system_channels {
            match declared.scope {
                ChannelScope::Global => {
                    ids.push(
                        store
                            .ensure_zone_channel(None, &declared.category, &declared.category)
                            .await?,
                    );
                }
                ChannelScope::Zone => {
                    for zone_id in zone_ids {
                        let name = format!("{} — {zone_id}", declared.category);
                        ids.push(
                            store
                                .ensure_zone_channel(Some(zone_id), &declared.category, &name)
                                .await?,
                        );
                    }
                }
            }
        }

        Ok(ids)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_global_and_zone_scoped_categories() {
        let config = SystemChannelConfig::from_yaml(
            r#"
schema_version: 1
system_channels:
  - category: trade
    scope: global
  - category: lfg
    scope: global
  - category: local
    scope: zone
"#,
        )
        .unwrap();

        assert_eq!(config.system_channels.len(), 3);
        assert_eq!(config.system_channels[0].category, "trade");
        assert_eq!(config.system_channels[0].scope, ChannelScope::Global);
        assert_eq!(config.system_channels[2].scope, ChannelScope::Zone);
    }
}
