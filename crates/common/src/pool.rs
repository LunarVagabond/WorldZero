//! Postgres/Redis connection pool builders, taking the config parsed in
//! [`crate::config`]. Pool construction only — see [`crate::migrate`] for
//! running schema migrations against a pool.

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

/// Built from typed connection info, not a `redis://` URL string, so a
/// password containing URL-special characters (`/`, `+`, `@`, ...) can't
/// break parsing. Shared by [`redis_pool`] and [`redis_pubsub_connection`].
fn redis_connection_info(config: &RedisConfig) -> deadpool_redis::ConnectionInfo {
    deadpool_redis::ConnectionInfo {
        addr: deadpool_redis::ConnectionAddr::Tcp(config.host.clone(), config.port),
        redis: deadpool_redis::RedisConnectionInfo {
            password: config.password.clone(),
            ..Default::default()
        },
    }
}

pub fn redis_pool(config: &RedisConfig, options: PoolOptions) -> Result<RedisPool> {
    let connection_info = redis_connection_info(config);

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

/// A dedicated (not pooled) connection suitable for a sustained pub/sub
/// subscription. Pooled connections ([`redis_pool`]/[`redis_connection`])
/// are multiplexed request/response connections meant to be borrowed and
/// returned quickly — a long-lived `SUBSCRIBE` doesn't fit that model, so
/// pub/sub always gets its own connection outside the pool.
pub async fn redis_pubsub_connection(
    config: &RedisConfig,
) -> Result<deadpool_redis::redis::aio::PubSub> {
    let client = deadpool_redis::redis::Client::open(deadpool_redis::redis::ConnectionInfo::from(
        redis_connection_info(config),
    ))
    .map_err(|e| Error::wrap("common", "failed to build Redis client", e))?;

    client
        .get_async_pubsub()
        .await
        .map_err(|e| Error::wrap("common", "failed to open a pub/sub connection", e))
}

#[cfg(test)]
mod tests {
    use super::*;

    // Real Postgres/Redis — set WZ_POSTGRES_*/WZ_REDIS_* and run with
    // `-- --include-ignored` (CI does this against its own Postgres/Redis
    // service containers; run locally with `cargo test -p common --
    // --ignored`). The `from_env()` call in each test is scoped inside its
    // own block, guarded by `test_env_lock::LOCK` and dropped before the
    // first `.await` — see that lock's doc comment for why: `config`'s
    // tests transiently clear/reset these same process-global env vars,
    // and until this lock existed that could race these tests reading
    // them (only visible once both ran in the same process together,
    // which `#[ignore]` had been silently preventing).

    #[tokio::test]
    #[ignore]
    async fn postgres_pool_acquires_a_connection() {
        let config = {
            let _guard = crate::test_env_lock::LOCK.lock().unwrap();
            PostgresConfig::from_env().expect("WZ_POSTGRES_* env vars set")
        };
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

        let config = {
            let _guard = crate::test_env_lock::LOCK.lock().unwrap();
            RedisConfig::from_env().expect("WZ_REDIS_* env vars set")
        };
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
