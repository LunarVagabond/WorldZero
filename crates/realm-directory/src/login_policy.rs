//! Login-time enforcement of the open-vs-bound policy every realm
//! carries (docs/specs/Realm_Character_Policy_Spec.md's "The flag") —
//! the single enforcement point #51 asks for, rather than checks
//! scattered across `auth`/`character`/`server`.
//!
//! Not wired into `server`'s combined process yet — `server::main`
//! still uses one placeholder realm (`placeholder_realm_id`), so there's
//! nothing to enforce there today. This is the policy engine itself,
//! real and tested, ready for whenever `server` resolves a connection's
//! target realm for real (alongside #50).

use std::time::Duration;

use character::{CharacterSessionLease, LeaseOutcome};
use common::id::{CharacterId, RealmId};
use common::{Error, Result};

use crate::store::{OpenOrBound, RealmStore};

pub struct LoginPolicy {
    realms: RealmStore,
    leases: CharacterSessionLease,
    lease_ttl: Duration,
}

impl LoginPolicy {
    /// `lease_ttl` is only consulted for open realms — see
    /// [`character::CharacterSessionLease::acquire`]'s doc comment for
    /// how to size it relative to however often the caller intends to
    /// renew.
    pub fn new(realms: RealmStore, leases: CharacterSessionLease, lease_ttl: Duration) -> Self {
        Self {
            realms,
            leases,
            lease_ttl,
        }
    }

    /// Authorizes `character_id` (whose home realm — the one it was
    /// created on, `characters.realm_id` — is `character_realm_id`) to
    /// log into `target_realm_id`.
    ///
    /// - **Bound** `target_realm_id`: allowed only if `character_realm_id
    ///   == target_realm_id`; otherwise rejected with a reason naming the
    ///   mismatch, never a generic auth failure.
    /// - **Open** `target_realm_id`: allowed from any realm in the group,
    ///   but only after acquiring #21's `character_sessions` lease for
    ///   `target_realm_id`/`zone_service_id` — a caller that gets `Ok(())`
    ///   back is both authorized *and* now holds the lease, so there's no
    ///   separate "check, then lease" step to forget or race.
    ///
    /// Errs if `target_realm_id` doesn't name a real realm.
    pub async fn authorize_login(
        &self,
        character_id: CharacterId,
        character_realm_id: RealmId,
        target_realm_id: RealmId,
        zone_service_id: &str,
    ) -> Result<()> {
        let realm = self.realms.get(target_realm_id).await?.ok_or_else(|| {
            Error::new(
                "realm-directory",
                format!("no realm with id {target_realm_id}"),
            )
        })?;

        match realm.open_or_bound {
            OpenOrBound::Bound => {
                if character_realm_id != target_realm_id {
                    return Err(Error::new(
                        "realm-directory",
                        format!(
                            "character {character_id} is bound to a different realm and cannot log into realm {target_realm_id}"
                        ),
                    ));
                }
                Ok(())
            }
            OpenOrBound::Open => {
                match self
                    .leases
                    .acquire(
                        character_id,
                        target_realm_id,
                        zone_service_id,
                        self.lease_ttl,
                    )
                    .await?
                {
                    LeaseOutcome::Acquired => Ok(()),
                    LeaseOutcome::AlreadyActive => Err(Error::new(
                        "realm-directory",
                        format!("character {character_id} is already logged in elsewhere"),
                    )),
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use character::CharacterSessionLease;
    use common::config::PostgresConfig;
    use common::id::AccountId;
    use common::pool::{PoolOptions, postgres_pool};
    use sqlx::PgPool;

    use super::*;

    // Real Postgres — set WZ_POSTGRES_* and run with `-- --ignored`.
    async fn pool() -> PgPool {
        let pg_config = PostgresConfig::from_env().expect("WZ_POSTGRES_* env vars set");
        postgres_pool(&pg_config, PoolOptions::default())
            .await
            .unwrap()
    }

    async fn create_character(pool: &PgPool, realm_id: RealmId) -> CharacterId {
        let account_id = AccountId::new();
        sqlx::query("INSERT INTO accounts (id, username, password_hash) VALUES ($1, $2, 'unused')")
            .bind(account_id.as_uuid())
            .bind(format!("login-policy-test-{account_id}"))
            .execute(pool)
            .await
            .unwrap();

        let character_id = CharacterId::new();
        sqlx::query(
            "INSERT INTO characters (id, account_id, name, realm_id, zone_id) VALUES ($1, $2, 'Aria', $3, 'greenwood-forest')",
        )
        .bind(character_id.as_uuid())
        .bind(account_id.as_uuid())
        .bind(realm_id.as_uuid())
        .execute(pool)
        .await
        .unwrap();

        character_id
    }

    fn policy(pool: PgPool) -> LoginPolicy {
        LoginPolicy::new(
            RealmStore::new(pool.clone()),
            CharacterSessionLease::new(pool),
            Duration::from_secs(30),
        )
    }

    #[tokio::test]
    #[ignore]
    async fn bound_character_logging_into_its_own_realm_is_allowed() {
        let pool = pool().await;
        let realms = RealmStore::new(pool.clone());
        let realm_id = realms
            .create("Bound Test Realm", OpenOrBound::Bound)
            .await
            .unwrap();
        let character_id = create_character(&pool, realm_id).await;

        policy(pool)
            .authorize_login(character_id, realm_id, realm_id, "zone-service-a")
            .await
            .unwrap();
    }

    #[tokio::test]
    #[ignore]
    async fn bound_character_logging_into_a_different_realm_is_rejected() {
        let pool = pool().await;
        let realms = RealmStore::new(pool.clone());
        let home_realm_id = realms
            .create("Home Realm", OpenOrBound::Bound)
            .await
            .unwrap();
        let other_realm_id = realms
            .create("Other Realm", OpenOrBound::Bound)
            .await
            .unwrap();
        let character_id = create_character(&pool, home_realm_id).await;

        let err = policy(pool)
            .authorize_login(
                character_id,
                home_realm_id,
                other_realm_id,
                "zone-service-a",
            )
            .await
            .unwrap_err();
        assert!(
            err.to_string().contains("bound to a different realm"),
            "{err}"
        );
    }

    #[tokio::test]
    #[ignore]
    async fn open_character_can_log_into_any_realm_in_the_group() {
        let pool = pool().await;
        let realms = RealmStore::new(pool.clone());
        let home_realm_id = realms
            .create("Open Home Realm", OpenOrBound::Open)
            .await
            .unwrap();
        let other_realm_id = realms
            .create("Open Other Realm", OpenOrBound::Open)
            .await
            .unwrap();
        let character_id = create_character(&pool, home_realm_id).await;

        policy(pool)
            .authorize_login(
                character_id,
                home_realm_id,
                other_realm_id,
                "zone-service-b",
            )
            .await
            .unwrap();
    }

    #[tokio::test]
    #[ignore]
    async fn open_character_already_leased_elsewhere_is_rejected() {
        let pool = pool().await;
        let realms = RealmStore::new(pool.clone());
        let realm_id = realms
            .create("Contended Realm", OpenOrBound::Open)
            .await
            .unwrap();
        let character_id = create_character(&pool, realm_id).await;

        policy(pool.clone())
            .authorize_login(character_id, realm_id, realm_id, "zone-service-a")
            .await
            .unwrap();

        let err = policy(pool)
            .authorize_login(character_id, realm_id, realm_id, "zone-service-b")
            .await
            .unwrap_err();
        assert!(
            err.to_string().contains("already logged in elsewhere"),
            "{err}"
        );
    }
}
