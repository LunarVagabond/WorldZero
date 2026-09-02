//! Realm population reporting (#137): two independent numbers, both
//! exposed as plain counts rather than a `low`/`med`/`high` bucket —
//! bucketing/display is a game-developer decision, not something this
//! crate should make for them (docs/PROPOSAL.md's Design Principles,
//! "policy, not hardcoding").
//!
//! - **Character census** — durable, Postgres-backed, how many
//!   characters exist on a realm at all (`character::CharacterStore::count_for_realm`).
//! - **Live connections** — ephemeral, Redis-backed
//!   ([`RealmPresence`], this module) — how many are connected *right
//!   now*.
//!
//! The two live in different places for the same reason `character`'s
//! own crate doc draws this line: durable state belongs in Postgres,
//! ephemeral state in Redis. They're combined here only for the
//! caller's convenience ([`RealmPresence::population`]), not because
//! they share a storage mechanism.

use std::time::Duration;

use character::CharacterStore;
use common::id::RealmId;
use common::pool::RedisPool;
use common::{Error, Result};
use deadpool_redis::redis::AsyncCommands;
use uuid::Uuid;

/// Both halves of a realm's population, together — the shape a caller
/// (a realm list, an admin surface) actually wants, per #137's "the data
/// both are computed from" framing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RealmPopulation {
    /// Strict single-realm census (`CharacterStore::count_for_realm`) — for
    /// an `open`-policy realm this is *not* the same number as "characters
    /// the requesting account can select here", since selection
    /// (`CharacterStore::list_by_account_in_open_realms`) deliberately
    /// spans every open realm's characters for that account, not just this
    /// one. A caller surfacing this on a realm picker should label it as a
    /// realm-specific census, not a selectable-character count.
    pub character_count: i64,
    pub live_connections: u64,
}

fn key(realm_id: RealmId) -> String {
    format!("realm:{realm_id}:connections")
}

fn unix_timestamp_secs() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("system clock is before the Unix epoch")
        .as_secs_f64()
}

/// A Redis sorted set per realm — member = a caller-chosen per-connection
/// id, score = the Unix timestamp it expires at. Modeled directly on
/// `character::CharacterSessionLease` (#21/#51): a plain `INCR`/`DECR`
/// counter would permanently over-count after any unclean disconnect
/// (crash, network partition, process kill) with nothing to ever bring
/// it back down, exactly the failure mode that design already rejected
/// for the *lease* half of connection tracking. Expiry-based scoring
/// means [`Self::count`] self-heals from a crash the same way the lease
/// does, with no separate sweeper process needed.
///
/// Deliberately its own mechanism, not `character_sessions` (that table
/// only ever holds *open*-realm rows, per its own doc comment — bound
/// realms skip it entirely, but still need a live count) and not #134's
/// service-registry (that crate's per-instance `load` is a caller-defined
/// signal for load-balancing decisions; overloading it to also mean
/// "this many players" would conflate two different concerns behind one
/// field with no enforced relationship between them).
pub struct RealmPresence {
    redis: RedisPool,
    ttl: Duration,
}

impl RealmPresence {
    /// `ttl` should be a small multiple of however often the caller
    /// intends to re-call [`Self::connect`] as a heartbeat — same sizing
    /// guidance as [`character::CharacterSessionLease::new`].
    pub fn new(redis: RedisPool, ttl: Duration) -> Self {
        Self { redis, ttl }
    }

    fn expires_at(&self) -> f64 {
        unix_timestamp_secs() + self.ttl.as_secs_f64()
    }

    /// Registers `connection_id` as live on `realm_id`. Also serves as
    /// the heartbeat/renewal call — repeating it for the same
    /// `connection_id` just refreshes its expiry (`ZADD` overwrites an
    /// existing member's score), so callers don't need a separate
    /// renewal method.
    pub async fn connect(&self, realm_id: RealmId, connection_id: Uuid) -> Result<()> {
        let mut conn = common::pool::redis_connection(&self.redis).await?;
        conn.zadd::<_, _, _, ()>(key(realm_id), connection_id.to_string(), self.expires_at())
            .await
            .map_err(|e| Error::wrap("realm-directory", "failed to record a live connection", e))
    }

    /// The clean-disconnect path — removes `connection_id` immediately
    /// rather than waiting out its TTL.
    pub async fn disconnect(&self, realm_id: RealmId, connection_id: Uuid) -> Result<()> {
        let mut conn = common::pool::redis_connection(&self.redis).await?;
        conn.zrem::<_, _, ()>(key(realm_id), connection_id.to_string())
            .await
            .map_err(|e| Error::wrap("realm-directory", "failed to remove a live connection", e))
    }

    /// Live connection count for `realm_id` right now — prunes any
    /// member whose score (expiry) has already passed before counting,
    /// so a crashed connection's entry stops counting the moment
    /// something asks, without needing a background sweep.
    pub async fn count(&self, realm_id: RealmId) -> Result<u64> {
        let mut conn = common::pool::redis_connection(&self.redis).await?;
        let now = unix_timestamp_secs();

        conn.zrembyscore::<_, _, _, ()>(key(realm_id), f64::NEG_INFINITY, now)
            .await
            .map_err(|e| {
                Error::wrap("realm-directory", "failed to prune expired connections", e)
            })?;

        conn.zcard(key(realm_id))
            .await
            .map_err(|e| Error::wrap("realm-directory", "failed to count live connections", e))
    }

    /// Both population numbers for `realm_id`, combined — see this
    /// module's doc comment for why they're computed from two different
    /// stores despite being returned together.
    pub async fn population(
        &self,
        character_store: &CharacterStore,
        realm_id: RealmId,
    ) -> Result<RealmPopulation> {
        let character_count = character_store.count_for_realm(realm_id).await?;
        let live_connections = self.count(realm_id).await?;
        Ok(RealmPopulation {
            character_count,
            live_connections,
        })
    }
}

#[cfg(test)]
mod tests {
    use common::config::RedisConfig;
    use common::pool::{PoolOptions, redis_pool};

    use super::*;

    // Real Redis — set WZ_REDIS_* and run with `-- --ignored`.
    fn presence(ttl: Duration) -> RealmPresence {
        let redis_config = RedisConfig::from_env().expect("WZ_REDIS_* env vars set");
        let redis = redis_pool(&redis_config, PoolOptions::default()).unwrap();
        RealmPresence::new(redis, ttl)
    }

    #[tokio::test]
    #[ignore]
    async fn connect_then_count_reflects_the_connection() {
        let presence = presence(Duration::from_secs(30));
        let realm_id = RealmId::new();
        let connection_id = Uuid::now_v7();

        assert_eq!(presence.count(realm_id).await.unwrap(), 0);

        presence.connect(realm_id, connection_id).await.unwrap();
        assert_eq!(presence.count(realm_id).await.unwrap(), 1);

        presence.disconnect(realm_id, connection_id).await.unwrap();
        assert_eq!(presence.count(realm_id).await.unwrap(), 0);
    }

    #[tokio::test]
    #[ignore]
    async fn multiple_connections_on_the_same_realm_are_all_counted() {
        let presence = presence(Duration::from_secs(30));
        let realm_id = RealmId::new();

        for _ in 0..3 {
            presence.connect(realm_id, Uuid::now_v7()).await.unwrap();
        }
        assert_eq!(presence.count(realm_id).await.unwrap(), 3);
    }

    #[tokio::test]
    #[ignore]
    async fn connections_on_different_realms_do_not_interfere() {
        let presence = presence(Duration::from_secs(30));
        let realm_a = RealmId::new();
        let realm_b = RealmId::new();

        presence.connect(realm_a, Uuid::now_v7()).await.unwrap();
        assert_eq!(presence.count(realm_a).await.unwrap(), 1);
        assert_eq!(presence.count(realm_b).await.unwrap(), 0);
    }

    /// The self-healing case #137's own docs asks for: an "unclean
    /// disconnect" is simulated by simply never calling `disconnect` —
    /// letting the TTL lapse instead — and `count` still recovers.
    #[tokio::test]
    #[ignore]
    async fn an_expired_connection_drops_out_of_the_count_without_disconnecting() {
        let presence = presence(Duration::from_secs(1));
        let realm_id = RealmId::new();

        presence.connect(realm_id, Uuid::now_v7()).await.unwrap();
        assert_eq!(presence.count(realm_id).await.unwrap(), 1);

        tokio::time::sleep(Duration::from_secs(2)).await;
        assert_eq!(presence.count(realm_id).await.unwrap(), 0);
    }

    #[tokio::test]
    #[ignore]
    async fn reconnecting_with_the_same_id_refreshes_rather_than_duplicates() {
        let presence = presence(Duration::from_secs(30));
        let realm_id = RealmId::new();
        let connection_id = Uuid::now_v7();

        presence.connect(realm_id, connection_id).await.unwrap();
        presence.connect(realm_id, connection_id).await.unwrap();
        assert_eq!(presence.count(realm_id).await.unwrap(), 1);
    }

    #[tokio::test]
    #[ignore]
    async fn population_combines_the_census_and_the_live_count() {
        use common::config::PostgresConfig;
        use common::id::AccountId;
        use common::pool::postgres_pool;

        let pg_config = PostgresConfig::from_env().expect("WZ_POSTGRES_* env vars set");
        let pg_pool = postgres_pool(&pg_config, PoolOptions::default())
            .await
            .unwrap();
        let account_id = AccountId::new();
        sqlx::query("INSERT INTO accounts (id, username, password_hash) VALUES ($1, $2, 'unused')")
            .bind(account_id.as_uuid())
            .bind(format!("population-test-{account_id}"))
            .execute(&pg_pool)
            .await
            .unwrap();
        // #170: character.realm_id is a real foreign key now — needs an
        // actual realms row, not just an unregistered RealmId::new().
        let realm_store = crate::RealmStore::new(pg_pool.clone());
        let realm_id = realm_store
            .create("Population Test Realm", crate::OpenOrBound::Open)
            .await
            .unwrap();

        let store = CharacterStore::new(
            pg_pool,
            character::AttributeSchema::from_yaml("schema_version: 1\nstats: []\n").unwrap(),
            Default::default(),
        );

        store
            .create(account_id, "Aria", realm_id, "greenwood-forest")
            .await
            .unwrap();

        let presence = presence(Duration::from_secs(30));
        presence.connect(realm_id, Uuid::now_v7()).await.unwrap();

        let population = presence.population(&store, realm_id).await.unwrap();
        assert_eq!(
            population,
            RealmPopulation {
                character_count: 1,
                live_connections: 1,
            }
        );
    }
}
