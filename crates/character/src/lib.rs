//! Character records, inventory, and declared-schema stats.
//!
//! No concrete logic yet beyond store bootstrap config. Design:
//! docs/PROPOSAL.md ("Data Model Extensibility: Declared Attribute Schemas")
//! and docs/specs/Data_Model_Spec.md.

pub fn postgres_config() -> common::Result<common::config::PostgresConfig> {
    common::config::PostgresConfig::from_env()
}
