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
        let session = manager.issue(account_id).await.unwrap();
        assert_eq!(session.account_id, account_id);

        let mut conn = redis_connection(&pool).await.unwrap();
        let stored: String = conn
            .get(format!("session:{}", session.token))
            .await
            .unwrap();
        assert_eq!(stored, account_id.to_string());
    }
}
