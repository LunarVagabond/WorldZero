//! Redis pub/sub message delivery (docs/specs/Chat_Spec.md, "Redis pub/sub delivery").
//! Delivery is ephemeral by design — a message only reaches whoever's
//! subscribed at send time. Durable logging (docs/specs/Chat_Spec.md,
//! "Durable message log", #174) is a separate, optional write-through to
//! [`crate::message_log::MessageLog`], never a substitute for this.

use std::sync::Arc;

use common::config::RedisConfig;
use common::id::{AccountId, ChannelId};
use common::pool::RedisPool;
use common::{Error, Result};
use deadpool_redis::redis::AsyncCommands;
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

use crate::message_log::MessageLog;
use crate::store::ChannelStore;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatMessage {
    pub channel_id: ChannelId,
    pub sender_account_id: AccountId,
    pub body: String,
    #[serde(with = "time::serde::rfc3339")]
    pub sent_at: OffsetDateTime,
}

fn topic(channel_id: ChannelId) -> String {
    format!("chat:{channel_id}")
}

pub struct ChatBus {
    redis: RedisPool,
    redis_config: RedisConfig,
    message_log: Option<Arc<MessageLog>>,
}

impl ChatBus {
    /// `redis_config` is needed alongside the pool because `subscribe`
    /// can't use a pooled connection — see [`common::pool::redis_pubsub_connection`].
    ///
    /// `message_log` is `None`/`Some` decided once here, at construction
    /// — not a per-message flag `publish` checks — matching the
    /// `WZ_CHAT_PERSISTENCE_ENABLED` toggle's "`None` end to end when
    /// disabled" discipline (docs/specs/Chat_Spec.md, "Durable message
    /// log", #174).
    pub fn new(
        redis: RedisPool,
        redis_config: RedisConfig,
        message_log: Option<Arc<MessageLog>>,
    ) -> Self {
        Self {
            redis,
            redis_config,
            message_log,
        }
    }

    /// Publishes `body` from `sender_account_id` to `channel_id`. For
    /// `direct`/`group`/`guild` channels, rejects a sender who isn't a
    /// member (checked against `store`) — `zone` channels aren't checked
    /// here, since their membership is implicit via `character.zone_id`,
    /// not `chat_channel_members` (docs/specs/Chat_Spec.md); validating a
    /// sender's actual zone is the gateway/world integration's job (#87).
    pub async fn publish(
        &self,
        store: &ChannelStore,
        channel_id: ChannelId,
        sender_account_id: AccountId,
        body: &str,
    ) -> Result<()> {
        if !store.is_member(channel_id, sender_account_id).await? {
            return Err(Error::new("chat", "sender is not a member of this channel"));
        }

        let message = ChatMessage {
            channel_id,
            sender_account_id,
            body: body.to_string(),
            sent_at: OffsetDateTime::now_utc(),
        };
        let payload = serde_json::to_string(&message)
            .map_err(|e| Error::wrap("chat", "failed to encode chat message", e))?;

        let mut conn = common::pool::redis_connection(&self.redis).await?;
        conn.publish::<_, _, ()>(topic(channel_id), payload)
            .await
            .map_err(|e| Error::wrap("chat", "failed to publish chat message", e))?;

        // Fire-and-forget, after delivery: the durable write never sits
        // on real-time delivery's critical path (docs/specs/Chat_Spec.md,
        // "Durable message log", #174's acceptance criteria) — a slow or
        // failed persist only affects the operator-side log, never a
        // player's chat latency. A crash before the spawned task runs
        // means a delivered message that never got logged; that's the
        // accepted tradeoff for not making Postgres a hard dependency of
        // live chat.
        if let Some(log) = self.message_log.clone() {
            let message = message.clone();
            tokio::spawn(async move {
                if let Err(e) = log.record(&message).await {
                    tracing::warn!(error = %e, "failed to persist chat message to durable log");
                }
            });
        }

        Ok(())
    }

    /// Subscribes to `channel_id`, returning a stream of decoded messages.
    /// Only ever produces messages published *after* the subscription is
    /// established — nothing is replayed from pub/sub itself, and the
    /// durable log (when enabled) has no read path either, see module docs.
    pub async fn subscribe(
        &self,
        channel_id: ChannelId,
    ) -> Result<impl futures_util::Stream<Item = ChatMessage> + use<>> {
        use futures_util::StreamExt;

        let mut pubsub = common::pool::redis_pubsub_connection(&self.redis_config).await?;
        pubsub
            .subscribe(topic(channel_id))
            .await
            .map_err(|e| Error::wrap("chat", "failed to subscribe to channel", e))?;

        Ok(pubsub.into_on_message().filter_map(|msg| async move {
            let payload: String = msg.get_payload().ok()?;
            serde_json::from_str(&payload).ok()
        }))
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use common::config::{PostgresConfig, RedisConfig};
    use common::pool::{PoolOptions, postgres_pool, redis_pool};
    use futures_util::StreamExt;

    use super::*;
    use crate::store::ChannelStore;

    // Real Postgres/Redis — set WZ_POSTGRES_*/WZ_REDIS_* and run with
    // `-- --ignored`.
    async fn bus_with_group_channel() -> (ChatBus, ChannelStore, ChannelId, AccountId, AccountId) {
        let pg_config = PostgresConfig::from_env().expect("WZ_POSTGRES_* env vars set");
        let pool = postgres_pool(&pg_config, PoolOptions::default())
            .await
            .unwrap();

        let redis_config = RedisConfig::from_env().expect("WZ_REDIS_* env vars set");
        let redis = redis_pool(&redis_config, PoolOptions::default()).unwrap();

        let member = AccountId::new();
        let outsider = AccountId::new();
        for id in [member, outsider] {
            sqlx::query(
                "INSERT INTO accounts (id, username, password_hash) VALUES ($1, $2, 'unused')",
            )
            .bind(id.as_uuid())
            .bind(format!("chat-pubsub-test-{id}"))
            .execute(&pool)
            .await
            .unwrap();
        }

        let store = ChannelStore::new(pool);
        let channel = store.create_group(member, "Test Channel").await.unwrap();

        (
            ChatBus::new(redis, redis_config, None),
            store,
            channel,
            member,
            outsider,
        )
    }

    #[tokio::test]
    #[ignore]
    async fn two_subscribers_both_receive_a_published_message() {
        let (bus, store, channel, member, _outsider) = bus_with_group_channel().await;

        let mut sub_a = Box::pin(bus.subscribe(channel).await.unwrap());
        let mut sub_b = Box::pin(bus.subscribe(channel).await.unwrap());

        bus.publish(&store, channel, member, "hello from the test")
            .await
            .unwrap();

        let received_a = tokio::time::timeout(Duration::from_secs(2), sub_a.next())
            .await
            .unwrap()
            .unwrap();
        let received_b = tokio::time::timeout(Duration::from_secs(2), sub_b.next())
            .await
            .unwrap()
            .unwrap();

        assert_eq!(received_a.body, "hello from the test");
        assert_eq!(received_a.sender_account_id, member);
        assert_eq!(received_b.body, "hello from the test");
    }

    #[tokio::test]
    #[ignore]
    async fn publish_from_a_non_member_is_rejected() {
        let (bus, store, channel, _member, outsider) = bus_with_group_channel().await;

        let err = bus
            .publish(&store, channel, outsider, "should not send")
            .await
            .unwrap_err();
        assert!(err.to_string().contains("not a member"), "{err}");
    }

    #[tokio::test]
    #[ignore]
    async fn publish_writes_through_to_the_durable_log_when_enabled() {
        use crate::message_log::MessageLog;

        let pg_config = PostgresConfig::from_env().expect("WZ_POSTGRES_* env vars set");
        let pool = postgres_pool(&pg_config, PoolOptions::default())
            .await
            .unwrap();
        let redis_config = RedisConfig::from_env().expect("WZ_REDIS_* env vars set");
        let redis = redis_pool(&redis_config, PoolOptions::default()).unwrap();

        let member = AccountId::new();
        sqlx::query("INSERT INTO accounts (id, username, password_hash) VALUES ($1, $2, 'unused')")
            .bind(member.as_uuid())
            .bind(format!("chat-pubsub-persist-test-{member}"))
            .execute(&pool)
            .await
            .unwrap();

        let store = ChannelStore::new(pool.clone());
        let channel = store.create_group(member, "Test Channel").await.unwrap();
        let bus = ChatBus::new(
            redis,
            redis_config,
            Some(std::sync::Arc::new(MessageLog::new(pool.clone()))),
        );

        bus.publish(&store, channel, member, "persisted message")
            .await
            .unwrap();

        // The write is fire-and-forget (spawned after publish returns),
        // so give it a moment to land before asserting on it.
        let mut count = 0i64;
        for _ in 0..20 {
            count = sqlx::query_scalar("SELECT COUNT(*) FROM chat_messages WHERE channel_id = $1")
                .bind(channel.as_uuid())
                .fetch_one(&pool)
                .await
                .unwrap();
            if count == 1 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert_eq!(count, 1);
    }
}
