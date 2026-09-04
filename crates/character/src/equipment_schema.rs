//! The declared equipment schema (`equipment.schema.yaml`) loader (#277,
//! split out of #245 with the design decision recorded on that now-closed
//! issue) — same "dev declares the domain specifics, core enforces
//! generically" pattern `ArchetypeSchema`/`CraftingSchema` already use.
//! The core has no opinion on what equipment slots exist or which items
//! are equippable — a game developer declares a flat list of `slots` and,
//! for each equippable `item_type`, which single slot it occupies and a
//! `stat_deltas` map applied while worn. An `item_type` not listed under
//! `items` can't be equipped at all.
//!
//! Like `ArchetypeSchema`'s presets, `stat_deltas` are validated at load
//! time against the declared [`crate::schema::AttributeSchema`] — but
//! only for *existence* ([`crate::schema::AttributeSchema::declares`]),
//! not bounds: a delta isn't a resulting value, so `validate_write`'s
//! min/max check doesn't apply here — that's enforced for real when the
//! delta is actually applied (`crate::equipment::CharacterStore::equip_item`).

use std::collections::{HashMap, HashSet};
use std::path::Path;

use common::{Error, Result};
use serde::Deserialize;

use crate::schema::AttributeSchema;

#[derive(Debug, Clone, Deserialize)]
pub struct EquipmentItem {
    pub item_type: String,
    pub slot: String,
    #[serde(default)]
    pub stat_deltas: HashMap<String, i64>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct EquipmentSchema {
    pub schema_version: u32,
    pub slots: Vec<String>,
    #[serde(default)]
    pub items: Vec<EquipmentItem>,
}

impl EquipmentSchema {
    /// Parses and validates `input` against `attribute_schema` — every
    /// declared item's `slot` must be one of `slots`, every `stat_deltas`
    /// key must be a real declared stat, and `item_type` is unique across
    /// `items` (one item_type maps to exactly one slot).
    pub fn from_yaml(input: &str, attribute_schema: &AttributeSchema) -> Result<Self> {
        let schema: Self = serde_yaml::from_str(input)
            .map_err(|e| Error::wrap("character", "failed to parse equipment.schema.yaml", e))?;

        if schema.slots.is_empty() {
            return Err(Error::new(
                "character",
                "equipment.schema.yaml must declare at least one slot",
            ));
        }

        let mut seen_slots = HashSet::new();
        for slot in &schema.slots {
            if !seen_slots.insert(slot.as_str()) {
                return Err(Error::new(
                    "character",
                    format!("equipment.schema.yaml declares the slot \"{slot}\" more than once"),
                ));
            }
        }

        let mut seen_items = HashSet::new();
        for item in &schema.items {
            if !seen_items.insert(item.item_type.as_str()) {
                return Err(Error::new(
                    "character",
                    format!(
                        "equipment.schema.yaml declares item_type \"{}\" more than once",
                        item.item_type
                    ),
                ));
            }

            if !seen_slots.contains(item.slot.as_str()) {
                return Err(Error::new(
                    "character",
                    format!(
                        "item \"{}\" declares slot \"{}\", which isn't in the declared slots list",
                        item.item_type, item.slot
                    ),
                ));
            }

            for stat_key in item.stat_deltas.keys() {
                if !attribute_schema.declares(stat_key) {
                    return Err(Error::new(
                        "character",
                        format!(
                            "item \"{}\" declares a stat_deltas entry for unknown stat \"{stat_key}\"",
                            item.item_type
                        ),
                    ));
                }
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

    /// Reads `equipment.schema.yaml` from the dev's config directory
    /// (`common::config::config_dir` — `WZ_CONFIG_DIR` or `./config`).
    pub fn from_config_dir(attribute_schema: &AttributeSchema) -> Result<Self> {
        Self::from_file(
            &common::config::config_dir().join("equipment.schema.yaml"),
            attribute_schema,
        )
    }

    /// `Err` if `item_type` isn't declared as equippable at all.
    pub fn resolve(&self, item_type: &str) -> Result<&EquipmentItem> {
        self.items
            .iter()
            .find(|i| i.item_type == item_type)
            .ok_or_else(|| Error::new("character", format!("item is not equippable: {item_type}")))
    }

    pub fn declares_slot(&self, slot: &str) -> bool {
        self.slots.iter().any(|s| s == slot)
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
  - key: attack
    type: int
    default: 10
    min: 0
    max: 1000
  - key: defense
    type: int
    default: 5
    min: 0
    max: 1000
"#,
        )
        .unwrap()
    }

    fn schema() -> EquipmentSchema {
        EquipmentSchema::from_yaml(
            r#"
schema_version: 1
slots:
  - head
  - weapon
items:
  - item_type: iron-helmet
    slot: head
    stat_deltas:
      defense: 5
  - item_type: iron-sword
    slot: weapon
    stat_deltas:
      attack: 10
  - item_type: cloth-cap
    slot: head
"#,
            &attribute_schema(),
        )
        .unwrap()
    }

    #[test]
    fn resolve_finds_a_declared_item_by_type() {
        let s = schema();
        let item = s.resolve("iron-sword").unwrap();
        assert_eq!(item.slot, "weapon");
        assert_eq!(item.stat_deltas.get("attack"), Some(&10));
    }

    #[test]
    fn resolve_rejects_an_item_not_declared_as_equippable() {
        let err = schema().resolve("torch").unwrap_err();
        assert!(err.to_string().contains("not equippable"), "{err}");
    }

    #[test]
    fn an_item_with_no_stat_deltas_is_valid() {
        assert!(
            schema()
                .resolve("cloth-cap")
                .unwrap()
                .stat_deltas
                .is_empty()
        );
    }

    #[test]
    fn an_empty_slots_list_is_rejected() {
        assert!(
            EquipmentSchema::from_yaml(
                "schema_version: 1\nslots: []\nitems: []",
                &attribute_schema()
            )
            .is_err()
        );
    }

    #[test]
    fn duplicate_slot_names_are_rejected() {
        let result = EquipmentSchema::from_yaml(
            "schema_version: 1\nslots: [head, head]\nitems: []",
            &attribute_schema(),
        );
        assert!(result.is_err());
    }

    #[test]
    fn duplicate_item_types_are_rejected() {
        let result = EquipmentSchema::from_yaml(
            r#"
schema_version: 1
slots: [head]
items:
  - item_type: iron-helmet
    slot: head
  - item_type: iron-helmet
    slot: head
"#,
            &attribute_schema(),
        );
        assert!(result.is_err());
    }

    #[test]
    fn an_item_naming_an_undeclared_slot_is_rejected() {
        let result = EquipmentSchema::from_yaml(
            r#"
schema_version: 1
slots: [head]
items:
  - item_type: iron-sword
    slot: weapon
"#,
            &attribute_schema(),
        );
        let err = result.unwrap_err();
        assert!(
            err.to_string().contains("isn't in the declared slots"),
            "{err}"
        );
    }

    #[test]
    fn an_item_naming_an_unknown_stat_in_stat_deltas_is_rejected() {
        let result = EquipmentSchema::from_yaml(
            r#"
schema_version: 1
slots: [head]
items:
  - item_type: iron-helmet
    slot: head
    stat_deltas:
      stamina: 5
"#,
            &attribute_schema(),
        );
        let err = result.unwrap_err();
        assert!(err.to_string().contains("unknown stat"), "{err}");
    }
}
