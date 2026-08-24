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
}
