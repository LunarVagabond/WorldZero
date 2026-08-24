//! Transfer audit trail (#55) — one append-only record per transfer
//! *attempt*, successful or failed, queryable by character. Feeds #56's
//! admin API ("recent transfer audit entries").
//!
//! A successful transfer's record is written as part of the same
//! transaction as the realm-move ([`TransferAuditLog::record`] takes a
//! generic `sqlx::Executor`, so [`crate::execute::TransferExecutor`]
//! passes it the live transaction) — "committed" and "audited" become
//! one atomic fact, never a transfer that succeeded with no record or a
//! record for a transfer that didn't actually happen. A failed attempt's
//! record can't use that transaction (it already rolled back, or never
//! opened) — it's written as its own standalone insert against the pool
//! instead, best-effort: a failure to record the audit entry itself
//! never masks or replaces the real transfer error being returned to
//! the caller (see [`crate::execute::TransferExecutor::transfer`]).
//!
//! **Append-only is an API-surface guarantee, not a database one** —
//! there's no `update`/`delete` method here, but nothing stops a raw
//! `UPDATE`/`DELETE` against `transfer_log` from outside this crate's
//! API. Noted as a real gap, not silently glossed over; closing it for
//! real would need DB-level role/grant restrictions, which nothing else
//! in this schema uses today either.

use common::id::{AccountId, CharacterId, RealmId};
use common::{Error, Result};
use sqlx::{PgPool, Row};
use uuid::Uuid;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TransferOutcome {
    Success,
    Failed { reason: String },
}

/// One transfer attempt, as recorded. `source_realm_id`/`gate_type` are
/// `None` when the attempt failed before either was ever determined
/// (e.g. `character_id` doesn't name a real character at all) — a
/// deliberately honest record of what was actually known, not a
/// best-guess backfill.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TransferLogRecord {
    pub id: Uuid,
    pub character_id: CharacterId,
    pub source_realm_id: Option<RealmId>,
    pub destination_realm_id: RealmId,
    pub gate_type: Option<String>,
    pub initiated_by: AccountId,
    pub outcome: TransferOutcome,
}

pub(crate) struct TransferLogEntry<'a> {
    pub id: Uuid,
    pub character_id: CharacterId,
    pub source_realm_id: Option<RealmId>,
    pub destination_realm_id: RealmId,
    pub gate_type: Option<&'a str>,
    pub initiated_by: AccountId,
    pub outcome: &'a TransferOutcome,
}

pub struct TransferAuditLog {
    pool: PgPool,
}

impl TransferAuditLog {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Appends one record via `executor` — pass `&self.pool` for a
    /// standalone write (a failed attempt), or `&mut *tx` to make the
    /// write part of an in-progress transaction (a successful one).
    pub(crate) async fn record<'e, E>(
        &self,
        executor: E,
        entry: &TransferLogEntry<'_>,
    ) -> Result<()>
    where
        E: sqlx::Executor<'e, Database = sqlx::Postgres>,
    {
        let (outcome, failure_reason) = match entry.outcome {
            TransferOutcome::Success => ("success", None),
            TransferOutcome::Failed { reason } => ("failed", Some(reason.as_str())),
        };

        sqlx::query(
            "INSERT INTO transfer_log \
                 (id, character_id, source_realm_id, destination_realm_id, gate_type, \
                  initiated_by, outcome, failure_reason) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8)",
        )
        .bind(entry.id)
        .bind(entry.character_id.as_uuid())
        .bind(entry.source_realm_id.map(|id| id.as_uuid()))
        .bind(entry.destination_realm_id.as_uuid())
        .bind(entry.gate_type)
        .bind(entry.initiated_by.as_uuid())
        .bind(outcome)
        .bind(failure_reason)
        .execute(executor)
        .await
        .map_err(|e| Error::wrap("transfer", "failed to write transfer audit record", e))?;

        Ok(())
    }

    /// `character_id`'s full transfer history, most recent first — the
    /// query shape #56's admin API needs at minimum.
    pub async fn history_for_character(
        &self,
        character_id: CharacterId,
    ) -> Result<Vec<TransferLogRecord>> {
        let rows = sqlx::query(
            "SELECT id, character_id, source_realm_id, destination_realm_id, gate_type, \
                    initiated_by, outcome, failure_reason \
             FROM transfer_log WHERE character_id = $1 ORDER BY created_at DESC",
        )
        .bind(character_id.as_uuid())
        .fetch_all(&self.pool)
        .await
        .map_err(|e| Error::wrap("transfer", "failed to read transfer audit history", e))?;

        rows.into_iter().map(row_to_record).collect()
    }
}

fn row_to_record(row: sqlx::postgres::PgRow) -> Result<TransferLogRecord> {
    let outcome: String = row.get("outcome");
    let failure_reason: Option<String> = row.get("failure_reason");
    let outcome = match outcome.as_str() {
        "success" => TransferOutcome::Success,
        "failed" => TransferOutcome::Failed {
            reason: failure_reason.unwrap_or_default(),
        },
        other => {
            return Err(Error::new(
                "transfer",
                format!("unrecognized outcome in storage: {other:?}"),
            ));
        }
    };

    Ok(TransferLogRecord {
        id: row.get("id"),
        character_id: CharacterId::from_uuid(row.get("character_id")),
        source_realm_id: row
            .get::<Option<uuid::Uuid>, _>("source_realm_id")
            .map(RealmId::from_uuid),
        destination_realm_id: RealmId::from_uuid(row.get("destination_realm_id")),
        gate_type: row.get("gate_type"),
        initiated_by: AccountId::from_uuid(row.get("initiated_by")),
        outcome,
    })
}
