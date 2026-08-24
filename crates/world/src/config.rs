//! Runtime-configurable knobs for `world`'s simulation. Every value has a
//! sane, documented default — a self-hoster gets a working zone-service
//! with zero config (docs/PROPOSAL.md, "The Developer Experience Bar"),
//! and can override any of them via env vars without a code change.

use std::time::Duration;

use common::{Error, Result};

/// 20 Hz: a common real-time-game tick rate (WoW-style tab-target combat
/// runs comfortably below this; it leaves headroom for the fixed-tick
/// baseline before anything genuinely twitch-reflex-timing-sensitive
/// would need a higher rate). Chosen deliberately — see #31's acceptance
/// criteria ("pick a rate, document why") — not left as an implicit
/// implementation detail. Revisit if a specific game built on the
/// framework demonstrates it needs faster.
const DEFAULT_TICK_RATE_HZ: u32 = 20;

/// Chosen as "big enough that a typical zone's active entity cluster
/// spans a handful of cells, not hundreds" — a starting point tuned
/// against nothing more than intuition; a self-hoster with an unusual
/// entity density is expected to override this, not treat it as load-bearing.
const DEFAULT_GRID_CELL_SIZE_METERS: f64 = 25.0;

/// A generous walking/light-jog speed cap for the Phase 1 baseline — high
/// enough that no reasonable player movement gets falsely rejected, low
/// enough that a spoofed "teleport" update is still caught. Game-specific
/// movement speeds (mounts, sprint, dashes) are expected to override this.
const DEFAULT_MAX_SPEED_METERS_PER_SECOND: f64 = 10.0;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct WorldConfig {
    pub tick_rate_hz: u32,
    pub grid_cell_size_meters: f64,
    pub max_speed_meters_per_second: f64,
}

impl Default for WorldConfig {
    fn default() -> Self {
        Self {
            tick_rate_hz: DEFAULT_TICK_RATE_HZ,
            grid_cell_size_meters: DEFAULT_GRID_CELL_SIZE_METERS,
            max_speed_meters_per_second: DEFAULT_MAX_SPEED_METERS_PER_SECOND,
        }
    }
}

impl WorldConfig {
    pub fn tick_interval(&self) -> Duration {
        Duration::from_secs_f64(1.0 / self.tick_rate_hz as f64)
    }

    /// Reads `WZ_WORLD_TICK_RATE_HZ` / `WZ_WORLD_GRID_CELL_SIZE_METERS` /
    /// `WZ_WORLD_MAX_SPEED_MPS`, all optional — an unset var keeps its
    /// default, but a *set-and-unparsable* one is a config error, not a
    /// silent fallback (same convention as `common::config`'s required
    /// vars: a typo should fail loudly, not quietly do the wrong thing).
    pub fn from_env() -> Result<Self> {
        let mut config = Self::default();

        if let Some(value) = optional_env("WZ_WORLD_TICK_RATE_HZ")? {
            config.tick_rate_hz = value
                .parse()
                .map_err(|_| invalid("WZ_WORLD_TICK_RATE_HZ", &value))?;
        }
        if let Some(value) = optional_env("WZ_WORLD_GRID_CELL_SIZE_METERS")? {
            config.grid_cell_size_meters = value
                .parse()
                .map_err(|_| invalid("WZ_WORLD_GRID_CELL_SIZE_METERS", &value))?;
        }
        if let Some(value) = optional_env("WZ_WORLD_MAX_SPEED_MPS")? {
            config.max_speed_meters_per_second = value
                .parse()
                .map_err(|_| invalid("WZ_WORLD_MAX_SPEED_MPS", &value))?;
        }

        if config.tick_rate_hz == 0 {
            return Err(Error::new(
                "world",
                "WZ_WORLD_TICK_RATE_HZ must be greater than 0",
            ));
        }
        if config.grid_cell_size_meters <= 0.0 {
            return Err(Error::new(
                "world",
                "WZ_WORLD_GRID_CELL_SIZE_METERS must be greater than 0",
            ));
        }
        if config.max_speed_meters_per_second <= 0.0 {
            return Err(Error::new(
                "world",
                "WZ_WORLD_MAX_SPEED_MPS must be greater than 0",
            ));
        }

        Ok(config)
    }
}

fn optional_env(var: &str) -> Result<Option<String>> {
    match std::env::var(var) {
        Ok(value) => Ok(Some(value)),
        Err(std::env::VarError::NotPresent) => Ok(None),
        Err(std::env::VarError::NotUnicode(_)) => {
            Err(Error::new("world", format!("{var} is not valid UTF-8")))
        }
    }
}

fn invalid(var: &str, value: &str) -> Error {
    Error::new("world", format!("{var} is not a valid number: {value:?}"))
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;

    static ENV_LOCK: Mutex<()> = Mutex::new(());
    const VARS: &[&str] = &[
        "WZ_WORLD_TICK_RATE_HZ",
        "WZ_WORLD_GRID_CELL_SIZE_METERS",
        "WZ_WORLD_MAX_SPEED_MPS",
    ];

    fn with_clean_env(f: impl FnOnce()) {
        let _guard = ENV_LOCK.lock().unwrap();
        for var in VARS {
            unsafe { std::env::remove_var(var) };
        }
        f();
    }

    #[test]
    fn defaults_apply_with_nothing_set() {
        with_clean_env(|| {
            assert_eq!(WorldConfig::from_env().unwrap(), WorldConfig::default());
        });
    }

    #[test]
    fn tick_interval_matches_the_configured_rate() {
        let config = WorldConfig {
            tick_rate_hz: 20,
            ..Default::default()
        };
        assert_eq!(config.tick_interval(), Duration::from_millis(50));
    }

    #[test]
    fn a_set_but_unparsable_var_is_a_loud_error_not_a_silent_default() {
        with_clean_env(|| {
            unsafe { std::env::set_var("WZ_WORLD_TICK_RATE_HZ", "banana") };
            let err = WorldConfig::from_env().unwrap_err();
            assert!(err.to_string().contains("WZ_WORLD_TICK_RATE_HZ"), "{err}");
            unsafe { std::env::remove_var("WZ_WORLD_TICK_RATE_HZ") };
        });
    }

    #[test]
    fn an_overridden_value_is_used_instead_of_the_default() {
        with_clean_env(|| {
            unsafe { std::env::set_var("WZ_WORLD_GRID_CELL_SIZE_METERS", "50") };
            let config = WorldConfig::from_env().unwrap();
            assert_eq!(config.grid_cell_size_meters, 50.0);
            unsafe { std::env::remove_var("WZ_WORLD_GRID_CELL_SIZE_METERS") };
        });
    }

    // A zero/negative value parses fine as a number (0 is a valid u32,
    // -1.0 a valid f64) — these three checks are what actually stop it
    // from reaching `GridIndex::new` (which asserts and panics on a
    // non-positive cell size) or `tick_interval` (which would divide by
    // zero). Previously untested — the exact scenario each check exists
    // to catch had never been exercised.

    #[test]
    fn a_zero_tick_rate_is_rejected() {
        with_clean_env(|| {
            unsafe { std::env::set_var("WZ_WORLD_TICK_RATE_HZ", "0") };
            let err = WorldConfig::from_env().unwrap_err();
            assert!(err.to_string().contains("WZ_WORLD_TICK_RATE_HZ"), "{err}");
            unsafe { std::env::remove_var("WZ_WORLD_TICK_RATE_HZ") };
        });
    }

    #[test]
    fn a_non_positive_grid_cell_size_is_rejected() {
        with_clean_env(|| {
            for bad_value in ["0", "-5"] {
                unsafe { std::env::set_var("WZ_WORLD_GRID_CELL_SIZE_METERS", bad_value) };
                let err = WorldConfig::from_env().unwrap_err();
                assert!(
                    err.to_string().contains("WZ_WORLD_GRID_CELL_SIZE_METERS"),
                    "{err}"
                );
            }
            unsafe { std::env::remove_var("WZ_WORLD_GRID_CELL_SIZE_METERS") };
        });
    }

    #[test]
    fn a_non_positive_max_speed_is_rejected() {
        with_clean_env(|| {
            for bad_value in ["0", "-1"] {
                unsafe { std::env::set_var("WZ_WORLD_MAX_SPEED_MPS", bad_value) };
                let err = WorldConfig::from_env().unwrap_err();
                assert!(err.to_string().contains("WZ_WORLD_MAX_SPEED_MPS"), "{err}");
            }
            unsafe { std::env::remove_var("WZ_WORLD_MAX_SPEED_MPS") };
        });
    }
}
