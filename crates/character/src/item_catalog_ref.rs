//! A plain, `content`-independent view of the central item catalog
//! (#287) — `crafting_schema`/`equipment_schema` validate their
//! `item_type` references against this rather than depending on
//! `content::ItemCatalogStore` directly (`character` sits below
//! `content` in this workspace's dependency graph — only `world`/`server`
//! depend on `content` today — and this ticket isn't the place to change
//! that layering just to share one type).
//!
//! `server::main` builds this from
//! `content::ItemCatalogStore::list`/`all_item_types` before loading
//! either schema; each loader's own unit tests build one by hand (see
//! those modules' `known_item_types()` test helpers).

use std::collections::{HashMap, HashSet};

/// Every `item_type` currently in the catalog, mapped to its declared
/// `tags` — enough for a loader to check both "does this item_type
/// exist" and "does it carry the tag my domain expects"
/// (`crate::crafting_schema`'s `craftable_output` check,
/// `crate::equipment_schema`'s `equippable` check).
pub type KnownItemTypes = HashMap<String, HashSet<String>>;

/// Whether `item_type` is known and its tag set contains `tag`.
pub fn has_tag(known_item_types: &KnownItemTypes, item_type: &str, tag: &str) -> bool {
    known_item_types
        .get(item_type)
        .is_some_and(|tags| tags.contains(tag))
}
