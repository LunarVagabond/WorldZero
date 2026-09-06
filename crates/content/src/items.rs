//! The central item catalog (#287, implementing #283's decision) —
//! `item_types` (docs/specs/Data_Model_Spec.md, "The item catalog"): the
//! single source of truth `crafting.schema.yaml`/`equipment.schema.yaml`
//! (`crate character::crafting_schema`/`character::equipment_schema`)
//! and `DropItem` (`server::session`) all validate their own `item_type`
//! references against, replacing the previous status quo of three
//! independent, cross-unvalidated declarations of the same opaque
//! string.
//!
//! Same fixed-core-plus-narrow-JSONB pattern `character::schema`'s
//! declared attribute schema already uses for character stats: a fixed
//! set of real columns (`item_type`, `display_name`, `tags`) plus one
//! narrow `metadata` JSONB column for genuinely per-item-arbitrary data
//! (an icon reference, flavor text) that core code never reads a
//! decision from. `tags` is deliberately an open `TEXT[]`, not a closed
//! enum or a lookup table — core systems check for a handful of
//! well-known tags ([`TAG_CRAFTABLE_OUTPUT`], [`TAG_EQUIPPABLE`],
//! [`TAG_TRADEABLE`], [`TAG_DROP_ONLY`]) but a dev, or a plugin via the
//! `register-item-type` host function, can add arbitrary tags of their
//! own without a core schema change — no tag is privileged, same
//! "no stat is privileged" philosophy stats.schema.yaml already follows.
//!
//! One store, two entry points: `make items` (`src/bin/items.rs`) for
//! dev-authored catalog entries, and `register-item-type`
//! (`plugin-host`'s WIT host function, wired up in
//! `server::plugin_startup`) for plugin-declared ones — both go through
//! [`ItemCatalogStore::register`], so `crafting_schema`/`equipment_schema`
//! validation (and anything else that checks the catalog) can't tell,
//! and doesn't need to care, which path an item type came from.

use std::collections::HashSet;

use common::{Error, Result};
use sqlx::{PgPool, Row};

/// A catalog entry declares it's usable as crafting output
/// (`crafting.schema.yaml`'s recipe `output`) — checked nowhere in core
/// yet (crafting today validates the *existence* of every referenced
/// item_type, not that a recipe's output is tagged this way), named here
/// so a dev/plugin has a real, spelled-consistently tag to reach for
/// rather than inventing their own ad hoc spelling.
pub const TAG_CRAFTABLE_OUTPUT: &str = "craftable_output";
/// An item usable in `equipment.schema.yaml`.
pub const TAG_EQUIPPABLE: &str = "equippable";
/// An item usable in a trade offer (`character::trade`).
pub const TAG_TRADEABLE: &str = "tradeable";
/// An item that only ever appears via `item_drop_sources` — never
/// craftable, never equippable, never a starting-inventory grant.
pub const TAG_DROP_ONLY: &str = "drop_only";

/// One row of the catalog. `metadata` is always a JSON *object*
/// (`{}` if the caller supplied nothing) — never an array or a bare
/// scalar, enforced by [`validate_entry`], same "narrow, structured, not
/// a dumping ground" discipline as every other JSONB column in this
/// codebase.
#[derive(Debug, Clone, PartialEq)]
pub struct ItemCatalogEntry {
    pub item_type: String,
    pub display_name: String,
    pub tags: Vec<String>,
    pub metadata: serde_json::Value,
}

impl ItemCatalogEntry {
    pub fn new(item_type: impl Into<String>, display_name: impl Into<String>) -> Self {
        Self {
            item_type: item_type.into(),
            display_name: display_name.into(),
            tags: Vec::new(),
            metadata: serde_json::Value::Object(serde_json::Map::new()),
        }
    }

    pub fn with_tags(mut self, tags: Vec<String>) -> Self {
        self.tags = tags;
        self
    }

    pub fn with_metadata(mut self, metadata: serde_json::Value) -> Self {
        self.metadata = metadata;
        self
    }

    pub fn has_tag(&self, tag: &str) -> bool {
        self.tags.iter().any(|t| t == tag)
    }
}

/// A named point where `item_type` can drop from — the item <->
/// spawn-table many-to-many `item_drop_sources` exists for. `zone_id`/
/// `spawn_table_id` are plain strings matching a `zone.manifest.yaml`'s
/// `id`/`spawn_tables[].id` — zone manifests are files, not database
/// rows, so there's nothing to foreign-key against, same as
/// `realm_zones.zone_id` (#47).
#[derive(Debug, Clone, PartialEq)]
pub struct DropSource {
    pub item_type: String,
    pub zone_id: String,
    pub spawn_table_id: String,
}

/// Every problem with `entry`, empty if it's valid — collects everything
/// rather than stopping at the first issue, same "report it all at once"
/// discipline `content::manifest::ZoneManifest::validate` already uses.
pub fn validate_entry(entry: &ItemCatalogEntry) -> Vec<String> {
    let mut problems = Vec::new();

    if entry.item_type.trim().is_empty() {
        problems.push("item_type: must not be empty".to_string());
    }
    if entry.display_name.trim().is_empty() {
        problems.push("display_name: must not be empty".to_string());
    }

    let mut seen_tags = HashSet::new();
    for tag in &entry.tags {
        if tag.trim().is_empty() {
            problems.push("tags: must not contain an empty string".to_string());
        } else if !seen_tags.insert(tag.as_str()) {
            problems.push(format!("tags: {tag:?} is declared more than once"));
        }
    }

    if !entry.metadata.is_object() {
        problems.push(format!(
            "metadata: must be a JSON object, got {}",
            metadata_type_name(&entry.metadata)
        ));
    }

    problems
}

fn metadata_type_name(value: &serde_json::Value) -> &'static str {
    match value {
        serde_json::Value::Null => "null",
        serde_json::Value::Bool(_) => "a boolean",
        serde_json::Value::Number(_) => "a number",
        serde_json::Value::String(_) => "a string",
        serde_json::Value::Array(_) => "an array",
        serde_json::Value::Object(_) => "an object",
    }
}

pub struct ItemCatalogStore {
    pool: PgPool,
}

impl ItemCatalogStore {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Validates `entry` (see [`validate_entry`]) and upserts it into
    /// `item_types` — the one write path both `make items` and
    /// `register-item-type` go through, so nothing downstream can tell
    /// them apart. A second call for an already-known `item_type`
    /// overwrites its `display_name`/`tags`/`metadata` — registering is
    /// idempotent-by-design, not append-only, matching `make realm
    /// ensure`'s "safe to re-run" spirit.
    pub async fn register(&self, entry: &ItemCatalogEntry) -> Result<()> {
        let problems = validate_entry(entry);
        if !problems.is_empty() {
            return Err(Error::new(
                "content",
                format!(
                    "item catalog entry {:?} is invalid: {}",
                    entry.item_type,
                    problems.join("; ")
                ),
            ));
        }

        sqlx::query(
            "INSERT INTO item_types (item_type, display_name, tags, metadata) \
             VALUES ($1, $2, $3, $4) \
             ON CONFLICT (item_type) DO UPDATE SET \
             display_name = EXCLUDED.display_name, \
             tags = EXCLUDED.tags, \
             metadata = EXCLUDED.metadata, \
             updated_at = now()",
        )
        .bind(&entry.item_type)
        .bind(&entry.display_name)
        .bind(&entry.tags)
        .bind(&entry.metadata)
        .execute(&self.pool)
        .await
        .map_err(|e| {
            Error::wrap(
                "content",
                format!("failed to register item type {:?}", entry.item_type),
                e,
            )
        })?;

        Ok(())
    }

    pub async fn get(&self, item_type: &str) -> Result<Option<ItemCatalogEntry>> {
        let row = sqlx::query(
            "SELECT item_type, display_name, tags, metadata FROM item_types WHERE item_type = $1",
        )
        .bind(item_type)
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| Error::wrap("content", "failed to read item catalog entry", e))?;

        Ok(row.map(row_to_entry))
    }

    /// Whether `item_type` is a known catalog entry — the actual check
    /// `DropItem`'s handling (`server::session`) makes per-request; see
    /// [`Self::list`] for the "check many at once, e.g. at startup" case.
    pub async fn exists(&self, item_type: &str) -> Result<bool> {
        Ok(self.get(item_type).await?.is_some())
    }

    /// Every catalog entry, ordered by `item_type` — small enough (a
    /// dev/plugin's own declared item set, not a public catalog at
    /// scale) that startup-time cross-validation loads it all once
    /// rather than round-tripping per reference.
    pub async fn list(&self) -> Result<Vec<ItemCatalogEntry>> {
        let rows = sqlx::query(
            "SELECT item_type, display_name, tags, metadata FROM item_types ORDER BY item_type",
        )
        .fetch_all(&self.pool)
        .await
        .map_err(|e| Error::wrap("content", "failed to list item catalog", e))?;

        Ok(rows.into_iter().map(row_to_entry).collect())
    }

    /// Every declared `item_type`, as a set — the shape
    /// `crafting_schema`/`equipment_schema`'s catalog-validation
    /// parameter actually wants (see those modules' `from_yaml`/
    /// `from_file` doc comments).
    pub async fn all_item_types(&self) -> Result<HashSet<String>> {
        Ok(self
            .list()
            .await?
            .into_iter()
            .map(|e| e.item_type)
            .collect())
    }

    pub async fn delete(&self, item_type: &str) -> Result<()> {
        sqlx::query("DELETE FROM item_types WHERE item_type = $1")
            .bind(item_type)
            .execute(&self.pool)
            .await
            .map_err(|e| Error::wrap("content", "failed to delete item catalog entry", e))?;
        Ok(())
    }

    /// Declares `item_type` as droppable from `zone_id`'s `spawn_table_id`
    /// — rejected if `item_type` isn't already a catalog entry (the
    /// table's own `REFERENCES item_types(item_type)` foreign key
    /// enforces this; surfaced here as a clear error rather than a raw
    /// constraint-violation message).
    pub async fn link_drop_source(&self, source: &DropSource) -> Result<()> {
        sqlx::query(
            "INSERT INTO item_drop_sources (item_type, zone_id, spawn_table_id) \
             VALUES ($1, $2, $3) ON CONFLICT DO NOTHING",
        )
        .bind(&source.item_type)
        .bind(&source.zone_id)
        .bind(&source.spawn_table_id)
        .execute(&self.pool)
        .await
        .map_err(|e| {
            if is_foreign_key_violation(&e) {
                Error::new(
                    "content",
                    format!(
                        "cannot link drop source: item_type {:?} is not in the item catalog \
                         (register it first with `make items ARGS=\"create {} <display name>\"`)",
                        source.item_type, source.item_type
                    ),
                )
            } else {
                Error::wrap("content", "failed to link item drop source", e)
            }
        })?;
        Ok(())
    }

    pub async fn unlink_drop_source(&self, source: &DropSource) -> Result<()> {
        sqlx::query(
            "DELETE FROM item_drop_sources WHERE item_type = $1 AND zone_id = $2 AND spawn_table_id = $3",
        )
        .bind(&source.item_type)
        .bind(&source.zone_id)
        .bind(&source.spawn_table_id)
        .execute(&self.pool)
        .await
        .map_err(|e| Error::wrap("content", "failed to unlink item drop source", e))?;
        Ok(())
    }

    /// Every declared drop source for `item_type`, as `(zone_id,
    /// spawn_table_id)` pairs — "where does this item drop from."
    pub async fn drop_sources_for_item(&self, item_type: &str) -> Result<Vec<(String, String)>> {
        let rows = sqlx::query(
            "SELECT zone_id, spawn_table_id FROM item_drop_sources \
             WHERE item_type = $1 ORDER BY zone_id, spawn_table_id",
        )
        .bind(item_type)
        .fetch_all(&self.pool)
        .await
        .map_err(|e| Error::wrap("content", "failed to list item drop sources", e))?;

        Ok(rows
            .into_iter()
            .map(|row| (row.get("zone_id"), row.get("spawn_table_id")))
            .collect())
    }
}

fn row_to_entry(row: sqlx::postgres::PgRow) -> ItemCatalogEntry {
    ItemCatalogEntry {
        item_type: row.get("item_type"),
        display_name: row.get("display_name"),
        tags: row.get("tags"),
        metadata: row.get("metadata"),
    }
}

fn is_foreign_key_violation(err: &sqlx::Error) -> bool {
    matches!(
        err.as_database_error().and_then(|e| e.code()),
        Some(code) if code == "23503"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_valid_entry_has_no_problems() {
        let entry = ItemCatalogEntry::new("wolf-fang", "Wolf Fang")
            .with_tags(vec![TAG_CRAFTABLE_OUTPUT.to_string()]);
        assert!(validate_entry(&entry).is_empty());
    }

    #[test]
    fn an_empty_item_type_is_rejected() {
        let entry = ItemCatalogEntry::new("", "Wolf Fang");
        let problems = validate_entry(&entry);
        assert!(
            problems.iter().any(|p| p.starts_with("item_type")),
            "{problems:?}"
        );
    }

    #[test]
    fn an_empty_display_name_is_rejected() {
        let entry = ItemCatalogEntry::new("wolf-fang", "");
        let problems = validate_entry(&entry);
        assert!(
            problems.iter().any(|p| p.starts_with("display_name")),
            "{problems:?}"
        );
    }

    #[test]
    fn a_duplicate_tag_is_rejected() {
        let entry = ItemCatalogEntry::new("wolf-fang", "Wolf Fang")
            .with_tags(vec!["a".to_string(), "a".to_string()]);
        let problems = validate_entry(&entry);
        assert!(
            problems
                .iter()
                .any(|p| p.contains("declared more than once")),
            "{problems:?}"
        );
    }

    #[test]
    fn an_empty_string_tag_is_rejected() {
        let entry = ItemCatalogEntry::new("wolf-fang", "Wolf Fang").with_tags(vec!["".to_string()]);
        let problems = validate_entry(&entry);
        assert!(
            problems.iter().any(|p| p.contains("empty string")),
            "{problems:?}"
        );
    }

    #[test]
    fn non_object_metadata_is_rejected() {
        let entry =
            ItemCatalogEntry::new("wolf-fang", "Wolf Fang").with_metadata(serde_json::json!(5));
        let problems = validate_entry(&entry);
        assert!(
            problems.iter().any(|p| p.starts_with("metadata")),
            "{problems:?}"
        );
    }

    #[test]
    fn has_tag_checks_membership() {
        let entry = ItemCatalogEntry::new("iron-sword", "Iron Sword")
            .with_tags(vec![TAG_EQUIPPABLE.to_string()]);
        assert!(entry.has_tag(TAG_EQUIPPABLE));
        assert!(!entry.has_tag(TAG_TRADEABLE));
    }
}
