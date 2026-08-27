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
    /// Whether a connection auto-joins this category's channel on
    /// entering a zone (initial join or a `ZoneChanged` transition) and
    /// auto-leaves it on exiting, rather than needing an explicit client
    /// `Join`/`Leave` (#186). Only meaningful for `scope: zone` — a
    /// `global` category is never zone-triggered, so `true` here on a
    /// `global` declaration is rejected at load time rather than silently
    /// ignored (`SystemChannelConfig::from_yaml` below). Defaults to
    /// `false`, matching every pre-#186 declaration's actual behavior.
    #[serde(default)]
    pub auto_join: bool,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SystemChannelConfig {
    pub schema_version: u32,
    pub system_channels: Vec<SystemChannelDeclaration>,
}

impl SystemChannelConfig {
    pub fn from_yaml(input: &str) -> Result<Self> {
        let config: Self = serde_yaml::from_str(input)
            .map_err(|e| Error::wrap("chat", "failed to parse chat.yaml", e))?;
        config.check_auto_join_only_on_zone_scope()?;
        Ok(config)
    }

    /// `auto_join: true` only makes sense paired with `scope: zone` — a
    /// `global` channel is never zone-triggered (#186's own acceptance
    /// criteria: "global channels must be completely unaffected"), so a
    /// declaration combining the two is refused at load time rather than
    /// silently never auto-joining anyone.
    fn check_auto_join_only_on_zone_scope(&self) -> Result<()> {
        for declared in &self.system_channels {
            if declared.auto_join && declared.scope == ChannelScope::Global {
                return Err(Error::new(
                    "chat",
                    format!(
                        "chat.yaml declares category {:?} with auto_join: true but scope: global — \
                         auto_join only applies to scope: zone",
                        declared.category
                    ),
                ));
            }
        }
        Ok(())
    }

    /// Every declared category with `scope: zone` and `auto_join: true`
    /// (#186) — what `server::chat_session::auto_join_zone_channels`
    /// iterates on entering a zone.
    pub fn auto_join_zone_categories(&self) -> Vec<&str> {
        self.system_channels
            .iter()
            .filter(|declared| declared.scope == ChannelScope::Zone && declared.auto_join)
            .map(|declared| declared.category.as_str())
            .collect()
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

    /// Same as `from_config_dir`, but a missing `chat.yaml` is treated as
    /// "no system channels declared" (an empty config) rather than an
    /// error — unlike `stats.schema.yaml`/`party.schema.yaml`/etc., chat
    /// itself is an optional service (`WZ_SERVICE_CHAT_ENABLED`), so a
    /// deployment that enables chat but declares no system channels (and
    /// in particular no zone-scoped `auto_join` category, #186) is a
    /// legitimate, common configuration, not a startup-time mistake. A
    /// malformed (present but unparsable) file still fails loudly, same
    /// as `from_file`.
    pub fn from_config_dir_or_default() -> Result<Self> {
        let path = common::config::config_dir().join("chat.yaml");
        if !path.exists() {
            return Ok(Self {
                schema_version: 1,
                system_channels: Vec::new(),
            });
        }
        Self::from_file(&path)
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
    fn defaults_auto_join_to_false_when_omitted() {
        let config = SystemChannelConfig::from_yaml(
            r#"
schema_version: 1
system_channels:
  - category: local
    scope: zone
"#,
        )
        .unwrap();

        assert!(!config.system_channels[0].auto_join);
        assert!(config.auto_join_zone_categories().is_empty());
    }

    #[test]
    fn auto_join_zone_categories_returns_only_zone_scoped_auto_join_declarations() {
        let config = SystemChannelConfig::from_yaml(
            r#"
schema_version: 1
system_channels:
  - category: trade
    scope: global
  - category: local
    scope: zone
    auto_join: true
  - category: dungeon-chat
    scope: zone
    auto_join: false
"#,
        )
        .unwrap();

        assert_eq!(config.auto_join_zone_categories(), vec!["local"]);
    }

    #[test]
    fn rejects_auto_join_paired_with_global_scope() {
        let err = SystemChannelConfig::from_yaml(
            r#"
schema_version: 1
system_channels:
  - category: trade
    scope: global
    auto_join: true
"#,
        )
        .unwrap_err();
        assert!(err.to_string().contains("trade"), "{err}");
        assert!(err.to_string().contains("auto_join"), "{err}");
    }

    #[test]
    fn from_config_dir_or_default_returns_an_empty_config_when_the_file_is_missing() {
        // No WZ_CONFIG_DIR pointed at a real chat.yaml here — this is the
        // common "chat enabled, no system channels declared" deployment
        // shape (#186), not an infra-dependent test, so it isn't `#[ignore]`d.
        let dir = std::env::temp_dir().join(format!(
            "wz-chat-schema-test-{}",
            common::id::ChannelId::new()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        // SAFETY: this test doesn't run concurrently with anything else
        // that reads WZ_CONFIG_DIR within this process — see other
        // `from_env`-adjacent tests in this crate for the same pattern.
        unsafe {
            std::env::set_var("WZ_CONFIG_DIR", &dir);
        }
        let config = SystemChannelConfig::from_config_dir_or_default().unwrap();
        unsafe {
            std::env::remove_var("WZ_CONFIG_DIR");
        }
        std::fs::remove_dir_all(&dir).ok();

        assert_eq!(config.schema_version, 1);
        assert!(config.system_channels.is_empty());
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
