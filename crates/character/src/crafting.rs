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
use rand::RngExt;
use sqlx::Row;

use crate::crafting_schema::Recipe;
use crate::store::CharacterStore;

/// The result of a successful `craft_item` (#289 added `stat_changes` to
/// what was previously a bare `Vec<(item_type, quantity)>`).
#[derive(Debug, Clone, PartialEq)]
pub struct CraftOutcome {
    /// `(item_type, resulting_quantity)` for every stack this craft
    /// touched — every consumed input (0 if a stack was fully consumed)
    /// followed by the granted output, in recipe declaration order.
    pub item_changes: Vec<(String, i64)>,
    /// `(stat_key, resulting_value)` for every declared `grants` delta
    /// (#289), in declaration order — empty if the recipe declares none.
    pub stat_changes: Vec<(String, i64)>,
}

impl CharacterStore {
    /// Resolves `recipe` against `character_id`'s current inventory and,
    /// in one transaction, consumes every declared input and grants the
    /// declared output — or changes nothing at all. Rejected (nothing
    /// consumed or granted) if any input is missing or insufficient, if
    /// the output would be a *new* stack and the character is already at
    /// `InventoryConfig::max_distinct_item_types` (same soft cap
    /// `grant_item` enforces), or (#289) if any declared `requires` isn't
    /// met by the character's *current* stat value — checked before the
    /// transaction opens, so an ungated craft never even attempts the
    /// input check.
    ///
    /// Every declared `grants` delta (#289) is applied *inside* the same
    /// transaction as the craft's item changes, via
    /// `apply_stat_delta_clamped_tx` — clamped to `stat`'s declared
    /// bounds rather than rejected (unlike `equipment_schema.rs`'s
    /// `stat_deltas`, which uses the rejecting `apply_stat_delta`): a
    /// capped growth stat like profession XP already at its max must
    /// never fail the craft that produced it, it should just stop
    /// growing. Clamping is what makes this safe to run atomically —
    /// with a rejecting write, "already at cap" would roll back the
    /// whole craft, silently blocking a recipe forever once its grant
    /// stat maxes out. A grant with a `below` ceiling is skipped
    /// entirely (no delta, not even a clamped one) if the character's
    /// current stat value has already reached it; a grant with a
    /// `chance` is independently rolled per grant entry, skipped
    /// entirely on a miss.
    pub async fn craft_item(
        &self,
        character_id: CharacterId,
        recipe: &Recipe,
    ) -> Result<CraftOutcome> {
        for req in &recipe.requires {
            let current = self.get_stat(character_id, &req.stat).await?;
            if current < req.min {
                return Err(Error::new(
                    "character",
                    format!(
                        "craft \"{}\" requires {} to be at least {}, character {} is at {}",
                        recipe.key, req.stat, req.min, character_id, current
                    ),
                ));
            }
        }

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

        let mut stat_changes = Vec::with_capacity(recipe.grants.len());
        for grant in &recipe.grants {
            if let Some(below) = grant.below {
                let current = self.get_stat_tx(&mut tx, character_id, &grant.stat).await?;
                if current >= below {
                    continue;
                }
            }
            if let Some(chance) = grant.chance
                && !rand::rng().random_bool(chance)
            {
                continue;
            }

            let new_value = self
                .apply_stat_delta_clamped_tx(&mut tx, character_id, &grant.stat, grant.amount)
                .await?;
            stat_changes.push((grant.stat.clone(), new_value));
        }

        tx.commit()
            .await
            .map_err(|e| Error::wrap("character", "failed to commit craft", e))?;

        Ok(CraftOutcome {
            item_changes: results,
            stat_changes,
        })
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

    /// A schema declaring the two stats `crafting_with_an_unmet_requires_stat_is_rejected`/
    /// `crafting_with_a_met_requires_stat_applies_its_grants` (#289) exercise.
    fn schema_with_profession_stats() -> AttributeSchema {
        AttributeSchema::from_yaml(
            r#"
schema_version: 1
stats:
  - key: profession.blacksmithing_level
    type: int
    default: 1
    min: 1
    max: 100
  - key: profession.blacksmithing_xp
    type: int
    default: 0
"#,
        )
        .unwrap()
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
            requires: Vec::new(),
            grants: Vec::new(),
        }
    }

    /// #289 — `dagger_recipe` with a `requires`/`grants` pair added: gated
    /// behind `profession.blacksmithing_level >= 5`, grants
    /// `profession.blacksmithing_xp` +10 on success.
    fn dagger_recipe_with_profession_hooks() -> Recipe {
        Recipe {
            requires: vec![crate::crafting_schema::StatRequirement {
                stat: "profession.blacksmithing_level".to_string(),
                min: 5,
            }],
            grants: vec![crate::crafting_schema::StatGrant {
                stat: "profession.blacksmithing_xp".to_string(),
                amount: 10,
                below: None,
                chance: None,
            }],
            ..dagger_recipe()
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
        store_with_character_and_schema(schema()).await
    }

    async fn store_with_character_and_schema(
        attribute_schema: AttributeSchema,
    ) -> (CharacterStore, CharacterId) {
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
        let store = CharacterStore::new(pool, attribute_schema, Default::default());
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

        let outcome = store
            .craft_item(character_id, &dagger_recipe())
            .await
            .unwrap();
        assert_eq!(
            outcome.item_changes,
            vec![
                ("wolf-fang".to_string(), 0),
                ("iron-ore".to_string(), 0),
                ("wolf-fang-dagger".to_string(), 1),
            ]
        );
        assert!(outcome.stat_changes.is_empty());

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
                ..Default::default()
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

    // #289 — requires/grants stat hooks.

    #[tokio::test]
    #[ignore]
    async fn crafting_with_an_unmet_requires_stat_is_rejected_and_consumes_nothing() {
        let (store, character_id) =
            store_with_character_and_schema(schema_with_profession_stats()).await;
        store
            .grant_item(character_id, "wolf-fang", 3)
            .await
            .unwrap();
        store.grant_item(character_id, "iron-ore", 2).await.unwrap();
        // profession.blacksmithing_level defaults to 1; the recipe
        // requires >= 5.

        let err = store
            .craft_item(character_id, &dagger_recipe_with_profession_hooks())
            .await
            .unwrap_err();
        assert!(
            err.to_string().contains("profession.blacksmithing_level"),
            "{err}"
        );

        // Nothing consumed — the requires check runs before the craft's
        // own transaction even opens.
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
    async fn crafting_with_a_met_requires_stat_succeeds_and_applies_grants() {
        let (store, character_id) =
            store_with_character_and_schema(schema_with_profession_stats()).await;
        store
            .grant_item(character_id, "wolf-fang", 3)
            .await
            .unwrap();
        store.grant_item(character_id, "iron-ore", 2).await.unwrap();
        store
            .set_stat(character_id, "profession.blacksmithing_level", 5)
            .await
            .unwrap();

        let outcome = store
            .craft_item(character_id, &dagger_recipe_with_profession_hooks())
            .await
            .unwrap();

        assert_eq!(
            outcome.item_changes,
            vec![
                ("wolf-fang".to_string(), 0),
                ("iron-ore".to_string(), 0),
                ("wolf-fang-dagger".to_string(), 1),
            ]
        );
        assert_eq!(
            outcome.stat_changes,
            vec![("profession.blacksmithing_xp".to_string(), 10)]
        );
        assert_eq!(
            store
                .get_stat(character_id, "profession.blacksmithing_xp")
                .await
                .unwrap(),
            10
        );
    }

    #[tokio::test]
    #[ignore]
    async fn crafting_grant_clamps_at_the_stats_declared_max_instead_of_failing_the_craft() {
        let schema = AttributeSchema::from_yaml(
            r#"
schema_version: 1
stats:
  - key: profession.blacksmithing_xp
    type: int
    default: 10
    max: 15
"#,
        )
        .unwrap();
        let (store, character_id) = store_with_character_and_schema(schema).await;
        store
            .grant_item(character_id, "wolf-fang", 3)
            .await
            .unwrap();
        store.grant_item(character_id, "iron-ore", 2).await.unwrap();

        // Starting at 10, +10 would overflow the declared max of 15 — the
        // grant must clamp to 15, not reject the whole craft.
        let recipe = Recipe {
            grants: vec![crate::crafting_schema::StatGrant {
                stat: "profession.blacksmithing_xp".to_string(),
                amount: 10,
                below: None,
                chance: None,
            }],
            ..dagger_recipe()
        };

        let outcome = store.craft_item(character_id, &recipe).await.unwrap();
        assert_eq!(
            outcome.stat_changes,
            vec![("profession.blacksmithing_xp".to_string(), 15)]
        );
        assert_eq!(
            store
                .get_stat(character_id, "profession.blacksmithing_xp")
                .await
                .unwrap(),
            15
        );
        // The craft itself still went through — clamping doesn't block it.
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
    async fn crafting_grant_with_a_below_ceiling_is_skipped_once_the_ceiling_is_reached() {
        let schema = AttributeSchema::from_yaml(
            r#"
schema_version: 1
stats:
  - key: profession.blacksmithing_xp
    type: int
    default: 5
"#,
        )
        .unwrap();
        let (store, character_id) = store_with_character_and_schema(schema).await;
        store
            .grant_item(character_id, "wolf-fang", 3)
            .await
            .unwrap();
        store.grant_item(character_id, "iron-ore", 2).await.unwrap();

        // Character is already at 5, and `below: 5` means the grant only
        // applies while strictly under that ceiling — it must be skipped
        // entirely (no delta at all, not even a clamped one), while the
        // craft itself still succeeds.
        let recipe = Recipe {
            grants: vec![crate::crafting_schema::StatGrant {
                stat: "profession.blacksmithing_xp".to_string(),
                amount: 10,
                below: Some(5),
                chance: None,
            }],
            ..dagger_recipe()
        };

        let outcome = store.craft_item(character_id, &recipe).await.unwrap();
        assert!(
            outcome.stat_changes.is_empty(),
            "{:?}",
            outcome.stat_changes
        );
        assert_eq!(
            store
                .get_stat(character_id, "profession.blacksmithing_xp")
                .await
                .unwrap(),
            5
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
    async fn crafting_grant_with_chance_one_always_applies() {
        let (store, character_id) =
            store_with_character_and_schema(schema_with_profession_stats()).await;
        store
            .grant_item(character_id, "wolf-fang", 3)
            .await
            .unwrap();
        store.grant_item(character_id, "iron-ore", 2).await.unwrap();

        let recipe = Recipe {
            grants: vec![crate::crafting_schema::StatGrant {
                stat: "profession.blacksmithing_xp".to_string(),
                amount: 10,
                below: None,
                chance: Some(1.0),
            }],
            ..dagger_recipe()
        };

        let outcome = store.craft_item(character_id, &recipe).await.unwrap();
        assert_eq!(
            outcome.stat_changes,
            vec![("profession.blacksmithing_xp".to_string(), 10)]
        );
    }

    #[tokio::test]
    #[ignore]
    async fn crafting_grant_with_a_chance_applies_only_probabilistically() {
        let schema = AttributeSchema::from_yaml(
            r#"
schema_version: 1
stats:
  - key: profession.blacksmithing_xp
    type: int
    default: 0
"#,
        )
        .unwrap();
        let (store, character_id) = store_with_character_and_schema(schema).await;

        let recipe = Recipe {
            grants: vec![crate::crafting_schema::StatGrant {
                stat: "profession.blacksmithing_xp".to_string(),
                amount: 1,
                below: None,
                chance: Some(0.5),
            }],
            ..dagger_recipe()
        };

        const TRIALS: i64 = 40;
        for _ in 0..TRIALS {
            store
                .grant_item(character_id, "wolf-fang", 3)
                .await
                .unwrap();
            store.grant_item(character_id, "iron-ore", 2).await.unwrap();
            store.craft_item(character_id, &recipe).await.unwrap();
        }

        let xp = store
            .get_stat(character_id, "profession.blacksmithing_xp")
            .await
            .unwrap();
        // Each of TRIALS independent crafts applies its +1 grant with
        // probability 0.5 — a wide band (not an exact value, which would
        // be flaky) is enough to prove `chance` is actually gating the
        // grant instead of always (or never) applying it.
        assert!(xp > 5 && xp < TRIALS - 5, "xp = {xp}");
    }
}
