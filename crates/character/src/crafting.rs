//! Atomic craft consume/produce (#216, implementing #215's decision) —
//! the one write path that turns a resolved `crafting_schema::Recipe`
//! into real inventory changes. Same atomic-transaction discipline
//! `transfer::TransferExecutor::transfer_inner` uses: everything below
//! runs in one Postgres transaction, so a craft that fails partway
//! (an input turns out insufficient) leaves the character's inventory
//! exactly as it was, never partially consumed. Unlike `inventory.rs`'s
//! `grant_item`/`remove_item` (deliberately non-transactional, see that
//! module's own doc comment), a craft's multi-row exchange genuinely
//! needs the all-or-nothing guarantee, so this doesn't just call those
//! two methods back to back.

use common::id::CharacterId;
use common::{Error, Result};
use sqlx::Row;

use crate::crafting_schema::Recipe;
use crate::store::CharacterStore;

impl CharacterStore {
    /// Resolves `recipe` against `character_id`'s current inventory and,
    /// in one transaction, consumes every declared input and grants the
    /// declared output — or changes nothing at all. Rejected (nothing
    /// consumed or granted) if any input is missing or insufficient, or
    /// if the output would be a *new* stack and the character is already
    /// at `InventoryConfig::max_distinct_item_types` (same soft cap
    /// `grant_item` enforces). Returns the resulting `(item_type,
    /// quantity)` for every stack this craft touched — every consumed
    /// input (0 if a stack was fully consumed) followed by the granted
    /// output — so the caller can push accurate `ItemChanged` messages
    /// without a second read.
    pub async fn craft_item(
        &self,
        character_id: CharacterId,
        recipe: &Recipe,
    ) -> Result<Vec<(String, i64)>> {
        let mut tx = self
            .pool()
            .begin()
            .await
            .map_err(|e| Error::wrap("character", "failed to start craft transaction", e))?;

        let mut current_quantities = Vec::with_capacity(recipe.inputs.len());
        for input in &recipe.inputs {
            let current: Option<i64> = sqlx::query_scalar(
                "SELECT quantity FROM items WHERE character_id = $1 AND item_type = $2 FOR UPDATE",
            )
            .bind(character_id.as_uuid())
            .bind(&input.item_type)
            .fetch_optional(&mut *tx)
            .await
            .map_err(|e| Error::wrap("character", "failed to check craft input", e))?;
            let current = current.unwrap_or(0);

            if current < input.amount {
                return Err(Error::new(
                    "character",
                    format!(
                        "craft \"{}\" requires {} of {:?}, character {} only has {}",
                        recipe.key, input.amount, input.item_type, character_id, current
                    ),
                ));
            }

            current_quantities.push(current);
        }

        // Capacity is checked against the pre-craft inventory state,
        // before any input is consumed — deliberately, so a craft that
        // would fully deplete an input stack (freeing a "slot") can't use
        // that same slot for its own new output within the same
        // transaction. Doing this check after the consume loop below
        // would let a craft "borrow" a slot it's about to vacate, which
        // isn't the intended cap semantics (same soft cap `grant_item`
        // enforces for an ordinary, non-craft grant).
        let output_already_owned: bool = sqlx::query(
            "SELECT 1 FROM items WHERE character_id = $1 AND item_type = $2 FOR UPDATE",
        )
        .bind(character_id.as_uuid())
        .bind(&recipe.output.item_type)
        .fetch_optional(&mut *tx)
        .await
        .map_err(|e| Error::wrap("character", "failed to check craft output stack", e))?
        .is_some();

        if !output_already_owned {
            let distinct_count: i64 =
                sqlx::query_scalar("SELECT COUNT(*) FROM items WHERE character_id = $1")
                    .bind(character_id.as_uuid())
                    .fetch_one(&mut *tx)
                    .await
                    .map_err(|e| Error::wrap("character", "failed to count item stacks", e))?;
            let max = i64::from(self.inventory_config().max_distinct_item_types);
            if distinct_count >= max {
                return Err(Error::new(
                    "character",
                    format!(
                        "inventory is full: {distinct_count} distinct item types already owned, \
                         limit is {max} (WZ_INVENTORY_MAX_ITEM_TYPES) — craft \"{}\" would add a new stack",
                        recipe.key
                    ),
                ));
            }
        }

        let mut results = Vec::with_capacity(recipe.inputs.len() + 1);
        for (input, current) in recipe.inputs.iter().zip(current_quantities) {
            let remaining = current - input.amount;
            if remaining == 0 {
                sqlx::query("DELETE FROM items WHERE character_id = $1 AND item_type = $2")
                    .bind(character_id.as_uuid())
                    .bind(&input.item_type)
                    .execute(&mut *tx)
                    .await
                    .map_err(|e| Error::wrap("character", "failed to consume craft input", e))?;
            } else {
                sqlx::query(
                    "UPDATE items SET quantity = $3, updated_at = now() \
                     WHERE character_id = $1 AND item_type = $2",
                )
                .bind(character_id.as_uuid())
                .bind(&input.item_type)
                .bind(remaining)
                .execute(&mut *tx)
                .await
                .map_err(|e| Error::wrap("character", "failed to consume craft input", e))?;
            }
            results.push((input.item_type.clone(), remaining));
        }

        let output_quantity: i64 = sqlx::query(
            "INSERT INTO items (id, character_id, item_type, quantity) \
             VALUES ($1, $2, $3, $4) \
             ON CONFLICT (character_id, item_type) \
             DO UPDATE SET quantity = items.quantity + EXCLUDED.quantity, updated_at = now() \
             RETURNING quantity",
        )
        .bind(uuid::Uuid::now_v7())
        .bind(character_id.as_uuid())
        .bind(&recipe.output.item_type)
        .bind(recipe.output.amount)
        .fetch_one(&mut *tx)
        .await
        .map_err(|e| Error::wrap("character", "failed to grant craft output", e))?
        .get("quantity");
        results.push((recipe.output.item_type.clone(), output_quantity));

        tx.commit()
            .await
            .map_err(|e| Error::wrap("character", "failed to commit craft", e))?;

        Ok(results)
    }
}

#[cfg(test)]
mod tests {
    use common::config::PostgresConfig;
    use common::id::{AccountId, RealmId};
    use common::pool::{PoolOptions, postgres_pool};

    use crate::crafting_schema::{CraftingInput, CraftingOutput, Recipe};
    use crate::inventory::InventoryConfig;
    use crate::schema::AttributeSchema;

    use super::*;

    fn schema() -> AttributeSchema {
        AttributeSchema::from_yaml("schema_version: 1\nstats: []\n").unwrap()
    }

    fn dagger_recipe() -> Recipe {
        Recipe {
            key: "wolf-fang-dagger".to_string(),
            category: "blacksmithing".to_string(),
            inputs: vec![
                CraftingInput {
                    item_type: "wolf-fang".to_string(),
                    amount: 3,
                },
                CraftingInput {
                    item_type: "iron-ore".to_string(),
                    amount: 2,
                },
            ],
            output: CraftingOutput {
                item_type: "wolf-fang-dagger".to_string(),
                amount: 1,
            },
        }
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

    async fn store_with_character() -> (CharacterStore, CharacterId) {
        let config = PostgresConfig::from_env().expect("WZ_POSTGRES_* env vars set");
        let pool = postgres_pool(&config, PoolOptions::default())
            .await
            .unwrap();

        let account_id = AccountId::new();
        sqlx::query("INSERT INTO accounts (id, username, password_hash) VALUES ($1, $2, 'unused')")
            .bind(account_id.as_uuid())
            .bind(format!("crafting-test-{account_id}"))
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

    #[tokio::test]
    #[ignore]
    async fn crafting_with_sufficient_inputs_consumes_and_grants_exactly_once() {
        let (store, character_id) = store_with_character().await;
        store
            .grant_item(character_id, "wolf-fang", 3)
            .await
            .unwrap();
        store.grant_item(character_id, "iron-ore", 2).await.unwrap();

        let results = store
            .craft_item(character_id, &dagger_recipe())
            .await
            .unwrap();
        assert_eq!(
            results,
            vec![
                ("wolf-fang".to_string(), 0),
                ("iron-ore".to_string(), 0),
                ("wolf-fang-dagger".to_string(), 1),
            ]
        );

        assert_eq!(
            store
                .item_quantity(character_id, "wolf-fang")
                .await
                .unwrap(),
            0
        );
        assert_eq!(
            store.item_quantity(character_id, "iron-ore").await.unwrap(),
            0
        );
        assert_eq!(
            store
                .item_quantity(character_id, "wolf-fang-dagger")
                .await
                .unwrap(),
            1
        );
    }

    #[tokio::test]
    #[ignore]
    async fn crafting_leaves_excess_input_quantity_behind() {
        let (store, character_id) = store_with_character().await;
        store
            .grant_item(character_id, "wolf-fang", 5)
            .await
            .unwrap();
        store.grant_item(character_id, "iron-ore", 4).await.unwrap();

        store
            .craft_item(character_id, &dagger_recipe())
            .await
            .unwrap();

        assert_eq!(
            store
                .item_quantity(character_id, "wolf-fang")
                .await
                .unwrap(),
            2
        );
        assert_eq!(
            store.item_quantity(character_id, "iron-ore").await.unwrap(),
            2
        );
    }

    #[tokio::test]
    #[ignore]
    async fn crafting_with_a_missing_input_fails_and_consumes_nothing() {
        let (store, character_id) = store_with_character().await;
        store
            .grant_item(character_id, "wolf-fang", 3)
            .await
            .unwrap();
        // No iron-ore granted at all.

        let err = store
            .craft_item(character_id, &dagger_recipe())
            .await
            .unwrap_err();
        assert!(err.to_string().contains("iron-ore"), "{err}");

        assert_eq!(
            store
                .item_quantity(character_id, "wolf-fang")
                .await
                .unwrap(),
            3
        );
        assert_eq!(
            store.item_quantity(character_id, "iron-ore").await.unwrap(),
            0
        );
        assert_eq!(
            store
                .item_quantity(character_id, "wolf-fang-dagger")
                .await
                .unwrap(),
            0
        );
    }

    #[tokio::test]
    #[ignore]
    async fn crafting_with_an_insufficient_input_fails_and_consumes_nothing() {
        let (store, character_id) = store_with_character().await;
        store
            .grant_item(character_id, "wolf-fang", 1)
            .await
            .unwrap();
        store.grant_item(character_id, "iron-ore", 2).await.unwrap();

        assert!(
            store
                .craft_item(character_id, &dagger_recipe())
                .await
                .is_err()
        );

        assert_eq!(
            store
                .item_quantity(character_id, "wolf-fang")
                .await
                .unwrap(),
            1
        );
        assert_eq!(
            store.item_quantity(character_id, "iron-ore").await.unwrap(),
            2
        );
    }

    #[tokio::test]
    #[ignore]
    async fn crafting_beyond_inventory_capacity_for_a_new_output_stack_is_rejected() {
        let config = PostgresConfig::from_env().expect("WZ_POSTGRES_* env vars set");
        let pool = postgres_pool(&config, PoolOptions::default())
            .await
            .unwrap();
        let account_id = AccountId::new();
        sqlx::query("INSERT INTO accounts (id, username, password_hash) VALUES ($1, $2, 'unused')")
            .bind(account_id.as_uuid())
            .bind(format!("crafting-capacity-test-{account_id}"))
            .execute(&pool)
            .await
            .unwrap();
        let realm_id = insert_realm(&pool).await;
        let store = CharacterStore::new(
            pool,
            schema(),
            InventoryConfig {
                max_distinct_item_types: 2,
            },
        );
        let character_id = store
            .create(account_id, "Test Character", realm_id, "greenwood-forest")
            .await
            .unwrap();
        store
            .grant_item(character_id, "wolf-fang", 3)
            .await
            .unwrap();
        store.grant_item(character_id, "iron-ore", 2).await.unwrap();
        // Already at the cap of 2 distinct stacks — the dagger would be a
        // third, brand-new stack.

        let err = store
            .craft_item(character_id, &dagger_recipe())
            .await
            .unwrap_err();
        assert!(err.to_string().contains("inventory is full"), "{err}");

        // Untouched — the whole craft rolled back.
        assert_eq!(
            store
                .item_quantity(character_id, "wolf-fang")
                .await
                .unwrap(),
            3
        );
        assert_eq!(
            store.item_quantity(character_id, "iron-ore").await.unwrap(),
            2
        );
    }
}
