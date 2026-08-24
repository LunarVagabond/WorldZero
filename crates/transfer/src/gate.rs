//! Transfer gating (#54) — an operator-configurable check in front of
//! #53's execution path: open (no gate), gated behind an in-game ticket
//! item (consumed on success), or gated behind a real-money purchase.
//! Per-realm-*pair* (`source_realm_id`, `destination_realm_id`), not
//! per-destination-realm or global — an operator may want a different
//! gate for realm A → realm B than for realm C → realm B
//! (docs/PROPOSAL.md's Design Principle #3, "policy, not hardcoding").

use async_trait::async_trait;
use common::id::{CharacterId, RealmId};
use common::{Error, Result};
use sqlx::{PgPool, Row};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TransferGate {
    /// No gate — the default when a realm pair has no configured row at
    /// all, not something an operator has to configure explicitly.
    Open,
    /// Consumes one unit of `item_type` from the transferring
    /// character's inventory on a *successful* transfer only — the
    /// consumption happens inside #53's same transaction (see
    /// [`crate::execute::TransferExecutor::transfer`]), so a failed
    /// transfer never touches the item.
    TicketItem { item_type: String },
    /// Requires [`PurchaseVerifier::verify_purchase`] to confirm
    /// `product_id` before the transfer proceeds. Actual payment-processor
    /// integration is explicitly out of #54's scope — this is only the
    /// gate type and the hook a real verifier plugs into.
    Purchase { product_id: String },
}

impl TransferGate {
    /// Also used by [`crate::audit`] to record which gate type was in
    /// effect for a given transfer attempt.
    pub(crate) fn as_db_type(&self) -> &'static str {
        match self {
            TransferGate::Open => "open",
            TransferGate::TicketItem { .. } => "ticket_item",
            TransferGate::Purchase { .. } => "purchase",
        }
    }
}

/// Confirms a real-money purchase entitling a character to a
/// purchase-gated transfer. `transfer` has no payment-processor
/// integration itself (#54's explicit "out of scope") — a deployment
/// that wants purchase-gated transfers implements this against whatever
/// payment system it actually uses.
#[async_trait]
pub trait PurchaseVerifier: Send + Sync {
    async fn verify_purchase(&self, character_id: CharacterId, product_id: &str) -> Result<bool>;
}

/// The default [`PurchaseVerifier`] wired in if a deployment doesn't
/// supply its own — always denies. A realm pair configured as
/// purchase-gated with no real verifier attached gets transfers that
/// always fail their gate check, never transfers that silently bypass
/// payment because nothing was actually wired up.
pub struct DenyAllPurchaseVerifier;

#[async_trait]
impl PurchaseVerifier for DenyAllPurchaseVerifier {
    async fn verify_purchase(&self, _character_id: CharacterId, _product_id: &str) -> Result<bool> {
        Ok(false)
    }
}

pub struct TransferGateStore {
    pool: PgPool,
}

impl TransferGateStore {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// [`TransferGate::Open`] if `source_realm_id`/`destination_realm_id`
    /// has no configured row — see [`TransferGate::Open`]'s doc comment
    /// for why that's the default, not an error.
    pub async fn get(
        &self,
        source_realm_id: RealmId,
        destination_realm_id: RealmId,
    ) -> Result<TransferGate> {
        let row = sqlx::query(
            "SELECT gate_type, ticket_item_type, purchase_product_id FROM transfer_gates \
             WHERE source_realm_id = $1 AND destination_realm_id = $2",
        )
        .bind(source_realm_id.as_uuid())
        .bind(destination_realm_id.as_uuid())
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| Error::wrap("transfer", "failed to read transfer gate", e))?;

        let Some(row) = row else {
            return Ok(TransferGate::Open);
        };

        row_to_gate(row)
    }

    /// Sets (or replaces) the gate for `source_realm_id` →
    /// `destination_realm_id`. Setting [`TransferGate::Open`] explicitly
    /// removes any configured row rather than storing an `'open'` row —
    /// keeps [`Self::get`]'s "no row means open" invariant true after an
    /// operator reverses a gate, not just before one's ever configured.
    pub async fn set(
        &self,
        source_realm_id: RealmId,
        destination_realm_id: RealmId,
        gate: TransferGate,
    ) -> Result<()> {
        if gate == TransferGate::Open {
            return self.clear(source_realm_id, destination_realm_id).await;
        }

        let (ticket_item_type, purchase_product_id) = match &gate {
            TransferGate::Open => unreachable!("handled above"),
            TransferGate::TicketItem { item_type } => (Some(item_type.as_str()), None),
            TransferGate::Purchase { product_id } => (None, Some(product_id.as_str())),
        };

        sqlx::query(
            "INSERT INTO transfer_gates (source_realm_id, destination_realm_id, gate_type, ticket_item_type, purchase_product_id) \
             VALUES ($1, $2, $3, $4, $5) \
             ON CONFLICT (source_realm_id, destination_realm_id) DO UPDATE \
                 SET gate_type = EXCLUDED.gate_type, \
                     ticket_item_type = EXCLUDED.ticket_item_type, \
                     purchase_product_id = EXCLUDED.purchase_product_id",
        )
        .bind(source_realm_id.as_uuid())
        .bind(destination_realm_id.as_uuid())
        .bind(gate.as_db_type())
        .bind(ticket_item_type)
        .bind(purchase_product_id)
        .execute(&self.pool)
        .await
        .map_err(|e| Error::wrap("transfer", "failed to set transfer gate", e))?;

        Ok(())
    }

    async fn clear(&self, source_realm_id: RealmId, destination_realm_id: RealmId) -> Result<()> {
        sqlx::query(
            "DELETE FROM transfer_gates WHERE source_realm_id = $1 AND destination_realm_id = $2",
        )
        .bind(source_realm_id.as_uuid())
        .bind(destination_realm_id.as_uuid())
        .execute(&self.pool)
        .await
        .map_err(|e| Error::wrap("transfer", "failed to clear transfer gate", e))?;
        Ok(())
    }
}

fn row_to_gate(row: sqlx::postgres::PgRow) -> Result<TransferGate> {
    let gate_type: String = row.get("gate_type");
    match gate_type.as_str() {
        "open" => Ok(TransferGate::Open),
        "ticket_item" => {
            let item_type: Option<String> = row.get("ticket_item_type");
            Ok(TransferGate::TicketItem {
                item_type: item_type.ok_or_else(|| {
                    Error::new("transfer", "ticket_item gate row missing ticket_item_type")
                })?,
            })
        }
        "purchase" => {
            let product_id: Option<String> = row.get("purchase_product_id");
            Ok(TransferGate::Purchase {
                product_id: product_id.ok_or_else(|| {
                    Error::new("transfer", "purchase gate row missing purchase_product_id")
                })?,
            })
        }
        other => Err(Error::new(
            "transfer",
            format!("unrecognized gate_type in storage: {other:?}"),
        )),
    }
}

#[cfg(test)]
mod tests {
    use common::config::PostgresConfig;
    use common::pool::{PoolOptions, postgres_pool};
    use realm_directory::RealmStore;

    use super::*;

    // Real Postgres — set WZ_POSTGRES_* and run with `-- --ignored`.
    async fn pool() -> PgPool {
        let pg_config = PostgresConfig::from_env().expect("WZ_POSTGRES_* env vars set");
        postgres_pool(&pg_config, PoolOptions::default())
            .await
            .unwrap()
    }

    async fn realm_pair(pool: &PgPool) -> (RealmId, RealmId) {
        let realms = RealmStore::new(pool.clone());
        let source = realms
            .create("Gate Test Source", realm_directory::OpenOrBound::Bound)
            .await
            .unwrap();
        let destination = realms
            .create("Gate Test Destination", realm_directory::OpenOrBound::Bound)
            .await
            .unwrap();
        (source, destination)
    }

    #[tokio::test]
    #[ignore]
    async fn an_unconfigured_pair_is_open() {
        let pool = pool().await;
        let (source, destination) = realm_pair(&pool).await;
        let gates = TransferGateStore::new(pool);
        assert_eq!(
            gates.get(source, destination).await.unwrap(),
            TransferGate::Open
        );
    }

    #[tokio::test]
    #[ignore]
    async fn set_then_get_round_trips_a_ticket_item_gate() {
        let pool = pool().await;
        let (source, destination) = realm_pair(&pool).await;
        let gates = TransferGateStore::new(pool);

        let gate = TransferGate::TicketItem {
            item_type: "realm_transfer_ticket".to_string(),
        };
        gates.set(source, destination, gate.clone()).await.unwrap();
        assert_eq!(gates.get(source, destination).await.unwrap(), gate);
    }

    #[tokio::test]
    #[ignore]
    async fn set_then_get_round_trips_a_purchase_gate() {
        let pool = pool().await;
        let (source, destination) = realm_pair(&pool).await;
        let gates = TransferGateStore::new(pool);

        let gate = TransferGate::Purchase {
            product_id: "realm-transfer-token".to_string(),
        };
        gates.set(source, destination, gate.clone()).await.unwrap();
        assert_eq!(gates.get(source, destination).await.unwrap(), gate);
    }

    #[tokio::test]
    #[ignore]
    async fn setting_open_after_a_gate_was_configured_clears_it() {
        let pool = pool().await;
        let (source, destination) = realm_pair(&pool).await;
        let gates = TransferGateStore::new(pool);

        gates
            .set(
                source,
                destination,
                TransferGate::TicketItem {
                    item_type: "realm_transfer_ticket".to_string(),
                },
            )
            .await
            .unwrap();
        gates
            .set(source, destination, TransferGate::Open)
            .await
            .unwrap();

        assert_eq!(
            gates.get(source, destination).await.unwrap(),
            TransferGate::Open
        );
    }

    #[tokio::test]
    #[ignore]
    async fn different_realm_pairs_have_independent_gates() {
        let pool = pool().await;
        let (source_a, destination_a) = realm_pair(&pool).await;
        let (source_b, destination_b) = realm_pair(&pool).await;
        let gates = TransferGateStore::new(pool);

        gates
            .set(
                source_a,
                destination_a,
                TransferGate::TicketItem {
                    item_type: "realm_transfer_ticket".to_string(),
                },
            )
            .await
            .unwrap();

        assert_eq!(
            gates.get(source_b, destination_b).await.unwrap(),
            TransferGate::Open
        );
    }
}
