//! The declared attribute schema (`stats.schema.yaml`) loader and the
//! validation rules applied at the API boundary
//! (docs/specs/Data_Model_Spec.md).

use std::path::Path;

use common::{Error, Result};
use serde::Deserialize;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum StatType {
    Int,
}

#[derive(Debug, Clone, Deserialize)]
pub struct StatDeclaration {
    pub key: String,
    #[serde(rename = "type")]
    pub value_type: StatType,
    pub default: i64,
    pub min: Option<i64>,
    pub max: Option<i64>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct AttributeSchema {
    pub schema_version: u32,
    pub stats: Vec<StatDeclaration>,
}

impl AttributeSchema {
    pub fn from_yaml(input: &str) -> Result<Self> {
        serde_yaml::from_str(input)
            .map_err(|e| Error::wrap("character", "failed to parse stats.schema.yaml", e))
    }

    pub fn from_file(path: &Path) -> Result<Self> {
        let contents = std::fs::read_to_string(path).map_err(|e| {
            Error::wrap("character", format!("failed to read {}", path.display()), e)
        })?;
        Self::from_yaml(&contents)
    }

    /// Reads `stats.schema.yaml` from the dev's config directory
    /// (`common::config::config_dir` — `WZ_CONFIG_DIR` or `./config`).
    pub fn from_config_dir() -> Result<Self> {
        Self::from_file(&common::config::config_dir().join("stats.schema.yaml"))
    }

    fn declaration(&self, key: &str) -> Result<&StatDeclaration> {
        self.stats
            .iter()
            .find(|s| s.key == key)
            .ok_or_else(|| Error::new("character", format!("unknown stat key: {key}")))
    }

    /// Whether `key` is a declared stat at all — for a caller (like
    /// `equipment_schema::EquipmentSchema`) that needs to validate a
    /// *delta* against a stat's existence without validating it as an
    /// absolute value (`validate_write`'s bounds check doesn't apply to a
    /// delta, only to a resulting value).
    pub fn declares(&self, key: &str) -> bool {
        self.declaration(key).is_ok()
    }

    /// Validates a write against the declared schema — unknown key or an
    /// out-of-bounds value are both rejected here, before the value ever
    /// reaches storage.
    pub fn validate_write(&self, key: &str, value: i64) -> Result<()> {
        let decl = self.declaration(key)?;

        if let Some(min) = decl.min
            && value < min
        {
            return Err(Error::new(
                "character",
                format!("stat {key} value {value} is below minimum {min}"),
            ));
        }
        if let Some(max) = decl.max
            && value > max
        {
            return Err(Error::new(
                "character",
                format!("stat {key} value {value} is above maximum {max}"),
            ));
        }

        Ok(())
    }

    /// Produces the `stats` blob a character should have after moving to
    /// a realm whose declared schema is `self` (#53's transfer flow),
    /// given `stored` — the character's current stats, possibly declared
    /// under a *different* source realm's schema. Applies
    /// [`Self::resolve_read`]'s exact per-key logic to every key this
    /// schema declares at once: present in `stored` → kept as-is (not
    /// re-validated against this schema's bounds — reading an
    /// already-stored value never re-validates, same as
    /// [`Self::resolve_read`]); missing → this schema's declared
    /// default. Any key `stored` has that this schema doesn't declare is
    /// silently dropped — the destination game never declared it, so
    /// there's nothing sensible to keep it as.
    pub fn migrate_stats(
        &self,
        stored: &serde_json::Map<String, serde_json::Value>,
    ) -> serde_json::Map<String, serde_json::Value> {
        self.stats
            .iter()
            .map(|decl| {
                let value = stored
                    .get(&decl.key)
                    .and_then(serde_json::Value::as_i64)
                    .unwrap_or(decl.default);
                (decl.key.clone(), serde_json::Value::from(value))
            })
            .collect()
    }

    /// Resolves a read: the stored value if present, otherwise the
    /// declared default. Rejects a key the schema doesn't declare at all.
    pub fn resolve_read(
        &self,
        stored: &serde_json::Map<String, serde_json::Value>,
        key: &str,
    ) -> Result<i64> {
        let decl = self.declaration(key)?;

        match stored.get(key).and_then(serde_json::Value::as_i64) {
            Some(value) => Ok(value),
            None => Ok(decl.default),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn example_schema() -> AttributeSchema {
        AttributeSchema::from_yaml(
            r#"
schema_version: 1
stats:
  - key: hp
    type: int
    default: 100
    min: 0
    max: 100
  - key: mana
    type: int
    default: 50
    min: 0
    max: 50
  - key: reputation.ironclad_guild
    type: int
    default: 0
"#,
        )
        .unwrap()
    }

    #[test]
    fn parses_the_proposals_example_format() {
        let schema = example_schema();
        assert_eq!(schema.schema_version, 1);
        assert_eq!(schema.stats.len(), 3);
        assert_eq!(schema.stats[0].key, "hp");
        assert_eq!(schema.stats[0].value_type, StatType::Int);
    }

    #[test]
    fn valid_write_is_accepted() {
        assert!(example_schema().validate_write("hp", 50).is_ok());
    }

    #[test]
    fn write_to_unknown_key_is_rejected() {
        let err = example_schema().validate_write("stamina", 10).unwrap_err();
        assert!(
            err.to_string().contains("unknown stat key: stamina"),
            "{err}"
        );
    }

    #[test]
    fn out_of_bounds_write_is_rejected() {
        let err = example_schema().validate_write("hp", 999).unwrap_err();
        assert!(err.to_string().contains("above maximum"), "{err}");

        let err = example_schema().validate_write("hp", -1).unwrap_err();
        assert!(err.to_string().contains("below minimum"), "{err}");
    }

    #[test]
    fn a_write_exactly_at_the_declared_bounds_is_accepted() {
        // The `<`/`>` (not `<=`/`>=`) boundary in `validate_write` —
        // previously only tested clearly-inside and clearly-outside
        // values, never the exact min/max themselves.
        assert!(example_schema().validate_write("hp", 0).is_ok());
        assert!(example_schema().validate_write("hp", 100).is_ok());
    }

    #[test]
    fn write_with_no_declared_bounds_accepts_any_value() {
        assert!(
            example_schema()
                .validate_write("reputation.ironclad_guild", 1_000_000)
                .is_ok()
        );
    }

    #[test]
    fn missing_key_read_falls_back_to_declared_default() {
        let stored = serde_json::Map::new();
        assert_eq!(example_schema().resolve_read(&stored, "mana").unwrap(), 50);
    }

    #[test]
    fn present_key_read_returns_stored_value() {
        let mut stored = serde_json::Map::new();
        stored.insert("hp".to_string(), serde_json::json!(42));
        assert_eq!(example_schema().resolve_read(&stored, "hp").unwrap(), 42);
    }

    #[test]
    fn read_of_unknown_key_is_rejected() {
        let stored = serde_json::Map::new();
        assert!(example_schema().resolve_read(&stored, "stamina").is_err());
    }

    #[test]
    fn migrate_stats_keeps_keys_the_destination_also_declares() {
        let mut stored = serde_json::Map::new();
        stored.insert("hp".to_string(), serde_json::json!(77));

        let migrated = example_schema().migrate_stats(&stored);
        assert_eq!(migrated.get("hp"), Some(&serde_json::json!(77)));
    }

    #[test]
    fn migrate_stats_fills_missing_keys_with_the_destinations_default() {
        let stored = serde_json::Map::new();
        let migrated = example_schema().migrate_stats(&stored);
        assert_eq!(migrated.get("mana"), Some(&serde_json::json!(50)));
    }

    #[test]
    fn migrate_stats_drops_keys_the_destination_does_not_declare() {
        let mut stored = serde_json::Map::new();
        stored.insert("hp".to_string(), serde_json::json!(10));
        stored.insert("stamina".to_string(), serde_json::json!(999));

        let migrated = example_schema().migrate_stats(&stored);
        assert!(!migrated.contains_key("stamina"));
        assert_eq!(migrated.len(), example_schema().stats.len());
    }
}
