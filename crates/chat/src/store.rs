//! Postgres-backed channel/membership store (docs/specs/Chat_Spec.md).

use common::id::{AccountId, ChannelId};
use common::{Error, Result};
use sqlx::{PgPool, Row};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChannelType {
    Direct,
    Group,
    Guild,
    Zone,
}

impl ChannelType {
    fn as_str(self) -> &'static str {
        match self {
            Self::Direct => "direct",
            Self::Group => "group",
            Self::Guild => "guild",
            Self::Zone => "zone",
        }
    }
}

pub struct ChannelStore {
    pool: PgPool,
}

impl ChannelStore {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Idempotent — a second call for the same pair returns the existing
    /// channel rather than creating a duplicate.
    pub async fn create_direct(&self, a: AccountId, b: AccountId) -> Result<ChannelId> {
        if let Some(existing) = self.find_direct(a, b).await? {
            return Ok(existing);
        }

        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|e| Error::wrap("chat", "failed to start transaction", e))?;

        let id = ChannelId::new();
        sqlx::query("INSERT INTO chat_channels (id, channel_type) VALUES ($1, 'direct')")
            .bind(id.as_uuid())
            .execute(&mut *tx)
            .await
            .map_err(|e| Error::wrap("chat", "failed to create direct channel", e))?;

        for account_id in [a, b] {
            sqlx::query(
                "INSERT INTO chat_channel_members (channel_id, account_id) VALUES ($1, $2)",
            )
            .bind(id.as_uuid())
            .bind(account_id.as_uuid())
            .execute(&mut *tx)
            .await
            .map_err(|e| Error::wrap("chat", "failed to add direct channel member", e))?;
        }

        tx.commit()
            .await
            .map_err(|e| Error::wrap("chat", "failed to commit direct channel creation", e))?;
        Ok(id)
    }

    async fn find_direct(&self, a: AccountId, b: AccountId) -> Result<Option<ChannelId>> {
        let id: Option<uuid::Uuid> = sqlx::query_scalar(
            "SELECT m1.channel_id FROM chat_channel_members m1 \
             JOIN chat_channel_members m2 ON m1.channel_id = m2.channel_id \
             JOIN chat_channels c ON c.id = m1.channel_id \
             WHERE c.channel_type = 'direct' AND m1.account_id = $1 AND m2.account_id = $2",
        )
        .bind(a.as_uuid())
        .bind(b.as_uuid())
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| Error::wrap("chat", "failed to look up direct channel", e))?;

        Ok(id.map(ChannelId::from_uuid))
    }

    pub async fn create_group(&self, creator: AccountId, name: &str) -> Result<ChannelId> {
        self.create_named(ChannelType::Group, creator, name).await
    }

    /// Structurally identical to `create_group` — `guild` is not backed by
    /// a real guild roster yet (docs/specs/Chat_Spec.md, "guild"), see #88.
    pub async fn create_guild(&self, creator: AccountId, name: &str) -> Result<ChannelId> {
        self.create_named(ChannelType::Guild, creator, name).await
    }

    async fn create_named(
        &self,
        channel_type: ChannelType,
        creator: AccountId,
        name: &str,
    ) -> Result<ChannelId> {
        if name.trim().is_empty() {
            return Err(Error::new("chat", "channel name must not be empty"));
        }

        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|e| Error::wrap("chat", "failed to start transaction", e))?;

        let id = ChannelId::new();
        sqlx::query("INSERT INTO chat_channels (id, channel_type, name, created_by) VALUES ($1, $2, $3, $4)")
            .bind(id.as_uuid())
            .bind(channel_type.as_str())
            .bind(name)
            .bind(creator.as_uuid())
            .execute(&mut *tx)
            .await
            .map_err(|e| Error::wrap("chat", "failed to create channel", e))?;

        sqlx::query("INSERT INTO chat_channel_members (channel_id, account_id) VALUES ($1, $2)")
            .bind(id.as_uuid())
            .bind(creator.as_uuid())
            .execute(&mut *tx)
            .await
            .map_err(|e| Error::wrap("chat", "failed to add creator as channel member", e))?;

        tx.commit()
            .await
            .map_err(|e| Error::wrap("chat", "failed to commit channel creation", e))?;
        Ok(id)
    }

    /// Idempotent — ensures `(zone_id, category)` has exactly one channel,
    /// relying on the migration's unique index to stay correct even under
    /// concurrent callers. That index needs `NULLS NOT DISTINCT`
    /// (`db/migrations/0006_chat_channels_zone_category_nulls_not_distinct/`)
    /// for this to actually hold for *global*-scope channels (`zone_id`
    /// `NULL`) — plain SQL unique indexes treat every `NULL` as distinct
    /// from every other `NULL`, so without it two concurrent callers
    /// ensuring the same global category could each pass the conflict
    /// check and create a duplicate channel.
    pub async fn ensure_zone_channel(
        &self,
        zone_id: Option<&str>,
        category: &str,
        name: &str,
    ) -> Result<ChannelId> {
        if let Some(existing) = self.find_zone_channel(zone_id, category).await? {
            return Ok(existing);
        }

        let id = ChannelId::new();
        let result = sqlx::query(
            "INSERT INTO chat_channels (id, channel_type, name, zone_id, category) VALUES ($1, 'zone', $2, $3, $4) \
             ON CONFLICT DO NOTHING",
        )
        .bind(id.as_uuid())
        .bind(name)
        .bind(zone_id)
        .bind(category)
        .execute(&self.pool)
        .await
        .map_err(|e| Error::wrap("chat", "failed to create zone channel", e))?;

        if result.rows_affected() == 1 {
            return Ok(id);
        }

        // Lost a race with a concurrent caller — look up what they created.
        self.find_zone_channel(zone_id, category)
            .await?
            .ok_or_else(|| {
                Error::new(
                    "chat",
                    "zone channel creation raced but no channel was found afterward",
                )
            })
    }

    async fn find_zone_channel(
        &self,
        zone_id: Option<&str>,
        category: &str,
    ) -> Result<Option<ChannelId>> {
        let id: Option<uuid::Uuid> = sqlx::query_scalar(
            "SELECT id FROM chat_channels WHERE channel_type = 'zone' AND zone_id IS NOT DISTINCT FROM $1 AND category = $2",
        )
        .bind(zone_id)
        .bind(category)
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| Error::wrap("chat", "failed to look up zone channel", e))?;

        Ok(id.map(ChannelId::from_uuid))
    }

    pub async fn join(&self, channel_id: ChannelId, account_id: AccountId) -> Result<()> {
        self.require_joinable(channel_id).await?;

        sqlx::query("INSERT INTO chat_channel_members (channel_id, account_id) VALUES ($1, $2) ON CONFLICT DO NOTHING")
            .bind(channel_id.as_uuid())
            .bind(account_id.as_uuid())
            .execute(&self.pool)
            .await
            .map_err(|e| Error::wrap("chat", "failed to join channel", e))?;

        Ok(())
    }

    pub async fn leave(&self, channel_id: ChannelId, account_id: AccountId) -> Result<()> {
        self.require_joinable(channel_id).await?;

        sqlx::query("DELETE FROM chat_channel_members WHERE channel_id = $1 AND account_id = $2")
            .bind(channel_id.as_uuid())
            .bind(account_id.as_uuid())
            .execute(&self.pool)
            .await
            .map_err(|e| Error::wrap("chat", "failed to leave channel", e))?;

        Ok(())
    }

    async fn require_joinable(&self, channel_id: ChannelId) -> Result<()> {
        let channel_type: String =
            sqlx::query_scalar("SELECT channel_type FROM chat_channels WHERE id = $1")
                .bind(channel_id.as_uuid())
                .fetch_optional(&self.pool)
                .await
                .map_err(|e| Error::wrap("chat", "failed to look up channel", e))?
                .ok_or_else(|| Error::new("chat", "channel does not exist"))?;

        match channel_type.as_str() {
            "group" | "guild" => Ok(()),
            other => Err(Error::new(
                "chat",
                format!(
                    "cannot join/leave a {other} channel — membership isn't explicit for this type"
                ),
            )),
        }
    }

    pub async fn members(&self, channel_id: ChannelId) -> Result<Vec<AccountId>> {
        let rows = sqlx::query("SELECT account_id FROM chat_channel_members WHERE channel_id = $1")
            .bind(channel_id.as_uuid())
            .fetch_all(&self.pool)
            .await
            .map_err(|e| Error::wrap("chat", "failed to list channel members", e))?;

        Ok(rows
            .into_iter()
            .map(|row| AccountId::from_uuid(row.get("account_id")))
            .collect())
    }

    /// Looks up `channel_id`'s `channel_type` — `None` if the channel
    /// doesn't exist. Backs [`crate::pubsub::ChatBus::publish`]'s
    /// zone-channel exemption from the ordinary membership check (#186):
    /// a `zone` channel never gets `chat_channel_members` rows (see
    /// `ensure_zone_channel`'s own doc comment, and
    /// docs/specs/Chat_Spec.md's channel-types table), so `is_member`
    /// would otherwise always be `false` for one and every send would be
    /// rejected.
    pub async fn channel_type(&self, channel_id: ChannelId) -> Result<Option<ChannelType>> {
        let channel_type: Option<String> =
            sqlx::query_scalar("SELECT channel_type FROM chat_channels WHERE id = $1")
                .bind(channel_id.as_uuid())
                .fetch_optional(&self.pool)
                .await
                .map_err(|e| Error::wrap("chat", "failed to look up channel type", e))?;

        Ok(channel_type.and_then(|t| match t.as_str() {
            "direct" => Some(ChannelType::Direct),
            "group" => Some(ChannelType::Group),
            "guild" => Some(ChannelType::Guild),
            "zone" => Some(ChannelType::Zone),
            _ => None,
        }))
    }

    pub async fn is_member(&self, channel_id: ChannelId, account_id: AccountId) -> Result<bool> {
        let exists: bool = sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM chat_channel_members WHERE channel_id = $1 AND account_id = $2)",
        )
        .bind(channel_id.as_uuid())
        .bind(account_id.as_uuid())
        .fetch_one(&self.pool)
        .await
        .map_err(|e| Error::wrap("chat", "failed to check channel membership", e))?;

        Ok(exists)
    }
}

#[cfg(test)]
mod tests {
    use common::config::PostgresConfig;
    use common::pool::{PoolOptions, postgres_pool};

    use super::*;

    // Real Postgres — set WZ_POSTGRES_* and run with `-- --ignored`. Each
    // test inserts its own throwaway accounts (chat_channels.created_by
    // and chat_channel_members.account_id are real FKs).
    async fn store_with_accounts(n: usize) -> (ChannelStore, Vec<AccountId>) {
        let config = PostgresConfig::from_env().expect("WZ_POSTGRES_* env vars set");
        let pool = postgres_pool(&config, PoolOptions::default())
            .await
            .unwrap();

        let mut accounts = Vec::with_capacity(n);
        for _ in 0..n {
            let id = AccountId::new();
            sqlx::query(
                "INSERT INTO accounts (id, username, password_hash) VALUES ($1, $2, 'unused')",
            )
            .bind(id.as_uuid())
            .bind(format!("chat-store-test-{id}"))
            .execute(&pool)
            .await
            .unwrap();
            accounts.push(id);
        }

        (ChannelStore::new(pool), accounts)
    }

    #[tokio::test]
    #[ignore]
    async fn create_direct_is_idempotent() {
        let (store, accounts) = store_with_accounts(2).await;
        let (a, b) = (accounts[0], accounts[1]);

        let first = store.create_direct(a, b).await.unwrap();
        let second = store.create_direct(a, b).await.unwrap();
        let reversed = store.create_direct(b, a).await.unwrap();

        assert_eq!(first, second);
        assert_eq!(first, reversed);
        assert_eq!(store.members(first).await.unwrap().len(), 2);
    }

    #[tokio::test]
    #[ignore]
    async fn group_create_join_leave_round_trips() {
        let (store, accounts) = store_with_accounts(2).await;
        let (creator, joiner) = (accounts[0], accounts[1]);

        let channel = store
            .create_group(creator, "Adventuring Party")
            .await
            .unwrap();
        assert!(store.is_member(channel, creator).await.unwrap());
        assert!(!store.is_member(channel, joiner).await.unwrap());

        store.join(channel, joiner).await.unwrap();
        assert!(store.is_member(channel, joiner).await.unwrap());
        assert_eq!(store.members(channel).await.unwrap().len(), 2);

        store.leave(channel, joiner).await.unwrap();
        assert!(!store.is_member(channel, joiner).await.unwrap());
    }

    #[tokio::test]
    #[ignore]
    async fn cannot_join_or_leave_a_direct_channel() {
        let (store, accounts) = store_with_accounts(3).await;
        let channel = store.create_direct(accounts[0], accounts[1]).await.unwrap();

        assert!(store.join(channel, accounts[2]).await.is_err());
        assert!(store.leave(channel, accounts[0]).await.is_err());
    }

    #[tokio::test]
    #[ignore]
    async fn ensure_zone_channel_is_idempotent_for_global_and_per_zone() {
        let (store, _accounts) = store_with_accounts(0).await;
        let category = format!("trade-test-{}", ChannelId::new());

        let global_a = store
            .ensure_zone_channel(None, &category, "Trade")
            .await
            .unwrap();
        let global_b = store
            .ensure_zone_channel(None, &category, "Trade")
            .await
            .unwrap();
        assert_eq!(global_a, global_b);

        let zone_a = store
            .ensure_zone_channel(Some("greenwood-forest"), &category, "Local")
            .await
            .unwrap();
        let zone_b = store
            .ensure_zone_channel(Some("stonebridge-village"), &category, "Local")
            .await
            .unwrap();
        assert_ne!(
            zone_a, zone_b,
            "different zones should get different channels for the same category"
        );
        assert_ne!(
            global_a, zone_a,
            "global and zone-scoped channels for the same category are distinct"
        );
    }

    #[tokio::test]
    #[ignore]
    async fn empty_channel_name_is_rejected() {
        let (store, accounts) = store_with_accounts(1).await;
        assert!(store.create_group(accounts[0], "  ").await.is_err());
    }

    #[tokio::test]
    #[ignore]
    async fn channel_type_resolves_every_variant_and_none_for_a_missing_channel() {
        let (store, accounts) = store_with_accounts(2).await;
        let (a, b) = (accounts[0], accounts[1]);

        let direct = store.create_direct(a, b).await.unwrap();
        let group = store.create_group(a, "Adventuring Party 2").await.unwrap();
        let zone = store
            .ensure_zone_channel(
                Some("greenwood-forest"),
                &format!("local-{}", ChannelId::new()),
                "Local",
            )
            .await
            .unwrap();

        assert_eq!(
            store.channel_type(direct).await.unwrap(),
            Some(ChannelType::Direct)
        );
        assert_eq!(
            store.channel_type(group).await.unwrap(),
            Some(ChannelType::Group)
        );
        assert_eq!(
            store.channel_type(zone).await.unwrap(),
            Some(ChannelType::Zone)
        );
        assert_eq!(store.channel_type(ChannelId::new()).await.unwrap(), None);
    }
}
