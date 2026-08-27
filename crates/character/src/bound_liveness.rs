//! Bound-realm connection liveness (#169) — the `character_bound_liveness`
//! table, a deliberate parallel to [`crate::session_lease`]'s
//! `character_sessions` rather than an extension of it: that table's own
//! module doc says bound realms never touch it, and the two problems
//! really are different shapes. `character_sessions` arbitrates
//! *contention* between zone-service instances that could all claim the
//! same open-realm character; a bound-realm character has exactly one
//! realm that could ever claim it, so there's nothing to arbitrate —
//! this is just "is this character connected right now," queryable from
//! outside the process holding the connection (`transfer::execute`'s
//! job, most notably).
//!
//! No `LeaseOutcome`-style acquire/reject here for the same reason: with
//! only one possible claimant, [`BoundRealmLiveness::join`] always
//! succeeds and doubles as its own heartbeat/renewal call, the same
//! "repeat call just refreshes the expiry" shape
//! `realm_directory::RealmPresence::connect` already uses for the
//! analogous every-realm live-connection count.

use std::time::Duration;

use common::id::{CharacterId, RealmId};
use common::{Error, Result};
use sqlx::PgPool;

pub struct BoundRealmLiveness {
    pool: PgPool,
}

impl BoundRealmLiveness {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Registers `character_id` as live on `realm_id`. Also the
    /// heartbeat/renewal call — repeating it for the same character just
    /// refreshes `expires_at` (`ON CONFLICT` unconditionally updates,
    /// unlike `character_sessions`'s conditional one, since there's no
    /// second claimant here to guard against).
    pub async fn join(
        &self,
        character_id: CharacterId,
        realm_id: RealmId,
        ttl: Duration,
    ) -> Result<()> {
        let ttl_seconds = ttl.as_secs() as f64;

        sqlx::query(
            "INSERT INTO character_bound_liveness (character_id, realm_id, connected_at, expires_at) \
             VALUES ($1, $2, now(), now() + $3 * interval '1 second') \
             ON CONFLICT (character_id) DO UPDATE \
                 SET realm_id = EXCLUDED.realm_id, \
                     connected_at = now(), \
                     expires_at = EXCLUDED.expires_at",
        )
        .bind(character_id.as_uuid())
        .bind(realm_id.as_uuid())
        .bind(ttl_seconds)
        .execute(&self.pool)
        .await
        .map_err(|e| Error::wrap("character", "failed to record bound-realm liveness", e))?;

        Ok(())
    }

    /// The clean-disconnect path — removes the row immediately rather
    /// than waiting out its TTL. A harmless no-op if none exists (never
    /// joined, or already expired), same as
    /// [`crate::session_lease::CharacterSessionLease::release`].
    pub async fn leave(&self, character_id: CharacterId) -> Result<()> {
        sqlx::query("DELETE FROM character_bound_liveness WHERE character_id = $1")
            .bind(character_id.as_uuid())
            .execute(&self.pool)
            .await
            .map_err(|e| Error::wrap("character", "failed to clear bound-realm liveness", e))?;
        Ok(())
    }

    /// Whether `character_id` currently has an unexpired liveness row —
    /// an associated function taking any `PgExecutor` (not `&self.pool`)
    /// so a caller already inside a transaction (`transfer::execute`) can
    /// check against that transaction's own view rather than a second,
    /// separately-committed connection.
    pub async fn is_live<'e, E>(executor: E, character_id: CharacterId) -> Result<bool>
    where
        E: sqlx::PgExecutor<'e>,
    {
        let row = sqlx::query(
            "SELECT 1 FROM character_bound_liveness WHERE character_id = $1 AND expires_at > now()",
        )
        .bind(character_id.as_uuid())
        .fetch_optional(executor)
        .await
        .map_err(|e| Error::wrap("character", "failed to check bound-realm liveness", e))?;

        Ok(row.is_some())
    }
}

#[cfg(test)]
mod tests {
    use common::config::PostgresConfig;
    use common::id::AccountId;
    use common::pool::{PoolOptions, postgres_pool};

    use super::*;

    // Real Postgres — set WZ_POSTGRES_* and run with `-- --ignored`.
    async fn liveness_with_character() -> (BoundRealmLiveness, CharacterId, RealmId) {
        let pg_config = PostgresConfig::from_env().expect("WZ_POSTGRES_* env vars set");
        let pool = postgres_pool(&pg_config, PoolOptions::default())
            .await
            .unwrap();

        let account_id = AccountId::new();
        sqlx::query("INSERT INTO accounts (id, username, password_hash) VALUES ($1, $2, 'unused')")
            .bind(account_id.as_uuid())
            .bind(format!("liveness-test-{account_id}"))
            .execute(&pool)
            .await
            .unwrap();

        let realm_id = RealmId::new();
        sqlx::query(
            "INSERT INTO realms (id, name, open_or_bound) VALUES ($1, 'Liveness Test Realm', 'bound')",
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

        (BoundRealmLiveness::new(pool), character_id, realm_id)
    }

    #[tokio::test]
    #[ignore]
    async fn a_character_that_never_joined_is_not_live() {
        let (liveness, character_id, _realm_id) = liveness_with_character().await;

        let pool = liveness.pool.clone();
        assert!(
            !BoundRealmLiveness::is_live(&pool, character_id)
                .await
                .unwrap()
        );
    }

    #[tokio::test]
    #[ignore]
    async fn joining_marks_the_character_live() {
        let (liveness, character_id, realm_id) = liveness_with_character().await;

        liveness
            .join(character_id, realm_id, Duration::from_secs(30))
            .await
            .unwrap();

        let pool = liveness.pool.clone();
        assert!(
            BoundRealmLiveness::is_live(&pool, character_id)
                .await
                .unwrap()
        );
    }

    #[tokio::test]
    #[ignore]
    async fn leaving_clears_liveness() {
        let (liveness, character_id, realm_id) = liveness_with_character().await;

        liveness
            .join(character_id, realm_id, Duration::from_secs(30))
            .await
            .unwrap();
        liveness.leave(character_id).await.unwrap();

        let pool = liveness.pool.clone();
        assert!(
            !BoundRealmLiveness::is_live(&pool, character_id)
                .await
                .unwrap()
        );
    }

    #[tokio::test]
    #[ignore]
    async fn rejoining_refreshes_rather_than_duplicates() {
        let (liveness, character_id, realm_id) = liveness_with_character().await;

        liveness
            .join(character_id, realm_id, Duration::from_secs(30))
            .await
            .unwrap();
        liveness
            .join(character_id, realm_id, Duration::from_secs(30))
            .await
            .unwrap();

        let pool = liveness.pool.clone();
        assert!(
            BoundRealmLiveness::is_live(&pool, character_id)
                .await
                .unwrap()
        );
    }

    #[tokio::test]
    #[ignore]
    async fn an_expired_join_is_no_longer_live() {
        let (liveness, character_id, realm_id) = liveness_with_character().await;

        liveness
            .join(character_id, realm_id, Duration::from_secs(1))
            .await
            .unwrap();

        tokio::time::sleep(Duration::from_secs(2)).await;

        let pool = liveness.pool.clone();
        assert!(
            !BoundRealmLiveness::is_live(&pool, character_id)
                .await
                .unwrap()
        );
    }
}
