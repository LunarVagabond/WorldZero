//! `server`'s half of the Plugin-Scoped Data Store (#149,
//! docs/PROPOSAL.md's "Plugin-Scoped Data Store"): `plugin_host` only
//! defines the interface (`plugin_host::PluginStateScope`,
//! `HostCallbacks::plugin_state_get`/`plugin_state_set`) — it has no
//! Postgres dependency of its own, same layering every other
//! `HostCallbacks` method already follows (the domain data lives with
//! whichever crate/process actually owns storage, `plugin-host` only
//! owns the sandboxed call boundary). This module is that storage: a
//! durable `PluginStateStore` (Postgres, character/zone scope) plus the
//! shared in-memory `PluginStateCache` every `plugin-state-get` call
//! answers from synchronously — see `plugin_startup::PluginCallbacks`
//! for why reads can't hit Postgres live from inside a sandboxed call
//! (same constraint `caller-role` already documents).
//!
//! Cache keys are a single flat `"<scope>:<id>:<key>"` string across all
//! three scopes, not three separate maps — simpler to thread through one
//! `Arc<Mutex<_>>` shared by `session`/`main`/`plugin_startup`/`world_actor`
//! than to plumb three.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use common::id::CharacterId;
use common::{Error, Result};
use plugin_host::PluginStateScope;
use sqlx::{PgPool, Row};

pub type PluginStateCache = Arc<Mutex<HashMap<String, Vec<u8>>>>;

/// The cache key for `scope`/`key` — shared by every reader/writer so
/// they can never disagree on how a scope maps to a cache entry.
pub fn cache_key(scope: &PluginStateScope, key: &str) -> String {
    match scope {
        PluginStateScope::Character(id) => format!("character:{id}:{key}"),
        PluginStateScope::Entity(id) => format!("entity:{id}:{key}"),
        PluginStateScope::Zone(id) => format!("zone:{id}:{key}"),
    }
}

pub struct PluginStateStore {
    pool: PgPool,
}

impl PluginStateStore {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Every key currently stored for `character_id` — the hydration
    /// read `session::handle_session` runs once at join, before the
    /// character's entity can receive any plugin call.
    pub async fn character_state(
        &self,
        character_id: CharacterId,
    ) -> Result<HashMap<String, Vec<u8>>> {
        let rows =
            sqlx::query("SELECT key, value FROM plugin_character_state WHERE character_id = $1")
                .bind(character_id.as_uuid())
                .fetch_all(&self.pool)
                .await
                .map_err(|e| Error::wrap("server", "failed to load plugin character state", e))?;

        Ok(rows
            .into_iter()
            .map(|row| (row.get("key"), row.get("value")))
            .collect())
    }

    /// Every key currently stored for `zone_id` — the hydration read
    /// `main` runs once per zone at startup, before that zone's actor
    /// (and the plugin instance it may carry) starts.
    pub async fn zone_state(&self, zone_id: &str) -> Result<HashMap<String, Vec<u8>>> {
        let rows = sqlx::query("SELECT key, value FROM plugin_zone_state WHERE zone_id = $1")
            .bind(zone_id)
            .fetch_all(&self.pool)
            .await
            .map_err(|e| Error::wrap("server", "failed to load plugin zone state", e))?;

        Ok(rows
            .into_iter()
            .map(|row| (row.get("key"), row.get("value")))
            .collect())
    }

    pub async fn set_character_state(
        &self,
        character_id: CharacterId,
        key: &str,
        value: &[u8],
    ) -> Result<()> {
        sqlx::query(
            "INSERT INTO plugin_character_state (character_id, key, value, updated_at) \
             VALUES ($1, $2, $3, now()) \
             ON CONFLICT (character_id, key) DO UPDATE \
                 SET value = EXCLUDED.value, updated_at = now()",
        )
        .bind(character_id.as_uuid())
        .bind(key)
        .bind(value)
        .execute(&self.pool)
        .await
        .map_err(|e| Error::wrap("server", "failed to write plugin character state", e))?;
        Ok(())
    }

    pub async fn set_zone_state(&self, zone_id: &str, key: &str, value: &[u8]) -> Result<()> {
        sqlx::query(
            "INSERT INTO plugin_zone_state (zone_id, key, value, updated_at) \
             VALUES ($1, $2, $3, now()) \
             ON CONFLICT (zone_id, key) DO UPDATE \
                 SET value = EXCLUDED.value, updated_at = now()",
        )
        .bind(zone_id)
        .bind(key)
        .bind(value)
        .execute(&self.pool)
        .await
        .map_err(|e| Error::wrap("server", "failed to write plugin zone state", e))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use common::config::PostgresConfig;
    use common::id::AccountId;
    use common::pool::{PoolOptions, postgres_pool};

    use super::*;

    // Real Postgres — set WZ_POSTGRES_* and run with `-- --ignored`.
    async fn pool() -> PgPool {
        let pg_config = PostgresConfig::from_env().expect("WZ_POSTGRES_* env vars set");
        postgres_pool(&pg_config, PoolOptions::default())
            .await
            .unwrap()
    }

    async fn create_character(pool: &PgPool) -> CharacterId {
        let account_id = AccountId::new();
        sqlx::query("INSERT INTO accounts (id, username, password_hash) VALUES ($1, $2, 'unused')")
            .bind(account_id.as_uuid())
            .bind(format!("plugin-state-test-{account_id}"))
            .execute(pool)
            .await
            .unwrap();

        // #170: character.realm_id is a real foreign key now — needs an
        // actual realms row, not just a bare UUID.
        let realm_id = uuid::Uuid::now_v7();
        sqlx::query("INSERT INTO realms (id, name, open_or_bound) VALUES ($1, 'Plugin State Test Realm', 'open')")
            .bind(realm_id)
            .execute(pool)
            .await
            .unwrap();

        let character_id = CharacterId::new();
        sqlx::query(
            "INSERT INTO characters (id, account_id, name, realm_id, zone_id) \
             VALUES ($1, $2, 'Aria', $3, 'greenwood-forest')",
        )
        .bind(character_id.as_uuid())
        .bind(account_id.as_uuid())
        .bind(realm_id)
        .execute(pool)
        .await
        .unwrap();

        character_id
    }

    #[tokio::test]
    #[ignore]
    async fn character_state_round_trips() {
        let pool = pool().await;
        let store = PluginStateStore::new(pool.clone());
        let character_id = create_character(&pool).await;

        assert!(
            store
                .character_state(character_id)
                .await
                .unwrap()
                .is_empty()
        );

        store
            .set_character_state(character_id, "class", b"warrior")
            .await
            .unwrap();
        let state = store.character_state(character_id).await.unwrap();
        assert_eq!(
            state.get("class").map(Vec::as_slice),
            Some(b"warrior".as_slice())
        );
    }

    #[tokio::test]
    #[ignore]
    async fn set_character_state_overwrites_an_existing_key() {
        let pool = pool().await;
        let store = PluginStateStore::new(pool.clone());
        let character_id = create_character(&pool).await;

        store
            .set_character_state(character_id, "class", b"warrior")
            .await
            .unwrap();
        store
            .set_character_state(character_id, "class", b"mage")
            .await
            .unwrap();

        let state = store.character_state(character_id).await.unwrap();
        assert_eq!(
            state.get("class").map(Vec::as_slice),
            Some(b"mage".as_slice())
        );
        assert_eq!(state.len(), 1);
    }

    #[tokio::test]
    #[ignore]
    async fn zone_state_round_trips_and_is_independent_of_character_state() {
        let pool = pool().await;
        let store = PluginStateStore::new(pool.clone());
        let zone_id = format!("test-zone-{}", uuid::Uuid::now_v7());

        assert!(store.zone_state(&zone_id).await.unwrap().is_empty());

        store
            .set_zone_state(&zone_id, "item_catalog", b"{\"sword\":{}}")
            .await
            .unwrap();
        let state = store.zone_state(&zone_id).await.unwrap();
        assert_eq!(
            state.get("item_catalog").map(Vec::as_slice),
            Some(b"{\"sword\":{}}".as_slice())
        );
    }

    #[test]
    fn cache_key_distinguishes_scopes_with_the_same_id_and_key() {
        let character = cache_key(&PluginStateScope::Character("abc".to_string()), "k");
        let entity = cache_key(&PluginStateScope::Entity("abc".to_string()), "k");
        let zone = cache_key(&PluginStateScope::Zone("abc".to_string()), "k");
        assert_ne!(character, entity);
        assert_ne!(character, zone);
        assert_ne!(entity, zone);
    }
}
