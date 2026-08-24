# Observability Spec

Corresponds to [Observability & Operations](../PROPOSAL.md#observability--operations) in the proposal. Tracing and the admin API are still placeholders below — logging and metrics are filled in, since those are decided.

## Logging

**Crate:** `tracing`, per the proposal's Observability & Operations decision — structured, span-aware, and the standard choice for exactly this in the Rust ecosystem. Paired with `tracing-subscriber` for formatting and output.

**Levels:** the standard five, nothing custom (for now — see "Open question" below):

| Level | Meaning |
|---|---|
| `TRACE` | Finest-grained diagnostic detail — noisy by design, off by default in normal operation. |
| `DEBUG` | Developer-facing detail useful while working on a specific area, not needed for routine operation. |
| `INFO` | Normal operational events worth a permanent record — service start/stop, a zone coming online, a realm registered. |
| `WARN` | Something worth noting but not broken — a retried operation, a degraded-but-functioning state, a deprecated path being hit. **Can wait until morning.** |
| `ERROR` | Reserved. Something is *actually* broken. This is the level that eventually pages a human out of bed once alerting is wired up (Datadog or similar) — not a general-purpose "something went wrong" bucket. |

**Format:** every service emits the same fixed shape, regardless of which crate is logging:

```
<TIMESTAMP> <LEVEL> <SOURCE> <MESSAGE>
```

- `TIMESTAMP` — RFC 3339 / ISO 8601, UTC.
- `LEVEL` — uppercase, one of the five above.
- `SOURCE` — the emitting module path (`tracing`'s `target`, e.g. `auth::session`).
- `MESSAGE` — the log line itself.

This is a custom `tracing-subscriber` formatting layer, not its default output shape — implemented once, shared by every crate, so no service can drift into its own format.

## Severity policy

The point of reserving `ERROR` is operational, not stylistic: once alerting exists, `ERROR` is the trigger that wakes someone up. If everything that's merely unfortunate also logs at `ERROR`, that signal is worthless by the time it matters. Concretely:

- Log at `ERROR` only for the core framework's own failures that genuinely need a human *now* — not for "this failed but we recovered," not for an expected edge case, not for bad input from a client.
- Log at `WARN` for the "worth knowing about, doesn't need anyone before morning" tier — this is where "this failed but we recovered" belongs.
- **This discipline applies to the core framework's own services, not to plugins.** The plugin host's logging host function ([v0 Host Functions (plugin calls out to the host)](../PROPOSAL.md#v0-host-functions-plugin-calls-out-to-the-host)) lets plugin authors log at any level they want without it carrying the same weight — a plugin author logging `ERROR` for their own quest-script bug is not the same signal as core logging `ERROR`, and downstream alerting (once it exists) should be scoped accordingly rather than paging on any `ERROR` from anywhere in the process.

## Metrics

**Crate:** `prometheus` (the standard Rust client library) — the proposal's own words: "boring and standard on purpose: most self-hosters already have Prometheus/Grafana experience or tooling." `common::metrics::Metrics` owns the `Registry` and every metric this build exposes; `common::metrics::serve` is a minimal hand-rolled HTTP responder for the `/metrics` scrape endpoint — deliberately not `axum`/`hyper`, since one static response is the entire surface a Prometheus scrape needs and a framework brings routing/middleware machinery this endpoint has no use for.

**What's exposed**, per the proposal's named set:

| Metric | Type | Labels | Meaning |
|---|---|---|---|
| `worldzero_zone_tick_duration_seconds` | Histogram | `zone_id` | How long one zone-service simulation tick took. |
| `worldzero_zone_entity_count` | Gauge | `zone_id` | Entities (players + NPCs) currently spawned in a zone, sampled once per tick. |
| `worldzero_zone_world_command_queue_depth` | Gauge | `zone_id` | Commands queued on a zone-service actor's command channel (`server::world_actor::WorldCommand`), not yet processed. |
| `worldzero_connection_count` | Gauge | *(none)* | Currently connected gateway sessions, process-wide. |

**Per-service, not globally aggregated:** the `zone_id` label is what makes a `world` instance's numbers distinguishable from another's (#45 can run several zone-service instances in one process) — a deployment scraping this can chart or alert on one misbehaving zone without it being averaged away by every other zone's healthy numbers. `worldzero_connection_count` has no `zone_id` label — a gateway connection isn't zone-scoped (it can cross zones mid-session, #45's `ZoneChanged` handoff), so it's tracked once for the whole process rather than attributed to whichever zone it happened to be in at scrape time.

**Endpoint:** `server::main` binds a second, separate listener (`WZ_METRICS_ADDR`, default `127.0.0.1:9090`) purely for `GET /metrics` scrapes — never sharing a port with the gateway's TCP/UDP game-traffic listeners (docs/specs/Networking_Spec.md), and never itself becoming a general-purpose HTTP surface for the framework (that's the future admin API below, a distinct concern).

**Runtime toggle:** `WZ_SERVICE_METRICS_ENABLED`, default `true` — same optional-service pattern `chat` established (`common::config::ServicesConfig`, decision #91). Disabled means `Option<Arc<Metrics>>` is `None` end to end: no `/metrics` listener binds, and every instrumentation call site (`world_actor`'s tick loop, `session`'s connection tracking) skips its `Some(...)` branch entirely — not a listener left running with nothing behind it.

## Log export/aggregation (decision: #120)

Core ships the fixed stdout format above and nothing more — no pluggable log-sink abstraction, no bundled/blessed Loki+Grafana or DataDog integration. The format is already portable enough that any mainstream shipper (Promtail, Filebeat, DataDog Agent, Vector) can tail it without WorldZero doing anything else. What ships beyond this is docs-only — example shipper configs, "reference, not maintained product," same framing already used for the Grafana dashboard (#59/#68) — see [Log Export/Aggregation Cookbook](Log_Export_Cookbook.md) (#122) for worked examples, including how to filter/tag log output by crate.

## Tracing and the admin API

Still placeholder — OpenTelemetry spans (#49) and the admin/introspection API surface (#56) are designed at a high level in the proposal but not yet at spec detail.
