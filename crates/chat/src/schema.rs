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

    #[test]
    fn rejects_malformed_yaml() {
        let err = SystemChannelConfig::from_yaml("not: [valid, chat.yaml").unwrap_err();
        assert!(
            err.to_string().contains("failed to parse chat.yaml"),
            "{err}"
        );
    }

    #[test]
    fn rejects_an_unrecognized_scope_value() {
        let err = SystemChannelConfig::from_yaml(
            r#"
schema_version: 1
system_channels:
  - category: trade
    scope: realm
"#,
        )
        .unwrap_err();
        assert!(
            err.to_string().contains("failed to parse chat.yaml"),
            "{err}"
        );
    }

    // `ensure_channels` — the actual global-vs-per-zone reconciliation
    // logic — had zero coverage before, not even ignored, unlike
    // everything else in `chat` that touches the DB. Real Postgres, set
    // WZ_POSTGRES_* and run with `-- --ignored`.

    async fn channel_store() -> crate::store::ChannelStore {
        use common::config::PostgresConfig;
        use common::pool::{PoolOptions, postgres_pool};

        let config = PostgresConfig::from_env().expect("WZ_POSTGRES_* env vars set");
        let pool = postgres_pool(&config, PoolOptions::default())
            .await
            .unwrap();
        crate::store::ChannelStore::new(pool)
    }

    // Categories are uniquified per test run (same pattern
    // `store.rs`'s own ignored tests use) — not just to avoid leftover
    // data between runs, but because these tests run in parallel against
    // one shared real Postgres instance, and a literal shared category
    // like "trade" across two test functions is exactly the scenario
    // that surfaced a real bug: two concurrent global-scope `ensure`
    // calls for the *same* category didn't converge on one channel
    // before `db/migrations/0006_.../up.sql`'s `NULLS NOT DISTINCT` fix
    // (`crate::store::ChannelStore::ensure_zone_channel`'s doc comment
    // has the full explanation). Keeping these uniquified is about
    // proper test isolation going forward, not about relying on that fix.

    #[tokio::test]
    #[ignore]
    async fn ensure_channels_creates_one_global_and_one_per_declared_zone() {
        let store = channel_store().await;
        let category = format!("trade-{}", common::id::ChannelId::new());
        let config = SystemChannelConfig::from_yaml(&format!(
            r#"
schema_version: 1
system_channels:
  - category: {category}
    scope: global
  - category: local-{category}
    scope: zone
"#
        ))
        .unwrap();

        let ids = config
            .ensure_channels(&store, &["zone-a".to_string(), "zone-b".to_string()])
            .await
            .unwrap();

        // One global channel, plus one zone-scoped channel per zone.
        assert_eq!(ids.len(), 3);
        assert_eq!(
            ids.iter().collect::<std::collections::HashSet<_>>().len(),
            3,
            "expected 3 distinct channel ids, got {ids:?}"
        );
    }

    #[tokio::test]
    #[ignore]
    async fn ensure_channels_is_idempotent_across_calls() {
        let store = channel_store().await;
        let category = format!("trade-{}", common::id::ChannelId::new());
        let config = SystemChannelConfig::from_yaml(&format!(
            r#"
schema_version: 1
system_channels:
  - category: {category}
    scope: global
"#
        ))
        .unwrap();

        let first = config.ensure_channels(&store, &[]).await.unwrap();
        let second = config.ensure_channels(&store, &[]).await.unwrap();
        assert_eq!(first, second);
    }
}
