//! Builds this process's own `/healthz`/`/readyz` reports (#181) on top of
//! `common::health`'s generic report/serving mechanics — this module is
//! where the actual "what does 'healthy' mean for a combined `server`
//! process" decisions live, since only `server::main` has the
//! Postgres/Redis pools, `ServicesConfig`, plugin count, and zone
//! manifest count a real check needs.
//!
//! **Liveness (`healthz`) vs. readiness (`readyz`):** both report the
//! same postgres/redis/chat/metrics/plugin_host checks — for postgres and
//! redis this is deliberately the *same* ping either way (see
//! `common::health::ping_postgres`'s own doc comment for why: the pool
//! real request traffic uses is the one connectivity that matters, there
//! is no separate "fresher" check to run). `readyz` additionally reports
//! `zone_manifests` and `migrations`, per #181's "readiness-only concerns
//! liveness has no reason to check."

use std::path::PathBuf;
use std::time::Instant;

use common::health::{CheckResult, HealthReport, ping_postgres, ping_redis, uptime_seconds};
use common::pool::RedisPool;
use sqlx::PgPool;

/// Everything a `/healthz`/`/readyz` report needs to read — a snapshot of
/// already-constructed process state, not a place new state lives.
/// `plugin_count`/`zone_count` are fixed once at startup (#152's plugins
/// and this process's zone manifests are both loaded once, before the
/// gateway ever accepts a connection, and never change afterward), so
/// there's no need to re-derive them from `plugins`/the zone registry on
/// every request.
pub struct HealthDeps {
    pub pool: PgPool,
    pub redis: RedisPool,
    pub chat_enabled: bool,
    pub metrics_enabled: bool,
    pub plugin_count: usize,
    pub zone_count: usize,
    pub migrations_dir: PathBuf,
    pub started_at: Instant,
}

fn report_base(deps: &HealthDeps) -> HealthReport {
    HealthReport::new(env!("CARGO_PKG_VERSION"), uptime_seconds(deps.started_at))
}

fn service_check(enabled: bool) -> CheckResult {
    if enabled {
        CheckResult::ok()
    } else {
        CheckResult::disabled()
    }
}

fn plugin_host_check(plugin_count: usize) -> CheckResult {
    CheckResult::ok()
        .with_detail("plugin_loaded", plugin_count > 0)
        .with_detail("plugin_count", plugin_count as u64)
}

/// #181's "zone manifests loaded" readiness-only check — `zone_count` is
/// always `>= 1` by the time `server::main` gets this far (it panics
/// earlier if `load_zone_manifests` finds none), so this only ever
/// reports the count for an operator's visibility, not a real failure
/// mode this process could actually be running with.
fn zone_manifests_check(zone_count: usize) -> CheckResult {
    CheckResult::ok().with_detail("zone_count", zone_count as u64)
}

async fn migrations_check(pool: &PgPool, migrations_dir: &std::path::Path) -> CheckResult {
    match common::migrate::migrations_current(pool, migrations_dir).await {
        Ok(true) => CheckResult::ok(),
        Ok(false) => CheckResult::unavailable("pending migrations"),
        Err(e) => CheckResult::unavailable(e.to_string()),
    }
}

/// `/healthz` — re-verifies the already-established Postgres/Redis pool
/// connections are still alive (`common::health::ping_postgres`/
/// `ping_redis`, a cheap round trip against the existing pool, not a
/// fresh reconnect) and reports every `ServicesConfig`-gated service's
/// on/off state. Cheap enough to run on a tight liveness interval.
pub async fn healthz(deps: &HealthDeps) -> HealthReport {
    let postgres = match ping_postgres(&deps.pool).await {
        Ok(()) => CheckResult::ok(),
        Err(e) => CheckResult::unavailable(e.to_string()),
    };
    let redis = match ping_redis(&deps.redis).await {
        Ok(()) => CheckResult::ok(),
        Err(e) => CheckResult::unavailable(e.to_string()),
    };

    report_base(deps)
        .with_check("postgres", postgres)
        .with_check("redis", redis)
        .with_check("chat", service_check(deps.chat_enabled))
        .with_check("metrics", service_check(deps.metrics_enabled))
        .with_check("plugin_host", plugin_host_check(deps.plugin_count))
}

/// `/readyz` — the same dependency checks `healthz` reports, plus
/// readiness-only checks `healthz` has no reason to run: zone manifests
/// loaded and migrations current.
pub async fn readyz(deps: &HealthDeps) -> HealthReport {
    let report = healthz(deps).await;
    let migrations = migrations_check(&deps.pool, &deps.migrations_dir).await;

    report
        .with_check("zone_manifests", zone_manifests_check(deps.zone_count))
        .with_check("migrations", migrations)
}
