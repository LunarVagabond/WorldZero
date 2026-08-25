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
    pub fn from_yaml(input: &str) -> Result<Self> {
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
        }

        Ok(schema)
    }

    pub fn from_file(path: &Path) -> Result<Self> {
        let contents = std::fs::read_to_string(path).map_err(|e| {
            Error::wrap("character", format!("failed to read {}", path.display()), e)
        })?;
        Self::from_yaml(&contents)
    }

    /// Reads `crafting.schema.yaml` from the dev's config directory
    /// (`common::config::config_dir` — `WZ_CONFIG_DIR` or `./config`).
    pub fn from_config_dir() -> Result<Self> {
        Self::from_file(&common::config::config_dir().join("crafting.schema.yaml"))
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
    use super::*;

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
        assert!(CraftingSchema::from_yaml("schema_version: 1\nrecipes: []").is_err());
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
        );
        assert!(result.is_err());
    }
}
