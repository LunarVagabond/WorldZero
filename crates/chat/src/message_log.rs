//! Durable, write-only log of published chat messages
//! (docs/specs/Chat_Spec.md, "Durable message log", #174) —
//! `chat_messages` (`db/migrations/0016_create_chat_messages`), separate
//! from [`crate::pubsub::ChatBus`]'s Redis pub/sub, which stays
//! delivery-only and never durable. Operator-side analytics/moderation/
//! disputes; no read method exists here on purpose — client-facing
//! history replay is explicitly out of scope for #174.

use common::{Error, Result};
use sqlx::PgPool;
use uuid::Uuid;

use crate::pubsub::ChatMessage;

/// `WZ_CHAT_PERSISTENCE_ENABLED` — a real toggle independent of
/// `WZ_SERVICE_CHAT_ENABLED` (an operator may want chat live but decline
/// to persist message content; retention/privacy is an operator call,
/// not one this project should force either way, per #174). Defaults to
/// `false`: persisting message *content* is a stronger commitment than
/// running chat itself, so an operator opts in rather than discovering
/// after the fact that every player's chat is being written to disk.
/// Same "unset keeps the default, set-but-unparsable is a config error"
/// discipline as `common::config::ServicesConfig::from_env`.
pub fn persistence_enabled_from_env() -> Result<bool> {
    persistence_enabled(false)
}

/// Same as [`persistence_enabled_from_env`], but starting from a
/// caller-supplied default instead of this function's hardcoded `false`
/// — for a deployment declaring its own default (e.g. `server`'s
/// `<config_dir>/game.yaml`) rather than this crate's blanket opt-out.
/// An explicitly-set env var still always wins over either source.
pub fn persistence_enabled(default: bool) -> Result<bool> {
    match std::env::var("WZ_CHAT_PERSISTENCE_ENABLED") {
        Ok(value) => value.parse().map_err(|_| {
            Error::new(
                "chat",
                format!(
                    "WZ_CHAT_PERSISTENCE_ENABLED must be \"true\" or \"false\", got: {value:?}"
                ),
            )
        }),
        Err(std::env::VarError::NotPresent) => Ok(default),
        Err(std::env::VarError::NotUnicode(_)) => Err(Error::new(
            "chat",
            "WZ_CHAT_PERSISTENCE_ENABLED is not valid UTF-8",
        )),
    }
}

pub struct MessageLog {
    pool: PgPool,
}

impl MessageLog {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    pub async fn record(&self, message: &ChatMessage) -> Result<()> {
        sqlx::query(
            "INSERT INTO chat_messages (id, channel_id, sender_account_id, body, sent_at) \
             VALUES ($1, $2, $3, $4, $5)",
        )
        .bind(Uuid::now_v7())
        .bind(message.channel_id.as_uuid())
        .bind(message.sender_account_id.as_uuid())
        .bind(&message.body)
        .bind(message.sent_at)
        .execute(&self.pool)
        .await
        .map_err(|e| Error::wrap("chat", "failed to record durable chat message", e))?;

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use common::config::PostgresConfig;
    use common::id::AccountId;
    use common::pool::{PoolOptions, postgres_pool};
    use time::OffsetDateTime;

    use super::*;
    use crate::store::ChannelStore;

    // Real Postgres — set WZ_POSTGRES_* and run with `-- --ignored`.
    // Same throwaway-account style as `store.rs`'s/`pubsub.rs`'s own
    // ignored tests (chat_messages.sender_account_id is a real FK).
    async fn log_with_channel() -> (MessageLog, PgPool, common::id::ChannelId, AccountId) {
        let config = PostgresConfig::from_env().expect("WZ_POSTGRES_* env vars set");
        let pool = postgres_pool(&config, PoolOptions::default())
            .await
            .unwrap();

        let account = AccountId::new();
        sqlx::query("INSERT INTO accounts (id, username, password_hash) VALUES ($1, $2, 'unused')")
            .bind(account.as_uuid())
            .bind(format!("chat-message-log-test-{account}"))
            .execute(&pool)
            .await
            .unwrap();

        let store = ChannelStore::new(pool.clone());
        let channel = store.create_group(account, "Test Channel").await.unwrap();

        (MessageLog::new(pool.clone()), pool, channel, account)
    }

    // std::env is process-global and `cargo test` runs unit tests in
    // this crate concurrently — one test function, run sequentially
    // within itself, rather than three that could race on the same var
    // (same concern `common::config`'s tests guard against with a lock;
    // this crate has nothing else touching env vars to race with yet).
    #[test]
    fn persistence_enabled_from_env() {
        unsafe { std::env::remove_var("WZ_CHAT_PERSISTENCE_ENABLED") };
        assert!(!super::persistence_enabled_from_env().unwrap());

        unsafe { std::env::set_var("WZ_CHAT_PERSISTENCE_ENABLED", "true") };
        assert!(super::persistence_enabled_from_env().unwrap());

        unsafe { std::env::set_var("WZ_CHAT_PERSISTENCE_ENABLED", "sure") };
        let err = super::persistence_enabled_from_env().unwrap_err();
        assert!(err.to_string().contains("WZ_CHAT_PERSISTENCE_ENABLED"));

        unsafe { std::env::remove_var("WZ_CHAT_PERSISTENCE_ENABLED") };
    }

    #[tokio::test]
    #[ignore]
    async fn record_persists_a_message_row() {
        let (log, pool, channel, account) = log_with_channel().await;

        let message = ChatMessage {
            channel_id: channel,
            sender_account_id: account,
            body: "hello from the durable log test".to_string(),
            sent_at: OffsetDateTime::now_utc(),
        };
        log.record(&message).await.unwrap();

        let (count, body): (i64, String) =
            sqlx::query_as("SELECT COUNT(*), MAX(body) FROM chat_messages WHERE channel_id = $1")
                .bind(channel.as_uuid())
                .fetch_one(&pool)
                .await
                .unwrap();

        assert_eq!(count, 1);
        assert_eq!(body, "hello from the durable log test");
    }

    #[tokio::test]
    #[ignore]
    async fn record_survives_the_sender_account_being_deleted() {
        let (log, pool, channel, account) = log_with_channel().await;

        let message = ChatMessage {
            channel_id: channel,
            sender_account_id: account,
            body: "should outlive the account".to_string(),
            sent_at: OffsetDateTime::now_utc(),
        };
        log.record(&message).await.unwrap();

        sqlx::query("DELETE FROM accounts WHERE id = $1")
            .bind(account.as_uuid())
            .execute(&pool)
            .await
            .unwrap();

        let sender: Option<uuid::Uuid> =
            sqlx::query_scalar("SELECT sender_account_id FROM chat_messages WHERE channel_id = $1")
                .bind(channel.as_uuid())
                .fetch_one(&pool)
                .await
                .unwrap();

        assert_eq!(
            sender, None,
            "sender_account_id should be nulled out, not the whole row deleted"
        );
    }
}
