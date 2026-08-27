//! Transfer execution (#53) — moving a bound-realm character to a
//! different bound realm as one atomic Postgres transaction, never an
//! implicit side effect of login (docs/PROPOSAL.md's Realm & Character
//! Policy Model). See docs/specs/Realm_Character_Policy_Spec.md's
//! "Transfers (bound realms only)" for the full mechanism this
//! implements.
//!
//! Gating (#54, [`crate::gate`]) is enforced inline below, inside the
//! same transaction as the realm move — see [`TransferExecutor::transfer`].
//! Every attempt, successful or failed, is recorded via the audit trail
//! (#55, [`crate::audit`]).

use std::sync::Arc;

use character::{AttributeSchema, BoundRealmLiveness};
use common::id::{AccountId, CharacterId, RealmId};
use common::{Error, Result};
use realm_directory::{OpenOrBound, RealmStore};
use sqlx::{PgPool, Row};
use uuid::Uuid;

use crate::audit::{TransferAuditLog, TransferLogEntry, TransferOutcome};
use crate::gate::{DenyAllPurchaseVerifier, PurchaseVerifier, TransferGate, TransferGateStore};

/// Everything one transfer attempt needs. `destination_schema` is the
/// caller's responsibility to resolve (e.g. whichever `stats.schema.yaml`
/// the destination realm's deployment declares) — this crate has no
/// opinion on *how* a deployment maps a realm to its schema, the same
/// "given as an input, not resolved here" shape #51/#52's `LoginPolicy`
/// already uses for realm resolution. `initiated_by` is whoever asked
/// for this transfer — usually the character's own account, but not
/// necessarily (an admin acting on a player's behalf); recorded as-is in
/// the audit trail, never inferred from `character_id`.
pub struct TransferRequest<'a> {
    pub character_id: CharacterId,
    pub destination_realm_id: RealmId,
    pub destination_schema: &'a AttributeSchema,
    pub initiated_by: AccountId,
}

/// What [`TransferExecutor::transfer`] has learned so far, threaded
/// through so a failure partway can still be audited with whatever was
/// actually determined before it failed — not a best-guess backfill.
#[derive(Default)]
struct AuditContext {
    source_realm_id: Option<RealmId>,
    gate_type: Option<&'static str>,
}

pub struct TransferExecutor {
    pool: PgPool,
    realms: RealmStore,
    gates: TransferGateStore,
    audit: TransferAuditLog,
    purchase_verifier: Arc<dyn PurchaseVerifier>,
}

impl TransferExecutor {
    /// Purchase-gated transfers are denied by [`DenyAllPurchaseVerifier`]
    /// until [`Self::with_purchase_verifier`] swaps in a real one — see
    /// that type's own doc comment for why that's the safe default
    /// rather than silently letting purchase-gated transfers through.
    pub fn new(
        pool: PgPool,
        realms: RealmStore,
        gates: TransferGateStore,
        audit: TransferAuditLog,
    ) -> Self {
        Self {
            pool,
            realms,
            gates,
            audit,
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
    /// - the character currently has an unexpired
    ///   [`character::BoundRealmLiveness`] row (#169) — a bound-realm
    ///   connection registers itself live on join and clears itself on
    ///   disconnect (`server::session::handle_session`), a parallel
    ///   mechanism to `character_sessions` rather than that table itself,
    ///   since `character_sessions` is explicitly open-realm-only
    ///   (docs/specs/Realm_Character_Policy_Spec.md's "Bound realms do
    ///   not use `character_sessions`") and transfer only ever applies to
    ///   bound characters. Closes the gap this check used to describe as
    ///   unreachable.
    /// - the source→destination realm pair's configured [`TransferGate`]
    ///   (#54) rejects it: a ticket-item gate with an insufficient stack,
    ///   or a purchase gate whose [`PurchaseVerifier`] doesn't confirm
    ///   it. A ticket-item gate's consumption happens inside this same
    ///   transaction, so a transfer that fails *after* consuming the
    ///   item is impossible — the whole point of doing it here rather
    ///   than as a separate step before the transaction opens.
    ///
    /// Every attempt is recorded in the audit trail (#55) regardless of
    /// outcome — a success's record is written inside the same
    /// transaction as the realm-move (so "committed" and "audited" are
    /// one atomic fact); a failure's record is a best-effort standalone
    /// write after the fact, since by then the transaction that would
    /// have carried it has already rolled back or never opened. A
    /// failure to write the audit record itself never replaces or masks
    /// the real transfer error returned to the caller.
    pub async fn transfer(&self, request: TransferRequest<'_>) -> Result<()> {
        let mut ctx = AuditContext::default();
        let result = self.transfer_inner(&request, &mut ctx).await;

        if let Err(e) = &result {
            let entry = TransferLogEntry {
                id: Uuid::now_v7(),
                character_id: request.character_id,
                source_realm_id: ctx.source_realm_id,
                destination_realm_id: request.destination_realm_id,
                gate_type: ctx.gate_type,
                initiated_by: request.initiated_by,
                outcome: &TransferOutcome::Failed {
                    reason: e.to_string(),
                },
            };
            if let Err(audit_err) = self.audit.record(&self.pool, &entry).await {
                tracing::error!(
                    error = %audit_err,
                    transfer_error = %e,
                    "failed to write transfer audit record for a failed transfer"
                );
            }
        }

        result
    }

    async fn transfer_inner(
        &self,
        request: &TransferRequest<'_>,
        ctx: &mut AuditContext,
    ) -> Result<()> {
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
        ctx.source_realm_id = Some(source_realm_id);
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

        if BoundRealmLiveness::is_live(&mut *tx, request.character_id).await? {
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
        let gate = self
            .gates
            .get(source_realm_id, request.destination_realm_id)
            .await?;
        ctx.gate_type = Some(gate.as_db_type());

        match gate {
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

        self.audit
            .record(
                &mut *tx,
                &TransferLogEntry {
                    id: Uuid::now_v7(),
                    character_id: request.character_id,
                    source_realm_id: ctx.source_realm_id,
                    destination_realm_id: request.destination_realm_id,
                    gate_type: ctx.gate_type,
                    initiated_by: request.initiated_by,
                    outcome: &TransferOutcome::Success,
                },
            )
            .await?;

        tx.commit()
            .await
            .map_err(|e| Error::wrap("transfer", "failed to commit transfer", e))?;
        Ok(())
    }
}
