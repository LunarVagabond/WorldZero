//! Player-directed inventory slot ordering (#276, split out of #243 with
//! the design decision recorded on that now-closed issue). `items.slot_index`
//! is a bare nullable position — `NULL` means "unsorted," and the core
//! never auto-assigns one; a stack keeps whatever slot it has across
//! ordinary `grant_item`/`remove_item`/`craft_item` traffic (none of
//! those touch `slot_index`), losing it only when the row itself is
//! deleted (the stack hit zero).
//!
//! Swap semantics on conflict, same atomic-transaction discipline as
//! `crafting.rs`: if the target slot is already occupied by another of
//! the caller's own stacks, that stack takes the mover's *previous* slot
//! (or goes back to unsorted if the mover had none) rather than
//! rejecting the move — ordinary drag-and-drop UX.

use common::id::CharacterId;
use common::{Error, Result};
use sqlx::Row;

use crate::store::CharacterStore;

/// The result of a successful `move_item_to_slot` — enough for the
/// caller to push one `ItemMoved` for the mover and, if a swap actually
/// happened, one more for the displaced stack.
#[derive(Debug, Clone, PartialEq)]
pub struct SlotMove {
    pub moved_item_type: String,
    pub moved_slot_index: i32,
    /// `(item_type, slot_index)` of a stack displaced by this move, if
    /// any — `slot_index` is `None` if the mover had no previous slot
    /// (the displaced stack goes back to unsorted).
    pub displaced: Option<(String, Option<i32>)>,
}

impl CharacterStore {
    /// Moves `character_id`'s stack of `item_type` to `slot_index`.
    /// Rejected (storage untouched) if the caller doesn't hold
    /// `item_type`, or if `slot_index` is outside
    /// `[0, InventoryConfig::slot_count)`.
    pub async fn move_item_to_slot(
        &self,
        character_id: CharacterId,
        item_type: &str,
        slot_index: i32,
    ) -> Result<SlotMove> {
        let slot_count = i32::try_from(self.inventory_config().slot_count).unwrap_or(i32::MAX);
        if slot_index < 0 || slot_index >= slot_count {
            return Err(Error::new(
                "character",
                format!(
                    "slot_index {slot_index} is out of range [0, {slot_count}) \
                     (WZ_INVENTORY_SLOT_COUNT)"
                ),
            ));
        }

        let mut tx =
            self.pool().begin().await.map_err(|e| {
                Error::wrap("character", "failed to start slot-move transaction", e)
            })?;

        let previous_slot: Option<i32> = sqlx::query_scalar(
            "SELECT slot_index FROM items WHERE character_id = $1 AND item_type = $2 FOR UPDATE",
        )
        .bind(character_id.as_uuid())
        .bind(item_type)
        .fetch_optional(&mut *tx)
        .await
        .map_err(|e| Error::wrap("character", "failed to read mover's current slot", e))?
        .ok_or_else(|| {
            Error::new(
                "character",
                format!("character {character_id} does not own item {item_type:?}"),
            )
        })?;

        let occupant: Option<String> = sqlx::query(
            "SELECT item_type FROM items \
             WHERE character_id = $1 AND slot_index = $2 AND item_type != $3 \
             FOR UPDATE",
        )
        .bind(character_id.as_uuid())
        .bind(slot_index)
        .bind(item_type)
        .fetch_optional(&mut *tx)
        .await
        .map_err(|e| Error::wrap("character", "failed to check target slot occupant", e))?
        .map(|row| row.get("item_type"));

        sqlx::query("UPDATE items SET slot_index = $3, updated_at = now() WHERE character_id = $1 AND item_type = $2")
            .bind(character_id.as_uuid())
            .bind(item_type)
            .bind(slot_index)
            .execute(&mut *tx)
            .await
            .map_err(|e| Error::wrap("character", "failed to move item to slot", e))?;

        if let Some(occupant_type) = &occupant {
            sqlx::query("UPDATE items SET slot_index = $3, updated_at = now() WHERE character_id = $1 AND item_type = $2")
                .bind(character_id.as_uuid())
                .bind(occupant_type)
                .bind(previous_slot)
                .execute(&mut *tx)
                .await
                .map_err(|e| Error::wrap("character", "failed to displace occupant slot", e))?;
        }

        tx.commit()
            .await
            .map_err(|e| Error::wrap("character", "failed to commit slot-move transaction", e))?;

        Ok(SlotMove {
            moved_item_type: item_type.to_string(),
            moved_slot_index: slot_index,
            displaced: occupant.map(|occupant_type| (occupant_type, previous_slot)),
        })
    }
}

#[cfg(test)]
mod tests {
    use common::config::PostgresConfig;
    use common::id::{AccountId, RealmId};
    use common::pool::{PoolOptions, postgres_pool};

    use crate::inventory::InventoryConfig;
    use crate::schema::AttributeSchema;

    use super::*;

    fn schema() -> AttributeSchema {
        AttributeSchema::from_yaml("schema_version: 1\nstats: []\n").unwrap()
    }

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
            .bind(format!("slots-test-{account_id}"))
            .execute(&pool)
            .await
            .unwrap();

        let realm_id = insert_realm(&pool).await;
        let store = CharacterStore::new(
            pool,
            schema(),
            InventoryConfig {
                slot_count: 8,
                ..Default::default()
            },
        );
        let character_id = store
            .create(account_id, "Test Character", realm_id, "greenwood-forest")
            .await
            .unwrap();

        (store, character_id)
    }

    #[tokio::test]
    #[ignore]
    async fn moving_an_item_to_an_empty_slot_places_it_there() {
        let (store, character_id) = store_with_character().await;
        store.grant_item(character_id, "torch", 1).await.unwrap();

        let result = store
            .move_item_to_slot(character_id, "torch", 3)
            .await
            .unwrap();
        assert_eq!(result.moved_slot_index, 3);
        assert!(result.displaced.is_none());
    }

    #[tokio::test]
    #[ignore]
    async fn moving_an_item_the_caller_does_not_own_is_rejected() {
        let (store, character_id) = store_with_character().await;
        let err = store
            .move_item_to_slot(character_id, "torch", 3)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("does not own"), "{err}");
    }

    #[tokio::test]
    #[ignore]
    async fn moving_to_an_out_of_range_slot_is_rejected() {
        let (store, character_id) = store_with_character().await;
        store.grant_item(character_id, "torch", 1).await.unwrap();
        let err = store
            .move_item_to_slot(character_id, "torch", 8)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("out of range"), "{err}");

        let err = store
            .move_item_to_slot(character_id, "torch", -1)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("out of range"), "{err}");
    }

    #[tokio::test]
    #[ignore]
    async fn moving_onto_an_occupied_slot_swaps_the_occupant_to_the_movers_previous_slot() {
        let (store, character_id) = store_with_character().await;
        store.grant_item(character_id, "torch", 1).await.unwrap();
        store.grant_item(character_id, "shield", 1).await.unwrap();

        store
            .move_item_to_slot(character_id, "torch", 2)
            .await
            .unwrap();
        store
            .move_item_to_slot(character_id, "shield", 5)
            .await
            .unwrap();

        // torch (slot 2) moves onto shield's slot (5) — shield takes
        // torch's previous slot (2).
        let result = store
            .move_item_to_slot(character_id, "torch", 5)
            .await
            .unwrap();
        assert_eq!(result.moved_slot_index, 5);
        assert_eq!(result.displaced, Some(("shield".to_string(), Some(2))));
    }

    #[tokio::test]
    #[ignore]
    async fn moving_onto_an_occupied_slot_when_mover_was_unsorted_sends_occupant_back_to_unsorted()
    {
        let (store, character_id) = store_with_character().await;
        store.grant_item(character_id, "torch", 1).await.unwrap();
        store.grant_item(character_id, "shield", 1).await.unwrap();

        store
            .move_item_to_slot(character_id, "shield", 5)
            .await
            .unwrap();

        // torch has never been placed (unsorted / NULL) — moving it onto
        // shield's slot sends shield back to unsorted.
        let result = store
            .move_item_to_slot(character_id, "torch", 5)
            .await
            .unwrap();
        assert_eq!(result.displaced, Some(("shield".to_string(), None)));
    }
}
