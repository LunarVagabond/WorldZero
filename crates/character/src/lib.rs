//! Character records, inventory, and declared-schema stats.
//!
//! Design: docs/PROPOSAL.md ("Data Model Extensibility: Declared Attribute
//! Schemas") and docs/specs/Data_Model_Spec.md.

pub mod archetype_schema;
pub mod bound_liveness;
pub mod crafting;
pub mod crafting_schema;
pub mod currency_schema;
pub mod inventory;
pub mod party;
pub mod party_schema;
pub mod schema;
pub mod session_lease;
pub mod store;

pub use archetype_schema::{ArchetypeSchema, CharacterArchetype};
pub use bound_liveness::BoundRealmLiveness;
pub use crafting_schema::{CraftingInput, CraftingOutput, CraftingSchema, Recipe};
pub use currency_schema::{Currency, CurrencyDenomination, CurrencySchema};
pub use party::PartyStore;
pub use party_schema::{PartySchema, PartyType};
pub use schema::{AttributeSchema, StatDeclaration, StatType};
pub use session_lease::{CharacterSessionLease, LeaseOutcome};
pub use store::{CharacterStore, CharacterSummary};

pub fn postgres_config() -> common::Result<common::config::PostgresConfig> {
    common::config::PostgresConfig::from_env()
}

pub async fn postgres_pool() -> common::Result<sqlx::PgPool> {
    let config = postgres_config()?;
    common::pool::postgres_pool(&config, common::pool::PoolOptions::default()).await
}

#[cfg(test)]
mod tests {
    use sqlx::Row;

    use super::*;

    #[tokio::test]
    #[ignore] // real Postgres — set WZ_POSTGRES_* and run with `-- --ignored`
    async fn acquires_a_connection_through_the_shared_pool() {
        let pool = postgres_pool().await.unwrap();
        let row = sqlx::query("SELECT 1 AS one")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(row.get::<i32, _>("one"), 1);
    }
}
