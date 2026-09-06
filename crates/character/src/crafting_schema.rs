//! The declared recipe schema (`crafting.schema.yaml`) loader (#216,
//! implementing #215's decision) — same "dev declares the domain
//! specifics, core enforces generically" pattern `GuildSchema`/
//! `PartySchema` already use: the core has no opinion on what a recipe
//! actually produces or which "profession" (`category`) it belongs to —
//! a game developer declares as many recipes as their game wants, each
//! naming a set of `inputs` (item_type + amount) and a single `output`
//! (item_type + amount). Core owns only the mechanical act of resolving
//! a recipe by key and atomically consuming/granting against it
//! (`crate::crafting::CharacterStore::craft_item`); quality rolls,
//! success chance, and profession/skill gating are all left to the
//! `on-craft-complete` plugin hook (#215's "Alternatives considered").

use std::path::Path;

use common::{Error, Result};
use serde::Deserialize;

use crate::item_catalog_ref::{self, KnownItemTypes};

/// The catalog tag ([`content::items::TAG_CRAFTABLE_OUTPUT`] in the
/// `content` crate — duplicated here as a plain string rather than a
/// shared constant, since `character` doesn't depend on `content`; see
/// `crate::item_catalog_ref`'s own doc comment for why) every recipe's
/// declared `output.item_type` must carry — the "this item participates
/// in crafting" cross-check #287 asks for, on top of plain existence.
const TAG_CRAFTABLE_OUTPUT: &str = "craftable_output";

#[derive(Debug, Clone, Deserialize)]
pub struct CraftingInput {
    pub item_type: String,
    pub amount: i64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct CraftingOutput {
    pub item_type: String,
    pub amount: i64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Recipe {
    pub key: String,
    /// Opaque, dev-owned grouping/display string (e.g. `"blacksmithing"`)
    /// — core stores and reports this but never validates or interprets
    /// it, same discipline `Attack.stat_key`/`item_type` already use
    /// (#215's decision doc).
    pub category: String,
    pub inputs: Vec<CraftingInput>,
    pub output: CraftingOutput,
}

#[derive(Debug, Clone, Deserialize)]
pub struct CraftingSchema {
    pub schema_version: u32,
    pub recipes: Vec<Recipe>,
}

impl CraftingSchema {
    /// Parses and validates `input` — `known_item_types` (#287) is every
    /// `item_type` currently declared in the central item catalog
    /// (`content::ItemCatalogStore::all_item_types`, populated from both
    /// `make items`-authored rows and any plugin's `register-item-type`
    /// call during `on_load`); every `item_type` this schema's recipes
    /// reference (as an input or an output) must be in it, or loading
    /// fails loudly, naming the specific recipe/field/item_type at
    /// fault, same as every other cross-reference this loader already
    /// checks (`recipe.key` uniqueness, positive amounts).
    pub fn from_yaml(input: &str, known_item_types: &KnownItemTypes) -> Result<Self> {
        let schema: Self = serde_yaml::from_str(input)
            .map_err(|e| Error::wrap("character", "failed to parse crafting.schema.yaml", e))?;

        if schema.recipes.is_empty() {
            return Err(Error::new(
                "character",
                "crafting.schema.yaml must declare at least one recipe",
            ));
        }

        let mut seen = std::collections::HashSet::new();
        for recipe in &schema.recipes {
            if !seen.insert(recipe.key.as_str()) {
                return Err(Error::new(
                    "character",
                    format!(
                        "crafting.schema.yaml declares the recipe key \"{}\" more than once",
                        recipe.key
                    ),
                ));
            }

            if recipe.inputs.is_empty() {
                return Err(Error::new(
                    "character",
                    format!(
                        "recipe \"{}\" declares no inputs — a recipe must consume at least one input",
                        recipe.key
                    ),
                ));
            }

            for input in &recipe.inputs {
                if input.amount <= 0 {
                    return Err(Error::new(
                        "character",
                        format!(
                            "recipe \"{}\" declares a non-positive amount ({}) for input {:?}",
                            recipe.key, input.amount, input.item_type
                        ),
                    ));
                }
                if !known_item_types.contains_key(&input.item_type) {
                    return Err(Error::new(
                        "character",
                        format!(
                            "crafting.schema.yaml: recipe \"{}\" input item_type {:?} is not \
                             declared in the item catalog — register it first with \
                             `make items ARGS=\"create {} <display name>\"` (or from a plugin's \
                             on_load via register-item-type)",
                            recipe.key, input.item_type, input.item_type
                        ),
                    ));
                }
            }

            if recipe.output.amount <= 0 {
                return Err(Error::new(
                    "character",
                    format!(
                        "recipe \"{}\" declares a non-positive output amount ({})",
                        recipe.key, recipe.output.amount
                    ),
                ));
            }
            if !known_item_types.contains_key(&recipe.output.item_type) {
                return Err(Error::new(
                    "character",
                    format!(
                        "crafting.schema.yaml: recipe \"{}\" output item_type {:?} is not \
                         declared in the item catalog — register it first with \
                         `make items ARGS=\"create {} <display name>\"` (or from a plugin's \
                         on_load via register-item-type)",
                        recipe.key, recipe.output.item_type, recipe.output.item_type
                    ),
                ));
            }
            // #287 — existence alone isn't enough: the catalog and this
            // schema must agree on what *kind* of thing the output is.
            // An item that exists but was never tagged `craftable_output`
            // (e.g. a drop-only trophy someone typo'd into a recipe) is a
            // load-time error, not a silent pass.
            if !item_catalog_ref::has_tag(
                known_item_types,
                &recipe.output.item_type,
                TAG_CRAFTABLE_OUTPUT,
            ) {
                return Err(Error::new(
                    "character",
                    format!(
                        "crafting.schema.yaml: recipe \"{}\" output item_type {:?} exists in the \
                         item catalog but isn't tagged {TAG_CRAFTABLE_OUTPUT:?} — add that tag \
                         with `make items ARGS=\"create {} <display name> {TAG_CRAFTABLE_OUTPUT}\"` \
                         (or via register-item-type) if this item really is meant to be a \
                         craftable output",
                        recipe.key, recipe.output.item_type, recipe.output.item_type
                    ),
                ));
            }
        }

        Ok(schema)
    }

    pub fn from_file(path: &Path, known_item_types: &KnownItemTypes) -> Result<Self> {
        let contents = std::fs::read_to_string(path).map_err(|e| {
            Error::wrap("character", format!("failed to read {}", path.display()), e)
        })?;
        Self::from_yaml(&contents, known_item_types)
    }

    /// Reads `crafting.schema.yaml` from the dev's config directory
    /// (`common::config::config_dir` — `WZ_CONFIG_DIR` or `./config`).
    pub fn from_config_dir(known_item_types: &KnownItemTypes) -> Result<Self> {
        Self::from_file(
            &common::config::config_dir().join("crafting.schema.yaml"),
            known_item_types,
        )
    }

    pub fn resolve(&self, key: &str) -> Result<&Recipe> {
        self.recipes
            .iter()
            .find(|r| r.key == key)
            .ok_or_else(|| Error::new("character", format!("unknown recipe: {key}")))
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::*;

    /// `wolf-fang`/`iron-ore`/`herb` are plain inputs (no tag required of
    /// an input by this loader); `wolf-fang-dagger`/`healing-tonic`/
    /// `dagger` are also every test recipe's declared *output*, so they
    /// carry `craftable_output` — the tag `from_yaml`'s output check
    /// requires (#287).
    fn known_item_types() -> KnownItemTypes {
        let tagged_output: HashSet<String> = [TAG_CRAFTABLE_OUTPUT]
            .into_iter()
            .map(String::from)
            .collect();
        [
            ("wolf-fang", HashSet::new()),
            ("iron-ore", HashSet::new()),
            ("herb", HashSet::new()),
            ("wolf-fang-dagger", tagged_output.clone()),
            ("healing-tonic", tagged_output.clone()),
            ("dagger", tagged_output),
        ]
        .into_iter()
        .map(|(item_type, tags)| (item_type.to_string(), tags))
        .collect()
    }

    fn schema() -> CraftingSchema {
        CraftingSchema::from_yaml(
            r#"
schema_version: 1
recipes:
  - key: wolf-fang-dagger
    category: blacksmithing
    inputs:
      - item_type: wolf-fang
        amount: 3
      - item_type: iron-ore
        amount: 2
    output:
      item_type: wolf-fang-dagger
      amount: 1
  - key: healing-tonic
    category: alchemy
    inputs:
      - item_type: herb
        amount: 2
    output:
      item_type: healing-tonic
      amount: 1
"#,
            &known_item_types(),
        )
        .unwrap()
    }

    #[test]
    fn resolve_finds_a_declared_recipe_by_key() {
        let s = schema();
        let recipe = s.resolve("wolf-fang-dagger").unwrap();
        assert_eq!(recipe.category, "blacksmithing");
        assert_eq!(recipe.inputs.len(), 2);
        assert_eq!(recipe.output.item_type, "wolf-fang-dagger");
        assert_eq!(recipe.output.amount, 1);
    }

    #[test]
    fn resolve_rejects_an_unknown_key() {
        assert!(schema().resolve("does-not-exist").is_err());
    }

    #[test]
    fn an_empty_recipes_list_is_rejected() {
        assert!(
            CraftingSchema::from_yaml("schema_version: 1\nrecipes: []", &known_item_types())
                .is_err()
        );
    }

    #[test]
    fn duplicate_recipe_keys_are_rejected() {
        let result = CraftingSchema::from_yaml(
            r#"
schema_version: 1
recipes:
  - key: dagger
    category: blacksmithing
    inputs:
      - item_type: iron-ore
        amount: 1
    output:
      item_type: dagger
      amount: 1
  - key: dagger
    category: blacksmithing
    inputs:
      - item_type: iron-ore
        amount: 2
    output:
      item_type: dagger
      amount: 1
"#,
            &known_item_types(),
        );
        assert!(result.is_err());
    }

    #[test]
    fn a_recipe_with_no_inputs_is_rejected() {
        let result = CraftingSchema::from_yaml(
            r#"
schema_version: 1
recipes:
  - key: dagger
    category: blacksmithing
    inputs: []
    output:
      item_type: dagger
      amount: 1
"#,
            &known_item_types(),
        );
        assert!(result.is_err());
    }

    #[test]
    fn a_non_positive_input_amount_is_rejected() {
        let result = CraftingSchema::from_yaml(
            r#"
schema_version: 1
recipes:
  - key: dagger
    category: blacksmithing
    inputs:
      - item_type: iron-ore
        amount: 0
    output:
      item_type: dagger
      amount: 1
"#,
            &known_item_types(),
        );
        assert!(result.is_err());
    }

    #[test]
    fn a_non_positive_output_amount_is_rejected() {
        let result = CraftingSchema::from_yaml(
            r#"
schema_version: 1
recipes:
  - key: dagger
    category: blacksmithing
    inputs:
      - item_type: iron-ore
        amount: 1
    output:
      item_type: dagger
      amount: 0
"#,
            &known_item_types(),
        );
        assert!(result.is_err());
    }

    // #287 — the actual point of this ticket: an item_type a recipe
    // references (as an input or an output) must be a real catalog
    // entry, or loading fails loudly, naming the recipe/field/item_type.
    #[test]
    fn an_unknown_input_item_type_is_rejected() {
        let result = CraftingSchema::from_yaml(
            r#"
schema_version: 1
recipes:
  - key: dagger
    category: blacksmithing
    inputs:
      - item_type: unobtainium
        amount: 1
    output:
      item_type: dagger
      amount: 1
"#,
            &known_item_types(),
        );
        let err = result.unwrap_err();
        let message = err.to_string();
        assert!(message.contains("recipe \"dagger\""), "{message}");
        assert!(message.contains("\"unobtainium\""), "{message}");
        assert!(
            message.contains("not declared in the item catalog"),
            "{message}"
        );
    }

    #[test]
    fn an_unknown_output_item_type_is_rejected() {
        let result = CraftingSchema::from_yaml(
            r#"
schema_version: 1
recipes:
  - key: dagger
    category: blacksmithing
    inputs:
      - item_type: iron-ore
        amount: 1
    output:
      item_type: unobtainium-dagger
      amount: 1
"#,
            &known_item_types(),
        );
        let err = result.unwrap_err();
        let message = err.to_string();
        assert!(message.contains("recipe \"dagger\""), "{message}");
        assert!(message.contains("\"unobtainium-dagger\""), "{message}");
        assert!(
            message.contains("not declared in the item catalog"),
            "{message}"
        );
    }

    #[test]
    fn an_empty_catalog_rejects_every_recipe() {
        let result = CraftingSchema::from_yaml(
            r#"
schema_version: 1
recipes:
  - key: dagger
    category: blacksmithing
    inputs:
      - item_type: iron-ore
        amount: 1
    output:
      item_type: dagger
      amount: 1
"#,
            &KnownItemTypes::new(),
        );
        assert!(result.is_err());
    }

    #[test]
    fn an_output_item_type_that_exists_but_lacks_the_craftable_output_tag_is_rejected() {
        let mut known = known_item_types();
        // Exists, but never tagged as a craftable output — e.g. a
        // `drop_only` trophy someone typo'd into a recipe.
        known.insert("dagger".to_string(), HashSet::new());
        let result = CraftingSchema::from_yaml(
            r#"
schema_version: 1
recipes:
  - key: dagger
    category: blacksmithing
    inputs:
      - item_type: iron-ore
        amount: 1
    output:
      item_type: dagger
      amount: 1
"#,
            &known,
        );
        let err = result.unwrap_err();
        let message = err.to_string();
        assert!(message.contains("\"dagger\""), "{message}");
        assert!(message.contains(TAG_CRAFTABLE_OUTPUT), "{message}");
        assert!(message.contains("isn't tagged"), "{message}");
    }
}
