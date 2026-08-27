//! Login-time enforcement of the open-vs-bound policy every realm
//! carries (docs/specs/Realm_Character_Policy_Spec.md's "The flag") —
//! the single enforcement point #51 asks for, rather than checks
//! scattered across `auth`/`character`/`server`.
//!
//! Wired into `server`'s combined process as of #136 —
//! `session::handle_session`'s `SelectCharacter` path calls
//! [`LoginPolicy::authorize_login`] before completing a login.

use std::time::Duration;

use character::{CharacterSessionLease, CharacterStore, CharacterSummary, LeaseOutcome};
use common::id::{AccountId, CharacterId, RealmId};
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

    /// The character-lookup half of #52: resolves `account_id`'s
    /// character for a login into `target_realm_id`, using the lookup
    /// that actually matches that realm's policy — a bound realm looks
    /// up strictly by `target_realm_id` ([`CharacterStore::find_by_account`]);
    /// an open realm looks across every open realm instead
    /// ([`CharacterStore::find_by_account_in_open_realms`]), so the same
    /// character is found no matter which open realm a player happens to
    /// connect through. Getting this branch wrong in either direction is
    /// exactly the bug #52 exists to prevent — a bound character never
    /// leaks into the open lookup, and an open character is never
    /// missed just because it was created on a *different* open realm
    /// than `target_realm_id`.
    ///
    /// Callers combine this with [`Self::authorize_login`]: resolve the
    /// character first (or create one, if this returns `None`), then
    /// authorize using the character's *actual* `realm_id` — not
    /// `target_realm_id` — as `character_realm_id`, so the bound-mismatch
    /// check in [`Self::authorize_login`] still fires correctly.
    ///
    /// Errs if `target_realm_id` doesn't name a real realm.
    pub async fn resolve_character(
        &self,
        character_store: &CharacterStore,
        account_id: AccountId,
        target_realm_id: RealmId,
    ) -> Result<Option<CharacterSummary>> {
        let realm = self.realms.get(target_realm_id).await?.ok_or_else(|| {
            Error::new(
                "realm-directory",
                format!("no realm with id {target_realm_id}"),
            )
        })?;

        match realm.open_or_bound {
            OpenOrBound::Bound => {
                character_store
                    .find_by_account(account_id, target_realm_id)
                    .await
            }
            OpenOrBound::Open => {
                character_store
                    .find_by_account_in_open_realms(account_id)
                    .await
            }
        }
    }

    /// The list-all counterpart to [`Self::resolve_character`] (#193) —
    /// same policy-aware branch, same "which lookup matches this realm's
    /// policy" reasoning, just returning every character instead of the
    /// single most-recent one. Errs if `target_realm_id` doesn't name a
    /// real realm.
    pub async fn list_characters(
        &self,
        character_store: &CharacterStore,
        account_id: AccountId,
        target_realm_id: RealmId,
    ) -> Result<Vec<CharacterSummary>> {
        let realm = self.realms.get(target_realm_id).await?.ok_or_else(|| {
            Error::new(
                "realm-directory",
                format!("no realm with id {target_realm_id}"),
            )
        })?;

        match realm.open_or_bound {
            OpenOrBound::Bound => {
                character_store
                    .list_by_account(account_id, target_realm_id)
                    .await
            }
            OpenOrBound::Open => {
                character_store
                    .list_by_account_in_open_realms(account_id)
                    .await
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use character::{CharacterSessionLease, CharacterStore};
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

    fn character_store(pool: PgPool) -> CharacterStore {
        CharacterStore::new(
            pool,
            character::AttributeSchema::from_yaml("schema_version: 1\nstats: []\n").unwrap(),
            Default::default(),
        )
    }

    async fn create_account(pool: &PgPool) -> AccountId {
        let account_id = AccountId::new();
        sqlx::query("INSERT INTO accounts (id, username, password_hash) VALUES ($1, $2, 'unused')")
            .bind(account_id.as_uuid())
            .bind(format!("login-policy-test-{account_id}"))
            .execute(pool)
            .await
            .unwrap();
        account_id
    }

    async fn create_character(pool: &PgPool, realm_id: RealmId) -> CharacterId {
        let account_id = create_account(pool).await;

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

    #[tokio::test]
    #[ignore]
    async fn resolve_character_finds_a_bound_character_only_on_its_own_realm() {
        let pool = pool().await;
        let realms = RealmStore::new(pool.clone());
        let home_realm_id = realms
            .create("Bound Home", OpenOrBound::Bound)
            .await
            .unwrap();
        let other_realm_id = realms
            .create("Bound Other", OpenOrBound::Bound)
            .await
            .unwrap();
        let account_id = create_account(&pool).await;
        let store = character_store(pool.clone());
        let character_id = store
            .create(account_id, "Aria", home_realm_id, "greenwood-forest")
            .await
            .unwrap();

        let found = policy(pool.clone())
            .resolve_character(&store, account_id, home_realm_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(found.id, character_id);

        assert!(
            policy(pool)
                .resolve_character(&store, account_id, other_realm_id)
                .await
                .unwrap()
                .is_none()
        );
    }

    /// #52's core scenario: a character created on one open realm must
    /// still resolve when the login target is a *different* open realm —
    /// [`RealmStore::create`]'s two calls below are deliberately distinct
    /// realms, not the same one reused.
    #[tokio::test]
    #[ignore]
    async fn resolve_character_finds_an_open_character_via_a_different_open_realm() {
        let pool = pool().await;
        let realms = RealmStore::new(pool.clone());
        let home_realm_id = realms.create("Open Home", OpenOrBound::Open).await.unwrap();
        let other_realm_id = realms
            .create("Open Other", OpenOrBound::Open)
            .await
            .unwrap();
        let account_id = create_account(&pool).await;
        let store = character_store(pool.clone());
        let character_id = store
            .create(account_id, "Aria", home_realm_id, "greenwood-forest")
            .await
            .unwrap();

        let found = policy(pool)
            .resolve_character(&store, account_id, other_realm_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(found.id, character_id);
        assert_eq!(found.realm_id, home_realm_id);
    }

    /// #52's acceptance criteria, end to end: a write made while
    /// "connected" through one open realm is visible when the same
    /// character is next resolved through a *different* open realm —
    /// simulating a player logging out of realm A and immediately back
    /// in through realm B, with no cache anywhere to go stale.
    #[tokio::test]
    #[ignore]
    async fn state_written_through_one_open_realm_is_visible_through_another() {
        let pool = pool().await;
        let realms = RealmStore::new(pool.clone());
        let realm_a = realms
            .create("Open Realm A", OpenOrBound::Open)
            .await
            .unwrap();
        let realm_b = realms
            .create("Open Realm B", OpenOrBound::Open)
            .await
            .unwrap();
        let account_id = create_account(&pool).await;
        let store = character_store(pool.clone());
        let character_id = store
            .create(account_id, "Aria", realm_a, "greenwood-forest")
            .await
            .unwrap();

        // "Log in" through realm A: authorize (acquires the lease), then
        // simulate that session doing real work.
        let policy_a = policy(pool.clone());
        policy_a
            .authorize_login(character_id, realm_a, realm_a, "zone-service-a")
            .await
            .unwrap();
        store
            .update_position(character_id, (42.0, 7.0, 0.0))
            .await
            .unwrap();

        // "Disconnect" — release the lease directly (mirrors what a real
        // disconnect handler does; `LoginPolicy` itself only ever
        // acquires, never releases, since it's the login-time half).
        CharacterSessionLease::new(pool.clone())
            .release(character_id)
            .await
            .unwrap();

        // "Log in" through realm B instead: resolve, then authorize using
        // the character's *actual* home realm — not `realm_b` — exactly
        // as `resolve_character`'s doc comment prescribes.
        let policy_b = policy(pool.clone());
        let resolved = policy_b
            .resolve_character(&store, account_id, realm_b)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(resolved.position, (42.0, 7.0, 0.0), "{resolved:?}");

        policy_b
            .authorize_login(character_id, resolved.realm_id, realm_b, "zone-service-b")
            .await
            .unwrap();
    }

    #[tokio::test]
    #[ignore]
    async fn list_characters_on_a_bound_realm_never_includes_another_realms_character() {
        let pool = pool().await;
        let realms = RealmStore::new(pool.clone());
        let home_realm_id = realms
            .create("Bound Home List", OpenOrBound::Bound)
            .await
            .unwrap();
        let other_realm_id = realms
            .create("Bound Other List", OpenOrBound::Bound)
            .await
            .unwrap();
        let account_id = create_account(&pool).await;
        let store = character_store(pool.clone());
        let character_id = store
            .create(account_id, "Aria", home_realm_id, "greenwood-forest")
            .await
            .unwrap();
        store
            .create(account_id, "Elsewhere", other_realm_id, "greenwood-forest")
            .await
            .unwrap();

        let listed = policy(pool)
            .list_characters(&store, account_id, home_realm_id)
            .await
            .unwrap();
        assert_eq!(listed.len(), 1, "{listed:?}");
        assert_eq!(listed[0].id, character_id);
    }

    #[tokio::test]
    #[ignore]
    async fn list_characters_on_an_open_realm_spans_the_whole_group() {
        let pool = pool().await;
        let realms = RealmStore::new(pool.clone());
        let home_realm_id = realms
            .create("Open Home List", OpenOrBound::Open)
            .await
            .unwrap();
        let other_realm_id = realms
            .create("Open Other List", OpenOrBound::Open)
            .await
            .unwrap();
        let account_id = create_account(&pool).await;
        let store = character_store(pool.clone());
        let character_id = store
            .create(account_id, "Aria", home_realm_id, "greenwood-forest")
            .await
            .unwrap();

        // Listed via the *other* open realm — same "any realm in the
        // group" reach as `resolve_character`.
        let listed = policy(pool)
            .list_characters(&store, account_id, other_realm_id)
            .await
            .unwrap();
        assert_eq!(listed.len(), 1, "{listed:?}");
        assert_eq!(listed[0].id, character_id);
    }
}
