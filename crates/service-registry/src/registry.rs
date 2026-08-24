//! Redis-backed instance registry.
//!
//! Each instance registers itself under `registry:<service_name>:<instance_id>`
//! as a JSON value with a TTL; [`ServiceRegistry::heartbeat`] refreshes that
//! TTL, and letting it lapse *is* failure detection — there is no separate
//! liveness check to run. [`ServiceRegistry::query`] lists live instances
//! with `KEYS` + `MGET` over that prefix, which is fine at the instance
//! counts this is meant for (one registry entry per running service
//! instance, not per request); switch to `SCAN` if that stops being true.
//!
//! Register/deregister are also published on `registry:events:<service_name>`
//! for subscribers who want push updates instead of polling `query` — but
//! TTL *expiry* is not published today, since that needs Redis keyspace
//! notifications enabled server-side (`notify-keyspace-events Ex`), which
//! isn't assumed here. A subscriber that needs to know about silent expiry
//! still has to poll `query`; that gap is deliberate, not an oversight, and
//! can be closed later if it turns out to matter in practice.

use common::config::RedisConfig;
use common::pool::RedisPool;
use common::{Error, Result};
use deadpool_redis::redis::AsyncCommands;
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use uuid::Uuid;

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct InstanceMetadata {
    /// Caller-defined load signal (e.g. connected-player count, CPU
    /// fraction) — unitless here on purpose, since what "load" means is
    /// specific to each service, not something this crate should dictate.
    pub load: Option<f64>,
    pub capacity: Option<u32>,
    /// Which realm this instance is currently serving, if any — set by
    /// realm/zone-service instances once `realm-directory` (#47) is
    /// actually wired into instance placement.
    pub realm_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct InstanceInfo {
    pub service_name: String,
    pub instance_id: Uuid,
    pub address: String,
    pub metadata: InstanceMetadata,
    #[serde(with = "time::serde::rfc3339")]
    pub registered_at: OffsetDateTime,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum RegistryEventKind {
    Registered,
    Deregistered,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RegistryEvent {
    pub kind: RegistryEventKind,
    pub service_name: String,
    pub instance_id: Uuid,
}

fn instance_key(service_name: &str, instance_id: Uuid) -> String {
    format!("registry:{service_name}:{instance_id}")
}

fn key_pattern(service_name: &str) -> String {
    format!("registry:{service_name}:*")
}

fn events_topic(service_name: &str) -> String {
    format!("registry:events:{service_name}")
}

pub struct ServiceRegistry {
    redis: RedisPool,
    redis_config: RedisConfig,
    ttl_seconds: u64,
}

impl ServiceRegistry {
    /// `ttl_seconds` should be a small multiple of however often callers
    /// intend to call [`Self::heartbeat`] — long enough that a normal
    /// scheduling delay doesn't cause a spurious expiry, short enough that
    /// a crashed instance disappears from [`Self::query`] promptly.
    pub fn new(redis: RedisPool, redis_config: RedisConfig, ttl_seconds: u64) -> Self {
        Self {
            redis,
            redis_config,
            ttl_seconds,
        }
    }

    pub async fn register(
        &self,
        service_name: &str,
        instance_id: Uuid,
        address: &str,
        metadata: InstanceMetadata,
    ) -> Result<()> {
        let info = InstanceInfo {
            service_name: service_name.to_string(),
            instance_id,
            address: address.to_string(),
            metadata,
            registered_at: OffsetDateTime::now_utc(),
        };
        let payload = serde_json::to_string(&info)
            .map_err(|e| Error::wrap("service-registry", "failed to encode instance info", e))?;

        let mut conn = common::pool::redis_connection(&self.redis).await?;
        conn.set_ex::<_, _, ()>(
            instance_key(service_name, instance_id),
            payload,
            self.ttl_seconds,
        )
        .await
        .map_err(|e| Error::wrap("service-registry", "failed to register instance", e))?;

        self.publish_event(
            &mut conn,
            service_name,
            instance_id,
            RegistryEventKind::Registered,
        )
        .await
    }

    /// Errs if `service_name`/`instance_id` isn't currently registered —
    /// either it was never registered, or its TTL already lapsed. Callers
    /// that hit this after a slow heartbeat loop should re-[`Self::register`]
    /// rather than treat it as transient.
    pub async fn heartbeat(&self, service_name: &str, instance_id: Uuid) -> Result<()> {
        let mut conn = common::pool::redis_connection(&self.redis).await?;
        let refreshed: bool = conn
            .expire(
                instance_key(service_name, instance_id),
                self.ttl_seconds as i64,
            )
            .await
            .map_err(|e| Error::wrap("service-registry", "failed to refresh instance TTL", e))?;

        if !refreshed {
            return Err(Error::new(
                "service-registry",
                "heartbeat on an instance that isn't registered (expired or never registered)",
            ));
        }
        Ok(())
    }

    /// Removes the registration immediately rather than waiting out the
    /// TTL — the clean-shutdown path, as opposed to letting a crash expire
    /// naturally.
    pub async fn deregister(&self, service_name: &str, instance_id: Uuid) -> Result<()> {
        let mut conn = common::pool::redis_connection(&self.redis).await?;
        conn.del::<_, ()>(instance_key(service_name, instance_id))
            .await
            .map_err(|e| Error::wrap("service-registry", "failed to deregister instance", e))?;

        self.publish_event(
            &mut conn,
            service_name,
            instance_id,
            RegistryEventKind::Deregistered,
        )
        .await
    }

    /// Live instances of `service_name` right now. An instance whose TTL
    /// lapses between the key scan and the value fetch is simply omitted,
    /// not reported as an error — that race is inherent to any TTL-based
    /// registry and callers should treat `query` as a snapshot, not a
    /// guarantee.
    pub async fn query(&self, service_name: &str) -> Result<Vec<InstanceInfo>> {
        let mut conn = common::pool::redis_connection(&self.redis).await?;

        let keys: Vec<String> = conn.keys(key_pattern(service_name)).await.map_err(|e| {
            Error::wrap("service-registry", "failed to list registered instances", e)
        })?;
        if keys.is_empty() {
            return Ok(Vec::new());
        }

        let payloads: Vec<Option<String>> = conn.mget(keys).await.map_err(|e| {
            Error::wrap(
                "service-registry",
                "failed to fetch registered instances",
                e,
            )
        })?;

        payloads
            .into_iter()
            .flatten()
            .map(|payload| {
                serde_json::from_str(&payload).map_err(|e| {
                    Error::wrap("service-registry", "failed to decode instance info", e)
                })
            })
            .collect()
    }

    /// A stream of [`RegistryEvent`]s for `service_name` — see the module
    /// doc for what this does and doesn't cover (no expiry events today).
    pub async fn subscribe(
        &self,
        service_name: &str,
    ) -> Result<impl futures_util::Stream<Item = RegistryEvent> + use<>> {
        use futures_util::StreamExt;

        let mut pubsub = common::pool::redis_pubsub_connection(&self.redis_config).await?;
        pubsub
            .subscribe(events_topic(service_name))
            .await
            .map_err(|e| {
                Error::wrap(
                    "service-registry",
                    "failed to subscribe to registry events",
                    e,
                )
            })?;

        Ok(pubsub.into_on_message().filter_map(|msg| async move {
            let payload: String = msg.get_payload().ok()?;
            serde_json::from_str(&payload).ok()
        }))
    }

    async fn publish_event(
        &self,
        conn: &mut deadpool_redis::Connection,
        service_name: &str,
        instance_id: Uuid,
        kind: RegistryEventKind,
    ) -> Result<()> {
        let event = RegistryEvent {
            kind,
            service_name: service_name.to_string(),
            instance_id,
        };
        let payload = serde_json::to_string(&event)
            .map_err(|e| Error::wrap("service-registry", "failed to encode registry event", e))?;

        conn.publish::<_, _, ()>(events_topic(service_name), payload)
            .await
            .map_err(|e| Error::wrap("service-registry", "failed to publish registry event", e))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use common::config::RedisConfig;
    use common::pool::{PoolOptions, redis_pool};
    use futures_util::StreamExt;

    use super::*;

    // Real Redis — set WZ_REDIS_* and run with `-- --ignored`.
    fn registry(ttl_seconds: u64) -> ServiceRegistry {
        let redis_config = RedisConfig::from_env().expect("WZ_REDIS_* env vars set");
        let redis = redis_pool(&redis_config, PoolOptions::default()).unwrap();
        ServiceRegistry::new(redis, redis_config, ttl_seconds)
    }

    #[tokio::test]
    #[ignore]
    async fn register_then_query_returns_the_instance() {
        let registry = registry(30);
        let service_name = format!("test-service-{}", Uuid::now_v7());
        let instance_id = Uuid::now_v7();

        registry
            .register(
                &service_name,
                instance_id,
                "127.0.0.1:9000",
                InstanceMetadata {
                    load: Some(0.5),
                    capacity: Some(100),
                    realm_id: None,
                },
            )
            .await
            .unwrap();

        let instances = registry.query(&service_name).await.unwrap();
        assert_eq!(instances.len(), 1);
        assert_eq!(instances[0].instance_id, instance_id);
        assert_eq!(instances[0].address, "127.0.0.1:9000");

        registry
            .deregister(&service_name, instance_id)
            .await
            .unwrap();
        assert!(registry.query(&service_name).await.unwrap().is_empty());
    }

    #[tokio::test]
    #[ignore]
    async fn heartbeat_on_an_unregistered_instance_errs() {
        let registry = registry(30);
        let service_name = format!("test-service-{}", Uuid::now_v7());

        let err = registry
            .heartbeat(&service_name, Uuid::now_v7())
            .await
            .unwrap_err();
        assert!(err.to_string().contains("isn't registered"), "{err}");
    }

    #[tokio::test]
    #[ignore]
    async fn expired_registration_drops_out_of_query() {
        let registry = registry(1);
        let service_name = format!("test-service-{}", Uuid::now_v7());
        let instance_id = Uuid::now_v7();

        registry
            .register(
                &service_name,
                instance_id,
                "127.0.0.1:9000",
                InstanceMetadata::default(),
            )
            .await
            .unwrap();
        assert_eq!(registry.query(&service_name).await.unwrap().len(), 1);

        tokio::time::sleep(Duration::from_secs(2)).await;
        assert!(registry.query(&service_name).await.unwrap().is_empty());
    }

    #[tokio::test]
    #[ignore]
    async fn subscriber_sees_register_and_deregister_events() {
        let registry = registry(30);
        let service_name = format!("test-service-{}", Uuid::now_v7());
        let instance_id = Uuid::now_v7();

        let mut events = Box::pin(registry.subscribe(&service_name).await.unwrap());

        registry
            .register(
                &service_name,
                instance_id,
                "127.0.0.1:9000",
                InstanceMetadata::default(),
            )
            .await
            .unwrap();
        let registered = tokio::time::timeout(Duration::from_secs(2), events.next())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(registered.kind, RegistryEventKind::Registered);
        assert_eq!(registered.instance_id, instance_id);

        registry
            .deregister(&service_name, instance_id)
            .await
            .unwrap();
        let deregistered = tokio::time::timeout(Duration::from_secs(2), events.next())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(deregistered.kind, RegistryEventKind::Deregistered);
    }
}
