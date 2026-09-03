//! Open-realm concurrency control — the `character_sessions` lease table
//! from docs/specs/Realm_Character_Policy_Spec.md's "Open realms:
//! concurrency control". One lease per currently-online character, held
//! by whichever zone-service instance is currently authoritative for it;
//! this is what prevents two realm processes from simulating the same
//! open-realm character at once (a split-brain bug, not a performance
//! nuisance).
//!
//! Bound realms never touch this — the contention it prevents can't
//! occur when a character has exactly one realm that could ever claim
//! it. Consumed by `realm-directory`'s login policy (#51) for open
//! realms only.

use std::time::Duration;

use common::id::{CharacterId, RealmId};
use common::{Error, Result};
use sqlx::PgPool;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LeaseOutcome {
    /// No unexpired lease existed for this character — it's now held by
    /// `zone_service_id`.
    Acquired,
    /// An unexpired lease already exists, held by a different
    /// zone-service instance — the caller must refuse this login rather
    /// than proceed.
    AlreadyActive,
}

pub struct CharacterSessionLease {
    pool: PgPool,
}

impl CharacterSessionLease {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// A single conditional insert/update, per the spec's chosen
    /// mechanism: acquires the lease if none exists or the existing one
    /// has expired, and is a no-op (returns [`LeaseOutcome::AlreadyActive`])
    /// if a different, still-unexpired lease is already held. Safe to
    /// call for a login retry — re-acquiring your own still-valid lease
    /// on the same `zone_service_id` also succeeds (`WHERE
    /// expires_at < now()` only blocks a *different*, live lease from
    /// being displaced, not the instance that already owns it — see the
    /// `ON CONFLICT` clause below, which unconditionally refreshes when
    /// `zone_service_id` matches).
    pub async fn acquire(
        &self,
        character_id: CharacterId,
        realm_id: RealmId,
        zone_service_id: &str,
        ttl: Duration,
    ) -> Result<LeaseOutcome> {
        let ttl_seconds = ttl.as_secs() as f64;

        let row: Option<(uuid::Uuid,)> = sqlx::query_as(
            "INSERT INTO character_sessions (character_id, realm_id, zone_service_id, leased_at, expires_at) \
             VALUES ($1, $2, $3, now(), now() + $4 * interval '1 second') \
             ON CONFLICT (character_id) DO UPDATE \
                 SET realm_id = EXCLUDED.realm_id, \
                     zone_service_id = EXCLUDED.zone_service_id, \
                     leased_at = now(), \
                     expires_at = EXCLUDED.expires_at \
                 WHERE character_sessions.expires_at < now() \
                    OR character_sessions.zone_service_id = EXCLUDED.zone_service_id \
             RETURNING character_id",
        )
        .bind(character_id.as_uuid())
        .bind(realm_id.as_uuid())
        .bind(zone_service_id)
        .bind(ttl_seconds)
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| Error::wrap("character", "failed to acquire session lease", e))?;

        Ok(if row.is_some() {
            LeaseOutcome::Acquired
        } else {
            LeaseOutcome::AlreadyActive
        })
    }

    /// Refreshes an already-held lease's expiry — called periodically by
    /// the owning instance, well inside the TTL window, for as long as
    /// the character stays connected. Errs if this instance no longer
    /// holds the lease (expired and reclaimed elsewhere, or never held
    /// it) rather than silently reviving a lapsed lease.
    pub async fn renew(
        &self,
        character_id: CharacterId,
        zone_service_id: &str,
        ttl: Duration,
    ) -> Result<()> {
        let ttl_seconds = ttl.as_secs() as f64;

        let result = sqlx::query(
            "UPDATE character_sessions SET expires_at = now() + $3 * interval '1 second' \
             WHERE character_id = $1 AND zone_service_id = $2",
        )
        .bind(character_id.as_uuid())
        .bind(zone_service_id)
        .bind(ttl_seconds)
        .execute(&self.pool)
        .await
        .map_err(|e| Error::wrap("character", "failed to renew session lease", e))?;

        if result.rows_affected() == 0 {
            return Err(Error::new(
                "character",
                "cannot renew a session lease this instance doesn't currently hold",
            ));
        }
        Ok(())
    }

    /// Whether `character_id` currently has an unexpired lease held by
    /// any zone-service instance — a plain read, no side effects (unlike
    /// [`Self::acquire`]). For a caller that just wants to know, e.g.
    /// `server::session`'s `DeleteCharacter` guarding against deleting a
    /// character that's currently connected somewhere (#246); mirrors
    /// [`crate::bound_liveness::BoundRealmLiveness::is_live_now`]'s shape
    /// for the open-realm side of that same check.
    pub async fn is_active(&self, character_id: CharacterId) -> Result<bool> {
        let row = sqlx::query(
            "SELECT 1 FROM character_sessions WHERE character_id = $1 AND expires_at > now()",
        )
        .bind(character_id.as_uuid())
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| Error::wrap("character", "failed to check session lease activity", e))?;

        Ok(row.is_some())
    }

    /// The clean-shutdown path — removes the lease immediately rather
    /// than waiting out its TTL. A harmless no-op if no lease is held
    /// (already expired, or never acquired), since a disconnect handler
    /// shouldn't have to check first.
    pub async fn release(&self, character_id: CharacterId) -> Result<()> {
        sqlx::query("DELETE FROM character_sessions WHERE character_id = $1")
            .bind(character_id.as_uuid())
            .execute(&self.pool)
            .await
            .map_err(|e| Error::wrap("character", "failed to release session lease", e))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use common::config::PostgresConfig;
    use common::id::AccountId;
    use common::pool::{PoolOptions, postgres_pool};

    use super::*;

    // Real Postgres — set WZ_POSTGRES_* and run with `-- --ignored`.
    async fn lease_with_character() -> (CharacterSessionLease, CharacterId, RealmId) {
        let pg_config = PostgresConfig::from_env().expect("WZ_POSTGRES_* env vars set");
        let pool = postgres_pool(&pg_config, PoolOptions::default())
            .await
            .unwrap();

        let account_id = AccountId::new();
        sqlx::query("INSERT INTO accounts (id, username, password_hash) VALUES ($1, $2, 'unused')")
            .bind(account_id.as_uuid())
            .bind(format!("lease-test-{account_id}"))
            .execute(&pool)
            .await
            .unwrap();

        let realm_id = RealmId::new();
        sqlx::query(
            "INSERT INTO realms (id, name, open_or_bound) VALUES ($1, 'Lease Test Realm', 'open')",
        )
        .bind(realm_id.as_uuid())
        .execute(&pool)
        .await
        .unwrap();

        let character_id = CharacterId::new();
        sqlx::query(
            "INSERT INTO characters (id, account_id, name, realm_id, zone_id) VALUES ($1, $2, 'Aria', $3, 'greenwood-forest')",
        )
        .bind(character_id.as_uuid())
        .bind(account_id.as_uuid())
        .bind(realm_id.as_uuid())
        .execute(&pool)
        .await
        .unwrap();

        (CharacterSessionLease::new(pool), character_id, realm_id)
    }

    #[tokio::test]
    #[ignore]
    async fn a_free_lease_is_acquired() {
        let (lease, character_id, realm_id) = lease_with_character().await;

        let outcome = lease
            .acquire(
                character_id,
                realm_id,
                "zone-service-a",
                Duration::from_secs(30),
            )
            .await
            .unwrap();
        assert_eq!(outcome, LeaseOutcome::Acquired);

        lease.release(character_id).await.unwrap();
    }

    #[tokio::test]
    #[ignore]
    async fn is_active_reflects_an_acquired_lease() {
        let (lease, character_id, realm_id) = lease_with_character().await;
        assert!(!lease.is_active(character_id).await.unwrap());

        lease
            .acquire(
                character_id,
                realm_id,
                "zone-service-a",
                Duration::from_secs(30),
            )
            .await
            .unwrap();
        assert!(lease.is_active(character_id).await.unwrap());

        lease.release(character_id).await.unwrap();
        assert!(!lease.is_active(character_id).await.unwrap());
    }

    #[tokio::test]
    #[ignore]
    async fn a_second_instance_cannot_acquire_an_unexpired_lease() {
        let (lease, character_id, realm_id) = lease_with_character().await;

        lease
            .acquire(
                character_id,
                realm_id,
                "zone-service-a",
                Duration::from_secs(30),
            )
            .await
            .unwrap();

        let outcome = lease
            .acquire(
                character_id,
                realm_id,
                "zone-service-b",
                Duration::from_secs(30),
            )
            .await
            .unwrap();
        assert_eq!(outcome, LeaseOutcome::AlreadyActive);
    }

    #[tokio::test]
    #[ignore]
    async fn the_owning_instance_can_reacquire_its_own_lease() {
        let (lease, character_id, realm_id) = lease_with_character().await;

        lease
            .acquire(
                character_id,
                realm_id,
                "zone-service-a",
                Duration::from_secs(30),
            )
            .await
            .unwrap();
        let outcome = lease
            .acquire(
                character_id,
                realm_id,
                "zone-service-a",
                Duration::from_secs(30),
            )
            .await
            .unwrap();
        assert_eq!(outcome, LeaseOutcome::Acquired);
    }

    #[tokio::test]
    #[ignore]
    async fn a_second_instance_can_acquire_after_expiry() {
        let (lease, character_id, realm_id) = lease_with_character().await;

        lease
            .acquire(
                character_id,
                realm_id,
                "zone-service-a",
                Duration::from_secs(1),
            )
            .await
            .unwrap();

        tokio::time::sleep(Duration::from_secs(2)).await;

        let outcome = lease
            .acquire(
                character_id,
                realm_id,
                "zone-service-b",
                Duration::from_secs(30),
            )
            .await
            .unwrap();
        assert_eq!(outcome, LeaseOutcome::Acquired);
    }

    #[tokio::test]
    #[ignore]
    async fn renew_fails_for_an_instance_that_does_not_hold_the_lease() {
        let (lease, character_id, _realm_id) = lease_with_character().await;

        let err = lease
            .renew(character_id, "zone-service-a", Duration::from_secs(30))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("doesn't currently hold"), "{err}");
    }
}
