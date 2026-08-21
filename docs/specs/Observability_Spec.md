# Observability Spec

Corresponds to [Observability & Operations](../PROPOSAL.md#observability--operations) in the proposal. Metrics/tracing and the admin API are still placeholders below — logging is filled in, since that's decided.

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

## Metrics, tracing, and the admin API

Still placeholder — Prometheus metrics, OpenTelemetry spans, and the admin/introspection API surface are designed at a high level in the proposal but not yet at spec detail.
