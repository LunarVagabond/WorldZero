//! Postgres/Redis connection pool builders, taking the config parsed in
//! [`crate::config`]. Pool construction only — schema/migrations are out of
//! scope here.

use std::time::Duration;

use sqlx::PgPool;
use sqlx::postgres::{PgConnectOptions, PgPoolOptions};

use crate::config::{PostgresConfig, RedisConfig};
use crate::error::{Error, Result};

pub type RedisPool = deadpool_redis::Pool;

#[derive(Debug, Clone)]
pub struct PoolOptions {
    pub max_connections: u32,
    pub acquire_timeout: Duration,
}

impl Default for PoolOptions {
    fn default() -> Self {
        Self {
            max_connections: 10,
            acquire_timeout: Duration::from_secs(5),
        }
    }
}

pub async fn postgres_pool(config: &PostgresConfig, options: PoolOptions) -> Result<PgPool> {
    let connect_options = PgConnectOptions::new()
        .host(&config.host)
        .port(config.port)
        .username(&config.user)
        .password(&config.password)
        .database(&config.database);

    PgPoolOptions::new()
        .max_connections(options.max_connections)
        .acquire_timeout(options.acquire_timeout)
        .connect_with(connect_options)
        .await
        .map_err(|e| Error::wrap("common", "failed to connect to Postgres", e))
}

pub fn redis_pool(config: &RedisConfig, options: PoolOptions) -> Result<RedisPool> {
    // Built from typed connection info, not a `redis://` URL string, so a
    // password containing URL-special characters (`/`, `+`, `@`, ...) can't
    // break parsing.
    let connection_info = deadpool_redis::ConnectionInfo {
        addr: deadpool_redis::ConnectionAddr::Tcp(config.host.clone(), config.port),
        redis: deadpool_redis::RedisConnectionInfo {
            password: config.password.clone(),
            ..Default::default()
        },
    };

    let redis_config = deadpool_redis::Config {
        url: None,
        connection: Some(connection_info),
        pool: Some(deadpool_redis::PoolConfig {
            max_size: options.max_connections as usize,
            timeouts: deadpool_redis::Timeouts {
                wait: Some(options.acquire_timeout),
                create: Some(options.acquire_timeout),
                recycle: Some(options.acquire_timeout),
            },
            ..Default::default()
        }),
    };

    redis_config
        .create_pool(Some(deadpool_redis::Runtime::Tokio1))
        .map_err(|e| Error::wrap("common", "failed to build Redis pool", e))
}

/// Acquires a connection from a Redis pool, surfacing failures (including
/// connection failures — deadpool builds pools lazily, so those show up
/// here rather than in [`redis_pool`]) through the shared error type.
pub async fn redis_connection(pool: &RedisPool) -> Result<deadpool_redis::Connection> {
    pool.get()
        .await
        .map_err(|e| Error::wrap("common", "failed to acquire a Redis connection", e))
}

#[cfg(test)]
mod tests {
    use super::*;

    // Real Postgres/Redis, not run in CI (no network path to the internal
    // Proxmox host from GitHub Actions) — set WZ_POSTGRES_*/WZ_REDIS_* to
    // run these locally: `cargo test -p common -- --ignored`.

    #[tokio::test]
    #[ignore]
    async fn postgres_pool_acquires_a_connection() {
        let config = PostgresConfig::from_env().expect("WZ_POSTGRES_* env vars set");
        let pool = postgres_pool(&config, PoolOptions::default())
            .await
            .unwrap();

        let row: (i32,) = sqlx::query_as("SELECT 1").fetch_one(&pool).await.unwrap();
        assert_eq!(row.0, 1);
    }

    #[tokio::test]
    #[ignore]
    async fn redis_pool_acquires_a_connection() {
        use deadpool_redis::redis::AsyncCommands;

        let config = RedisConfig::from_env().expect("WZ_REDIS_* env vars set");
        let pool = redis_pool(&config, PoolOptions::default()).unwrap();

        let mut conn = redis_connection(&pool).await.unwrap();
        let () = conn.set("wz:pool-smoke-test", "ok").await.unwrap();
        let value: String = conn.get("wz:pool-smoke-test").await.unwrap();
        assert_eq!(value, "ok");
    }

    #[tokio::test]
    async fn postgres_pool_surfaces_connection_failure_as_shared_error() {
        let config = PostgresConfig {
            host: "192.0.2.1".to_string(), // TEST-NET-1, guaranteed unroutable
            port: 5432,
            user: "nobody".to_string(),
            password: "nobody".to_string(),
            database: "nobody".to_string(),
        };
        let options = PoolOptions {
            max_connections: 1,
            acquire_timeout: Duration::from_millis(200),
        };

        let err = postgres_pool(&config, options).await.unwrap_err();
        assert!(err.to_string().starts_with("[common]"), "{err}");
    }

    #[tokio::test]
    async fn redis_connection_surfaces_connection_failure_as_shared_error() {
        let config = RedisConfig {
            host: "192.0.2.1".to_string(), // TEST-NET-1, guaranteed unroutable
            port: 6379,
            password: None,
        };
        let options = PoolOptions {
            max_connections: 1,
            acquire_timeout: Duration::from_millis(200),
        };

        let pool = redis_pool(&config, options).unwrap();
        let Err(err) = redis_connection(&pool).await else {
            panic!("expected connecting to an unroutable host to fail");
        };
        assert!(err.to_string().starts_with("[common]"), "{err}");
    }
}
