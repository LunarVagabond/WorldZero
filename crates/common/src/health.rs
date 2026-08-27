//! `/healthz` (liveness) and `/readyz` (readiness) HTTP endpoints —
//! #181, docs/PROPOSAL.md's Observability & Operations: "Health/readiness
//! endpoints per service, for orchestration platforms (Kubernetes, or
//! Agones specifically for game-server lifecycle)."
//!
//! Same "hand-rolled, minimal HTTP, no framework dependency" precedent
//! `common::metrics::serve` established (#48) — still a small,
//! single-purpose surface, just returning a JSON body per request instead
//! of one fixed Prometheus exposition string. This module owns the
//! generic report/serving mechanics (what a check result looks like, how
//! it rolls up into one status, how it's rendered and served); it does
//! *not* know what checks a given deployment actually runs — `server::main`
//! decides that, since only it has the Postgres/Redis pools,
//! `ServicesConfig`, plugin-host state, and zone manifests a real check
//! needs to inspect. `ping_postgres`/`ping_redis` below are the one piece
//! of check *logic* that lives here rather than in `server`: they're
//! generic enough (and this crate already owns the pool types) that
//! duplicating them per binary would be exactly the drift this crate
//! exists to prevent.

use std::collections::BTreeMap;
use std::future::Future;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Instant;

use serde_json::{Map, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

use crate::error::{Error, Result};
use crate::pool::RedisPool;

/// One check's (or the whole report's) status. Shared vocabulary between
/// a single `checks.<name>.status` entry and the top-level `status` field
/// — the top-level value is always one of these four, computed by rolling
/// up every check (see [`HealthReport::status`]), never set independently
/// of them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    /// Fully functional.
    Ok,
    /// Not entirely healthy, but still capable of serving traffic — e.g.
    /// a non-critical check failed. Still a `200`, since pulling a
    /// still-capable instance out of rotation over a non-critical
    /// failure would do more harm than the failure itself.
    Degraded,
    /// Not capable of serving traffic right now — a required
    /// dependency/check failed. Maps to `503`.
    Unavailable,
    /// Intentionally turned off via `ServicesConfig` (or equivalent) —
    /// distinct from `Unavailable` on purpose: an operator needs to be
    /// able to tell "off on purpose" from "should be on and isn't" at a
    /// glance. Never rolls up into a worse overall status by itself.
    Disabled,
}

impl Status {
    fn as_str(self) -> &'static str {
        match self {
            Status::Ok => "ok",
            Status::Degraded => "degraded",
            Status::Unavailable => "unavailable",
            Status::Disabled => "disabled",
        }
    }

    /// The HTTP status code the *top-level* status drives — per #181's
    /// decision, this is the one value Kubernetes' own `httpGet` probe
    /// acts on (it doesn't parse the JSON body), so it must always agree
    /// with the body's own `status` field. `Disabled` never appears as a
    /// top-level status (see [`HealthReport::status`]'s rollup), but is
    /// handled here anyway so this stays a total function.
    fn http_status_code(self) -> u16 {
        match self {
            Status::Ok | Status::Degraded | Status::Disabled => 200,
            Status::Unavailable => 503,
        }
    }
}

/// One entry under `checks` in the JSON body — a status plus whatever
/// extra, check-specific detail is worth an operator seeing (e.g.
/// `plugin_loaded: true`, or a reason for a failed check).
#[derive(Debug, Clone)]
pub struct CheckResult {
    pub status: Status,
    detail: Map<String, Value>,
}

impl CheckResult {
    pub fn ok() -> Self {
        Self {
            status: Status::Ok,
            detail: Map::new(),
        }
    }

    pub fn disabled() -> Self {
        Self {
            status: Status::Disabled,
            detail: Map::new(),
        }
    }

    pub fn degraded(reason: impl Into<String>) -> Self {
        let mut detail = Map::new();
        detail.insert("reason".to_string(), Value::String(reason.into()));
        Self {
            status: Status::Degraded,
            detail,
        }
    }

    pub fn unavailable(reason: impl Into<String>) -> Self {
        let mut detail = Map::new();
        detail.insert("reason".to_string(), Value::String(reason.into()));
        Self {
            status: Status::Unavailable,
            detail,
        }
    }

    /// Attaches one extra `key: value` field to this check's JSON entry
    /// (e.g. `plugin_count`, `zone_count`) — chainable, so a caller can
    /// build up a check result in one expression.
    pub fn with_detail(mut self, key: impl Into<String>, value: impl Into<Value>) -> Self {
        self.detail.insert(key.into(), value.into());
        self
    }

    fn into_json(self) -> Value {
        let mut map = self.detail;
        map.insert(
            "status".to_string(),
            Value::String(self.status.as_str().to_string()),
        );
        Value::Object(map)
    }
}

/// The full `/healthz`/`/readyz` JSON body — one instance built fresh per
/// request (checks are cheap pings/flag reads, not something worth
/// caching), per #181's response shape.
pub struct HealthReport {
    pub version: String,
    pub uptime_seconds: u64,
    /// `BTreeMap` rather than `HashMap` so the rendered JSON's `checks`
    /// key order is stable across requests — easier for an operator (or a
    /// test) diffing two responses to eyeball, and costs nothing given
    /// how few entries this ever holds.
    pub checks: BTreeMap<String, CheckResult>,
}

impl HealthReport {
    pub fn new(version: impl Into<String>, uptime_seconds: u64) -> Self {
        Self {
            version: version.into(),
            uptime_seconds,
            checks: BTreeMap::new(),
        }
    }

    pub fn with_check(mut self, name: impl Into<String>, result: CheckResult) -> Self {
        self.checks.insert(name.into(), result);
        self
    }

    /// The top-level status, rolled up from every check: `Unavailable` if
    /// any check is `Unavailable`, else `Degraded` if any check is
    /// `Degraded`, else `Ok`. `Disabled` checks never affect this — an
    /// intentionally-off optional service reporting `disabled` says
    /// nothing about whether the process as a whole is healthy.
    pub fn status(&self) -> Status {
        let mut worst = Status::Ok;
        for check in self.checks.values() {
            match check.status {
                Status::Unavailable => return Status::Unavailable,
                Status::Degraded if worst == Status::Ok => worst = Status::Degraded,
                Status::Ok | Status::Degraded | Status::Disabled => {}
            }
        }
        worst
    }

    pub fn http_status_code(&self) -> u16 {
        self.status().http_status_code()
    }

    fn into_json(self) -> Value {
        let status = self.status();
        let checks: Map<String, Value> = self
            .checks
            .into_iter()
            .map(|(name, result)| (name, result.into_json()))
            .collect();

        let mut map = Map::new();
        map.insert(
            "status".to_string(),
            Value::String(status.as_str().to_string()),
        );
        map.insert("version".to_string(), Value::String(self.version));
        map.insert(
            "uptime_seconds".to_string(),
            Value::Number(self.uptime_seconds.into()),
        );
        map.insert("checks".to_string(), Value::Object(checks));
        Value::Object(map)
    }

    /// `into_json` builds a plain `serde_json::Value` rather than this
    /// struct deriving `Serialize` directly — the top-level `status`
    /// field is computed from `checks`, not a stored field, so a derived
    /// impl couldn't produce it without a redundant stored copy that
    /// could drift from the real rollup.
    fn render(self) -> String {
        serde_json::to_string(&self.into_json())
            .expect("a Value built entirely from Strings/Numbers/Maps always serializes")
    }
}

/// A quick "is this still alive" ping against an *already-established*
/// `PgPool` connection — a plain `SELECT 1`, not a fresh connect. What
/// `/healthz` calls per #181's liveness contract; `/readyz` uses the same
/// function for its own Postgres check, since the pool real request
/// traffic uses *is* the real connectivity that matters — readiness's
/// extra depth over liveness is the additional readiness-only checks
/// (plugin/manifest/migration state), not a different way of reaching
/// Postgres.
pub async fn ping_postgres(pool: &sqlx::PgPool) -> Result<()> {
    sqlx::query_scalar::<_, i32>("SELECT 1")
        .fetch_one(pool)
        .await
        .map(|_| ())
        .map_err(|e| Error::wrap("common", "postgres ping failed", e))
}

/// Same idea as [`ping_postgres`], against an already-established Redis
/// pool connection.
pub async fn ping_redis(pool: &RedisPool) -> Result<()> {
    use deadpool_redis::redis::AsyncCommands;

    let mut conn = crate::pool::redis_connection(pool).await?;
    let _: String = conn
        .ping::<String>()
        .await
        .map_err(|e| Error::wrap("common", "redis ping failed", e))?;
    Ok(())
}

/// Wall-clock seconds since `started_at` — shared helper so `server`
/// doesn't hand-roll `Instant` arithmetic at each of the two call sites
/// (`/healthz` and `/readyz` build a fresh report per request, both from
/// the same process-startup `Instant`).
pub fn uptime_seconds(started_at: Instant) -> u64 {
    started_at.elapsed().as_secs()
}

type ReportFuture = Pin<Box<dyn Future<Output = HealthReport> + Send>>;
type ReportFn = Arc<dyn Fn() -> ReportFuture + Send + Sync>;

/// Serves `/healthz` and `/readyz` over plain HTTP on `addr`, dispatching
/// on the request line's path to whichever of `healthz`/`readyz` applies
/// and responding `404` to anything else. One listener for both paths
/// (rather than two separate listeners/ports) — #181 leaves "own
/// listener/addr, or share a listener on separate paths" as an
/// implementation choice; this picks a listener of its own (distinct
/// from `metrics::serve`'s, via `WZ_HEALTH_ADDR`) rather than folding
/// onto the `/metrics` listener, since metrics is itself an optional,
/// independently-toggled service (`WZ_SERVICE_METRICS_ENABLED`) and
/// liveness/readiness need to stay reachable regardless of that toggle —
/// but keeps the two health paths on one listener/port rather than two,
/// since they're the same concern at two different depths, not two
/// independent surfaces.
pub async fn serve(
    addr: SocketAddr,
    healthz: impl Fn() -> ReportFuture + Send + Sync + 'static,
    readyz: impl Fn() -> ReportFuture + Send + Sync + 'static,
) -> Result<()> {
    let healthz: ReportFn = Arc::new(healthz);
    let readyz: ReportFn = Arc::new(readyz);

    let listener = TcpListener::bind(addr).await.map_err(|e| {
        Error::wrap(
            "common",
            format!("failed to bind health listener on {addr}"),
            e,
        )
    })?;
    tracing::info!(%addr, "health endpoint listening");

    loop {
        let (mut stream, _peer) = match listener.accept().await {
            Ok(accepted) => accepted,
            Err(e) => {
                tracing::warn!(error = %e, "failed to accept a health connection");
                continue;
            }
        };
        let healthz = healthz.clone();
        let readyz = readyz.clone();

        tokio::spawn(async move {
            // Only the request line's path matters — enough of the
            // request to read a `GET /path HTTP/1.1` line, same
            // "don't bother parsing headers this endpoint never uses"
            // stance `metrics::serve` already takes.
            let mut buf = [0u8; 1024];
            let n = match stream.read(&mut buf).await {
                Ok(n) => n,
                Err(_) => return,
            };
            let request = String::from_utf8_lossy(&buf[..n]);
            let path = request
                .lines()
                .next()
                .and_then(|line| line.split_whitespace().nth(1))
                .unwrap_or("");

            let report = match path {
                "/healthz" => Some(healthz().await),
                "/readyz" => Some(readyz().await),
                _ => None,
            };

            let response = match report {
                Some(report) => {
                    let status_code = report.http_status_code();
                    let status_text = match status_code {
                        200 => "OK",
                        _ => "Service Unavailable",
                    };
                    let body = report.render();
                    format!(
                        "HTTP/1.1 {status_code} {status_text}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                        body.len(),
                        body
                    )
                }
                None => {
                    let body = r#"{"error":"not found"}"#;
                    format!(
                        "HTTP/1.1 404 Not Found\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                        body.len(),
                        body
                    )
                }
            };
            let _ = stream.write_all(response.as_bytes()).await;
            let _ = stream.shutdown().await;
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ok_report_with_no_checks_is_status_ok_and_200() {
        let report = HealthReport::new("0.1.0", 42);
        assert_eq!(report.status(), Status::Ok);
        assert_eq!(report.http_status_code(), 200);
    }

    #[test]
    fn one_unavailable_check_makes_the_whole_report_unavailable() {
        let report = HealthReport::new("0.1.0", 1)
            .with_check("postgres", CheckResult::ok())
            .with_check("redis", CheckResult::unavailable("connection refused"));
        assert_eq!(report.status(), Status::Unavailable);
        assert_eq!(report.http_status_code(), 503);
    }

    #[test]
    fn degraded_check_without_any_unavailable_check_stays_200() {
        let report = HealthReport::new("0.1.0", 1)
            .with_check("postgres", CheckResult::ok())
            .with_check("plugin_host", CheckResult::degraded("no plugins loaded"));
        assert_eq!(report.status(), Status::Degraded);
        assert_eq!(report.http_status_code(), 200);
    }

    #[test]
    fn disabled_checks_never_worsen_the_overall_status() {
        let report = HealthReport::new("0.1.0", 1)
            .with_check("postgres", CheckResult::ok())
            .with_check("chat", CheckResult::disabled())
            .with_check("metrics", CheckResult::disabled());
        assert_eq!(report.status(), Status::Ok);
    }

    #[test]
    fn render_produces_the_documented_json_shape() {
        let report = HealthReport::new("0.1.0", 4213)
            .with_check("postgres", CheckResult::ok())
            .with_check("chat", CheckResult::disabled())
            .with_check(
                "plugin_host",
                CheckResult::ok().with_detail("plugin_loaded", true),
            );
        let rendered = report.render();
        let value: Value = serde_json::from_str(&rendered).unwrap();

        assert_eq!(value["status"], "ok");
        assert_eq!(value["version"], "0.1.0");
        assert_eq!(value["uptime_seconds"], 4213);
        assert_eq!(value["checks"]["postgres"]["status"], "ok");
        assert_eq!(value["checks"]["chat"]["status"], "disabled");
        assert_eq!(value["checks"]["plugin_host"]["status"], "ok");
        assert_eq!(value["checks"]["plugin_host"]["plugin_loaded"], true);
    }

    #[tokio::test]
    async fn serve_dispatches_healthz_and_readyz_by_path_and_404s_otherwise() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        drop(listener);

        tokio::spawn(async move {
            let _ = serve(
                addr,
                || {
                    Box::pin(async {
                        HealthReport::new("0.1.0", 1).with_check("postgres", CheckResult::ok())
                    })
                },
                || {
                    Box::pin(async {
                        HealthReport::new("0.1.0", 1)
                            .with_check("postgres", CheckResult::ok())
                            .with_check("migrations", CheckResult::ok())
                    })
                },
            )
            .await;
        });

        let mut stream = None;
        for _ in 0..50 {
            match tokio::net::TcpStream::connect(addr).await {
                Ok(s) => {
                    stream = Some(s);
                    break;
                }
                Err(_) => tokio::time::sleep(std::time::Duration::from_millis(10)).await,
            }
        }
        let mut stream = stream.expect("health listener never came up");
        stream
            .write_all(b"GET /healthz HTTP/1.1\r\nHost: localhost\r\n\r\n")
            .await
            .unwrap();
        let mut response = Vec::new();
        stream.read_to_end(&mut response).await.unwrap();
        let response = String::from_utf8(response).unwrap();
        assert!(response.starts_with("HTTP/1.1 200 OK"), "{response}");
        assert!(response.contains("\"postgres\""), "{response}");
        assert!(!response.contains("\"migrations\""), "{response}");

        let mut stream = tokio::net::TcpStream::connect(addr).await.unwrap();
        stream
            .write_all(b"GET /readyz HTTP/1.1\r\nHost: localhost\r\n\r\n")
            .await
            .unwrap();
        let mut response = Vec::new();
        stream.read_to_end(&mut response).await.unwrap();
        let response = String::from_utf8(response).unwrap();
        assert!(response.starts_with("HTTP/1.1 200 OK"), "{response}");
        assert!(response.contains("\"migrations\""), "{response}");

        let mut stream = tokio::net::TcpStream::connect(addr).await.unwrap();
        stream
            .write_all(b"GET /nope HTTP/1.1\r\nHost: localhost\r\n\r\n")
            .await
            .unwrap();
        let mut response = Vec::new();
        stream.read_to_end(&mut response).await.unwrap();
        let response = String::from_utf8(response).unwrap();
        assert!(response.starts_with("HTTP/1.1 404 Not Found"), "{response}");
    }
}
