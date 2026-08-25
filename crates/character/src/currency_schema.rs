//! The declared currency schema (`currency.schema.yaml`) loader (#218,
//! implementing #217's decision) — same "dev declares the domain
//! specifics, core enforces generically" pattern `character::PartySchema`/
//! `guild::GuildSchema` already establish: the core has no opinion on how
//! many currencies a game has, what they're called, or whether any of
//! them has a denomination ladder at all — a game developer declares one
//! or more independent currency systems, each optionally with an ordered
//! list of denominations.
//!
//! **Storage stays a single flat integer balance per `(character,
//! currency_key)`, always in that currency's base unit** — a
//! denomination ladder is a pure display/conversion concept, computed
//! fresh on every read via [`CurrencySchema::breakdown`], never persisted
//! as separate per-denomination numbers (#217's decision: this avoids
//! "carry the 1" bookkeeping entirely, since there's only ever one real
//! stored number per currency). A currency with no denominations
//! declared is just a flat balance — today's pre-#218 single-currency
//! behavior, generalized to a named key instead of one hardcoded column.

use std::collections::HashSet;
use std::path::Path;

use common::{Error, Result};
use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
pub struct CurrencyDenomination {
    pub key: String,
    /// The conversion ratio to this currency's base unit — e.g. `silver:
    /// 100` means 100 base units (copper) equal one silver. Must be
    /// positive; a ratio of exactly `1` is the base unit itself (e.g.
    /// `copper: 1` in the worked example in this crate's docs/example).
    pub ratio: i64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Currency {
    pub key: String,
    /// `None`/empty means this currency is a flat balance with no
    /// denomination ladder — [`CurrencySchema::breakdown`] then returns
    /// the raw balance as this currency's own single "denomination."
    #[serde(default)]
    pub denominations: Vec<CurrencyDenomination>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct CurrencySchema {
    pub schema_version: u32,
    pub currencies: Vec<Currency>,
}

impl CurrencySchema {
    pub fn from_yaml(input: &str) -> Result<Self> {
        let schema: Self = serde_yaml::from_str(input)
            .map_err(|e| Error::wrap("character", "failed to parse currency.schema.yaml", e))?;

        if schema.currencies.is_empty() {
            return Err(Error::new(
                "character",
                "currency.schema.yaml must declare at least one currency",
            ));
        }

        let mut seen_currencies = HashSet::new();
        for currency in &schema.currencies {
            if !seen_currencies.insert(currency.key.as_str()) {
                return Err(Error::new(
                    "character",
                    format!(
                        "currency.schema.yaml declares the currency key \"{}\" more than once",
                        currency.key
                    ),
                ));
            }

            let mut seen_denominations = HashSet::new();
            let mut previous_ratio: Option<i64> = None;
            for denomination in &currency.denominations {
                if denomination.ratio <= 0 {
                    return Err(Error::new(
                        "character",
                        format!(
                            "currency {:?}'s denomination \"{}\" has a non-positive ratio ({}); \
                             ratios must be positive",
                            currency.key, denomination.key, denomination.ratio
                        ),
                    ));
                }
                if !seen_denominations.insert(denomination.key.as_str()) {
                    return Err(Error::new(
                        "character",
                        format!(
                            "currency {:?} declares the denomination key \"{}\" more than once",
                            currency.key, denomination.key
                        ),
                    ));
                }
                // Denominations must be declared in ascending ratio
                // order (smallest/base unit first) — chosen over
                // silently sorting them so that `currency.schema.yaml`'s
                // declared order always matches what `breakdown` walks
                // (largest first), and a dev who mis-orders their own
                // ladder gets a loud load-time error instead of a
                // schema that quietly means something other than what
                // they wrote.
                if let Some(previous) = previous_ratio
                    && denomination.ratio <= previous
                {
                    return Err(Error::new(
                        "character",
                        format!(
                            "currency {:?}'s denominations must be declared in strictly \
                             ascending ratio order; \"{}\" (ratio {}) does not come after \
                             a smaller ratio",
                            currency.key, denomination.key, denomination.ratio
                        ),
                    ));
                }
                previous_ratio = Some(denomination.ratio);
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

    /// Reads `currency.schema.yaml` from the dev's config directory
    /// (`common::config::config_dir` — `WZ_CONFIG_DIR` or `./config`).
    pub fn from_config_dir() -> Result<Self> {
        Self::from_file(&common::config::config_dir().join("currency.schema.yaml"))
    }

    pub fn resolve(&self, key: &str) -> Result<&Currency> {
        self.currencies
            .iter()
            .find(|c| c.key == key)
            .ok_or_else(|| Error::new("character", format!("unknown currency: {key}")))
    }

    /// Whether `key` names a declared currency — the check
    /// `character_store.modify_currency`'s callers use before ever
    /// touching storage, so an unknown currency key is rejected the same
    /// "loud, not a silent no-op" way an unknown stat key is.
    pub fn is_declared(&self, key: &str) -> bool {
        self.currencies.iter().any(|c| c.key == key)
    }

    /// Converts `raw_balance` (always in `currency_key`'s base unit, the
    /// one number actually stored) into a denomination breakdown —
    /// largest denomination first, cascading divide-and-remainder, pure
    /// computation with no storage involved. A currency declared with no
    /// denominations returns a single `(currency_key, raw_balance)`
    /// entry — there's nothing to break down.
    ///
    /// Worked example: `copper: 1, silver: 100, gold: 10000` against a
    /// stored balance of `10847` yields `[("gold", 1), ("silver", 8),
    /// ("copper", 47)]` (10847 / 10000 = 1 remainder 847; 847 / 100 = 8
    /// remainder 47; 47 is the remaining copper).
    pub fn breakdown(&self, currency_key: &str, raw_balance: i64) -> Result<Vec<(String, i64)>> {
        let currency = self.resolve(currency_key)?;

        if currency.denominations.is_empty() {
            return Ok(vec![(currency_key.to_string(), raw_balance)]);
        }

        let mut remaining = raw_balance;
        let mut result = Vec::with_capacity(currency.denominations.len());
        for denomination in currency.denominations.iter().rev() {
            let count = remaining / denomination.ratio;
            remaining %= denomination.ratio;
            result.push((denomination.key.clone(), count));
        }
        Ok(result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn schema() -> CurrencySchema {
        CurrencySchema::from_yaml(
            r#"
schema_version: 1
currencies:
  - key: gold
    denominations:
      - key: copper
        ratio: 1
      - key: silver
        ratio: 100
      - key: gold
        ratio: 10000
  - key: honor
"#,
        )
        .unwrap()
    }

    #[test]
    fn resolve_finds_a_declared_currency_by_key() {
        let s = schema();
        assert_eq!(s.resolve("gold").unwrap().key, "gold");
        assert_eq!(s.resolve("honor").unwrap().denominations.len(), 0);
    }

    #[test]
    fn resolve_rejects_an_unknown_key() {
        assert!(schema().resolve("does-not-exist").is_err());
    }

    #[test]
    fn is_declared_reflects_resolve() {
        let s = schema();
        assert!(s.is_declared("gold"));
        assert!(s.is_declared("honor"));
        assert!(!s.is_declared("platinum"));
    }

    #[test]
    fn an_empty_currencies_list_is_rejected() {
        assert!(CurrencySchema::from_yaml("schema_version: 1\ncurrencies: []").is_err());
    }

    #[test]
    fn duplicate_currency_keys_are_rejected() {
        let result = CurrencySchema::from_yaml(
            r#"
schema_version: 1
currencies:
  - key: gold
  - key: gold
"#,
        );
        assert!(result.is_err());
    }

    #[test]
    fn duplicate_denomination_keys_within_a_currency_are_rejected() {
        let result = CurrencySchema::from_yaml(
            r#"
schema_version: 1
currencies:
  - key: gold
    denominations:
      - key: copper
        ratio: 1
      - key: copper
        ratio: 100
"#,
        );
        assert!(result.is_err());
    }

    #[test]
    fn a_non_positive_ratio_is_rejected() {
        for bad_ratio in [0, -1] {
            let result = CurrencySchema::from_yaml(&format!(
                r#"
schema_version: 1
currencies:
  - key: gold
    denominations:
      - key: copper
        ratio: {bad_ratio}
"#
            ));
            assert!(result.is_err(), "ratio {bad_ratio} should be rejected");
        }
    }

    #[test]
    fn denominations_out_of_ascending_order_are_rejected() {
        let result = CurrencySchema::from_yaml(
            r#"
schema_version: 1
currencies:
  - key: gold
    denominations:
      - key: silver
        ratio: 100
      - key: copper
        ratio: 1
"#,
        );
        assert!(result.is_err());
    }

    #[test]
    fn equal_consecutive_ratios_are_rejected() {
        let result = CurrencySchema::from_yaml(
            r#"
schema_version: 1
currencies:
  - key: gold
    denominations:
      - key: copper
        ratio: 1
      - key: penny
        ratio: 1
"#,
        );
        assert!(result.is_err());
    }

    #[test]
    fn breakdown_computes_the_worked_example() {
        let s = schema();
        assert_eq!(
            s.breakdown("gold", 10847).unwrap(),
            vec![
                ("gold".to_string(), 1),
                ("silver".to_string(), 8),
                ("copper".to_string(), 47),
            ]
        );
    }

    #[test]
    fn breakdown_handles_a_zero_balance() {
        let s = schema();
        assert_eq!(
            s.breakdown("gold", 0).unwrap(),
            vec![
                ("gold".to_string(), 0),
                ("silver".to_string(), 0),
                ("copper".to_string(), 0),
            ]
        );
    }

    #[test]
    fn breakdown_of_a_currency_with_no_denominations_is_the_raw_balance() {
        let s = schema();
        assert_eq!(
            s.breakdown("honor", 42).unwrap(),
            vec![("honor".to_string(), 42)]
        );
    }

    #[test]
    fn breakdown_rejects_an_unknown_currency() {
        assert!(schema().breakdown("platinum", 1).is_err());
    }
}
