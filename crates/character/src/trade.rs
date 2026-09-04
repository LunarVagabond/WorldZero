//! Atomic player-to-player trade execution (#278, split out of #244 with
//! the design decision recorded on that now-closed issue) — the one
//! write path that turns two already-negotiated `TradeOfferInput`s into
//! a real, simultaneous exchange. Same atomic-transaction discipline
//! `crafting.rs`'s `craft_item` uses: everything below runs in one
//! Postgres transaction, locking every row involved (`FOR UPDATE`)
//! before touching any of them, so a trade that turns out invalid at
//! execution time (either side has since spent or lost something the
//! negotiation didn't know about) leaves both characters' inventory and
//! currency exactly as they were — never a half-completed trade.
//!
//! This module has no notion of the negotiation itself (offer state,
//! confirm/cancel, the anti-scam "any change resets confirmation" rule)
//! — that's `server::session`'s job, using an in-memory session the same
//! "ephemeral, not durable" way `PendingPartyInvites` already is. By the
//! time `execute_trade` is called, both sides have already confirmed;
//! this only re-validates that what was offered is still actually held.

use std::collections::HashMap;

use common::id::CharacterId;
use common::{Error, Result};
use sqlx::Row;

use crate::store::CharacterStore;

/// One side's negotiated offer, as sets of `(key, positive amount)` —
/// zero/negative amounts have no meaning here (a caller clears an offered
/// key by omitting it, same "0 removes it" convention `server::session`'s
/// `TradeOfferItem`/`TradeOfferCurrency` already establish at the wire
/// level) and are rejected defensively rather than silently ignored.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct TradeOfferInput {
    pub items: Vec<(String, i64)>,
    pub currency: Vec<(String, i64)>,
}

/// Resulting `(key, value)` pairs for one side of a completed trade —
/// enough for the caller to push accurate `ItemChanged`/`CurrencyChanged`
/// for everything that changed, both what was given away and what was
/// received, without a second read.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct TradeSideResult {
    pub item_changes: Vec<(String, i64)>,
    pub currency_changes: Vec<(String, i64)>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct TradeResult {
    pub a: TradeSideResult,
    pub b: TradeSideResult,
}

impl CharacterStore {
    /// Exchanges `offer_a` (given by `character_a`, received by
    /// `character_b`) for `offer_b` (given by `character_b`, received by
    /// `character_a`) — atomically, in one transaction. Rejected (nothing
    /// changed) if either side doesn't actually hold what it offered *at
    /// this moment* — re-validated here, not trusted from whatever the
    /// negotiation last observed, since either side's holdings can have
    /// changed since the last offer update (spent, dropped, traded away
    /// elsewhere). A receiving side hitting `InventoryConfig::max_distinct_item_types`
    /// on a *new* item type it doesn't already own also rejects the whole
    /// trade, same capacity rule `grant_item`/`craft_item` already
    /// enforce.
    pub async fn execute_trade(
        &self,
        character_a: CharacterId,
        offer_a: &TradeOfferInput,
        character_b: CharacterId,
        offer_b: &TradeOfferInput,
    ) -> Result<TradeResult> {
        for offer in [offer_a, offer_b] {
            for (item_type, quantity) in &offer.items {
                if *quantity <= 0 {
                    return Err(Error::new(
                        "character",
                        format!(
                            "trade offer quantity for {item_type:?} must be positive, got {quantity}"
                        ),
                    ));
                }
            }
            for (currency_key, amount) in &offer.currency {
                if *amount <= 0 {
                    return Err(Error::new(
                        "character",
                        format!(
                            "trade offer amount for {currency_key:?} must be positive, got {amount}"
                        ),
                    ));
                }
            }
        }

        let mut tx = self
            .pool()
            .begin()
            .await
            .map_err(|e| Error::wrap("character", "failed to start trade transaction", e))?;

        // Lock and validate every row involved before mutating any of
        // them — a trade that fails validation partway through never
        // leaves an earlier check's lock as the only trace of the
        // attempt; the whole transaction rolls back.
        for (character_id, offer) in [(character_a, offer_a), (character_b, offer_b)] {
            for (item_type, quantity) in &offer.items {
                let current: Option<i64> = sqlx::query_scalar(
                    "SELECT quantity FROM items WHERE character_id = $1 AND item_type = $2 FOR UPDATE",
                )
                .bind(character_id.as_uuid())
                .bind(item_type)
                .fetch_optional(&mut *tx)
                .await
                .map_err(|e| Error::wrap("character", "failed to check trade item offer", e))?;
                let current = current.unwrap_or(0);
                if current < *quantity {
                    return Err(Error::new(
                        "character",
                        format!(
                            "character {character_id} offered {quantity} of {item_type:?} but only holds {current}"
                        ),
                    ));
                }
            }
            for (currency_key, amount) in &offer.currency {
                let current: Option<i64> = sqlx::query_scalar(
                    "SELECT balance FROM character_currency WHERE character_id = $1 AND currency_key = $2 FOR UPDATE",
                )
                .bind(character_id.as_uuid())
                .bind(currency_key)
                .fetch_optional(&mut *tx)
                .await
                .map_err(|e| Error::wrap("character", "failed to check trade currency offer", e))?;
                let current = current.unwrap_or(0);
                if current < *amount {
                    return Err(Error::new(
                        "character",
                        format!(
                            "character {character_id} offered {amount} of currency {currency_key:?} but only holds {current}"
                        ),
                    ));
                }
            }
        }

        let mut a_changes: HashMap<String, i64> = HashMap::new();
        let mut b_changes: HashMap<String, i64> = HashMap::new();
        let mut a_currency_changes: HashMap<String, i64> = HashMap::new();
        let mut b_currency_changes: HashMap<String, i64> = HashMap::new();

        // Consume both sides' offered items/currency first, then grant —
        // same ordering `equipment.rs` uses, so a capacity check on the
        // grant side never "borrows" a slot its own consumption is about
        // to free (matches `craft_item`'s own capacity-check ordering
        // note).
        consume_items(&mut tx, character_a, &offer_a.items, &mut a_changes).await?;
        consume_items(&mut tx, character_b, &offer_b.items, &mut b_changes).await?;
        consume_currency(
            &mut tx,
            character_a,
            &offer_a.currency,
            &mut a_currency_changes,
        )
        .await?;
        consume_currency(
            &mut tx,
            character_b,
            &offer_b.currency,
            &mut b_currency_changes,
        )
        .await?;

        grant_items(&mut tx, self, character_b, &offer_a.items, &mut b_changes).await?;
        grant_items(&mut tx, self, character_a, &offer_b.items, &mut a_changes).await?;
        grant_currency(
            &mut tx,
            character_b,
            &offer_a.currency,
            &mut b_currency_changes,
        )
        .await?;
        grant_currency(
            &mut tx,
            character_a,
            &offer_b.currency,
            &mut a_currency_changes,
        )
        .await?;

        tx.commit()
            .await
            .map_err(|e| Error::wrap("character", "failed to commit trade", e))?;

        Ok(TradeResult {
            a: TradeSideResult {
                item_changes: a_changes.into_iter().collect(),
                currency_changes: a_currency_changes.into_iter().collect(),
            },
            b: TradeSideResult {
                item_changes: b_changes.into_iter().collect(),
                currency_changes: b_currency_changes.into_iter().collect(),
            },
        })
    }
}

type Tx<'a> = sqlx::Transaction<'a, sqlx::Postgres>;

async fn consume_items(
    tx: &mut Tx<'_>,
    character_id: CharacterId,
    items: &[(String, i64)],
    changes: &mut HashMap<String, i64>,
) -> Result<()> {
    for (item_type, quantity) in items {
        // Already locked and validated as sufficient above.
        let current: i64 = sqlx::query_scalar(
            "SELECT quantity FROM items WHERE character_id = $1 AND item_type = $2",
        )
        .bind(character_id.as_uuid())
        .bind(item_type)
        .fetch_one(&mut **tx)
        .await
        .map_err(|e| Error::wrap("character", "failed to re-read trade item offer", e))?;
        let remaining = current - quantity;
        if remaining == 0 {
            sqlx::query("DELETE FROM items WHERE character_id = $1 AND item_type = $2")
                .bind(character_id.as_uuid())
                .bind(item_type)
                .execute(&mut **tx)
                .await
                .map_err(|e| Error::wrap("character", "failed to consume traded item", e))?;
        } else {
            sqlx::query(
                "UPDATE items SET quantity = $3, updated_at = now() WHERE character_id = $1 AND item_type = $2",
            )
            .bind(character_id.as_uuid())
            .bind(item_type)
            .bind(remaining)
            .execute(&mut **tx)
            .await
            .map_err(|e| Error::wrap("character", "failed to consume traded item", e))?;
        }
        changes.insert(item_type.clone(), remaining);
    }
    Ok(())
}

async fn grant_items(
    tx: &mut Tx<'_>,
    store: &CharacterStore,
    character_id: CharacterId,
    items: &[(String, i64)],
    changes: &mut HashMap<String, i64>,
) -> Result<()> {
    for (item_type, quantity) in items {
        let already_owned: bool =
            sqlx::query("SELECT 1 FROM items WHERE character_id = $1 AND item_type = $2")
                .bind(character_id.as_uuid())
                .bind(item_type)
                .fetch_optional(&mut **tx)
                .await
                .map_err(|e| Error::wrap("character", "failed to check traded item receipt", e))?
                .is_some();

        if !already_owned {
            let distinct_count: i64 =
                sqlx::query_scalar("SELECT COUNT(*) FROM items WHERE character_id = $1")
                    .bind(character_id.as_uuid())
                    .fetch_one(&mut **tx)
                    .await
                    .map_err(|e| Error::wrap("character", "failed to count item stacks", e))?;
            let max = i64::from(store.inventory_config().max_distinct_item_types);
            if distinct_count >= max {
                return Err(Error::new(
                    "character",
                    format!(
                        "character {character_id} inventory is full: {distinct_count} distinct item types \
                         already owned, limit is {max} (WZ_INVENTORY_MAX_ITEM_TYPES) — trade would add {item_type:?}"
                    ),
                ));
            }
        }

        let new_quantity: i64 = sqlx::query(
            "INSERT INTO items (id, character_id, item_type, quantity) VALUES ($1, $2, $3, $4) \
             ON CONFLICT (character_id, item_type) \
             DO UPDATE SET quantity = items.quantity + EXCLUDED.quantity, updated_at = now() \
             RETURNING quantity",
        )
        .bind(uuid::Uuid::now_v7())
        .bind(character_id.as_uuid())
        .bind(item_type)
        .bind(quantity)
        .fetch_one(&mut **tx)
        .await
        .map_err(|e| Error::wrap("character", "failed to grant traded item", e))?
        .get("quantity");
        changes.insert(item_type.clone(), new_quantity);
    }
    Ok(())
}

async fn consume_currency(
    tx: &mut Tx<'_>,
    character_id: CharacterId,
    currency: &[(String, i64)],
    changes: &mut HashMap<String, i64>,
) -> Result<()> {
    for (currency_key, amount) in currency {
        let current: i64 = sqlx::query_scalar(
            "SELECT balance FROM character_currency WHERE character_id = $1 AND currency_key = $2",
        )
        .bind(character_id.as_uuid())
        .bind(currency_key)
        .fetch_one(&mut **tx)
        .await
        .map_err(|e| Error::wrap("character", "failed to re-read trade currency offer", e))?;
        let new_balance = current - amount;
        sqlx::query(
            "UPDATE character_currency SET balance = $3 WHERE character_id = $1 AND currency_key = $2",
        )
        .bind(character_id.as_uuid())
        .bind(currency_key)
        .bind(new_balance)
        .execute(&mut **tx)
        .await
        .map_err(|e| Error::wrap("character", "failed to consume traded currency", e))?;
        changes.insert(currency_key.clone(), new_balance);
    }
    Ok(())
}

async fn grant_currency(
    tx: &mut Tx<'_>,
    character_id: CharacterId,
    currency: &[(String, i64)],
    changes: &mut HashMap<String, i64>,
) -> Result<()> {
    for (currency_key, amount) in currency {
        let new_balance: i64 = sqlx::query(
            "INSERT INTO character_currency (character_id, currency_key, balance) VALUES ($1, $2, $3) \
             ON CONFLICT (character_id, currency_key) \
             DO UPDATE SET balance = character_currency.balance + EXCLUDED.balance \
             RETURNING balance",
        )
        .bind(character_id.as_uuid())
        .bind(currency_key)
        .bind(amount)
        .fetch_one(&mut **tx)
        .await
        .map_err(|e| Error::wrap("character", "failed to grant traded currency", e))?
        .get("balance");
        changes.insert(currency_key.clone(), new_balance);
    }
    Ok(())
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

    async fn store_with_two_characters() -> (CharacterStore, CharacterId, CharacterId) {
        let config = PostgresConfig::from_env().expect("WZ_POSTGRES_* env vars set");
        let pool = postgres_pool(&config, PoolOptions::default())
            .await
            .unwrap();

        let realm_id = insert_realm(&pool).await;
        let store = CharacterStore::new(pool.clone(), schema(), InventoryConfig::default());

        let mut characters = Vec::with_capacity(2);
        for _ in 0..2 {
            let account_id = AccountId::new();
            sqlx::query(
                "INSERT INTO accounts (id, username, password_hash) VALUES ($1, $2, 'unused')",
            )
            .bind(account_id.as_uuid())
            .bind(format!("trade-test-{account_id}"))
            .execute(&pool)
            .await
            .unwrap();
            characters.push(
                store
                    .create(account_id, "Test Character", realm_id, "greenwood-forest")
                    .await
                    .unwrap(),
            );
        }

        (store, characters[0], characters[1])
    }

    #[tokio::test]
    #[ignore]
    async fn a_simple_item_for_currency_trade_exchanges_both_sides() {
        let (store, a, b) = store_with_two_characters().await;
        store.grant_item(a, "sword", 1).await.unwrap();
        store.modify_currency(b, "gold", 100).await.unwrap();

        let offer_a = TradeOfferInput {
            items: vec![("sword".to_string(), 1)],
            currency: vec![],
        };
        let offer_b = TradeOfferInput {
            items: vec![],
            currency: vec![("gold".to_string(), 100)],
        };

        store.execute_trade(a, &offer_a, b, &offer_b).await.unwrap();

        assert_eq!(store.item_quantity(a, "sword").await.unwrap(), 0);
        assert_eq!(store.item_quantity(b, "sword").await.unwrap(), 1);
        assert_eq!(store.currency_balance(a, "gold").await.unwrap(), 100);
        assert_eq!(store.currency_balance(b, "gold").await.unwrap(), 0);
    }

    #[tokio::test]
    #[ignore]
    async fn a_trade_where_one_side_no_longer_holds_the_offer_is_rejected_and_nothing_changes() {
        let (store, a, b) = store_with_two_characters().await;
        store.grant_item(a, "sword", 1).await.unwrap();
        store.modify_currency(b, "gold", 50).await.unwrap();

        let offer_a = TradeOfferInput {
            items: vec![("sword".to_string(), 1)],
            currency: vec![],
        };
        // b claims to offer 100 gold but only actually has 50.
        let offer_b = TradeOfferInput {
            items: vec![],
            currency: vec![("gold".to_string(), 100)],
        };

        let err = store
            .execute_trade(a, &offer_a, b, &offer_b)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("only holds"), "{err}");

        assert_eq!(store.item_quantity(a, "sword").await.unwrap(), 1);
        assert_eq!(store.currency_balance(b, "gold").await.unwrap(), 50);
    }

    #[tokio::test]
    #[ignore]
    async fn a_two_way_item_swap_exchanges_both_items() {
        let (store, a, b) = store_with_two_characters().await;
        store.grant_item(a, "sword", 1).await.unwrap();
        store.grant_item(b, "shield", 1).await.unwrap();

        let offer_a = TradeOfferInput {
            items: vec![("sword".to_string(), 1)],
            currency: vec![],
        };
        let offer_b = TradeOfferInput {
            items: vec![("shield".to_string(), 1)],
            currency: vec![],
        };

        let result = store.execute_trade(a, &offer_a, b, &offer_b).await.unwrap();

        assert_eq!(store.item_quantity(a, "shield").await.unwrap(), 1);
        assert_eq!(store.item_quantity(b, "sword").await.unwrap(), 1);
        assert!(result.a.item_changes.contains(&("sword".to_string(), 0)));
        assert!(result.a.item_changes.contains(&("shield".to_string(), 1)));
        assert!(result.b.item_changes.contains(&("shield".to_string(), 0)));
        assert!(result.b.item_changes.contains(&("sword".to_string(), 1)));
    }

    #[tokio::test]
    #[ignore]
    async fn a_zero_or_negative_offered_quantity_is_rejected() {
        let (store, a, b) = store_with_two_characters().await;
        let offer_a = TradeOfferInput {
            items: vec![("sword".to_string(), 0)],
            currency: vec![],
        };
        let err = store
            .execute_trade(a, &offer_a, b, &TradeOfferInput::default())
            .await
            .unwrap_err();
        assert!(err.to_string().contains("must be positive"), "{err}");
    }
}
