//! Persistent, account-scoped guilds (#179): a durable roster with a
//! dev-declared rank hierarchy, permission-gated actions, and optional
//! metadata (MOTD, tag).
//!
//! Design: docs/specs/Chat_Spec.md's "Guild system" section, and the
//! same "core enforces generically, dev declares the actual
//! names/numbers" pattern `character::PartySchema` already uses for
//! party types (docs/PROPOSAL.md, "Data Model Extensibility: Declared
//! Attribute Schemas").

pub mod schema;
pub mod store;

pub use schema::{GuildPermission, GuildRank, GuildSchema};
pub use store::GuildStore;

pub fn postgres_config() -> common::Result<common::config::PostgresConfig> {
    common::config::PostgresConfig::from_env()
}

pub async fn postgres_pool() -> common::Result<sqlx::PgPool> {
    let config = postgres_config()?;
    common::pool::postgres_pool(&config, common::pool::PoolOptions::default()).await
}
