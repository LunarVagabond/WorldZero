//! Equip/unequip against the declared `equipment.schema.yaml` (#277,
//! split out of #245 with the design decision recorded on that now-closed
//! issue) — the mechanism half of the equipment system: the schema says
//! *what* an item does when worn, this module does the actual multi-step
//! exchange (stat delta, inventory, `equipped_items` row).
//!
//! Deliberately **not** wrapped in one Postgres transaction — same
//! "best-effort, not fully transactional" stance `inventory.rs`'s own doc
//! comment already takes, and how `craft_item`'s own multi-step exchange
//! is sequenced (stat delta before inventory, so a delta rejected by
//! `AttributeSchema`'s declared bounds leaves the rest of the exchange
//! untouched). A displaced occupant (equipping into an already-occupied
//! slot) is unequipped *first*, before the new item's own delta is
//! applied — if the new item's delta then fails, the occupant has already
//! been returned to inventory and its own delta reversed; this is an
//! accepted soft-UX-limit tradeoff, not a data-integrity boundary, same
//! as every other non-transactional write in this crate.

use std::collections::HashMap;

use common::id::CharacterId;
use common::{Error, Result};
use sqlx::Row;

use crate::equipment_schema::EquipmentSchema;
use crate::store::CharacterStore;

/// The result of a successful `equip_item` — enough for the caller to
/// push `StatChanged`/`ItemChanged` for every value that actually
/// changed, then one `EquipmentChanged` for the slot's new occupant.
#[derive(Debug, Clone, PartialEq)]
pub struct EquipOutcome {
    pub slot: String,
    pub item_type: String,
    /// `(stat_key, resulting_value)`, in application order — the new
    /// item's own deltas, preceded by a displaced occupant's *reversed*
    /// deltas if this equip swapped one out.
    pub stat_changes: Vec<(String, i64)>,
    /// `(item_type, resulting_quantity)`, in application order — the
    /// displaced occupant granted back (if any), then the newly equipped
    /// item removed from inventory.
    pub item_changes: Vec<(String, i64)>,
}

/// The result of a successful `unequip_item`.
#[derive(Debug, Clone, PartialEq)]
pub struct UnequipOutcome {
    pub slot: String,
    pub item_type: String,
    pub stat_changes: Vec<(String, i64)>,
    pub item_changes: Vec<(String, i64)>,
}

impl CharacterStore {
    /// Equips `item_type` from `character_id`'s inventory into the slot
    /// `equipment_schema` declares for it. Rejected (nothing changed) if
    /// `item_type` isn't declared as equippable at all, or the caller
    /// doesn't hold it. If the target slot is already occupied by another
    /// item, that item is unequipped first (its deltas reversed, granted
    /// back to inventory) rather than the request being rejected.
    pub async fn equip_item(
        &self,
        character_id: CharacterId,
        item_type: &str,
        equipment_schema: &EquipmentSchema,
    ) -> Result<EquipOutcome> {
        let declared = equipment_schema.resolve(item_type)?;
        let slot = declared.slot.clone();
        let stat_deltas = declared.stat_deltas.clone();

        if self.item_quantity(character_id, item_type).await? < 1 {
            return Err(Error::new(
                "character",
                format!("character {character_id} does not own item {item_type:?}"),
            ));
        }

        let mut stat_changes = Vec::new();
        let mut item_changes = Vec::new();

        if let Some(occupant_type) = self.equipped_in_slot(character_id, &slot).await? {
            let occupant = self
                .unequip_inner(character_id, &slot, &occupant_type, equipment_schema)
                .await?;
            stat_changes.extend(occupant.stat_changes);
            item_changes.extend(occupant.item_changes);
        }

        for (stat_key, delta) in &stat_deltas {
            let new_value = self
                .apply_stat_delta(character_id, stat_key, *delta)
                .await?;
            stat_changes.push((stat_key.clone(), new_value));
        }

        let remaining = self.remove_item(character_id, item_type, 1).await?;
        item_changes.push((item_type.to_string(), remaining));

        sqlx::query(
            "INSERT INTO equipped_items (character_id, slot, item_type) VALUES ($1, $2, $3) \
             ON CONFLICT (character_id, slot) DO UPDATE SET item_type = EXCLUDED.item_type, equipped_at = now()",
        )
        .bind(character_id.as_uuid())
        .bind(&slot)
        .bind(item_type)
        .execute(self.pool())
        .await
        .map_err(|e| Error::wrap("character", "failed to record equipped item", e))?;

        Ok(EquipOutcome {
            slot,
            item_type: item_type.to_string(),
            stat_changes,
            item_changes,
        })
    }

    /// Unequips whatever currently occupies `slot` for `character_id`.
    /// Rejected if the slot isn't currently occupied.
    pub async fn unequip_item(
        &self,
        character_id: CharacterId,
        slot: &str,
        equipment_schema: &EquipmentSchema,
    ) -> Result<UnequipOutcome> {
        let Some(item_type) = self.equipped_in_slot(character_id, slot).await? else {
            return Err(Error::new(
                "character",
                format!("character {character_id} has nothing equipped in slot {slot:?}"),
            ));
        };

        self.unequip_inner(character_id, slot, &item_type, equipment_schema)
            .await
    }

    /// Shared by `equip_item` (displacing an occupant) and `unequip_item`
    /// — reverses `item_type`'s deltas, grants it back to inventory, and
    /// clears the `equipped_items` row.
    async fn unequip_inner(
        &self,
        character_id: CharacterId,
        slot: &str,
        item_type: &str,
        equipment_schema: &EquipmentSchema,
    ) -> Result<UnequipOutcome> {
        let stat_deltas: HashMap<String, i64> = equipment_schema
            .resolve(item_type)
            .map(|declared| declared.stat_deltas.clone())
            .unwrap_or_default();

        let mut stat_changes = Vec::new();
        for (stat_key, delta) in &stat_deltas {
            let new_value = self
                .apply_stat_delta(character_id, stat_key, -delta)
                .await?;
            stat_changes.push((stat_key.clone(), new_value));
        }

        let new_quantity = self.grant_item(character_id, item_type, 1).await?;

        sqlx::query("DELETE FROM equipped_items WHERE character_id = $1 AND slot = $2")
            .bind(character_id.as_uuid())
            .bind(slot)
            .execute(self.pool())
            .await
            .map_err(|e| Error::wrap("character", "failed to clear equipped slot", e))?;

        Ok(UnequipOutcome {
            slot: slot.to_string(),
            item_type: item_type.to_string(),
            stat_changes,
            item_changes: vec![(item_type.to_string(), new_quantity)],
        })
    }

    /// `None` if `slot` isn't currently occupied for `character_id`.
    pub async fn equipped_in_slot(
        &self,
        character_id: CharacterId,
        slot: &str,
    ) -> Result<Option<String>> {
        let row = sqlx::query(
            "SELECT item_type FROM equipped_items WHERE character_id = $1 AND slot = $2",
        )
        .bind(character_id.as_uuid())
        .bind(slot)
        .fetch_optional(self.pool())
        .await
        .map_err(|e| Error::wrap("character", "failed to read equipped slot", e))?;
        Ok(row.map(|r| r.get("item_type")))
    }

    /// Every currently-equipped `(slot, item_type)` pair for
    /// `character_id` — inspection/debugging convenience, same status as
    /// `list_items`.
    pub async fn list_equipped(&self, character_id: CharacterId) -> Result<Vec<(String, String)>> {
        let rows =
            sqlx::query("SELECT slot, item_type FROM equipped_items WHERE character_id = $1")
                .bind(character_id.as_uuid())
                .fetch_all(self.pool())
                .await
                .map_err(|e| Error::wrap("character", "failed to list equipped items", e))?;
        Ok(rows
            .into_iter()
            .map(|r| (r.get("slot"), r.get("item_type")))
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use common::config::PostgresConfig;
    use common::id::{AccountId, RealmId};
    use common::pool::{PoolOptions, postgres_pool};

    use crate::equipment_schema::EquipmentSchema;
    use crate::schema::AttributeSchema;

    use super::*;

    fn attribute_schema() -> AttributeSchema {
        AttributeSchema::from_yaml(
            r#"
schema_version: 1
stats:
  - key: attack
    type: int
    default: 10
    min: 0
    max: 1000
  - key: defense
    type: int
    default: 5
    min: 0
    max: 1000
"#,
        )
        .unwrap()
    }

    fn equipment_schema() -> EquipmentSchema {
        EquipmentSchema::from_yaml(
            r#"
schema_version: 1
slots:
  - head
  - weapon
items:
  - item_type: iron-helmet
    slot: head
    stat_deltas:
      defense: 5
  - item_type: cloth-cap
    slot: head
    stat_deltas:
      defense: 1
  - item_type: iron-sword
    slot: weapon
    stat_deltas:
      attack: 10
"#,
            &attribute_schema(),
        )
        .unwrap()
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
            .bind(format!("equipment-test-{account_id}"))
            .execute(&pool)
            .await
            .unwrap();

        let realm_id = insert_realm(&pool).await;
        let store = CharacterStore::new(pool, attribute_schema(), Default::default());
        let character_id = store
            .create(account_id, "Test Character", realm_id, "greenwood-forest")
            .await
            .unwrap();

        (store, character_id)
    }

    #[tokio::test]
    #[ignore]
    async fn equipping_an_owned_item_applies_deltas_and_removes_it_from_inventory() {
        let (store, character_id) = store_with_character().await;
        store
            .grant_item(character_id, "iron-helmet", 1)
            .await
            .unwrap();

        let outcome = store
            .equip_item(character_id, "iron-helmet", &equipment_schema())
            .await
            .unwrap();

        assert_eq!(outcome.slot, "head");
        assert_eq!(outcome.item_type, "iron-helmet");
        assert_eq!(outcome.stat_changes, vec![("defense".to_string(), 10)]);
        assert_eq!(outcome.item_changes, vec![("iron-helmet".to_string(), 0)]);
        assert_eq!(
            store.equipped_in_slot(character_id, "head").await.unwrap(),
            Some("iron-helmet".to_string())
        );
        assert_eq!(
            store
                .item_quantity(character_id, "iron-helmet")
                .await
                .unwrap(),
            0
        );
    }

    #[tokio::test]
    #[ignore]
    async fn equipping_an_item_the_caller_does_not_own_is_rejected() {
        let (store, character_id) = store_with_character().await;
        let err = store
            .equip_item(character_id, "iron-helmet", &equipment_schema())
            .await
            .unwrap_err();
        assert!(err.to_string().contains("does not own"), "{err}");
    }

    #[tokio::test]
    #[ignore]
    async fn equipping_an_item_not_declared_as_equippable_is_rejected() {
        let (store, character_id) = store_with_character().await;
        store.grant_item(character_id, "torch", 1).await.unwrap();
        let err = store
            .equip_item(character_id, "torch", &equipment_schema())
            .await
            .unwrap_err();
        assert!(err.to_string().contains("not equippable"), "{err}");
    }

    #[tokio::test]
    #[ignore]
    async fn equipping_into_an_occupied_slot_unequips_the_occupant_first() {
        let (store, character_id) = store_with_character().await;
        store
            .grant_item(character_id, "cloth-cap", 1)
            .await
            .unwrap();
        store
            .grant_item(character_id, "iron-helmet", 1)
            .await
            .unwrap();

        store
            .equip_item(character_id, "cloth-cap", &equipment_schema())
            .await
            .unwrap();
        let outcome = store
            .equip_item(character_id, "iron-helmet", &equipment_schema())
            .await
            .unwrap();

        // defense starts at the declared default (5), goes to 6 after
        // equipping cloth-cap (+1). Displacing it back to 5 (-1), then
        // applying iron-helmet's own +5 lands on 10.
        assert_eq!(
            outcome.stat_changes,
            vec![("defense".to_string(), 5), ("defense".to_string(), 10)]
        );
        assert_eq!(
            outcome.item_changes,
            vec![("cloth-cap".to_string(), 1), ("iron-helmet".to_string(), 0)]
        );
        assert_eq!(
            store.equipped_in_slot(character_id, "head").await.unwrap(),
            Some("iron-helmet".to_string())
        );
        assert_eq!(
            store
                .item_quantity(character_id, "cloth-cap")
                .await
                .unwrap(),
            1
        );
    }

    #[tokio::test]
    #[ignore]
    async fn unequipping_reverses_deltas_and_returns_the_item_to_inventory() {
        let (store, character_id) = store_with_character().await;
        store
            .grant_item(character_id, "iron-helmet", 1)
            .await
            .unwrap();
        store
            .equip_item(character_id, "iron-helmet", &equipment_schema())
            .await
            .unwrap();

        let outcome = store
            .unequip_item(character_id, "head", &equipment_schema())
            .await
            .unwrap();

        assert_eq!(outcome.item_type, "iron-helmet");
        assert_eq!(outcome.stat_changes, vec![("defense".to_string(), 5)]);
        assert_eq!(outcome.item_changes, vec![("iron-helmet".to_string(), 1)]);
        assert_eq!(
            store.equipped_in_slot(character_id, "head").await.unwrap(),
            None
        );
        assert_eq!(
            store
                .item_quantity(character_id, "iron-helmet")
                .await
                .unwrap(),
            1
        );
    }

    #[tokio::test]
    #[ignore]
    async fn unequipping_an_empty_slot_is_rejected() {
        let (store, character_id) = store_with_character().await;
        let err = store
            .unequip_item(character_id, "head", &equipment_schema())
            .await
            .unwrap_err();
        assert!(err.to_string().contains("nothing equipped"), "{err}");
    }
}
