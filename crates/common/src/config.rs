//! Env-var-based connection config for Postgres and Redis. Parsing only —
//! building a pool from this is a separate concern (issue #72). No
//! `localhost`/default fallbacks: both stores are always reached over the
//! network, so a missing var fails fast and names itself.

use std::path::PathBuf;

use crate::error::{Error, Result};

/// Where a game developer's own config files (declared attribute schemas,
/// content packs, etc.) live: `WZ_CONFIG_DIR` if set, otherwise `./config`
/// relative to the process's working directory.
///
/// Unlike [`PostgresConfig`]/[`RedisConfig`], this one **does** default —
/// it's a filesystem convention, not a credential, so "the obvious default
/// just works" is the right behavior. Each crate that reads dev-provided
/// files from here defines its own expected filename (e.g. `character`'s
/// `stats.schema.yaml`) so a dev only ever has to know the flat list of
/// filenames this directory expects, never a crate's internal path.
pub fn config_dir() -> PathBuf {
    std::env::var("WZ_CONFIG_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("config"))
}

#[derive(Debug, Clone)]
pub struct PostgresConfig {
    pub host: String,
    pub port: u16,
    pub user: String,
    pub password: String,
    pub database: String,
}

impl PostgresConfig {
    /// Reads `WZ_POSTGRES_{HOST,PORT,USER,PASSWORD,DATABASE}`.
    pub fn from_env() -> Result<Self> {
        Ok(Self {
            host: required_env("WZ_POSTGRES_HOST")?,
            port: required_port_env("WZ_POSTGRES_PORT")?,
            user: required_env("WZ_POSTGRES_USER")?,
            password: required_env("WZ_POSTGRES_PASSWORD")?,
            database: required_env("WZ_POSTGRES_DATABASE")?,
        })
    }
}

#[derive(Debug, Clone)]
pub struct RedisConfig {
    pub host: String,
    pub port: u16,
    pub password: Option<String>,
}

impl RedisConfig {
    /// Reads `WZ_REDIS_HOST`/`WZ_REDIS_PORT` (required) and
    /// `WZ_REDIS_PASSWORD` (optional — not all Redis instances need auth).
    pub fn from_env() -> Result<Self> {
        Ok(Self {
            host: required_env("WZ_REDIS_HOST")?,
            port: required_port_env("WZ_REDIS_PORT")?,
            password: std::env::var("WZ_REDIS_PASSWORD").ok(),
        })
    }
}

fn required_env(var: &'static str) -> Result<String> {
    std::env::var(var).map_err(|_| {
        Error::new(
            "common",
            format!("missing required environment variable: {var}"),
        )
    })
}

fn required_port_env(var: &'static str) -> Result<u16> {
    let raw = required_env(var)?;
    raw.parse().map_err(|_| {
        Error::new(
            "common",
            format!("{var} must be a valid port number, got: {raw}"),
        )
    })
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;

    // std::env is process-global; serialize these tests so they don't race.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    fn with_clean_env(vars: &[&str], f: impl FnOnce()) {
        let _guard = ENV_LOCK.lock().unwrap();
        for var in vars {
            unsafe { std::env::remove_var(var) };
        }
        f();
        for var in vars {
            unsafe { std::env::remove_var(var) };
        }
    }

    const PG_VARS: &[&str] = &[
        "WZ_POSTGRES_HOST",
        "WZ_POSTGRES_PORT",
        "WZ_POSTGRES_USER",
        "WZ_POSTGRES_PASSWORD",
        "WZ_POSTGRES_DATABASE",
    ];

    #[test]
    fn postgres_from_env_reads_all_fields() {
        with_clean_env(PG_VARS, || {
            unsafe {
                std::env::set_var("WZ_POSTGRES_HOST", "db.internal");
                std::env::set_var("WZ_POSTGRES_PORT", "5432");
                std::env::set_var("WZ_POSTGRES_USER", "wz");
                std::env::set_var("WZ_POSTGRES_PASSWORD", "hunter2");
                std::env::set_var("WZ_POSTGRES_DATABASE", "worldzero");
            }

            let config = PostgresConfig::from_env().unwrap();
            assert_eq!(config.host, "db.internal");
            assert_eq!(config.port, 5432);
            assert_eq!(config.user, "wz");
            assert_eq!(config.password, "hunter2");
            assert_eq!(config.database, "worldzero");
        });
    }

    #[test]
    fn postgres_from_env_fails_fast_and_names_missing_var() {
        with_clean_env(PG_VARS, || {
            let err = PostgresConfig::from_env().unwrap_err();
            assert!(err.to_string().contains("WZ_POSTGRES_HOST"), "{err}");
        });
    }

    #[test]
    fn postgres_from_env_rejects_invalid_port() {
        with_clean_env(PG_VARS, || {
            unsafe {
                std::env::set_var("WZ_POSTGRES_HOST", "db.internal");
                std::env::set_var("WZ_POSTGRES_PORT", "not-a-port");
                std::env::set_var("WZ_POSTGRES_USER", "wz");
                std::env::set_var("WZ_POSTGRES_PASSWORD", "hunter2");
                std::env::set_var("WZ_POSTGRES_DATABASE", "worldzero");
            }

            let err = PostgresConfig::from_env().unwrap_err();
            assert!(err.to_string().contains("WZ_POSTGRES_PORT"), "{err}");
        });
    }

    const REDIS_VARS: &[&str] = &["WZ_REDIS_HOST", "WZ_REDIS_PORT", "WZ_REDIS_PASSWORD"];

    #[test]
    fn redis_from_env_allows_missing_password() {
        with_clean_env(REDIS_VARS, || {
            unsafe {
                std::env::set_var("WZ_REDIS_HOST", "cache.internal");
                std::env::set_var("WZ_REDIS_PORT", "6379");
            }

            let config = RedisConfig::from_env().unwrap();
            assert_eq!(config.host, "cache.internal");
            assert_eq!(config.port, 6379);
            assert_eq!(config.password, None);
        });
    }

    #[test]
    fn redis_from_env_fails_fast_without_localhost_fallback() {
        with_clean_env(REDIS_VARS, || {
            let err = RedisConfig::from_env().unwrap_err();
            assert!(err.to_string().contains("WZ_REDIS_HOST"), "{err}");
        });
    }

    #[test]
    fn config_dir_defaults_to_config() {
        with_clean_env(&["WZ_CONFIG_DIR"], || {
            assert_eq!(config_dir(), std::path::PathBuf::from("config"));
        });
    }

    #[test]
    fn config_dir_honors_override() {
        with_clean_env(&["WZ_CONFIG_DIR"], || {
            unsafe { std::env::set_var("WZ_CONFIG_DIR", "/srv/mygame/config") };
            assert_eq!(config_dir(), std::path::PathBuf::from("/srv/mygame/config"));
        });
    }
}
