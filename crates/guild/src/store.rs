//! Guild storage (#179) — a durable, `AccountId`-keyed roster with a
//! dev-declared rank hierarchy (`schema::GuildSchema`,
//! `guild.schema.yaml`). Deliberately keyed by `AccountId`, not
//! `CharacterId` like `character::PartyStore`: it matches
//! `chat_channel_members`' existing `account_id` keying (so the guild
//! roster and its synced chat channel roster never need a
//! character→account translation to compare), and a guild persists
//! across a single character's logout/deletion — it isn't tied to one
//! character the way a party is.
//!
//! `GuildStore` knows nothing about `chat` — `server::session` is the
//! only place that creates/syncs the optional chat channel a guild's
//! `chat_channel_id` column may point at (see the migration's own doc
//! comment for why that column has no foreign key).
//!
//! Every mutation other than `create`/`accept_invite`/`kick` derives
//! the acting account's own guild from its current membership rather
//! than taking a `GuildId` parameter — an account is in at most one
//! guild at a time (enforced by the migration's unique index), so
//! there's never an ambiguity to resolve.

use common::id::{AccountId, ChannelId, GuildId, RealmId};
use common::{Error, Result};
use sqlx::PgPool;

use crate::schema::{GuildPermission, GuildSchema};

pub struct GuildInfo {
    pub id: GuildId,
    pub name: String,
    pub motd: Option<String>,
    pub tag: Option<String>,
    pub chat_channel_id: Option<ChannelId>,
}

pub struct GuildStore {
    pool: PgPool,
    schema: GuildSchema,
}

impl GuildStore {
    pub fn new(pool: PgPool, schema: GuildSchema) -> Self {
        Self { pool, schema }
    }

    pub async fn create(
        &self,
        founder: AccountId,
        name: &str,
        realm_id: RealmId,
        chat_channel_id: Option<ChannelId>,
    ) -> Result<GuildId> {
        if name.trim().is_empty() {
            return Err(Error::new("guild", "guild name must not be empty"));
        }

        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|e| Error::wrap("guild", "failed to start transaction", e))?;

        if Self::guild_of_tx(&mut tx, founder).await?.is_some() {
            return Err(Error::new("guild", "account is already in a guild"));
        }

        let id = GuildId::new();
        sqlx::query(
            "INSERT INTO guilds (id, name, realm_id, chat_channel_id) VALUES ($1, $2, $3, $4)",
        )
        .bind(id.as_uuid())
        .bind(name)
        .bind(realm_id.as_uuid())
        .bind(chat_channel_id.map(|c| c.as_uuid()))
        .execute(&mut *tx)
        .await
        .map_err(|e| Error::wrap("guild", "failed to create guild", e))?;

        sqlx::query(
            "INSERT INTO guild_members (guild_id, account_id, rank_key) VALUES ($1, $2, $3)",
        )
        .bind(id.as_uuid())
        .bind(founder.as_uuid())
        .bind(&self.schema.founder_rank().key)
        .execute(&mut *tx)
        .await
        .map_err(|e| Error::wrap("guild", "failed to add founder to new guild", e))?;

        tx.commit()
            .await
            .map_err(|e| Error::wrap("guild", "failed to commit guild creation", e))?;
        Ok(id)
    }

    /// The accept side of an invite/accept flow — the real trigger lives
    /// in `server::session`. Unlike `PartyStore::accept_invite`, this
    /// never creates a guild on the fly: the inviter must already belong
    /// to one (via `GuildCreate`) and hold the `Invite` permission.
    pub async fn accept_invite(&self, inviter: AccountId, invitee: AccountId) -> Result<GuildId> {
        if inviter == invitee {
            return Err(Error::new("guild", "an account cannot invite itself"));
        }

        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|e| Error::wrap("guild", "failed to start transaction", e))?;

        if Self::guild_of_tx(&mut tx, invitee).await?.is_some() {
            return Err(Error::new("guild", "invitee is already in a guild"));
        }

        let (guild_id, inviter_rank_key) = Self::membership_tx(&mut tx, inviter)
            .await?
            .ok_or_else(|| Error::new("guild", "inviter is not in a guild"))?;

        let inviter_rank = self.schema.resolve(&inviter_rank_key)?;
        if !inviter_rank.has(GuildPermission::Invite) {
            return Err(Error::new(
                "guild",
                "inviter's rank does not have permission to invite",
            ));
        }

        sqlx::query(
            "INSERT INTO guild_members (guild_id, account_id, rank_key) VALUES ($1, $2, $3)",
        )
        .bind(guild_id.as_uuid())
        .bind(invitee.as_uuid())
        .bind(&self.schema.default_member_rank().key)
        .execute(&mut *tx)
        .await
        .map_err(|e| Error::wrap("guild", "failed to add invitee to guild", e))?;

        tx.commit()
            .await
            .map_err(|e| Error::wrap("guild", "failed to commit guild invite acceptance", e))?;
        Ok(guild_id)
    }

    /// Removes `target` from `actor`'s guild. `actor` must hold the
    /// `Kick` permission; `target` may never be at the founder rank
    /// (schema index 0) — that member must be demoted first by another
    /// founder-rank member, or the guild disbanded.
    pub async fn kick(&self, actor: AccountId, target: AccountId) -> Result<()> {
        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|e| Error::wrap("guild", "failed to start transaction", e))?;

        let (guild_id, actor_rank_key) = Self::membership_tx(&mut tx, actor)
            .await?
            .ok_or_else(|| Error::new("guild", "actor is not in a guild"))?;
        let actor_rank = self.schema.resolve(&actor_rank_key)?;
        if !actor_rank.has(GuildPermission::Kick) {
            return Err(Error::new(
                "guild",
                "actor's rank does not have permission to kick",
            ));
        }

        let (target_guild_id, target_rank_key) = Self::membership_tx(&mut tx, target)
            .await?
            .ok_or_else(|| Error::new("guild", "target is not in a guild"))?;
        if target_guild_id != guild_id {
            return Err(Error::new("guild", "target is not in actor's guild"));
        }
        if self.schema.is_founder_rank(&target_rank_key) {
            return Err(Error::new(
                "guild",
                "cannot kick a member at the founder rank",
            ));
        }

        sqlx::query("DELETE FROM guild_members WHERE guild_id = $1 AND account_id = $2")
            .bind(guild_id.as_uuid())
            .bind(target.as_uuid())
            .execute(&mut *tx)
            .await
            .map_err(|e| Error::wrap("guild", "failed to remove guild member", e))?;

        tx.commit()
            .await
            .map_err(|e| Error::wrap("guild", "failed to commit kick", e))?;
        Ok(())
    }

    pub async fn promote(
        &self,
        actor: AccountId,
        target: AccountId,
        new_rank_key: &str,
    ) -> Result<()> {
        self.change_rank(actor, target, new_rank_key, GuildPermission::Promote)
            .await
    }

    pub async fn demote(
        &self,
        actor: AccountId,
        target: AccountId,
        new_rank_key: &str,
    ) -> Result<()> {
        self.change_rank(actor, target, new_rank_key, GuildPermission::Demote)
            .await
    }

    /// Shared by `promote`/`demote` — the two only differ in which
    /// permission gates them; the actual rank move is identical. Moving
    /// anyone into or out of the founder rank (schema index 0) is
    /// restricted to an actor who already holds the founder rank
    /// themselves, regardless of `required_permission` — the one core
    /// invariant `schema::GuildSchema`'s doc comment describes.
    async fn change_rank(
        &self,
        actor: AccountId,
        target: AccountId,
        new_rank_key: &str,
        required_permission: GuildPermission,
    ) -> Result<()> {
        let new_rank = self.schema.resolve(new_rank_key)?;

        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|e| Error::wrap("guild", "failed to start transaction", e))?;

        let (guild_id, actor_rank_key) = Self::membership_tx(&mut tx, actor)
            .await?
            .ok_or_else(|| Error::new("guild", "actor is not in a guild"))?;
        let actor_rank = self.schema.resolve(&actor_rank_key)?;
        if !actor_rank.has(required_permission) {
            return Err(Error::new(
                "guild",
                format!("actor's rank does not have permission to {required_permission:?}"),
            ));
        }

        let (target_guild_id, target_rank_key) = Self::membership_tx(&mut tx, target)
            .await?
            .ok_or_else(|| Error::new("guild", "target is not in a guild"))?;
        if target_guild_id != guild_id {
            return Err(Error::new("guild", "target is not in actor's guild"));
        }

        let moves_founder_rank = self.schema.is_founder_rank(new_rank_key)
            || self.schema.is_founder_rank(&target_rank_key);
        if moves_founder_rank && !self.schema.is_founder_rank(&actor_rank_key) {
            return Err(Error::new(
                "guild",
                "only a founder-rank member may promote or demote into or out of the founder rank",
            ));
        }

        sqlx::query(
            "UPDATE guild_members SET rank_key = $1 WHERE guild_id = $2 AND account_id = $3",
        )
        .bind(&new_rank.key)
        .bind(guild_id.as_uuid())
        .bind(target.as_uuid())
        .execute(&mut *tx)
        .await
        .map_err(|e| Error::wrap("guild", "failed to update guild member rank", e))?;

        tx.commit()
            .await
            .map_err(|e| Error::wrap("guild", "failed to commit rank change", e))?;
        Ok(())
    }

    /// A founder-rank member cannot leave while other members remain —
    /// they must promote a successor to the founder rank or disband
    /// first. A founder-rank member who is the guild's last member
    /// leaving dissolves the guild entirely, mirroring
    /// `PartyStore::leave`'s "no such thing as a one-member party" rule.
    pub async fn leave(&self, account_id: AccountId) -> Result<()> {
        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|e| Error::wrap("guild", "failed to start transaction", e))?;

        let (guild_id, rank_key) = Self::membership_tx(&mut tx, account_id)
            .await?
            .ok_or_else(|| Error::new("guild", "account is not in a guild"))?;

        if self.schema.is_founder_rank(&rank_key) {
            let others: i64 = sqlx::query_scalar(
                "SELECT COUNT(*) FROM guild_members WHERE guild_id = $1 AND account_id != $2",
            )
            .bind(guild_id.as_uuid())
            .bind(account_id.as_uuid())
            .fetch_one(&mut *tx)
            .await
            .map_err(|e| Error::wrap("guild", "failed to count other guild members", e))?;

            if others > 0 {
                return Err(Error::new(
                    "guild",
                    "a founder-rank member must transfer leadership or disband before leaving a guild with other members",
                ));
            }

            sqlx::query("DELETE FROM guilds WHERE id = $1")
                .bind(guild_id.as_uuid())
                .execute(&mut *tx)
                .await
                .map_err(|e| Error::wrap("guild", "failed to dissolve guild", e))?;
        } else {
            sqlx::query("DELETE FROM guild_members WHERE guild_id = $1 AND account_id = $2")
                .bind(guild_id.as_uuid())
                .bind(account_id.as_uuid())
                .execute(&mut *tx)
                .await
                .map_err(|e| Error::wrap("guild", "failed to remove guild member", e))?;
        }

        tx.commit()
            .await
            .map_err(|e| Error::wrap("guild", "failed to commit guild leave", e))?;
        Ok(())
    }

    /// Only a founder-rank member may disband — deletes the guild and
    /// cascades its membership rows.
    pub async fn disband(&self, requested_by: AccountId) -> Result<()> {
        let (guild_id, rank_key) = self
            .membership_of(requested_by)
            .await?
            .ok_or_else(|| Error::new("guild", "account is not in a guild"))?;
        if !self.schema.is_founder_rank(&rank_key) {
            return Err(Error::new(
                "guild",
                "only a founder-rank member may disband a guild",
            ));
        }

        sqlx::query("DELETE FROM guilds WHERE id = $1")
            .bind(guild_id.as_uuid())
            .execute(&self.pool)
            .await
            .map_err(|e| Error::wrap("guild", "failed to disband guild", e))?;
        Ok(())
    }

    pub async fn set_motd(&self, actor: AccountId, motd: Option<&str>) -> Result<()> {
        let guild_id = self
            .require_permission(actor, GuildPermission::EditMotd)
            .await?;
        sqlx::query("UPDATE guilds SET motd = $1 WHERE id = $2")
            .bind(motd)
            .bind(guild_id.as_uuid())
            .execute(&self.pool)
            .await
            .map_err(|e| Error::wrap("guild", "failed to update guild motd", e))?;
        Ok(())
    }

    pub async fn set_tag(&self, actor: AccountId, tag: Option<&str>) -> Result<()> {
        let guild_id = self
            .require_permission(actor, GuildPermission::EditTag)
            .await?;
        sqlx::query("UPDATE guilds SET tag = $1 WHERE id = $2")
            .bind(tag)
            .bind(guild_id.as_uuid())
            .execute(&self.pool)
            .await
            .map_err(|e| Error::wrap("guild", "failed to update guild tag", e))?;
        Ok(())
    }

    pub async fn rename(&self, actor: AccountId, name: &str) -> Result<()> {
        if name.trim().is_empty() {
            return Err(Error::new("guild", "guild name must not be empty"));
        }
        let guild_id = self
            .require_permission(actor, GuildPermission::Rename)
            .await?;
        sqlx::query("UPDATE guilds SET name = $1 WHERE id = $2")
            .bind(name)
            .bind(guild_id.as_uuid())
            .execute(&self.pool)
            .await
            .map_err(|e| Error::wrap("guild", "failed to rename guild", e))?;
        Ok(())
    }

    /// Looks up `actor`'s guild and rejects unless their current rank
    /// carries `required_permission`, returning the guild id on success
    /// — the shared precondition every metadata-editing method needs.
    async fn require_permission(
        &self,
        actor: AccountId,
        required_permission: GuildPermission,
    ) -> Result<GuildId> {
        let (guild_id, rank_key) = self
            .membership_of(actor)
            .await?
            .ok_or_else(|| Error::new("guild", "actor is not in a guild"))?;
        let rank = self.schema.resolve(&rank_key)?;
        if !rank.has(required_permission) {
            return Err(Error::new(
                "guild",
                format!("actor's rank does not have permission to {required_permission:?}"),
            ));
        }
        Ok(guild_id)
    }

    pub async fn members_of(&self, guild_id: GuildId) -> Result<Vec<(AccountId, String)>> {
        let rows: Vec<(uuid::Uuid, String)> =
            sqlx::query_as("SELECT account_id, rank_key FROM guild_members WHERE guild_id = $1")
                .bind(guild_id.as_uuid())
                .fetch_all(&self.pool)
                .await
                .map_err(|e| Error::wrap("guild", "failed to look up guild members", e))?;

        Ok(rows
            .into_iter()
            .map(|(id, rank_key)| (AccountId::from_uuid(id), rank_key))
            .collect())
    }

    pub async fn guild_of(&self, account_id: AccountId) -> Result<Option<GuildId>> {
        Ok(self.membership_of(account_id).await?.map(|(id, _)| id))
    }

    pub async fn info(&self, guild_id: GuildId) -> Result<Option<GuildInfo>> {
        type GuildRow = (
            uuid::Uuid,
            String,
            Option<String>,
            Option<String>,
            Option<uuid::Uuid>,
        );
        let row: Option<GuildRow> =
            sqlx::query_as("SELECT id, name, motd, tag, chat_channel_id FROM guilds WHERE id = $1")
                .bind(guild_id.as_uuid())
                .fetch_optional(&self.pool)
                .await
                .map_err(|e| Error::wrap("guild", "failed to look up guild", e))?;

        Ok(row.map(|(id, name, motd, tag, chat_channel_id)| GuildInfo {
            id: GuildId::from_uuid(id),
            name,
            motd,
            tag,
            chat_channel_id: chat_channel_id.map(ChannelId::from_uuid),
        }))
    }

    async fn membership_of(&self, account_id: AccountId) -> Result<Option<(GuildId, String)>> {
        let row: Option<(uuid::Uuid, String)> =
            sqlx::query_as("SELECT guild_id, rank_key FROM guild_members WHERE account_id = $1")
                .bind(account_id.as_uuid())
                .fetch_optional(&self.pool)
                .await
                .map_err(|e| Error::wrap("guild", "failed to look up guild membership", e))?;

        Ok(row.map(|(guild_id, rank_key)| (GuildId::from_uuid(guild_id), rank_key)))
    }

    async fn membership_tx(
        tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
        account_id: AccountId,
    ) -> Result<Option<(GuildId, String)>> {
        let row: Option<(uuid::Uuid, String)> =
            sqlx::query_as("SELECT guild_id, rank_key FROM guild_members WHERE account_id = $1")
                .bind(account_id.as_uuid())
                .fetch_optional(&mut **tx)
                .await
                .map_err(|e| Error::wrap("guild", "failed to look up guild membership", e))?;

        Ok(row.map(|(guild_id, rank_key)| (GuildId::from_uuid(guild_id), rank_key)))
    }

    async fn guild_of_tx(
        tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
        account_id: AccountId,
    ) -> Result<Option<GuildId>> {
        Ok(Self::membership_tx(tx, account_id).await?.map(|(id, _)| id))
    }
}

#[cfg(test)]
mod tests {
    use common::config::PostgresConfig;
    use common::id::RealmId;
    use common::pool::{PoolOptions, postgres_pool};

    use super::*;

    fn schema() -> GuildSchema {
        GuildSchema::from_yaml(
            r#"
schema_version: 1
ranks:
  - key: leader
    name: Guild Master
    permissions: [invite, kick, promote, demote, edit_motd, edit_tag, rename]
  - key: officer
    name: Officer
    permissions: [invite, kick, edit_motd]
  - key: member
    name: Member
"#,
        )
        .unwrap()
    }

    // Real Postgres — set WZ_POSTGRES_* and run with `-- --ignored`.
    async fn store_with_accounts(count: usize) -> (GuildStore, RealmId, Vec<AccountId>) {
        let pg_config = PostgresConfig::from_env().expect("WZ_POSTGRES_* env vars set");
        let pool = postgres_pool(&pg_config, PoolOptions::default())
            .await
            .unwrap();

        let realm_id = RealmId::new();
        sqlx::query(
            "INSERT INTO realms (id, name, open_or_bound) VALUES ($1, 'Guild Test Realm', 'open')",
        )
        .bind(realm_id.as_uuid())
        .execute(&pool)
        .await
        .unwrap();

        let mut account_ids = Vec::new();
        for i in 0..count {
            let account_id = AccountId::new();
            sqlx::query(
                "INSERT INTO accounts (id, username, password_hash) VALUES ($1, $2, 'unused')",
            )
            .bind(account_id.as_uuid())
            .bind(format!("guild-test-{account_id}-{i}"))
            .execute(&pool)
            .await
            .unwrap();
            account_ids.push(account_id);
        }

        (GuildStore::new(pool, schema()), realm_id, account_ids)
    }

    #[tokio::test]
    #[ignore]
    async fn creating_a_guild_places_the_founder_at_the_founder_rank() {
        let (store, realm_id, accounts) = store_with_accounts(1).await;
        let founder = accounts[0];

        let guild_id = store
            .create(founder, "Test Guild", realm_id, None)
            .await
            .unwrap();

        let members = store.members_of(guild_id).await.unwrap();
        assert_eq!(members, vec![(founder, "leader".to_string())]);
    }

    #[tokio::test]
    #[ignore]
    async fn accepting_an_invite_from_a_permitted_rank_joins_at_the_default_member_rank() {
        let (store, realm_id, accounts) = store_with_accounts(2).await;
        let [founder, invitee] = accounts[..] else {
            unreachable!()
        };

        let guild_id = store
            .create(founder, "Test Guild", realm_id, None)
            .await
            .unwrap();
        store.accept_invite(founder, invitee).await.unwrap();

        let mut members = store.members_of(guild_id).await.unwrap();
        members.sort();
        let mut expected = vec![
            (founder, "leader".to_string()),
            (invitee, "member".to_string()),
        ];
        expected.sort();
        assert_eq!(members, expected);
    }

    #[tokio::test]
    #[ignore]
    async fn an_already_guilded_invitee_is_rejected() {
        let (store, realm_id, accounts) = store_with_accounts(3).await;
        let [a, b, c] = accounts[..] else {
            unreachable!()
        };

        store.create(a, "Guild A", realm_id, None).await.unwrap();
        store.accept_invite(a, b).await.unwrap();
        store.create(c, "Guild C", realm_id, None).await.unwrap();

        assert!(store.accept_invite(c, b).await.is_err());
    }

    #[tokio::test]
    #[ignore]
    async fn an_account_cannot_invite_itself() {
        let (store, realm_id, accounts) = store_with_accounts(1).await;
        let a = accounts[0];
        store.create(a, "Guild A", realm_id, None).await.unwrap();
        assert!(store.accept_invite(a, a).await.is_err());
    }

    #[tokio::test]
    #[ignore]
    async fn a_rank_without_invite_permission_cannot_invite() {
        let (store, realm_id, accounts) = store_with_accounts(3).await;
        let [founder, member, outsider] = accounts[..] else {
            unreachable!()
        };

        store
            .create(founder, "Test Guild", realm_id, None)
            .await
            .unwrap();
        store.accept_invite(founder, member).await.unwrap();
        // `member` joined at the "member" rank, which has no permissions.
        assert!(store.accept_invite(member, outsider).await.is_err());
    }

    #[tokio::test]
    #[ignore]
    async fn a_rank_with_kick_permission_can_kick_a_non_founder_member() {
        let (store, realm_id, accounts) = store_with_accounts(2).await;
        let [founder, member] = accounts[..] else {
            unreachable!()
        };

        let guild_id = store
            .create(founder, "Test Guild", realm_id, None)
            .await
            .unwrap();
        store.accept_invite(founder, member).await.unwrap();
        store.kick(founder, member).await.unwrap();

        assert_eq!(
            store.members_of(guild_id).await.unwrap(),
            vec![(founder, "leader".to_string())]
        );
    }

    #[tokio::test]
    #[ignore]
    async fn the_founder_rank_cannot_be_kicked() {
        let (store, realm_id, accounts) = store_with_accounts(2).await;
        let [founder, member] = accounts[..] else {
            unreachable!()
        };

        store
            .create(founder, "Test Guild", realm_id, None)
            .await
            .unwrap();
        store.accept_invite(founder, member).await.unwrap();
        // Promote member to officer (has kick permission) so we can prove
        // the founder-rank protection, not just a permission failure.
        store.promote(founder, member, "officer").await.unwrap();

        assert!(store.kick(member, founder).await.is_err());
    }

    #[tokio::test]
    #[ignore]
    async fn promoting_and_demoting_moves_a_member_between_non_founder_ranks() {
        let (store, realm_id, accounts) = store_with_accounts(2).await;
        let [founder, member] = accounts[..] else {
            unreachable!()
        };

        let guild_id = store
            .create(founder, "Test Guild", realm_id, None)
            .await
            .unwrap();
        store.accept_invite(founder, member).await.unwrap();

        store.promote(founder, member, "officer").await.unwrap();
        let members = store.members_of(guild_id).await.unwrap();
        assert!(members.contains(&(member, "officer".to_string())));

        store.demote(founder, member, "member").await.unwrap();
        let members = store.members_of(guild_id).await.unwrap();
        assert!(members.contains(&(member, "member".to_string())));
    }

    #[tokio::test]
    #[ignore]
    async fn only_a_founder_rank_member_may_promote_someone_into_the_founder_rank() {
        let (store, realm_id, accounts) = store_with_accounts(3).await;
        let [founder, officer, member] = accounts[..] else {
            unreachable!()
        };

        store
            .create(founder, "Test Guild", realm_id, None)
            .await
            .unwrap();
        store.accept_invite(founder, officer).await.unwrap();
        store.promote(founder, officer, "officer").await.unwrap();
        store.accept_invite(founder, member).await.unwrap();

        // Officer has Promote permission but isn't founder-rank, so
        // moving `member` into the founder rank must still be rejected.
        let result = store.promote(officer, member, "leader").await;
        assert!(result.is_err(), "{result:?}");
    }

    #[tokio::test]
    #[ignore]
    async fn a_non_founder_member_can_simply_leave() {
        let (store, realm_id, accounts) = store_with_accounts(2).await;
        let [founder, member] = accounts[..] else {
            unreachable!()
        };

        let guild_id = store
            .create(founder, "Test Guild", realm_id, None)
            .await
            .unwrap();
        store.accept_invite(founder, member).await.unwrap();
        store.leave(member).await.unwrap();

        assert_eq!(
            store.members_of(guild_id).await.unwrap(),
            vec![(founder, "leader".to_string())]
        );
    }

    #[tokio::test]
    #[ignore]
    async fn a_founder_cannot_leave_while_other_members_remain() {
        let (store, realm_id, accounts) = store_with_accounts(2).await;
        let [founder, member] = accounts[..] else {
            unreachable!()
        };

        store
            .create(founder, "Test Guild", realm_id, None)
            .await
            .unwrap();
        store.accept_invite(founder, member).await.unwrap();

        assert!(store.leave(founder).await.is_err());
    }

    #[tokio::test]
    #[ignore]
    async fn a_lone_founder_leaving_dissolves_the_guild() {
        let (store, realm_id, accounts) = store_with_accounts(1).await;
        let founder = accounts[0];

        let guild_id = store
            .create(founder, "Test Guild", realm_id, None)
            .await
            .unwrap();
        store.leave(founder).await.unwrap();

        assert!(store.members_of(guild_id).await.unwrap().is_empty());
        assert!(store.guild_of(founder).await.unwrap().is_none());
    }

    #[tokio::test]
    #[ignore]
    async fn disband_is_founder_rank_only() {
        let (store, realm_id, accounts) = store_with_accounts(2).await;
        let [founder, member] = accounts[..] else {
            unreachable!()
        };

        let guild_id = store
            .create(founder, "Test Guild", realm_id, None)
            .await
            .unwrap();
        store.accept_invite(founder, member).await.unwrap();

        assert!(store.disband(member).await.is_err());
        store.disband(founder).await.unwrap();
        assert!(store.members_of(guild_id).await.unwrap().is_empty());
    }

    #[tokio::test]
    #[ignore]
    async fn metadata_edits_are_permission_gated() {
        let (store, realm_id, accounts) = store_with_accounts(2).await;
        let [founder, member] = accounts[..] else {
            unreachable!()
        };

        let guild_id = store
            .create(founder, "Test Guild", realm_id, None)
            .await
            .unwrap();
        store.accept_invite(founder, member).await.unwrap();

        // `member` rank has no permissions at all.
        assert!(store.set_motd(member, Some("hi")).await.is_err());
        assert!(store.rename(member, "New Name").await.is_err());

        store.set_motd(founder, Some("Welcome!")).await.unwrap();
        store.rename(founder, "New Name").await.unwrap();

        let info = store.info(guild_id).await.unwrap().unwrap();
        assert_eq!(info.motd.as_deref(), Some("Welcome!"));
        assert_eq!(info.name, "New Name");
    }
}
