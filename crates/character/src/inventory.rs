//! Core inventory (item stacks) and currency read/write —
//! docs/specs/Data_Model_Spec.md's `items` table and `character_currency`
//! table (#218 — one row per `(character_id, currency_key)`, replacing
//! the old single `characters.currency_balance` column), following the
//! same "fixed core schema, the framework never interprets the meaning"
//! discipline as `stats` JSONB (see this crate's `schema`/`store`
//! modules): the framework knows an `item_type` string and a quantity
//! exist, never what an item *does* — that's entirely plugin-owned. Same
//! for a `currency_key`: the framework validates it against
//! `crate::currency_schema::CurrencySchema` at the call site (not inside
//! this module), never here.
//!
//! Not transactional against a concurrent writer, same as `store`'s
//! `get_stat`/`set_stat` — the open-realm concurrency boundary is the
//! session lease design in docs/specs/Realm_Character_Policy_Spec.md,
//! not per-write locking on these rows either.

use common::id::CharacterId;
use common::{Error, Result};
use sqlx::Row;

use crate::store::CharacterStore;

/// Chosen as "generous enough that a typical game's early-game inventory
/// never bumps into it by accident" — a starting point tuned against
/// nothing more than intuition, same status as `world::WorldConfig`'s
/// defaults. A self-hoster with an unusual inventory-size policy is
/// expected to override this via `WZ_INVENTORY_MAX_ITEM_TYPES`, not
/// treat it as load-bearing.
const DEFAULT_MAX_DISTINCT_ITEM_TYPES: u32 = 40;

/// Per-deployment inventory capacity — enforced by the core (a grant
/// that would exceed it is rejected, never silently ignored or
/// truncated), but configurable per game rather than a hardcoded number,
/// same "solid default, never a wall" spirit as every other dev-facing
/// limit in this crate (`AttributeSchema`'s per-stat bounds).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct InventoryConfig {
    /// The cap is on *distinct item types* (stacks), not total item
    /// count — granting more of an item type the character already owns
    /// never counts against this, only a brand-new stack does. Matches
    /// the classic "N inventory slots" mental model rather than an
    /// arbitrary sum that would penalize stacking.
    pub max_distinct_item_types: u32,
}

impl Default for InventoryConfig {
    fn default() -> Self {
        Self {
            max_distinct_item_types: DEFAULT_MAX_DISTINCT_ITEM_TYPES,
        }
    }
}

impl InventoryConfig {
    /// Reads `WZ_INVENTORY_MAX_ITEM_TYPES`, optional — an unset var keeps
    /// the default, but a *set-and-unparsable* one is a config error, not
    /// a silent fallback (same convention as `world::WorldConfig::from_env`).
    pub fn from_env() -> Result<Self> {
        let mut config = Self::default();

        if let Ok(value) = std::env::var("WZ_INVENTORY_MAX_ITEM_TYPES") {
            config.max_distinct_item_types = value.parse().map_err(|_| {
                Error::new(
                    "character",
                    format!("WZ_INVENTORY_MAX_ITEM_TYPES is not a valid number: {value:?}"),
                )
            })?;
        }

        if config.max_distinct_item_types == 0 {
            return Err(Error::new(
                "character",
                "WZ_INVENTORY_MAX_ITEM_TYPES must be greater than 0",
            ));
        }

        Ok(config)
    }
}

impl CharacterStore {
    /// Adds `quantity` to `character_id`'s stack of `item_type`,
    /// creating the stack if it doesn't exist yet. `quantity` must be
    /// positive — granting zero or a negative amount is a caller
    /// mistake, not a valid no-op. Rejected (storage untouched) if
    /// `item_type` would be a *new* stack and the character is already
    /// at `InventoryConfig::max_distinct_item_types` — granting more of
    /// an already-owned item type is never blocked by this, only a
    /// brand-new one is. Returns the stack's new total.
    ///
    /// The existence check and the capacity check below aren't done in
    /// one transaction with the insert (same non-transactional stance as
    /// the rest of this module) — a capacity check under a concurrent
    /// grant race could theoretically let a character end up one stack
    /// over the configured cap. Acceptable: this is a soft UX limit
    /// (matching a real game's "inventory full" message), not a security
    /// or data-integrity boundary.
    pub async fn grant_item(
        &self,
        character_id: CharacterId,
        item_type: &str,
        quantity: i64,
    ) -> Result<i64> {
        if quantity <= 0 {
            return Err(Error::new(
                "character",
                format!("grant_item quantity must be positive, got {quantity}"),
            ));
        }

        let already_owned = self.item_quantity(character_id, item_type).await? > 0;
        if !already_owned {
            let distinct_count: i64 =
                sqlx::query_scalar("SELECT COUNT(*) FROM items WHERE character_id = $1")
                    .bind(character_id.as_uuid())
                    .fetch_one(self.pool())
                    .await
                    .map_err(|e| Error::wrap("character", "failed to count item stacks", e))?;
            let max = i64::from(self.inventory_config().max_distinct_item_types);
            if distinct_count >= max {
                return Err(Error::new(
                    "character",
                    format!(
                        "inventory is full: {distinct_count} distinct item types already owned, \
                         limit is {max} (WZ_INVENTORY_MAX_ITEM_TYPES)"
                    ),
                ));
            }
        }

        let new_quantity: i64 = sqlx::query_scalar(
            "INSERT INTO items (id, character_id, item_type, quantity) \
             VALUES ($1, $2, $3, $4) \
             ON CONFLICT (character_id, item_type) \
             DO UPDATE SET quantity = items.quantity + EXCLUDED.quantity, updated_at = now() \
             RETURNING quantity",
        )
        .bind(uuid::Uuid::now_v7())
        .bind(character_id.as_uuid())
        .bind(item_type)
        .bind(quantity)
        .fetch_one(self.pool())
        .await
        .map_err(|e| Error::wrap("character", "failed to grant item", e))?;

        Ok(new_quantity)
    }

    /// Removes `quantity` from `character_id`'s stack of `item_type`.
    /// Rejected (storage untouched) if `quantity` isn't positive, if no
    /// stack exists, or if the stack holds fewer than `quantity` — never
    /// a negative stack. A stack that reaches exactly zero is deleted
    /// rather than left as an empty row. Returns the stack's new total
    /// (0 if the stack was deleted).
    pub async fn remove_item(
        &self,
        character_id: CharacterId,
        item_type: &str,
        quantity: i64,
    ) -> Result<i64> {
        if quantity <= 0 {
            return Err(Error::new(
                "character",
                format!("remove_item quantity must be positive, got {quantity}"),
            ));
        }

        let current = self.item_quantity(character_id, item_type).await?;
        if current < quantity {
            return Err(Error::new(
                "character",
                format!("cannot remove {quantity} of {item_type:?}, only {current} owned"),
            ));
        }

        let remaining = current - quantity;
        if remaining == 0 {
            sqlx::query("DELETE FROM items WHERE character_id = $1 AND item_type = $2")
                .bind(character_id.as_uuid())
                .bind(item_type)
                .execute(self.pool())
                .await
                .map_err(|e| Error::wrap("character", "failed to remove item stack", e))?;
        } else {
            sqlx::query(
                "UPDATE items SET quantity = $3, updated_at = now() \
                 WHERE character_id = $1 AND item_type = $2",
            )
            .bind(character_id.as_uuid())
            .bind(item_type)
            .bind(remaining)
            .execute(self.pool())
            .await
            .map_err(|e| Error::wrap("character", "failed to remove item quantity", e))?;
        }

        Ok(remaining)
    }

    /// `0` if the character has no stack of `item_type` — not an error;
    /// owning none of an item is the ordinary case, not an exceptional one.
    pub async fn item_quantity(&self, character_id: CharacterId, item_type: &str) -> Result<i64> {
        let quantity: Option<i64> = sqlx::query_scalar(
            "SELECT quantity FROM items WHERE character_id = $1 AND item_type = $2",
        )
        .bind(character_id.as_uuid())
        .bind(item_type)
        .fetch_optional(self.pool())
        .await
        .map_err(|e| Error::wrap("character", "failed to read item quantity", e))?;

        Ok(quantity.unwrap_or(0))
    }

    /// Every item stack `character_id` currently owns, as `(item_type,
    /// quantity)` pairs — inspection/debugging convenience, not a hot
    /// path (unlike `stats`, there's no single-row-fetch reason to avoid
    /// a query per call here).
    pub async fn list_items(&self, character_id: CharacterId) -> Result<Vec<(String, i64)>> {
        let rows = sqlx::query("SELECT item_type, quantity FROM items WHERE character_id = $1")
            .bind(character_id.as_uuid())
            .fetch_all(self.pool())
            .await
            .map_err(|e| Error::wrap("character", "failed to list items", e))?;

        Ok(rows
            .into_iter()
            .map(|row| (row.get("item_type"), row.get("quantity")))
            .collect())
    }

    /// `character_id`'s current balance of `currency_key` — `0` if no
    /// row exists yet (never having touched a currency is the ordinary
    /// case, not an exceptional one; same "no row means zero" convention
    /// `item_quantity` already uses).
    pub async fn currency_balance(
        &self,
        character_id: CharacterId,
        currency_key: &str,
    ) -> Result<i64> {
        let balance: Option<i64> = sqlx::query_scalar(
            "SELECT balance FROM character_currency WHERE character_id = $1 AND currency_key = $2",
        )
        .bind(character_id.as_uuid())
        .bind(currency_key)
        .fetch_optional(self.pool())
        .await
        .map_err(|e| Error::wrap("character", "failed to read currency balance", e))?;

        Ok(balance.unwrap_or(0))
    }

    /// Adjusts `character_id`'s balance of `currency_key` by `delta`
    /// (positive or negative) and returns the new balance — every
    /// `(character, currency_key)` pair has its own fully independent
    /// balance (#218), so a delta on one currency never touches another.
    /// Rejected (storage untouched) if the result would go negative —
    /// the same `balance >= 0` invariant `db/migrations` enforces at the
    /// column level via `CHECK`, checked here first so the caller gets a
    /// clear `character`-crate error instead of a raw constraint
    /// violation surfacing from `sqlx`.
    pub async fn modify_currency(
        &self,
        character_id: CharacterId,
        currency_key: &str,
        delta: i64,
    ) -> Result<i64> {
        let current = self.currency_balance(character_id, currency_key).await?;
        let new_balance = current
            .checked_add(delta)
            .ok_or_else(|| Error::new("character", "currency balance delta overflowed"))?;
        if new_balance < 0 {
            return Err(Error::new(
                "character",
                format!("currency delta {delta} would take balance {current} negative"),
            ));
        }

        sqlx::query(
            "INSERT INTO character_currency (character_id, currency_key, balance) \
             VALUES ($1, $2, $3) \
             ON CONFLICT (character_id, currency_key) DO UPDATE SET balance = EXCLUDED.balance",
        )
        .bind(character_id.as_uuid())
        .bind(currency_key)
        .bind(new_balance)
        .execute(self.pool())
        .await
        .map_err(|e| Error::wrap("character", "failed to update currency balance", e))?;

        Ok(new_balance)
    }
}

#[cfg(test)]
mod tests {
    use common::config::PostgresConfig;
    use common::id::{AccountId, RealmId};
    use common::pool::{PoolOptions, postgres_pool};

    use crate::schema::AttributeSchema;

    use super::*;

    fn schema() -> AttributeSchema {
        AttributeSchema::from_yaml("schema_version: 1\nstats: []\n").unwrap()
    }

    /// A real, throwaway `realms` row (#170: `characters.realm_id` is a
    /// real `FOREIGN KEY` now).
    async fn insert_realm(pool: &sqlx::PgPool) -> RealmId {
        let realm_id = RealmId::new();
        sqlx::query("INSERT INTO realms (id, name, open_or_bound) VALUES ($1, $2, 'open')")
            .bind(realm_id.as_uuid())
            .bind(format!("Test Realm {realm_id}"))
            .execute(pool)
            .await
            .unwrap();
        realm_id
    }

    // Real Postgres — set WZ_POSTGRES_* and run with `-- --ignored`.
    async fn store_with_character() -> (CharacterStore, CharacterId) {
        let config = PostgresConfig::from_env().expect("WZ_POSTGRES_* env vars set");
        let pool = postgres_pool(&config, PoolOptions::default())
            .await
            .unwrap();

        let account_id = AccountId::new();
        sqlx::query("INSERT INTO accounts (id, username, password_hash) VALUES ($1, $2, 'unused')")
            .bind(account_id.as_uuid())
            .bind(format!("inventory-test-{account_id}"))
            .execute(&pool)
            .await
            .unwrap();

        let realm_id = insert_realm(&pool).await;
        let store = CharacterStore::new(pool, schema(), Default::default());
        let character_id = store
            .create(account_id, "Test Character", realm_id, "greenwood-forest")
            .await
            .unwrap();

        (store, character_id)
    }

    async fn store_with_character_and_capacity(
        max_distinct_item_types: u32,
    ) -> (CharacterStore, CharacterId) {
        let config = PostgresConfig::from_env().expect("WZ_POSTGRES_* env vars set");
        let pool = postgres_pool(&config, PoolOptions::default())
            .await
            .unwrap();

        let account_id = AccountId::new();
        sqlx::query("INSERT INTO accounts (id, username, password_hash) VALUES ($1, $2, 'unused')")
            .bind(account_id.as_uuid())
            .bind(format!("inventory-capacity-test-{account_id}"))
            .execute(&pool)
            .await
            .unwrap();

        let realm_id = insert_realm(&pool).await;
        let store = CharacterStore::new(
            pool,
            schema(),
            InventoryConfig {
                max_distinct_item_types,
            },
        );
        let character_id = store
            .create(account_id, "Test Character", realm_id, "greenwood-forest")
            .await
            .unwrap();

        (store, character_id)
    }

    #[test]
    fn inventory_config_defaults_apply_with_nothing_set() {
        // WZ_INVENTORY_MAX_ITEM_TYPES is intentionally not set/unset here
        // (unlike world::WorldConfig's tests) — this crate's test suite
        // never sets it elsewhere, so there's no cross-test env race to
        // guard against with a lock.
        assert_eq!(
            InventoryConfig::from_env().unwrap(),
            InventoryConfig::default()
        );
    }

    #[tokio::test]
    #[ignore]
    async fn granting_a_new_item_type_beyond_capacity_is_rejected() {
        let (store, character_id) = store_with_character_and_capacity(1).await;
        store.grant_item(character_id, "torch", 1).await.unwrap();
        let err = store
            .grant_item(character_id, "shield", 1)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("inventory is full"), "{err}");
    }

    #[tokio::test]
    #[ignore]
    async fn granting_more_of_an_already_owned_item_type_is_not_blocked_by_capacity() {
        let (store, character_id) = store_with_character_and_capacity(1).await;
        store.grant_item(character_id, "torch", 1).await.unwrap();
        let total = store.grant_item(character_id, "torch", 5).await.unwrap();
        assert_eq!(total, 6);
    }

    #[tokio::test]
    #[ignore]
    async fn granting_an_item_creates_a_new_stack() {
        let (store, character_id) = store_with_character().await;
        let total = store.grant_item(character_id, "torch", 3).await.unwrap();
        assert_eq!(total, 3);
        assert_eq!(store.item_quantity(character_id, "torch").await.unwrap(), 3);
    }

    #[tokio::test]
    #[ignore]
    async fn granting_the_same_item_type_twice_accumulates() {
        let (store, character_id) = store_with_character().await;
        store.grant_item(character_id, "torch", 3).await.unwrap();
        let total = store.grant_item(character_id, "torch", 2).await.unwrap();
        assert_eq!(total, 5);
    }

    #[tokio::test]
    #[ignore]
    async fn removing_part_of_a_stack_leaves_the_remainder() {
        let (store, character_id) = store_with_character().await;
        store.grant_item(character_id, "torch", 5).await.unwrap();
        let remaining = store.remove_item(character_id, "torch", 2).await.unwrap();
        assert_eq!(remaining, 3);
        assert_eq!(store.item_quantity(character_id, "torch").await.unwrap(), 3);
    }

    #[tokio::test]
    #[ignore]
    async fn removing_an_entire_stack_deletes_it() {
        let (store, character_id) = store_with_character().await;
        store.grant_item(character_id, "torch", 5).await.unwrap();
        let remaining = store.remove_item(character_id, "torch", 5).await.unwrap();
        assert_eq!(remaining, 0);
        assert_eq!(store.item_quantity(character_id, "torch").await.unwrap(), 0);
        assert!(store.list_items(character_id).await.unwrap().is_empty());
    }

    #[tokio::test]
    #[ignore]
    async fn removing_more_than_owned_is_rejected_and_the_stack_is_untouched() {
        let (store, character_id) = store_with_character().await;
        store.grant_item(character_id, "torch", 2).await.unwrap();
        assert!(store.remove_item(character_id, "torch", 5).await.is_err());
        assert_eq!(store.item_quantity(character_id, "torch").await.unwrap(), 2);
    }

    #[tokio::test]
    #[ignore]
    async fn removing_an_item_the_character_never_had_is_rejected() {
        let (store, character_id) = store_with_character().await;
        assert!(store.remove_item(character_id, "torch", 1).await.is_err());
    }

    #[tokio::test]
    #[ignore]
    async fn granting_a_non_positive_quantity_is_rejected() {
        let (store, character_id) = store_with_character().await;
        for bad_quantity in [0, -1] {
            let err = store
                .grant_item(character_id, "torch", bad_quantity)
                .await
                .unwrap_err();
            assert!(err.to_string().contains("must be positive"), "{err}");
        }
        // Never created a stack at all — not a zero-quantity row.
        assert_eq!(store.item_quantity(character_id, "torch").await.unwrap(), 0);
        assert!(store.list_items(character_id).await.unwrap().is_empty());
    }

    #[tokio::test]
    #[ignore]
    async fn removing_a_non_positive_quantity_is_rejected() {
        let (store, character_id) = store_with_character().await;
        store.grant_item(character_id, "torch", 5).await.unwrap();
        for bad_quantity in [0, -1] {
            let err = store
                .remove_item(character_id, "torch", bad_quantity)
                .await
                .unwrap_err();
            assert!(err.to_string().contains("must be positive"), "{err}");
        }
        // Untouched by either rejected call.
        assert_eq!(store.item_quantity(character_id, "torch").await.unwrap(), 5);
    }

    #[tokio::test]
    #[ignore]
    async fn list_items_returns_every_distinct_stack() {
        let (store, character_id) = store_with_character().await;
        store.grant_item(character_id, "torch", 3).await.unwrap();
        store.grant_item(character_id, "shield", 1).await.unwrap();

        let mut items = store.list_items(character_id).await.unwrap();
        items.sort();
        assert_eq!(
            items,
            vec![("shield".to_string(), 1), ("torch".to_string(), 3)]
        );
    }

    #[tokio::test]
    #[ignore]
    async fn a_new_character_starts_with_zero_currency() {
        let (store, character_id) = store_with_character().await;
        assert_eq!(
            store.currency_balance(character_id, "gold").await.unwrap(),
            0
        );
    }

    #[tokio::test]
    #[ignore]
    async fn modify_currency_applies_a_positive_delta() {
        let (store, character_id) = store_with_character().await;
        let balance = store
            .modify_currency(character_id, "gold", 100)
            .await
            .unwrap();
        assert_eq!(balance, 100);
    }

    #[tokio::test]
    #[ignore]
    async fn modify_currency_going_negative_is_rejected_and_unapplied() {
        let (store, character_id) = store_with_character().await;
        store
            .modify_currency(character_id, "gold", 50)
            .await
            .unwrap();
        assert!(
            store
                .modify_currency(character_id, "gold", -100)
                .await
                .is_err()
        );
        assert_eq!(
            store.currency_balance(character_id, "gold").await.unwrap(),
            50
        );
    }

    #[tokio::test]
    #[ignore]
    async fn modify_currency_overflow_is_rejected_and_unapplied() {
        let (store, character_id) = store_with_character().await;
        store
            .modify_currency(character_id, "gold", i64::MAX)
            .await
            .unwrap();

        let err = store
            .modify_currency(character_id, "gold", i64::MAX)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("overflowed"), "{err}");
        assert_eq!(
            store.currency_balance(character_id, "gold").await.unwrap(),
            i64::MAX
        );
    }

    #[tokio::test]
    #[ignore]
    async fn two_currencies_on_the_same_character_have_independent_balances() {
        let (store, character_id) = store_with_character().await;
        store
            .modify_currency(character_id, "gold", 100)
            .await
            .unwrap();
        store
            .modify_currency(character_id, "honor", 5)
            .await
            .unwrap();

        assert_eq!(
            store.currency_balance(character_id, "gold").await.unwrap(),
            100
        );
        assert_eq!(
            store.currency_balance(character_id, "honor").await.unwrap(),
            5
        );

        // Draining "honor" to zero (and rejecting going further negative)
        // never touches "gold".
        store
            .modify_currency(character_id, "honor", -5)
            .await
            .unwrap();
        assert!(
            store
                .modify_currency(character_id, "honor", -1)
                .await
                .is_err()
        );
        assert_eq!(
            store.currency_balance(character_id, "gold").await.unwrap(),
            100
        );
        assert_eq!(
            store.currency_balance(character_id, "honor").await.unwrap(),
            0
        );
    }
}
