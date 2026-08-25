//! The declared party-type schema (`party.schema.yaml`) loader (#178) —
//! same "dev declares the domain specifics, core enforces generically"
//! pattern `schema.rs`'s `AttributeSchema` already uses for character
//! stats (docs/specs/Data_Model_Spec.md): the core has no opinion on
//! what a "normal" party is or whether a 3-man "rush" group should
//! exist at all — a game developer names whatever party sizes/shapes
//! their game wants, and `PartyStore` enforces whichever cap the
//! resulting party was actually formed under.

use std::path::Path;

use common::{Error, Result};
use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
pub struct PartyType {
    pub key: String,
    /// `None` means no cap at all — a dev who wants an uncapped raid-
    /// style group just omits this rather than needing a sentinel
    /// "unlimited" magic number.
    pub max_members: Option<u32>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct PartySchema {
    pub schema_version: u32,
    pub party_types: Vec<PartyType>,
}

impl PartySchema {
    pub fn from_yaml(input: &str) -> Result<Self> {
        let schema: Self = serde_yaml::from_str(input)
            .map_err(|e| Error::wrap("character", "failed to parse party.schema.yaml", e))?;
        if schema.party_types.is_empty() {
            return Err(Error::new(
                "character",
                "party.schema.yaml must declare at least one party_types entry",
            ));
        }
        Ok(schema)
    }

    pub fn from_file(path: &Path) -> Result<Self> {
        let contents = std::fs::read_to_string(path).map_err(|e| {
            Error::wrap("character", format!("failed to read {}", path.display()), e)
        })?;
        Self::from_yaml(&contents)
    }

    /// Reads `party.schema.yaml` from the dev's config directory
    /// (`common::config::config_dir` — `WZ_CONFIG_DIR` or `./config`).
    pub fn from_config_dir() -> Result<Self> {
        Self::from_file(&common::config::config_dir().join("party.schema.yaml"))
    }

    /// The party type a fresh `PartyInvite` gets when the client passes
    /// an empty `party_type` string — the first declared entry, same
    /// "declared order is meaningful" convention a dev controls simply
    /// by how they order `party.schema.yaml`.
    pub fn default_type(&self) -> &PartyType {
        &self.party_types[0]
    }

    pub fn resolve(&self, key: &str) -> Result<&PartyType> {
        self.party_types
            .iter()
            .find(|t| t.key == key)
            .ok_or_else(|| Error::new("character", format!("unknown party type: {key}")))
    }
}

#[cfg(test)]
mod tests {
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

    #[test]
    fn default_type_is_the_first_declared_entry() {
        assert_eq!(schema().default_type().key, "normal");
    }

    #[test]
    fn resolve_finds_a_declared_type_by_key() {
        let s = schema();
        assert_eq!(s.resolve("rush").unwrap().max_members, Some(3));
    }

    #[test]
    fn resolve_rejects_an_unknown_key() {
        assert!(schema().resolve("does-not-exist").is_err());
    }

    #[test]
    fn a_type_with_no_max_members_declared_is_uncapped() {
        assert_eq!(schema().resolve("raid").unwrap().max_members, None);
    }

    #[test]
    fn an_empty_party_types_list_is_rejected() {
        assert!(PartySchema::from_yaml("schema_version: 1\nparty_types: []").is_err());
    }
}
