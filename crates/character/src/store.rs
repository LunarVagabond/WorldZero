//! Postgres-backed character stats read/write — the one write path into
//! the `stats` JSONB column, validated against the declared schema before
//! anything reaches storage (docs/specs/Data_Model_Spec.md).

use common::id::{AccountId, CharacterId, RealmId};
use common::{Error, Result};
use sqlx::PgPool;

use crate::schema::AttributeSchema;

pub struct CharacterStore {
    pool: PgPool,
    schema: AttributeSchema,
}

impl CharacterStore {
    pub fn new(pool: PgPool, schema: AttributeSchema) -> Self {
        Self { pool, schema }
    }

    /// Not part of the declared-schema validation story — just enough to
    /// have a character row to read/write stats against. A fuller
    /// character CRUD API is future work.
    pub async fn create(
        &self,
        account_id: AccountId,
        name: &str,
        realm_id: RealmId,
        zone_id: &str,
    ) -> Result<CharacterId> {
        let id = CharacterId::new();

        sqlx::query("INSERT INTO characters (id, account_id, name, realm_id, zone_id) VALUES ($1, $2, $3, $4, $5)")
            .bind(id.as_uuid())
            .bind(account_id.as_uuid())
            .bind(name)
            .bind(realm_id.as_uuid())
            .bind(zone_id)
            .execute(&self.pool)
            .await
            .map_err(|e| Error::wrap("character", "failed to create character", e))?;

        Ok(id)
    }

    /// Validates against the declared schema, then writes — a rejected
    /// write never reaches the `stats` column.
    pub async fn set_stat(&self, character_id: CharacterId, key: &str, value: i64) -> Result<()> {
        self.schema.validate_write(key, value)?;

        sqlx::query(
            "UPDATE characters SET stats = jsonb_set(stats, $2, to_jsonb($3::bigint), true), updated_at = now() WHERE id = $1",
        )
        .bind(character_id.as_uuid())
        .bind(vec![key.to_string()])
        .bind(value)
        .execute(&self.pool)
        .await
        .map_err(|e| Error::wrap("character", "failed to write stat", e))?;

        Ok(())
    }

    /// Falls back to the schema's declared default when the key is absent
    /// from the stored `stats` blob.
    pub async fn get_stat(&self, character_id: CharacterId, key: &str) -> Result<i64> {
        let stats: serde_json::Value =
            sqlx::query_scalar("SELECT stats FROM characters WHERE id = $1")
                .bind(character_id.as_uuid())
                .fetch_one(&self.pool)
                .await
                .map_err(|e| Error::wrap("character", "failed to read character stats", e))?;

        let stored = stats.as_object().cloned().unwrap_or_default();
        self.schema.resolve_read(&stored, key)
    }
}

#[cfg(test)]
mod tests {
    use common::config::PostgresConfig;
    use common::pool::{PoolOptions, postgres_pool};

    use super::*;

    fn schema() -> AttributeSchema {
        AttributeSchema::from_yaml(
            r#"
schema_version: 1
stats:
  - key: hp
    type: int
    default: 100
    min: 0
    max: 100
"#,
        )
        .unwrap()
    }

    // Real Postgres — set WZ_POSTGRES_* and run with `-- --ignored`.
    // Inserts its own throwaway account (character.account_id is a real FK)
    // rather than depending on the `auth` crate just for test setup.
    async fn store_with_character() -> (CharacterStore, CharacterId) {
        let config = PostgresConfig::from_env().expect("WZ_POSTGRES_* env vars set");
        let pool = postgres_pool(&config, PoolOptions::default())
            .await
            .unwrap();

        let account_id = AccountId::new();
        sqlx::query("INSERT INTO accounts (id, username, password_hash) VALUES ($1, $2, 'unused')")
            .bind(account_id.as_uuid())
            .bind(format!("stats-test-{account_id}"))
            .execute(&pool)
            .await
            .unwrap();

        let store = CharacterStore::new(pool, schema());
        let character_id = store
            .create(
                account_id,
                "Test Character",
                RealmId::new(),
                "greenwood-forest",
            )
            .await
            .unwrap();

        (store, character_id)
    }

    #[tokio::test]
    #[ignore]
    async fn missing_key_reads_the_declared_default() {
        let (store, character_id) = store_with_character().await;
        assert_eq!(store.get_stat(character_id, "hp").await.unwrap(), 100);
    }

    #[tokio::test]
    #[ignore]
    async fn valid_write_then_read_round_trips() {
        let (store, character_id) = store_with_character().await;
        store.set_stat(character_id, "hp", 42).await.unwrap();
        assert_eq!(store.get_stat(character_id, "hp").await.unwrap(), 42);
    }

    #[tokio::test]
    #[ignore]
    async fn out_of_bounds_write_never_reaches_storage() {
        let (store, character_id) = store_with_character().await;
        assert!(store.set_stat(character_id, "hp", 999).await.is_err());
        // Untouched — still the declared default, not a partially-applied bad value.
        assert_eq!(store.get_stat(character_id, "hp").await.unwrap(), 100);
    }

    #[tokio::test]
    #[ignore]
    async fn unknown_key_write_is_rejected() {
        let (store, character_id) = store_with_character().await;
        let err = store
            .set_stat(character_id, "stamina", 10)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("unknown stat key"), "{err}");
    }
}
