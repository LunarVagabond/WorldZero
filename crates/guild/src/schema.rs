//! The declared rank schema (`guild.schema.yaml`) loader (#179) — same
//! "dev declares the domain specifics, core enforces generically"
//! pattern `character::PartySchema` already uses for party types: the
//! core has no opinion on what a rank is called or exactly which
//! actions it grants — a game developer names as many ranks as their
//! game wants and assigns each a set of permissions drawn from a fixed
//! core-defined list.
//!
//! One invariant is enforced by core, not left to the dev: the first
//! declared rank (index 0) is always the guild's founding/leader rank —
//! whoever creates a guild is placed there, and only that rank may
//! disband the guild or move anyone into or out of it. This guarantees
//! every guild always has someone able to manage and dissolve it,
//! regardless of how permissions are otherwise assigned.

use std::path::Path;

use common::{Error, Result};
use serde::Deserialize;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GuildPermission {
    Invite,
    Kick,
    Promote,
    Demote,
    EditMotd,
    EditTag,
    Rename,
}

#[derive(Debug, Clone, Deserialize)]
pub struct GuildRank {
    pub key: String,
    pub name: String,
    #[serde(default)]
    pub permissions: Vec<GuildPermission>,
}

impl GuildRank {
    pub fn has(&self, permission: GuildPermission) -> bool {
        self.permissions.contains(&permission)
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct GuildSchema {
    pub schema_version: u32,
    pub ranks: Vec<GuildRank>,
}

impl GuildSchema {
    pub fn from_yaml(input: &str) -> Result<Self> {
        let schema: Self = serde_yaml::from_str(input)
            .map_err(|e| Error::wrap("guild", "failed to parse guild.schema.yaml", e))?;
        if schema.ranks.is_empty() {
            return Err(Error::new(
                "guild",
                "guild.schema.yaml must declare at least one rank",
            ));
        }
        let mut seen = std::collections::HashSet::new();
        for rank in &schema.ranks {
            if !seen.insert(rank.key.as_str()) {
                return Err(Error::new(
                    "guild",
                    format!(
                        "guild.schema.yaml declares the rank key \"{}\" more than once",
                        rank.key
                    ),
                ));
            }
        }
        Ok(schema)
    }

    pub fn from_file(path: &Path) -> Result<Self> {
        let contents = std::fs::read_to_string(path)
            .map_err(|e| Error::wrap("guild", format!("failed to read {}", path.display()), e))?;
        Self::from_yaml(&contents)
    }

    /// Reads `guild.schema.yaml` from the dev's config directory
    /// (`common::config::config_dir` — `WZ_CONFIG_DIR` or `./config`).
    pub fn from_config_dir() -> Result<Self> {
        Self::from_file(&common::config::config_dir().join("guild.schema.yaml"))
    }

    /// The founding/leader rank every new guild's creator is placed
    /// into — always the first declared entry (see this module's doc
    /// comment for why that's a core invariant, not a dev choice).
    pub fn founder_rank(&self) -> &GuildRank {
        &self.ranks[0]
    }

    /// The rank a fresh invite's accepter joins at — the last declared
    /// entry, i.e. the lowest-authority rank a dev declared.
    pub fn default_member_rank(&self) -> &GuildRank {
        &self.ranks[self.ranks.len() - 1]
    }

    pub fn resolve(&self, key: &str) -> Result<&GuildRank> {
        self.ranks
            .iter()
            .find(|r| r.key == key)
            .ok_or_else(|| Error::new("guild", format!("unknown guild rank: {key}")))
    }

    pub fn is_founder_rank(&self, key: &str) -> bool {
        self.founder_rank().key == key
    }
}

#[cfg(test)]
mod tests {
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

    #[test]
    fn founder_rank_is_the_first_declared_entry() {
        assert_eq!(schema().founder_rank().key, "leader");
    }

    #[test]
    fn default_member_rank_is_the_last_declared_entry() {
        assert_eq!(schema().default_member_rank().key, "member");
    }

    #[test]
    fn resolve_finds_a_declared_rank_by_key() {
        let s = schema();
        assert!(s.resolve("officer").unwrap().has(GuildPermission::Invite));
        assert!(!s.resolve("officer").unwrap().has(GuildPermission::Rename));
    }

    #[test]
    fn resolve_rejects_an_unknown_key() {
        assert!(schema().resolve("does-not-exist").is_err());
    }

    #[test]
    fn a_rank_with_no_permissions_declared_has_none() {
        assert!(schema().resolve("member").unwrap().permissions.is_empty());
    }

    #[test]
    fn is_founder_rank_identifies_only_rank_zero() {
        let s = schema();
        assert!(s.is_founder_rank("leader"));
        assert!(!s.is_founder_rank("officer"));
    }

    #[test]
    fn an_empty_ranks_list_is_rejected() {
        assert!(GuildSchema::from_yaml("schema_version: 1\nranks: []").is_err());
    }

    #[test]
    fn duplicate_rank_keys_are_rejected() {
        let result = GuildSchema::from_yaml(
            r#"
schema_version: 1
ranks:
  - key: leader
    name: A
  - key: leader
    name: B
"#,
        );
        assert!(result.is_err());
    }

    #[test]
    fn an_unknown_permission_string_is_rejected() {
        let result = GuildSchema::from_yaml(
            r#"
schema_version: 1
ranks:
  - key: leader
    name: A
    permissions: [not_a_real_permission]
"#,
        );
        assert!(result.is_err());
    }
}
