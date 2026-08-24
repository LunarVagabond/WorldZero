//! Postgres-backed character stats read/write — the one write path into
//! the `stats` JSONB column, validated against the declared schema before
//! anything reaches storage (docs/specs/Data_Model_Spec.md).

use common::id::{AccountId, CharacterId, RealmId};
use common::{Error, Result};
use sqlx::{PgPool, Row};

use crate::schema::AttributeSchema;

/// Just enough of a character row for the phase-1 "load a character if
/// one exists" flow (`server`, #39's acceptance criteria) — not a full
/// character read model. `realm_id` was added for #52: callers that
/// resolve a character via [`CharacterStore::find_by_account_in_open_realms`]
/// don't already know which realm it landed on (an open-realm character
/// can be created on any one of them), and need it back to check the
/// bound/open policy correctly.
#[derive(Debug, Clone, PartialEq)]
pub struct CharacterSummary {
    pub id: CharacterId,
    pub name: String,
    pub realm_id: RealmId,
    pub zone_id: String,
    pub position: (f64, f64, f64),
}

pub struct CharacterStore {
    pool: PgPool,
    schema: AttributeSchema,
    inventory_config: crate::inventory::InventoryConfig,
}

impl CharacterStore {
    pub fn new(
        pool: PgPool,
        schema: AttributeSchema,
        inventory_config: crate::inventory::InventoryConfig,
    ) -> Self {
        Self {
            pool,
            schema,
            inventory_config,
        }
    }

    /// Lets `crate::inventory`'s `impl CharacterStore` block reach the
    /// same pool this one write path (and every other `CharacterStore`
    /// method) already uses — kept `pub(crate)`, not `pub`, since a
    /// caller outside this crate has no legitimate reason to touch the
    /// pool directly instead of going through a `CharacterStore` method.
    pub(crate) fn pool(&self) -> &PgPool {
        &self.pool
    }

    pub(crate) fn inventory_config(&self) -> &crate::inventory::InventoryConfig {
        &self.inventory_config
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

    /// The open-realm-friendly "does this account already have a
    /// character (in this realm), or do we need to create one" lookup —
    /// picks the single most-recently-created one if an account somehow
    /// has more than one (a real "which character" selection flow is
    /// future work; phase 1 is one character per account per realm in
    /// practice, per `#39`'s "load a character if one exists").
    pub async fn find_by_account(
        &self,
        account_id: AccountId,
        realm_id: RealmId,
    ) -> Result<Option<CharacterSummary>> {
        let row = sqlx::query(
            "SELECT id, name, realm_id, zone_id, position_x, position_y, position_z FROM characters \
             WHERE account_id = $1 AND realm_id = $2 ORDER BY created_at DESC LIMIT 1",
        )
        .bind(account_id.as_uuid())
        .bind(realm_id.as_uuid())
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| Error::wrap("character", "failed to look up character by account", e))?;

        Ok(row.map(row_to_character_summary))
    }

    /// The open-realm-group-aware counterpart to [`Self::find_by_account`]
    /// (#52) — finds an account's character among realms currently
    /// flagged `open` (docs/specs/Realm_Character_Policy_Spec.md's "The
    /// flag"), regardless of which specific open realm it happens to be
    /// recorded against, rather than requiring a caller to already know
    /// which one. **Never returns a bound-realm character**, even if
    /// `account_id` has one — an open-realm lookup leaking a
    /// bound-realm character out of its binding would defeat the whole
    /// point of the binding. Read consistency across realms comes for
    /// free here: there's no caching layer in this crate (every method
    /// reads straight from `self.pool`), so this always sees the most
    /// recent write regardless of which realm process made it.
    ///
    /// `character` has no concept of realm policy itself — this method
    /// only encodes "`realms.open_or_bound = 'open'`" as a join
    /// condition, a fact about data shape, not a policy decision.
    /// Deciding *when* to call this vs. [`Self::find_by_account`] is
    /// `realm-directory::LoginPolicy::resolve_character`'s job.
    pub async fn find_by_account_in_open_realms(
        &self,
        account_id: AccountId,
    ) -> Result<Option<CharacterSummary>> {
        let row = sqlx::query(
            "SELECT c.id, c.name, c.realm_id, c.zone_id, c.position_x, c.position_y, c.position_z \
             FROM characters c JOIN realms r ON c.realm_id = r.id \
             WHERE c.account_id = $1 AND r.open_or_bound = 'open' \
             ORDER BY c.created_at DESC LIMIT 1",
        )
        .bind(account_id.as_uuid())
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| {
            Error::wrap(
                "character",
                "failed to look up account's open-realm character",
                e,
            )
        })?;

        Ok(row.map(row_to_character_summary))
    }

    /// Total character count for `realm_id`, regardless of whether any
    /// of them are currently online — the durable "census" half of
    /// #137's realm population reporting. Deliberately returns a plain
    /// count, not a `low`/`med`/`high` bucket: bucketing is a caller/
    /// display decision (see #137), not something this crate should
    /// bake in.
    pub async fn count_for_realm(&self, realm_id: RealmId) -> Result<i64> {
        sqlx::query_scalar("SELECT COUNT(*) FROM characters WHERE realm_id = $1")
            .bind(realm_id.as_uuid())
            .fetch_one(&self.pool)
            .await
            .map_err(|e| Error::wrap("character", "failed to count characters for realm", e))
    }

    /// Persists a character's current simulated position — called on
    /// disconnect (and is safe to call periodically) so a reconnect finds
    /// the character where it actually was, not where it started
    /// (#39's "persists across a disconnect/reconnect" acceptance criteria).
    pub async fn update_position(
        &self,
        character_id: CharacterId,
        position: (f64, f64, f64),
    ) -> Result<()> {
        sqlx::query(
            "UPDATE characters SET position_x = $2, position_y = $3, position_z = $4, updated_at = now() WHERE id = $1",
        )
        .bind(character_id.as_uuid())
        .bind(position.0)
        .bind(position.1)
        .bind(position.2)
        .execute(&self.pool)
        .await
        .map_err(|e| Error::wrap("character", "failed to update character position", e))?;

        Ok(())
    }

    /// Same as [`Self::update_position`], plus updating `zone_id` in the
    /// same statement — used on disconnect once a character may have
    /// crossed a zone link (#45) mid-session, so the persisted zone
    /// always reflects where the character actually ended up, not just
    /// where they started the connection.
    #[tracing::instrument(skip(self, position), fields(%character_id, zone_id))]
    pub async fn update_position_and_zone(
        &self,
        character_id: CharacterId,
        position: (f64, f64, f64),
        zone_id: &str,
    ) -> Result<()> {
        sqlx::query(
            "UPDATE characters SET position_x = $2, position_y = $3, position_z = $4, zone_id = $5, updated_at = now() WHERE id = $1",
        )
        .bind(character_id.as_uuid())
        .bind(position.0)
        .bind(position.1)
        .bind(position.2)
        .bind(zone_id)
        .execute(&self.pool)
        .await
        .map_err(|e| Error::wrap("character", "failed to update character position and zone", e))?;

        Ok(())
    }

    /// Validates against the declared schema, then writes — a rejected
    /// write never reaches the `stats` column.
    #[tracing::instrument(skip(self), fields(%character_id))]
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

    /// Reads the current value (falling back to the declared default, same
    /// as `get_stat`), adds `delta`, and writes the result back through
    /// `set_stat` — the "reduce a plugin-specified stat by N" primitive
    /// `docs/PROPOSAL.md`'s "v0 Host Functions" names, backing
    /// `plugin_host`'s `apply-stat-delta`. Not transactional against a
    /// concurrent writer (same as `get_stat`/`set_stat` individually) —
    /// the open-realm concurrency boundary is the session lease in
    /// `docs/specs/Realm_Character_Policy_Spec.md`, not per-write locking
    /// on this column. Returns the resulting value.
    pub async fn apply_stat_delta(
        &self,
        character_id: CharacterId,
        key: &str,
        delta: i64,
    ) -> Result<i64> {
        let current = self.get_stat(character_id, key).await?;
        let new_value = current
            .checked_add(delta)
            .ok_or_else(|| Error::new("character", format!("stat delta overflowed for {key:?}")))?;
        self.set_stat(character_id, key, new_value).await?;
        Ok(new_value)
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

fn row_to_character_summary(row: sqlx::postgres::PgRow) -> CharacterSummary {
    CharacterSummary {
        id: CharacterId::from_uuid(row.get("id")),
        name: row.get("name"),
        realm_id: RealmId::from_uuid(row.get("realm_id")),
        zone_id: row.get("zone_id"),
        position: (
            row.get("position_x"),
            row.get("position_y"),
            row.get("position_z"),
        ),
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

        let store = CharacterStore::new(pool, schema(), Default::default());
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

    async fn store_with_account() -> (CharacterStore, AccountId, RealmId) {
        let config = PostgresConfig::from_env().expect("WZ_POSTGRES_* env vars set");
        let pool = postgres_pool(&config, PoolOptions::default())
            .await
            .unwrap();

        let account_id = AccountId::new();
        sqlx::query("INSERT INTO accounts (id, username, password_hash) VALUES ($1, $2, 'unused')")
            .bind(account_id.as_uuid())
            .bind(format!("find-by-account-test-{account_id}"))
            .execute(&pool)
            .await
            .unwrap();

        (
            CharacterStore::new(pool, schema(), Default::default()),
            account_id,
            RealmId::new(),
        )
    }

    #[tokio::test]
    #[ignore]
    async fn find_by_account_returns_none_before_any_character_exists() {
        let (store, account_id, realm_id) = store_with_account().await;
        assert_eq!(
            store.find_by_account(account_id, realm_id).await.unwrap(),
            None
        );
    }

    #[tokio::test]
    #[ignore]
    async fn find_by_account_returns_the_created_character() {
        let (store, account_id, realm_id) = store_with_account().await;
        let character_id = store
            .create(account_id, "Aria", realm_id, "greenwood-forest")
            .await
            .unwrap();

        let found = store
            .find_by_account(account_id, realm_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(found.id, character_id);
        assert_eq!(found.name, "Aria");
        assert_eq!(found.zone_id, "greenwood-forest");
        assert_eq!(found.position, (0.0, 0.0, 0.0));
    }

    #[tokio::test]
    #[ignore]
    async fn find_by_account_does_not_cross_realms() {
        let (store, account_id, realm_id) = store_with_account().await;
        store
            .create(account_id, "Aria", realm_id, "greenwood-forest")
            .await
            .unwrap();

        let other_realm = RealmId::new();
        assert_eq!(
            store
                .find_by_account(account_id, other_realm)
                .await
                .unwrap(),
            None
        );
    }

    #[tokio::test]
    #[ignore]
    async fn count_for_realm_only_counts_that_realms_characters() {
        let (store, account_id, realm_id) = store_with_account().await;
        assert_eq!(store.count_for_realm(realm_id).await.unwrap(), 0);

        store
            .create(account_id, "Aria", realm_id, "greenwood-forest")
            .await
            .unwrap();
        assert_eq!(store.count_for_realm(realm_id).await.unwrap(), 1);

        // A second character on a different realm must not be counted
        // against this one.
        store
            .create(account_id, "Bram", RealmId::new(), "greenwood-forest")
            .await
            .unwrap();
        assert_eq!(store.count_for_realm(realm_id).await.unwrap(), 1);
    }

    /// Real (Postgres-persisted) `open`/`bound` realm rows, unlike
    /// [`store_with_account`]'s ad hoc [`RealmId::new()`] — needed here
    /// since [`CharacterStore::find_by_account_in_open_realms`] joins
    /// against the real `realms` table, which an unregistered id would
    /// never match.
    async fn store_with_realms() -> (CharacterStore, AccountId, PgPool, RealmId, RealmId) {
        let config = PostgresConfig::from_env().expect("WZ_POSTGRES_* env vars set");
        let pool = postgres_pool(&config, PoolOptions::default())
            .await
            .unwrap();

        let account_id = AccountId::new();
        sqlx::query("INSERT INTO accounts (id, username, password_hash) VALUES ($1, $2, 'unused')")
            .bind(account_id.as_uuid())
            .bind(format!("open-realm-lookup-test-{account_id}"))
            .execute(&pool)
            .await
            .unwrap();

        let open_realm_id = RealmId::new();
        sqlx::query(
            "INSERT INTO realms (id, name, open_or_bound) VALUES ($1, 'Open Test Realm', 'open')",
        )
        .bind(open_realm_id.as_uuid())
        .execute(&pool)
        .await
        .unwrap();

        let bound_realm_id = RealmId::new();
        sqlx::query(
            "INSERT INTO realms (id, name, open_or_bound) VALUES ($1, 'Bound Test Realm', 'bound')",
        )
        .bind(bound_realm_id.as_uuid())
        .execute(&pool)
        .await
        .unwrap();

        (
            CharacterStore::new(pool.clone(), schema(), Default::default()),
            account_id,
            pool,
            open_realm_id,
            bound_realm_id,
        )
    }

    #[tokio::test]
    #[ignore]
    async fn find_by_account_in_open_realms_finds_a_character_created_on_a_different_open_realm() {
        let (store, account_id, _pool, open_realm_id, _bound_realm_id) = store_with_realms().await;
        let character_id = store
            .create(account_id, "Aria", open_realm_id, "greenwood-forest")
            .await
            .unwrap();

        // A second, unrelated open realm — the whole point is that the
        // lookup doesn't need to already know which specific open realm
        // the character lives on.
        let found = store
            .find_by_account_in_open_realms(account_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(found.id, character_id);
        assert_eq!(found.realm_id, open_realm_id);
    }

    #[tokio::test]
    #[ignore]
    async fn find_by_account_in_open_realms_never_returns_a_bound_realm_character() {
        let (store, account_id, _pool, _open_realm_id, bound_realm_id) = store_with_realms().await;
        store
            .create(account_id, "Aria", bound_realm_id, "greenwood-forest")
            .await
            .unwrap();

        assert_eq!(
            store
                .find_by_account_in_open_realms(account_id)
                .await
                .unwrap(),
            None
        );
    }

    #[tokio::test]
    #[ignore]
    async fn update_position_then_find_reflects_the_new_position() {
        let (store, account_id, realm_id) = store_with_account().await;
        let character_id = store
            .create(account_id, "Aria", realm_id, "greenwood-forest")
            .await
            .unwrap();

        store
            .update_position(character_id, (12.5, -3.0, 0.0))
            .await
            .unwrap();

        let found = store
            .find_by_account(account_id, realm_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(found.position, (12.5, -3.0, 0.0));
    }

    #[tokio::test]
    #[ignore]
    async fn update_position_and_zone_updates_both_together() {
        let (store, account_id, realm_id) = store_with_account().await;
        let character_id = store
            .create(account_id, "Aria", realm_id, "greenwood-forest")
            .await
            .unwrap();

        store
            .update_position_and_zone(character_id, (1.0, 2.0, 0.0), "stonebridge-village")
            .await
            .unwrap();

        let found = store
            .find_by_account(account_id, realm_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(found.position, (1.0, 2.0, 0.0));
        assert_eq!(found.zone_id, "stonebridge-village");
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
    async fn apply_stat_delta_reduces_from_the_declared_default() {
        let (store, character_id) = store_with_character().await;
        let new_value = store
            .apply_stat_delta(character_id, "hp", -30)
            .await
            .unwrap();
        assert_eq!(new_value, 70);
        assert_eq!(store.get_stat(character_id, "hp").await.unwrap(), 70);
    }

    #[tokio::test]
    #[ignore]
    async fn apply_stat_delta_out_of_bounds_is_rejected_and_unapplied() {
        let (store, character_id) = store_with_character().await;
        assert!(
            store
                .apply_stat_delta(character_id, "hp", -1000)
                .await
                .is_err()
        );
        assert_eq!(store.get_stat(character_id, "hp").await.unwrap(), 100);
    }

    #[tokio::test]
    #[ignore]
    async fn apply_stat_delta_overflow_is_rejected_and_unapplied() {
        // An unbounded stat (no min/max declared) so the schema's own
        // bounds check can't be what rejects this — isolates the
        // `checked_add` overflow guard itself, which `hp`'s [0,100]
        // bounds (used everywhere else in this suite) would always mask.
        let unbounded_schema = AttributeSchema::from_yaml(
            r#"
schema_version: 1
stats:
  - key: score
    type: int
    default: 0
"#,
        )
        .unwrap();

        let config = PostgresConfig::from_env().expect("WZ_POSTGRES_* env vars set");
        let pool = postgres_pool(&config, PoolOptions::default())
            .await
            .unwrap();
        let account_id = AccountId::new();
        sqlx::query("INSERT INTO accounts (id, username, password_hash) VALUES ($1, $2, 'unused')")
            .bind(account_id.as_uuid())
            .bind(format!("stat-overflow-test-{account_id}"))
            .execute(&pool)
            .await
            .unwrap();
        let store = CharacterStore::new(pool, unbounded_schema, Default::default());
        let character_id = store
            .create(
                account_id,
                "Test Character",
                RealmId::new(),
                "greenwood-forest",
            )
            .await
            .unwrap();

        store
            .apply_stat_delta(character_id, "score", i64::MAX)
            .await
            .unwrap();

        let err = store
            .apply_stat_delta(character_id, "score", i64::MAX)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("overflowed"), "{err}");
        assert_eq!(
            store.get_stat(character_id, "score").await.unwrap(),
            i64::MAX
        );
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
