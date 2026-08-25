//! Redis-backed session issuance, shared by every provider
//! (docs/specs/Auth_Spec.md, "Session token format").

use common::id::AccountId;
use common::pool::{RedisPool, redis_connection};
use common::{Error, Result};
use deadpool_redis::redis::AsyncCommands;
use rand::Rng;
use time::OffsetDateTime;

use crate::provider::Session;

const SESSION_TTL_SECONDS: u64 = 24 * 60 * 60;

pub struct SessionManager {
    redis: RedisPool,
}

impl SessionManager {
    pub fn new(redis: RedisPool) -> Self {
        Self { redis }
    }

    #[tracing::instrument(skip(self), fields(%account_id))]
    pub async fn issue(&self, account_id: AccountId) -> Result<Session> {
        let token = generate_token();
        let key = format!("session:{token}");

        let mut conn = redis_connection(&self.redis).await?;
        conn.set_ex::<_, _, ()>(&key, account_id.to_string(), SESSION_TTL_SECONDS)
            .await
            .map_err(|e| Error::wrap("auth", "failed to persist session", e))?;

        Ok(Session {
            token,
            account_id,
            expires_at: OffsetDateTime::now_utc()
                + time::Duration::seconds(SESSION_TTL_SECONDS as i64),
        })
    }

    /// Resolves a `session_token` back to the account it belongs to (#195)
    /// — `None` for an unknown or already-expired token, never an error,
    /// since "the token doesn't resolve" is an ordinary, expected outcome
    /// a caller (`UsernamePasswordProvider::resume`) turns into its own
    /// clear rejection, not a storage-layer failure.
    ///
    /// **Sliding expiration, same token reused (a deliberate choice, not
    /// an oversight):** a successful resolve refreshes the key's TTL back
    /// to the full window rather than minting a new token or leaving the
    /// original expiry untouched — the same shape an ordinary web session
    /// cookie already has. Reusing the token (instead of rotating it on
    /// every resume) keeps the client-side contract simple: whatever
    /// token `Authenticated` last handed back keeps working for as long
    /// as the connection keeps reconnecting within the TTL window, with
    /// nothing extra for the client to track. This is a bearer-token
    /// security model — presenting the raw token is sufficient, no second
    /// factor — matching how the token is already stored and used today;
    /// see docs/specs/Auth_Spec.md's "Gateway handshake" for the full
    /// writeup of that choice.
    #[tracing::instrument(skip(self, token))]
    pub async fn resolve(&self, token: &str) -> Result<Option<AccountId>> {
        let key = format!("session:{token}");
        let mut conn = redis_connection(&self.redis).await?;

        let stored: Option<String> = conn
            .get(&key)
            .await
            .map_err(|e| Error::wrap("auth", "failed to look up session", e))?;
        let Some(stored) = stored else {
            return Ok(None);
        };
        let account_id: AccountId = stored
            .parse()
            .map_err(|e| Error::wrap("auth", "corrupt session record", e))?;

        conn.expire::<_, ()>(&key, SESSION_TTL_SECONDS as i64)
            .await
            .map_err(|e| Error::wrap("auth", "failed to renew session", e))?;

        Ok(Some(account_id))
    }
}

fn generate_token() -> String {
    use base64::Engine;

    let mut bytes = [0u8; 32];
    rand::rng().fill_bytes(&mut bytes);
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

#[cfg(test)]
mod tests {
    use common::config::RedisConfig;
    use common::pool::{PoolOptions, redis_pool};

    use super::*;

    // Real Redis, not run in CI — set WZ_REDIS_* and run with `-- --ignored`.
    #[tokio::test]
    #[ignore]
    async fn issues_a_session_stored_in_redis() {
        let config = RedisConfig::from_env().expect("WZ_REDIS_* env vars set");
        let pool = redis_pool(&config, PoolOptions::default()).unwrap();
        let manager = SessionManager::new(pool.clone());

        let account_id = AccountId::new();
        let before = OffsetDateTime::now_utc();
        let session = manager.issue(account_id).await.unwrap();
        let after = OffsetDateTime::now_utc();
        assert_eq!(session.account_id, account_id);

        // `expires_at` should be ~24h out, not some other duration a
        // refactor could silently change — bounded by wall-clock time
        // taken immediately before/after the call, not an exact match.
        assert!(
            session.expires_at >= before + time::Duration::seconds(SESSION_TTL_SECONDS as i64)
                && session.expires_at
                    <= after + time::Duration::seconds(SESSION_TTL_SECONDS as i64),
            "expires_at {} not within the expected window around a {SESSION_TTL_SECONDS}s TTL",
            session.expires_at
        );

        let mut conn = redis_connection(&pool).await.unwrap();
        let key = format!("session:{}", session.token);
        let stored: String = conn.get(&key).await.unwrap();
        assert_eq!(stored, account_id.to_string());

        // Redis's own TTL on the key should match, not just the value
        // returned in `Session` — a bug that set the two inconsistently
        // (e.g. hardcoded a different constant in `set_ex`) would pass
        // the `expires_at` assertion above but silently expire sessions
        // at the wrong time.
        let ttl: i64 = conn.ttl(&key).await.unwrap();
        assert!(
            ttl > 0 && ttl as u64 <= SESSION_TTL_SECONDS,
            "unexpected TTL on the session key: {ttl}"
        );
    }

    #[tokio::test]
    #[ignore]
    async fn resolve_finds_the_account_behind_an_issued_token() {
        let config = RedisConfig::from_env().expect("WZ_REDIS_* env vars set");
        let pool = redis_pool(&config, PoolOptions::default()).unwrap();
        let manager = SessionManager::new(pool);

        let account_id = AccountId::new();
        let session = manager.issue(account_id).await.unwrap();

        assert_eq!(
            manager.resolve(&session.token).await.unwrap(),
            Some(account_id)
        );
    }

    #[tokio::test]
    #[ignore]
    async fn resolve_returns_none_for_an_unknown_token() {
        let config = RedisConfig::from_env().expect("WZ_REDIS_* env vars set");
        let pool = redis_pool(&config, PoolOptions::default()).unwrap();
        let manager = SessionManager::new(pool);

        assert_eq!(manager.resolve("not-a-real-token").await.unwrap(), None);
    }

    /// #195's sliding-expiration choice, verified directly against
    /// Redis's own TTL — a resolve should refresh the key back to the
    /// full window, not leave whatever was left over from `issue`.
    #[tokio::test]
    #[ignore]
    async fn resolve_renews_the_tokens_ttl() {
        let config = RedisConfig::from_env().expect("WZ_REDIS_* env vars set");
        let pool = redis_pool(&config, PoolOptions::default()).unwrap();
        let manager = SessionManager::new(pool.clone());

        let account_id = AccountId::new();
        let session = manager.issue(account_id).await.unwrap();
        let key = format!("session:{}", session.token);

        // Artificially shrink the TTL so a renewal is actually observable
        // (issuing already sets it to the full window).
        let mut conn = redis_connection(&pool).await.unwrap();
        conn.expire::<_, ()>(&key, 5).await.unwrap();
        let shrunk_ttl: i64 = conn.ttl(&key).await.unwrap();
        assert!(shrunk_ttl <= 5, "{shrunk_ttl}");

        manager.resolve(&session.token).await.unwrap();

        let renewed_ttl: i64 = conn.ttl(&key).await.unwrap();
        assert!(
            renewed_ttl > shrunk_ttl,
            "resolve should have renewed the TTL: was {shrunk_ttl}, now {renewed_ttl}"
        );
    }
}
