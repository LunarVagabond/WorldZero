//! Transfer execution (#53) — moving a bound-realm character to a
//! different bound realm as one atomic Postgres transaction, never an
//! implicit side effect of login (docs/PROPOSAL.md's Realm & Character
//! Policy Model). See docs/specs/Realm_Character_Policy_Spec.md's
//! "Transfers (bound realms only)" for the full mechanism this
//! implements.
//!
//! Gating (#54, [`crate::gate`]) is enforced inline below, inside the
//! same transaction as the realm move — see [`TransferExecutor::transfer`].
//! The audit trail (#55) isn't built yet.

use std::sync::Arc;

use character::AttributeSchema;
use common::id::{CharacterId, RealmId};
use common::{Error, Result};
use realm_directory::{OpenOrBound, RealmStore};
use sqlx::{PgPool, Row};

use crate::gate::{DenyAllPurchaseVerifier, PurchaseVerifier, TransferGate, TransferGateStore};

/// Everything one transfer attempt needs. `destination_schema` is the
/// caller's responsibility to resolve (e.g. whichever `stats.schema.yaml`
/// the destination realm's deployment declares) — this crate has no
/// opinion on *how* a deployment maps a realm to its schema, the same
/// "given as an input, not resolved here" shape #51/#52's `LoginPolicy`
/// already uses for realm resolution.
pub struct TransferRequest<'a> {
    pub character_id: CharacterId,
    pub destination_realm_id: RealmId,
    pub destination_schema: &'a AttributeSchema,
}

pub struct TransferExecutor {
    pool: PgPool,
    realms: RealmStore,
    gates: TransferGateStore,
    purchase_verifier: Arc<dyn PurchaseVerifier>,
}

impl TransferExecutor {
    /// Purchase-gated transfers are denied by [`DenyAllPurchaseVerifier`]
    /// until [`Self::with_purchase_verifier`] swaps in a real one — see
    /// that type's own doc comment for why that's the safe default
    /// rather than silently letting purchase-gated transfers through.
    pub fn new(pool: PgPool, realms: RealmStore, gates: TransferGateStore) -> Self {
        Self {
            pool,
            realms,
            gates,
            purchase_verifier: Arc::new(DenyAllPurchaseVerifier),
        }
    }

    pub fn with_purchase_verifier(mut self, purchase_verifier: Arc<dyn PurchaseVerifier>) -> Self {
        self.purchase_verifier = purchase_verifier;
        self
    }

    /// Executes `request`. Everything below runs in one Postgres
    /// transaction — commits or rolls back atomically, so a rejected or
    /// interrupted transfer leaves the character exactly as it was on
    /// the source realm, never partially moved (docs/specs's "no
    /// partial-transfer state" / "failed transfer leaves the character
    /// usable on the source realm" guarantees come directly from
    /// ordinary Postgres transactional semantics, not bespoke
    /// saga/compensation logic).
    ///
    /// Rejects (transaction never commits) if:
    /// - `character_id` doesn't name a real character
    /// - the character's *current* realm is open — "transfer" has no
    ///   meaning for a character that can already log into any realm in
    ///   its group
    /// - `destination_realm_id` doesn't name a real realm, or is itself
    ///   open — transferring *into* an open pool isn't a defined
    ///   operation anywhere else in this codebase, so it's rejected here
    ///   rather than left ambiguous
    /// - the character currently holds an unexpired `character_sessions`
    ///   lease. **Known gap:** bound-realm characters never write to that
    ///   table at all (docs/specs/Realm_Character_Policy_Spec.md's
    ///   "Bound realms do not use `character_sessions`"), and transfer
    ///   only ever applies to bound characters — so today this check can
    ///   never actually fire for the case it's meant to guard.
    ///   Closing that gap needs real liveness tracking for a
    ///   bound-realm connection that's queryable from outside the
    ///   connected process, which doesn't exist yet (a `server`-wiring
    ///   concern, #136-adjacent); kept here, not deleted, since it's
    ///   correct for whatever future case *does* populate a lease row
    ///   for a character reaching this check.
    /// - the source→destination realm pair's configured [`TransferGate`]
    ///   (#54) rejects it: a ticket-item gate with an insufficient stack,
    ///   or a purchase gate whose [`PurchaseVerifier`] doesn't confirm
    ///   it. A ticket-item gate's consumption happens inside this same
    ///   transaction, so a transfer that fails *after* consuming the
    ///   item is impossible — the whole point of doing it here rather
    ///   than as a separate step before the transaction opens.
    pub async fn transfer(&self, request: TransferRequest<'_>) -> Result<()> {
        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|e| Error::wrap("transfer", "failed to start transaction", e))?;

        let row = sqlx::query("SELECT realm_id, stats FROM characters WHERE id = $1 FOR UPDATE")
            .bind(request.character_id.as_uuid())
            .fetch_optional(&mut *tx)
            .await
            .map_err(|e| Error::wrap("transfer", "failed to load character", e))?
            .ok_or_else(|| {
                Error::new(
                    "transfer",
                    format!("no character with id {}", request.character_id),
                )
            })?;
        let source_realm_id = RealmId::from_uuid(row.get("realm_id"));
        let stats: serde_json::Value = row.get("stats");

        let source_realm = self.realms.get(source_realm_id).await?.ok_or_else(|| {
            Error::new(
                "transfer",
                format!("character's own realm {source_realm_id} no longer exists"),
            )
        })?;
        if source_realm.open_or_bound == OpenOrBound::Open {
            return Err(Error::new(
                "transfer",
                format!(
                    "character {} is on an open realm and cannot be transferred \
                     (it can already log into any realm in its group)",
                    request.character_id
                ),
            ));
        }

        let destination_realm = self
            .realms
            .get(request.destination_realm_id)
            .await?
            .ok_or_else(|| {
                Error::new(
                    "transfer",
                    format!("no realm with id {}", request.destination_realm_id),
                )
            })?;
        if destination_realm.open_or_bound == OpenOrBound::Open {
            return Err(Error::new(
                "transfer",
                format!(
                    "realm {} is open and is not a valid transfer destination",
                    request.destination_realm_id
                ),
            ));
        }

        let has_active_lease = sqlx::query(
            "SELECT 1 FROM character_sessions WHERE character_id = $1 AND expires_at > now()",
        )
        .bind(request.character_id.as_uuid())
        .fetch_optional(&mut *tx)
        .await
        .map_err(|e| Error::wrap("transfer", "failed to check for an active session", e))?;
        if has_active_lease.is_some() {
            return Err(Error::new(
                "transfer",
                format!(
                    "character {} is currently logged in and cannot be transferred",
                    request.character_id
                ),
            ));
        }

        // The gate check itself doesn't need to run inside `tx` (gate
        // config changes are rare, not a correctness-sensitive race),
        // but a ticket-item gate's *consumption* below does — otherwise
        // a transfer that fails after consuming the item would leave
        // the item gone with nothing to show for it, violating #54's
        // "does NOT consume it if the transfer fails" criterion.
        match self
            .gates
            .get(source_realm_id, request.destination_realm_id)
            .await?
        {
            TransferGate::Open => {}
            TransferGate::TicketItem { item_type } => {
                let current: Option<i64> = sqlx::query_scalar(
                    "SELECT quantity FROM items WHERE character_id = $1 AND item_type = $2 FOR UPDATE",
                )
                .bind(request.character_id.as_uuid())
                .bind(&item_type)
                .fetch_optional(&mut *tx)
                .await
                .map_err(|e| Error::wrap("transfer", "failed to check ticket item", e))?;

                if current.unwrap_or(0) < 1 {
                    return Err(Error::new(
                        "transfer",
                        format!(
                            "transfer requires ticket item {item_type:?}, which character {} does not have",
                            request.character_id
                        ),
                    ));
                }

                if current == Some(1) {
                    sqlx::query("DELETE FROM items WHERE character_id = $1 AND item_type = $2")
                        .bind(request.character_id.as_uuid())
                        .bind(&item_type)
                        .execute(&mut *tx)
                        .await
                        .map_err(|e| Error::wrap("transfer", "failed to consume ticket item", e))?;
                } else {
                    sqlx::query(
                        "UPDATE items SET quantity = quantity - 1, updated_at = now() \
                         WHERE character_id = $1 AND item_type = $2",
                    )
                    .bind(request.character_id.as_uuid())
                    .bind(&item_type)
                    .execute(&mut *tx)
                    .await
                    .map_err(|e| Error::wrap("transfer", "failed to consume ticket item", e))?;
                }
            }
            TransferGate::Purchase { product_id } => {
                let verified = self
                    .purchase_verifier
                    .verify_purchase(request.character_id, &product_id)
                    .await?;
                if !verified {
                    return Err(Error::new(
                        "transfer",
                        format!(
                            "transfer requires a verified purchase ({product_id:?}), \
                             none found for character {}",
                            request.character_id
                        ),
                    ));
                }
            }
        }

        let stored_stats = stats.as_object().cloned().unwrap_or_default();
        let migrated_stats = request.destination_schema.migrate_stats(&stored_stats);

        sqlx::query(
            "UPDATE characters SET realm_id = $2, stats = $3, updated_at = now() WHERE id = $1",
        )
        .bind(request.character_id.as_uuid())
        .bind(request.destination_realm_id.as_uuid())
        .bind(serde_json::Value::Object(migrated_stats))
        .execute(&mut *tx)
        .await
        .map_err(|e| Error::wrap("transfer", "failed to update character realm", e))?;

        tx.commit()
            .await
            .map_err(|e| Error::wrap("transfer", "failed to commit transfer", e))?;
        Ok(())
    }
}
