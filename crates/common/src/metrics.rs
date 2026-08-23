//! Prometheus-compatible metrics (docs/PROPOSAL.md's "Observability &
//! Operations": "Metrics, Prometheus-compatible, per service — tick
//! duration, entity counts, connection counts, queue depths. Boring and
//! standard on purpose: most self-hosters already have Prometheus/Grafana
//! experience or tooling.") — #48.
//!
//! **Per-service, not globally aggregated.** `world`'s per-zone metrics
//! (`tick_duration_seconds`, `entity_count`, `world_command_queue_depth`)
//! carry a `zone_id` label, so a zone-service instance's numbers are
//! distinguishable from another's (#45 runs several in one process) —
//! collapsing them into one unlabeled counter would lose exactly the
//! signal an operator needs when only one zone is misbehaving.
//! `connection_count` has no `zone_id` label: a gateway TCP connection
//! isn't zone-scoped (it can cross zones mid-session, #45), so it's
//! tracked once, process-wide.
//!
//! **Runtime toggle**, same pattern as chat's `WZ_SERVICE_CHAT_ENABLED`
//! (`common::config::ServicesConfig`, decided in #91): default enabled,
//! `WZ_SERVICE_METRICS_ENABLED=false` disables it. Disabled means *no*
//! `/metrics` HTTP listener and *no* instrumentation at all runs — not a
//! listener that's still up serving an empty response. `server::main`
//! is what makes that end-to-end (`Option<Arc<Metrics>>` threaded
//! through exactly like chat's `Option<ChatDeps>`).

use std::net::SocketAddr;

use prometheus::{
    Encoder, Histogram, HistogramOpts, HistogramVec, IntGauge, IntGaugeVec, Opts, Registry,
    TextEncoder,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

use crate::error::{Error, Result};

/// Every metric this build exposes, plus the `Registry` they're
/// collected through. One instance per process, shared (behind an
/// `Arc`) by every zone-service actor and every connection session that
/// needs to record something.
pub struct Metrics {
    registry: Registry,
    pub tick_duration_seconds: HistogramVec,
    pub entity_count: IntGaugeVec,
    pub world_command_queue_depth: IntGaugeVec,
    pub connection_count: IntGauge,
}

impl Metrics {
    /// Registers every metric fresh — panics on a registration failure
    /// (a duplicate/malformed metric name), since that's a programming
    /// error in this module, not a runtime condition a caller could
    /// meaningfully recover from.
    pub fn new() -> Self {
        let registry = Registry::new();

        let tick_duration_seconds = HistogramVec::new(
            HistogramOpts::new(
                "worldzero_zone_tick_duration_seconds",
                "How long one zone-service simulation tick took to run.",
            ),
            &["zone_id"],
        )
        .expect("worldzero_zone_tick_duration_seconds is a valid metric");
        registry
            .register(Box::new(tick_duration_seconds.clone()))
            .expect("worldzero_zone_tick_duration_seconds registers cleanly");

        let entity_count = IntGaugeVec::new(
            Opts::new(
                "worldzero_zone_entity_count",
                "Entities (players and NPCs) currently spawned in a zone.",
            ),
            &["zone_id"],
        )
        .expect("worldzero_zone_entity_count is a valid metric");
        registry
            .register(Box::new(entity_count.clone()))
            .expect("worldzero_zone_entity_count registers cleanly");

        let world_command_queue_depth = IntGaugeVec::new(
            Opts::new(
                "worldzero_zone_world_command_queue_depth",
                "Commands queued on a zone-service actor's command channel, not yet processed.",
            ),
            &["zone_id"],
        )
        .expect("worldzero_zone_world_command_queue_depth is a valid metric");
        registry
            .register(Box::new(world_command_queue_depth.clone()))
            .expect("worldzero_zone_world_command_queue_depth registers cleanly");

        let connection_count = IntGauge::with_opts(Opts::new(
            "worldzero_connection_count",
            "Currently connected gateway sessions, process-wide (not per zone — a connection can cross zones, #45).",
        ))
        .expect("worldzero_connection_count is a valid metric");
        registry
            .register(Box::new(connection_count.clone()))
            .expect("worldzero_connection_count registers cleanly");

        Self {
            registry,
            tick_duration_seconds,
            entity_count,
            world_command_queue_depth,
            connection_count,
        }
    }

    /// One zone's tick-duration histogram, pre-resolved against the
    /// `zone_id` label — callers on a hot path (the tick loop) should
    /// hold onto this rather than re-resolving the label on every tick.
    pub fn tick_duration_for_zone(&self, zone_id: &str) -> Histogram {
        self.tick_duration_seconds.with_label_values(&[zone_id])
    }

    /// One zone's entity-count gauge, pre-resolved against the `zone_id`
    /// label — same reasoning as `tick_duration_for_zone`.
    pub fn entity_count_for_zone(&self, zone_id: &str) -> IntGauge {
        self.entity_count.with_label_values(&[zone_id])
    }

    /// One zone's command-queue-depth gauge, pre-resolved against the
    /// `zone_id` label.
    pub fn queue_depth_for_zone(&self, zone_id: &str) -> IntGauge {
        self.world_command_queue_depth.with_label_values(&[zone_id])
    }

    /// Renders every registered metric in the Prometheus text exposition
    /// format (the same encoder `TextEncoder` produces for any
    /// `/metrics` scrape) — what the HTTP listener below serves, factored
    /// out so it's also directly testable without opening a real socket.
    pub fn render(&self) -> Result<String> {
        let metric_families = self.registry.gather();
        let mut buffer = Vec::new();
        TextEncoder::new()
            .encode(&metric_families, &mut buffer)
            .map_err(|e| Error::wrap("common", "failed to encode metrics", e))?;
        String::from_utf8(buffer)
            .map_err(|e| Error::wrap("common", "metrics encoder produced non-UTF-8 output", e))
    }
}

impl Default for Metrics {
    fn default() -> Self {
        Self::new()
    }
}

/// Serves `metrics.render()` over plain HTTP on `addr` — every request,
/// regardless of method or path, gets the same `200 text/plain` body;
/// there's exactly one thing to scrape, so there's nothing to route.
/// Deliberately hand-rolled rather than pulling in an HTTP framework
/// (`axum`/`hyper`) for this one static response: a minimal
/// request-line-then-respond loop is the whole surface a Prometheus
/// scrape needs, and framework dependencies bring routing/middleware/TLS
/// machinery this endpoint has no use for (this crate's own doc comment:
/// "not a general-purpose dumping ground" — the same restraint applies
/// to what it pulls in, not just what it exposes). Runs until the
/// process exits; `server::main` only calls this when
/// `ServicesConfig::metrics_enabled` is true.
pub async fn serve(addr: SocketAddr, metrics: std::sync::Arc<Metrics>) -> Result<()> {
    let listener = TcpListener::bind(addr).await.map_err(|e| {
        Error::wrap(
            "common",
            format!("failed to bind metrics listener on {addr}"),
            e,
        )
    })?;
    tracing::info!(%addr, "metrics endpoint listening");

    loop {
        let (mut stream, _peer) = match listener.accept().await {
            Ok(accepted) => accepted,
            Err(e) => {
                tracing::warn!(error = %e, "failed to accept a metrics connection");
                continue;
            }
        };
        let metrics = metrics.clone();

        tokio::spawn(async move {
            // Enough to drain a real HTTP request line/headers without
            // caring what they say — this endpoint has one response
            // regardless of method, path, or headers.
            let mut discard = [0u8; 1024];
            let _ = stream.read(&mut discard).await;

            let body = match metrics.render() {
                Ok(body) => body,
                Err(e) => {
                    tracing::warn!(error = %e, "failed to render metrics");
                    return;
                }
            };
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/plain; version=0.0.4\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            let _ = stream.write_all(response.as_bytes()).await;
            let _ = stream.shutdown().await;
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn render_includes_every_registered_metric_name() {
        let metrics = Metrics::new();
        // Touch each metric once so it actually appears in `gather()` —
        // an `IntGaugeVec`/`HistogramVec` with no observed label
        // combination yet has nothing to render.
        metrics
            .tick_duration_for_zone("greenwood-forest")
            .observe(0.01);
        metrics.entity_count_for_zone("greenwood-forest").set(3);
        metrics.queue_depth_for_zone("greenwood-forest").set(0);
        metrics.connection_count.set(1);

        let rendered = metrics.render().unwrap();
        assert!(rendered.contains("worldzero_zone_tick_duration_seconds"));
        assert!(rendered.contains("worldzero_zone_entity_count"));
        assert!(rendered.contains("worldzero_zone_world_command_queue_depth"));
        assert!(rendered.contains("worldzero_connection_count"));
        assert!(rendered.contains(r#"zone_id="greenwood-forest""#));
    }

    #[test]
    fn different_zones_are_distinguishable_labels_not_one_aggregate() {
        let metrics = Metrics::new();
        metrics.entity_count_for_zone("greenwood-forest").set(5);
        metrics.entity_count_for_zone("stonebridge-village").set(2);

        let rendered = metrics.render().unwrap();
        assert!(rendered.contains(r#"zone_id="greenwood-forest""#));
        assert!(rendered.contains(r#"zone_id="stonebridge-village""#));
    }

    #[tokio::test]
    async fn serve_responds_to_a_real_http_scrape() {
        let metrics = std::sync::Arc::new(Metrics::new());
        metrics.connection_count.set(7);

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        drop(listener);

        let server_metrics = metrics.clone();
        tokio::spawn(async move {
            let _ = serve(addr, server_metrics).await;
        });

        // Give the listener a moment to actually bind before connecting.
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
        let mut stream = stream.expect("metrics listener never came up");

        stream
            .write_all(b"GET /metrics HTTP/1.1\r\nHost: localhost\r\n\r\n")
            .await
            .unwrap();

        let mut response = Vec::new();
        stream.read_to_end(&mut response).await.unwrap();
        let response = String::from_utf8(response).unwrap();

        assert!(response.starts_with("HTTP/1.1 200 OK"), "{response}");
        assert!(
            response.contains("worldzero_connection_count 7"),
            "{response}"
        );
    }
}
