//! The declared character-archetype schema (`character.archetypes.yaml`)
//! loader (#213, implementing #212's decision) — same "dev declares the
//! domain specifics, core enforces generically" pattern
//! `character::PartySchema`/`guild::GuildSchema` already establish: the
//! core has no opinion on what classes/races/archetypes a game has, or
//! even whether it has any at all — a game developer names as many
//! archetypes as their game wants, each with a starting-stat preset
//! drawn from whatever `stats.schema.yaml`/[`crate::schema::AttributeSchema`]
//! already declares.
//!
//! Unlike `PartySchema`/`GuildSchema`, validating an archetype's preset
//! requires a second schema — the declared [`crate::schema::AttributeSchema`]
//! — since a preset is only meaningful in terms of what stats actually
//! exist and what bounds they're declared with. That validation happens
//! once, at load time (not deferred to whenever a character is actually
//! created under a given archetype), so a misconfigured preset fails
//! loudly at startup the same way every other declared schema does,
//! rather than surfacing as a mysterious rejected `CreateCharacter` much
//! later.

use std::collections::{HashMap, HashSet};
use std::path::Path;

use common::{Error, Result};
use serde::Deserialize;

use crate::schema::AttributeSchema;

#[derive(Debug, Clone, Deserialize)]
pub struct CharacterArchetype {
    pub key: String,
    pub name: String,
    pub description: String,
    /// Which of `stats.schema.yaml`'s declared stats this archetype sets
    /// away from their bare default, and to what. A stat this archetype
    /// doesn't mention here simply keeps its declared default — an
    /// archetype preset is a set of deltas from the schema's baseline,
    /// not a full restatement of every stat.
    #[serde(default)]
    pub stats: HashMap<String, i64>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ArchetypeSchema {
    pub schema_version: u32,
    pub archetypes: Vec<CharacterArchetype>,
}

impl ArchetypeSchema {
    /// Parses and validates `input` against `attribute_schema` — every
    /// preset value in every declared archetype must be a known stat key
    /// within that stat's declared bounds ([`AttributeSchema::validate_write`]),
    /// not just structurally well-formed YAML.
    pub fn from_yaml(input: &str, attribute_schema: &AttributeSchema) -> Result<Self> {
        let schema: Self = serde_yaml::from_str(input).map_err(|e| {
            Error::wrap("character", "failed to parse character.archetypes.yaml", e)
        })?;

        if schema.archetypes.is_empty() {
            return Err(Error::new(
                "character",
                "character.archetypes.yaml must declare at least one archetype",
            ));
        }

        let mut seen = HashSet::new();
        for archetype in &schema.archetypes {
            if !seen.insert(archetype.key.as_str()) {
                return Err(Error::new(
                    "character",
                    format!(
                        "character.archetypes.yaml declares the archetype key \"{}\" more than once",
                        archetype.key
                    ),
                ));
            }
            for (stat_key, value) in &archetype.stats {
                attribute_schema
                    .validate_write(stat_key, *value)
                    .map_err(|e| {
                        Error::new(
                            "character",
                            format!(
                                "archetype \"{}\" preset for stat \"{stat_key}\" is invalid: {e}",
                                archetype.key
                            ),
                        )
                    })?;
            }
        }

        Ok(schema)
    }

    pub fn from_file(path: &Path, attribute_schema: &AttributeSchema) -> Result<Self> {
        let contents = std::fs::read_to_string(path).map_err(|e| {
            Error::wrap("character", format!("failed to read {}", path.display()), e)
        })?;
        Self::from_yaml(&contents, attribute_schema)
    }

    /// Reads `character.archetypes.yaml` from the dev's config directory
    /// (`common::config::config_dir` — `WZ_CONFIG_DIR` or `./config`).
    pub fn from_config_dir(attribute_schema: &AttributeSchema) -> Result<Self> {
        Self::from_file(
            &common::config::config_dir().join("character.archetypes.yaml"),
            attribute_schema,
        )
    }

    /// The archetype a fresh `CreateCharacter` gets when the client
    /// passes an empty `archetype_key` — the first declared entry, same
    /// "declared order is meaningful" convention `PartySchema::default_type`/
    /// `GuildSchema::founder_rank` already use.
    pub fn default_archetype(&self) -> &CharacterArchetype {
        &self.archetypes[0]
    }

    pub fn resolve(&self, key: &str) -> Result<&CharacterArchetype> {
        self.archetypes
            .iter()
            .find(|a| a.key == key)
            .ok_or_else(|| Error::new("character", format!("unknown archetype: {key}")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn attribute_schema() -> AttributeSchema {
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

    fn schema() -> ArchetypeSchema {
        ArchetypeSchema::from_yaml(
            r#"
schema_version: 1
archetypes:
  - key: warrior
    name: Warrior
    description: A frontline fighter.
    stats:
      hp: 100
      mana: 10
  - key: mage
    name: Mage
    description: A spellcaster.
    stats:
      hp: 50
      mana: 50
  - key: rogue
    name: Rogue
    description: A sneaky striker.
"#,
            &attribute_schema(),
        )
        .unwrap()
    }

    #[test]
    fn default_archetype_is_the_first_declared_entry() {
        assert_eq!(schema().default_archetype().key, "warrior");
    }

    #[test]
    fn resolve_finds_a_declared_archetype_by_key() {
        let s = schema();
        assert_eq!(s.resolve("mage").unwrap().stats.get("mana"), Some(&50));
    }

    #[test]
    fn resolve_rejects_an_unknown_key() {
        assert!(schema().resolve("does-not-exist").is_err());
    }

    #[test]
    fn an_archetype_with_no_stats_declared_has_none() {
        assert!(schema().resolve("rogue").unwrap().stats.is_empty());
    }

    #[test]
    fn an_empty_archetypes_list_is_rejected() {
        assert!(
            ArchetypeSchema::from_yaml("schema_version: 1\narchetypes: []", &attribute_schema())
                .is_err()
        );
    }

    #[test]
    fn duplicate_archetype_keys_are_rejected() {
        let result = ArchetypeSchema::from_yaml(
            r#"
schema_version: 1
archetypes:
  - key: warrior
    name: A
    description: A
  - key: warrior
    name: B
    description: B
"#,
            &attribute_schema(),
        );
        assert!(result.is_err());
    }

    #[test]
    fn an_out_of_bounds_preset_value_is_rejected() {
        let result = ArchetypeSchema::from_yaml(
            r#"
schema_version: 1
archetypes:
  - key: warrior
    name: Warrior
    description: A frontline fighter.
    stats:
      hp: 999
"#,
            &attribute_schema(),
        );
        let err = result.unwrap_err();
        assert!(err.to_string().contains("above maximum"), "{err}");
    }

    #[test]
    fn a_preset_referencing_an_unknown_stat_key_is_rejected() {
        let result = ArchetypeSchema::from_yaml(
            r#"
schema_version: 1
archetypes:
  - key: warrior
    name: Warrior
    description: A frontline fighter.
    stats:
      stamina: 10
"#,
            &attribute_schema(),
        );
        assert!(result.is_err());
    }
}
