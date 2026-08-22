//! Shared helpers for chat's dev-facing demo tooling — not part of
//! chat's real API surface, just the "find-or-create by name" convenience
//! both demo entry points need so re-running with the same name rejoins
//! the same thing instead of creating a duplicate every time. `pub` only
//! because `src/bin/*` binaries compile as separate crates and need to
//! reach this from outside.

use common::id::{AccountId, ChannelId};
use common::{Error, Result};
use sqlx::PgPool;

use crate::store::ChannelStore;

/// A stable demo account per username — re-running with the same name
/// rejoins the same identity instead of creating a new one every time.
/// Only used by `bin/demo`'s `--no-gateway` direct mode now — gateway
/// mode authenticates for real via `auth::gateway_protocol`
/// (docs/specs/Auth_Spec.md, "Gateway handshake"), so this bypass
/// intentionally isn't a security boundary: no password, and `chat-demo-`
/// prefixed so it can never collide with a real registered username.
pub async fn find_or_create_demo_account(pool: &PgPool, username: &str) -> Result<AccountId> {
    let demo_username = format!("chat-demo-{username}");

    if let Some(id) =
        sqlx::query_scalar::<_, uuid::Uuid>("SELECT id FROM accounts WHERE username = $1")
            .bind(&demo_username)
            .fetch_optional(pool)
            .await
            .map_err(|e| Error::wrap("chat", "failed to look up demo account", e))?
    {
        return Ok(AccountId::from_uuid(id));
    }

    let id = AccountId::new();
    sqlx::query("INSERT INTO accounts (id, username, password_hash) VALUES ($1, $2, 'demo')")
        .bind(id.as_uuid())
        .bind(&demo_username)
        .execute(pool)
        .await
        .map_err(|e| Error::wrap("chat", "failed to create demo account", e))?;
    Ok(id)
}

/// Finds an existing `group` channel by name, or creates one owned by
/// `creator` — generalized from the old demo's single hardcoded "demo"
/// channel so `/join <name>` lands everyone naming the same channel in
/// the same place. `ChannelStore::create_group` itself isn't idempotent
/// on purpose (a player naming a new group twice makes two channels) —
/// this is the demo tooling's own "same name means same channel"
/// convenience layered on top of it.
pub async fn find_or_create_named_channel(
    pool: &PgPool,
    store: &ChannelStore,
    creator: AccountId,
    name: &str,
) -> Result<ChannelId> {
    if let Some(id) = sqlx::query_scalar::<_, uuid::Uuid>(
        "SELECT id FROM chat_channels WHERE channel_type = 'group' AND name = $1",
    )
    .bind(name)
    .fetch_optional(pool)
    .await
    .map_err(|e| Error::wrap("chat", "failed to look up channel", e))?
    {
        return Ok(ChannelId::from_uuid(id));
    }

    store.create_group(creator, name).await
}
