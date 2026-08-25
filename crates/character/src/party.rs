//! Party/group storage (#178) — a small, durable roster of characters,
//! not accounts, per-character the same way #142's reconnect-placement
//! logic already keys group state off the specific character rather
//! than the account. Deliberately not `chat`'s job: a party's *chat
//! channel* is one consequence of party membership, not the party
//! itself. `members_of` is exactly the query #142's placement primitive
//! (`server::zone_registry::ZoneRegistry::join_layer_of`) needs, both
//! for the live "someone just joined my party" trigger and reconnect
//! placement.
//!
//! Party *size* is dev-declared, not hardcoded — `party_schema.rs`'s
//! `PartySchema` (`party.schema.yaml`), same "core enforces generically,
//! dev declares the actual numbers/names" pattern `schema.rs`'s
//! `AttributeSchema` already uses for character stats. A game with a
//! 5-man "normal" party and a 3-man "rush" group declares both as
//! separate `party_types` entries; a party is founded under one of
//! them (whichever the founding invite named, or the schema's first
//! declared entry if it named none) and stays under that type's cap for
//! its whole life.

use common::id::{CharacterId, PartyId};
use common::{Error, Result};

use crate::party_schema::PartySchema;

pub struct PartyStore {
    pool: sqlx::PgPool,
    schema: PartySchema,
}

impl PartyStore {
    pub fn new(pool: sqlx::PgPool, schema: PartySchema) -> Self {
        Self { pool, schema }
    }

    /// Every other character currently in `character_id`'s party — empty
    /// if it isn't in one. Never includes `character_id` itself.
    pub async fn members_of(&self, character_id: CharacterId) -> Result<Vec<CharacterId>> {
        let rows: Vec<(uuid::Uuid,)> = sqlx::query_as(
            "SELECT pm2.character_id FROM party_members pm1 \
             JOIN party_members pm2 ON pm1.party_id = pm2.party_id \
             WHERE pm1.character_id = $1 AND pm2.character_id != $1",
        )
        .bind(character_id.as_uuid())
        .fetch_all(&self.pool)
        .await
        .map_err(|e| Error::wrap("character", "failed to look up party members", e))?;

        Ok(rows
            .into_iter()
            .map(|(id,)| CharacterId::from_uuid(id))
            .collect())
    }

    /// The accept side of an invite/accept flow (#178, the real trigger
    /// lives in `server::session`) — `invitee` joins `inviter`'s party,
    /// creating one first if `inviter` wasn't already in one. Rejects
    /// (with a clear error, not a raw constraint violation) if either
    /// character is already in a party, or if the party is already at
    /// its declared type's `max_members` cap. `requested_party_type`
    /// only matters when a *new* party is being founded — an empty
    /// string resolves to `PartySchema::default_type`; joining an
    /// *existing* party always uses whatever type it was actually
    /// founded under, ignoring this argument (a party's type doesn't
    /// change mid-life just because a later invite named a different
    /// one).
    pub async fn accept_invite(
        &self,
        inviter: CharacterId,
        invitee: CharacterId,
        requested_party_type: &str,
    ) -> Result<PartyId> {
        if inviter == invitee {
            return Err(Error::new(
                "character",
                "a character cannot party with itself",
            ));
        }

        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|e| Error::wrap("character", "failed to start transaction", e))?;

        let invitee_party: Option<(uuid::Uuid,)> =
            sqlx::query_as("SELECT party_id FROM party_members WHERE character_id = $1")
                .bind(invitee.as_uuid())
                .fetch_optional(&mut *tx)
                .await
                .map_err(|e| Error::wrap("character", "failed to look up invitee's party", e))?;
        if invitee_party.is_some() {
            return Err(Error::new("character", "invitee is already in a party"));
        }

        let inviter_party: Option<(uuid::Uuid, String)> = sqlx::query_as(
            "SELECT p.id, p.party_type FROM party_members pm \
             JOIN parties p ON p.id = pm.party_id WHERE pm.character_id = $1",
        )
        .bind(inviter.as_uuid())
        .fetch_optional(&mut *tx)
        .await
        .map_err(|e| Error::wrap("character", "failed to look up inviter's party", e))?;

        let (party_id, party_type_key, current_size) = match inviter_party {
            Some((id, party_type_key)) => {
                let size: i64 =
                    sqlx::query_scalar("SELECT COUNT(*) FROM party_members WHERE party_id = $1")
                        .bind(id)
                        .fetch_one(&mut *tx)
                        .await
                        .map_err(|e| {
                            Error::wrap("character", "failed to count party members", e)
                        })?;
                (PartyId::from_uuid(id), party_type_key, size)
            }
            None => {
                let party_type = if requested_party_type.is_empty() {
                    self.schema.default_type()
                } else {
                    self.schema.resolve(requested_party_type)?
                };
                let id = PartyId::new();
                sqlx::query("INSERT INTO parties (id, party_type) VALUES ($1, $2)")
                    .bind(id.as_uuid())
                    .bind(&party_type.key)
                    .execute(&mut *tx)
                    .await
                    .map_err(|e| Error::wrap("character", "failed to create party", e))?;
                sqlx::query("INSERT INTO party_members (party_id, character_id) VALUES ($1, $2)")
                    .bind(id.as_uuid())
                    .bind(inviter.as_uuid())
                    .execute(&mut *tx)
                    .await
                    .map_err(|e| {
                        Error::wrap("character", "failed to add inviter to new party", e)
                    })?;
                (id, party_type.key.clone(), 1)
            }
        };

        let party_type = self.schema.resolve(&party_type_key)?;
        if let Some(max_members) = party_type.max_members
            && current_size >= i64::from(max_members)
        {
            return Err(Error::new(
                "character",
                format!(
                    "party is already at its \"{party_type_key}\" cap of {max_members} members"
                ),
            ));
        }

        sqlx::query("INSERT INTO party_members (party_id, character_id) VALUES ($1, $2)")
            .bind(party_id.as_uuid())
            .bind(invitee.as_uuid())
            .execute(&mut *tx)
            .await
            .map_err(|e| Error::wrap("character", "failed to add invitee to party", e))?;

        tx.commit()
            .await
            .map_err(|e| Error::wrap("character", "failed to commit party formation", e))?;
        Ok(party_id)
    }

    /// Removes `character_id` from its party — a real storage mutation,
    /// not a chat-channel leave (#178's acceptance criteria). If this
    /// would drop the party to a single remaining member, the whole
    /// party is dissolved instead of leaving a "party of one" around —
    /// there's no such thing as a one-member party. Errs if
    /// `character_id` isn't currently in a party at all.
    pub async fn leave(&self, character_id: CharacterId) -> Result<()> {
        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|e| Error::wrap("character", "failed to start transaction", e))?;

        let party: Option<(uuid::Uuid,)> =
            sqlx::query_as("SELECT party_id FROM party_members WHERE character_id = $1")
                .bind(character_id.as_uuid())
                .fetch_optional(&mut *tx)
                .await
                .map_err(|e| Error::wrap("character", "failed to look up party membership", e))?;
        let Some((party_id,)) = party else {
            return Err(Error::new("character", "character is not in a party"));
        };

        sqlx::query("DELETE FROM party_members WHERE party_id = $1 AND character_id = $2")
            .bind(party_id)
            .bind(character_id.as_uuid())
            .execute(&mut *tx)
            .await
            .map_err(|e| Error::wrap("character", "failed to remove party member", e))?;

        let remaining: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM party_members WHERE party_id = $1")
                .bind(party_id)
                .fetch_one(&mut *tx)
                .await
                .map_err(|e| {
                    Error::wrap("character", "failed to count remaining party members", e)
                })?;

        if remaining < 2 {
            sqlx::query("DELETE FROM parties WHERE id = $1")
                .bind(party_id)
                .execute(&mut *tx)
                .await
                .map_err(|e| Error::wrap("character", "failed to dissolve party", e))?;
        }

        tx.commit()
            .await
            .map_err(|e| Error::wrap("character", "failed to commit party leave", e))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use common::config::PostgresConfig;
    use common::id::{AccountId, RealmId};
    use common::pool::{PoolOptions, postgres_pool};

    use super::*;

    fn schema() -> PartySchema {
        PartySchema::from_yaml(
            r#"
schema_version: 1
party_types:
  - key: normal
    max_members: 5
  - key: rush
    max_members: 3
  - key: raid
"#,
        )
        .unwrap()
    }

    // Real Postgres — set WZ_POSTGRES_* and run with `-- --ignored`.
    async fn store_with_characters(count: usize) -> (PartyStore, Vec<CharacterId>) {
        let pg_config = PostgresConfig::from_env().expect("WZ_POSTGRES_* env vars set");
        let pool = postgres_pool(&pg_config, PoolOptions::default())
            .await
            .unwrap();

        let account_id = AccountId::new();
        sqlx::query("INSERT INTO accounts (id, username, password_hash) VALUES ($1, $2, 'unused')")
            .bind(account_id.as_uuid())
            .bind(format!("party-test-{account_id}"))
            .execute(&pool)
            .await
            .unwrap();

        let realm_id = RealmId::new();
        sqlx::query(
            "INSERT INTO realms (id, name, open_or_bound) VALUES ($1, 'Party Test Realm', 'open')",
        )
        .bind(realm_id.as_uuid())
        .execute(&pool)
        .await
        .unwrap();

        let mut character_ids = Vec::new();
        for i in 0..count {
            let character_id = CharacterId::new();
            sqlx::query(
                "INSERT INTO characters (id, account_id, name, realm_id, zone_id) VALUES ($1, $2, $3, $4, 'greenwood-forest')",
            )
            .bind(character_id.as_uuid())
            .bind(account_id.as_uuid())
            .bind(format!("PartyMember{i}"))
            .bind(realm_id.as_uuid())
            .execute(&pool)
            .await
            .unwrap();
            character_ids.push(character_id);
        }

        (PartyStore::new(pool, schema()), character_ids)
    }

    #[tokio::test]
    #[ignore]
    async fn accepting_an_invite_creates_a_party_and_members_of_sees_each_other() {
        let (store, characters) = store_with_characters(2).await;
        let [a, b] = characters[..] else {
            unreachable!()
        };

        store.accept_invite(a, b, "").await.unwrap();

        assert_eq!(store.members_of(a).await.unwrap(), vec![b]);
        assert_eq!(store.members_of(b).await.unwrap(), vec![a]);
    }

    #[tokio::test]
    #[ignore]
    async fn a_third_accept_joins_the_same_existing_party() {
        let (store, characters) = store_with_characters(3).await;
        let [a, b, c] = characters[..] else {
            unreachable!()
        };

        store.accept_invite(a, b, "").await.unwrap();
        store.accept_invite(a, c, "").await.unwrap();

        let mut members_of_a = store.members_of(a).await.unwrap();
        members_of_a.sort();
        let mut expected = vec![b, c];
        expected.sort();
        assert_eq!(members_of_a, expected);

        let mut members_of_b = store.members_of(b).await.unwrap();
        members_of_b.sort();
        let mut expected_for_b = vec![a, c];
        expected_for_b.sort();
        assert_eq!(members_of_b, expected_for_b);
    }

    #[tokio::test]
    #[ignore]
    async fn an_already_partied_invitee_is_rejected() {
        let (store, characters) = store_with_characters(3).await;
        let [a, b, c] = characters[..] else {
            unreachable!()
        };

        store.accept_invite(a, b, "").await.unwrap();
        assert!(store.accept_invite(c, b, "").await.is_err());
    }

    #[tokio::test]
    #[ignore]
    async fn a_character_cannot_party_with_itself() {
        let (store, characters) = store_with_characters(1).await;
        let a = characters[0];
        assert!(store.accept_invite(a, a, "").await.is_err());
    }

    #[tokio::test]
    #[ignore]
    async fn leaving_a_two_member_party_dissolves_it_entirely() {
        let (store, characters) = store_with_characters(2).await;
        let [a, b] = characters[..] else {
            unreachable!()
        };

        store.accept_invite(a, b, "").await.unwrap();
        store.leave(a).await.unwrap();

        assert!(store.members_of(b).await.unwrap().is_empty());
        // b is no longer in a party either — the whole thing dissolved,
        // not just a's own membership.
        assert!(store.leave(b).await.is_err());
    }

    #[tokio::test]
    #[ignore]
    async fn leaving_a_larger_party_keeps_the_remaining_members_together() {
        let (store, characters) = store_with_characters(3).await;
        let [a, b, c] = characters[..] else {
            unreachable!()
        };

        store.accept_invite(a, b, "").await.unwrap();
        store.accept_invite(a, c, "").await.unwrap();
        store.leave(a).await.unwrap();

        assert_eq!(store.members_of(b).await.unwrap(), vec![c]);
        assert_eq!(store.members_of(c).await.unwrap(), vec![b]);
    }

    #[tokio::test]
    #[ignore]
    async fn leaving_without_a_party_is_an_error() {
        let (store, characters) = store_with_characters(1).await;
        assert!(store.leave(characters[0]).await.is_err());
    }

    #[tokio::test]
    #[ignore]
    async fn a_party_founded_under_a_capped_type_rejects_an_invite_past_the_cap() {
        // "rush" caps at 3 — a, b, c fill it; a fourth invite must fail.
        let (store, characters) = store_with_characters(4).await;
        let [a, b, c, d] = characters[..] else {
            unreachable!()
        };

        store.accept_invite(a, b, "rush").await.unwrap();
        store.accept_invite(a, c, "rush").await.unwrap();
        let result = store.accept_invite(a, d, "rush").await;
        assert!(result.is_err(), "{result:?}");
    }

    #[tokio::test]
    #[ignore]
    async fn an_uncapped_party_type_accepts_past_typical_sizes() {
        let (store, characters) = store_with_characters(5).await;
        let [a, b, c, d, e] = characters[..] else {
            unreachable!()
        };

        store.accept_invite(a, b, "raid").await.unwrap();
        store.accept_invite(a, c, "raid").await.unwrap();
        store.accept_invite(a, d, "raid").await.unwrap();
        store.accept_invite(a, e, "raid").await.unwrap();

        assert_eq!(store.members_of(a).await.unwrap().len(), 4);
    }

    #[tokio::test]
    #[ignore]
    async fn joining_an_existing_party_ignores_a_different_requested_type() {
        // The party was founded as "rush" (cap 3); a later invite naming
        // "normal" (cap 5) doesn't change the party's actual cap — it's
        // still governed by "rush", so a third member still fills it.
        let (store, characters) = store_with_characters(4).await;
        let [a, b, c, d] = characters[..] else {
            unreachable!()
        };

        store.accept_invite(a, b, "rush").await.unwrap();
        store.accept_invite(a, c, "normal").await.unwrap();
        let result = store.accept_invite(a, d, "normal").await;
        assert!(result.is_err(), "{result:?}");
    }

    #[tokio::test]
    #[ignore]
    async fn an_unknown_requested_party_type_is_rejected() {
        let (store, characters) = store_with_characters(2).await;
        let [a, b] = characters[..] else {
            unreachable!()
        };
        assert!(store.accept_invite(a, b, "not-a-real-type").await.is_err());
    }
}
