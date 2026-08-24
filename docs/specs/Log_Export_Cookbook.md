# Log Export/Aggregation Cookbook

Corresponds to [Log export/aggregation](Observability_Spec.md#log-exportaggregation-decision-120) in the Observability Spec, decided in [#120](https://github.com/LunarVagabond/WorldZero/issues/120): core ships the fixed stdout log format and nothing more — no pluggable log-sink abstraction, no bundled/blessed Loki, DataDog, or ELK integration. This page is that decision's one additional deliverable.

**This is an example, not a maintained integration.** Nothing here is core-supported, versioned, or tested in CI — it's "here's one way to do it," the same framing as the (still-pending) reference Grafana dashboard for metrics ([#59](https://github.com/LunarVagabond/WorldZero/issues/59)/[#68](https://github.com/LunarVagabond/WorldZero/issues/68)). Copy what's useful, change what isn't, and don't expect an issue filed against this page to get the same response as one against actual server code.

## The format you're parsing

Every WorldZero service (`server`, or any crate's own `bin`) writes the same fixed shape to stdout, one line per log event (`common::logging`, [`docs/specs/Observability_Spec.md`](Observability_Spec.md#logging)):

```
<TIMESTAMP> <LEVEL> <SOURCE> <MESSAGE>
```

- **`TIMESTAMP`** — RFC 3339 / ISO 8601, always UTC. Example: `2026-08-24T14:03:11.482112Z`.
- **`LEVEL`** — uppercase: `TRACE`, `DEBUG`, `INFO`, `WARN`, or `ERROR`.
- **`SOURCE`** — `tracing`'s `target`: the emitting Rust module path, which always starts with the emitting **crate's** identifier. Real examples from the codebase: `auth::gateway_protocol`, `chat::pubsub`, `plugin_host::runtime`, `server::world_actor`.
- **`MESSAGE`** — everything after that, verbatim. A log call's own structured fields (`tracing::warn!(entity_id, error = %e, "...")`) are appended to the message as `key=value` pairs by `common::logging`'s formatter, not broken out as separate columns — so `MESSAGE` itself can contain further `key=value` pairs a shipper may also want to parse.

A real line:

```
2026-08-24T14:03:11.482112Z WARN server::world_actor plugin on_npc_tick hook failed entity=e5e2... error=trap
```

## Filtering/tagging by crate

**The crate name is always the first `::`-delimited segment of `SOURCE`.** This is the thing every example below extracts into its own label/field so a log reader can filter down to one crate (or exclude one) without full-text-searching the message body.

One naming gotcha worth calling out explicitly: Cargo package names with a hyphen become underscored Rust identifiers, so the crate segment in `SOURCE` doesn't always look like the directory name under `crates/`:

| Directory (`crates/...`) | `SOURCE` prefix you'll actually see |
|---|---|
| `auth`, `character`, `chat`, `common`, `content`, `gateway`, `server`, `transfer`, `world` | same |
| `plugin-host` | `plugin_host` |
| `realm-directory` | `realm_directory` |

If a filter for `plugin-host`'s logs isn't matching anything, this is almost always why.

## Zero-tooling: `grep`/`journalctl`

Before reaching for a shipper at all — the format is plain text, so this works with nothing installed beyond what's already on the box:

```sh
# Everything from auth, any level
journalctl -u worldzero | grep -E '^\S+ \S+ auth(::|$)'

# Everything from plugin-host specifically (note the underscore, per above)
journalctl -u worldzero | grep -E '^\S+ \S+ plugin_host(::|$)'

# Every WARN or ERROR, any crate
journalctl -u worldzero | grep -E '^\S+ (WARN|ERROR) '
```

The `(::|$)` guards against `auth` matching `auth_something_else` — match the crate segment exactly, not as a prefix.

## Worked example: Promtail → Loki

[Promtail](https://grafana.com/docs/loki/latest/send-data/promtail/) is Loki's log shipper — a reasonable default worked example here since it pairs naturally with the same Grafana ecosystem the (pending) reference dashboard targets for metrics. Point it at wherever your process manager captures WorldZero's stdout (a log file, or `journald` via Promtail's `journal` scrape target).

```yaml
# promtail-config.yaml
server:
  http_listen_port: 9080

positions:
  filename: /tmp/positions.yaml

clients:
  - url: http://localhost:3100/loki/api/v1/push

scrape_configs:
  - job_name: worldzero
    static_configs:
      - targets: [localhost]
        labels:
          job: worldzero
          __path__: /var/log/worldzero/*.log
    pipeline_stages:
      # Split the fixed line shape into named captures.
      - regex:
          expression: '^(?P<timestamp>\S+) (?P<level>\S+) (?P<source>\S+) (?P<message>.*)$'

      # Pull just the crate (the segment before the first "::", or the
      # whole thing if there isn't one) out of SOURCE for filtering —
      # kept as a separate capture from `source` itself, see the label
      # cardinality note below.
      - regex:
          source: source
          expression: '^(?P<crate>[^:]+)'

      # Promote level/crate to real Loki labels — {job="worldzero",
      # crate="auth"} or {job="worldzero", level="ERROR"} both become
      # ordinary LogQL label selectors, not string searches.
      - labels:
          level:
          crate:

      # Use WorldZero's own timestamp rather than Promtail's ingest time.
      - timestamp:
          source: timestamp
          format: RFC3339
```

Result: in Grafana's Loki explore view, `{job="worldzero", crate="auth"}` shows only `auth`'s log lines, `{job="worldzero", crate="world", level="WARN"}` narrows further to `world`'s warnings, and `{job="worldzero"} |= "plugin_host::runtime"` still works as a full-text fallback for filtering on the full module path (not just the crate) without promoting that higher-cardinality value to a label.

**Cardinality note:** deliberately *not* promoting the full `source` (module path) to a Loki label above, only the coarser `crate`. Loki charges real cost per distinct label value; `SOURCE` has dozens of distinct module paths and grows every time someone adds a new module, while `crate` stays bounded to the number of crates in the workspace. Filter on module path via a line/pattern match (`|=`/`|~`) instead of a label if you need that granularity.

## Other shippers

Same regex, different config syntax — every mainstream shipper can parse this format the same way:

- **Vector:** a [`remap` transform](https://vector.dev/docs/reference/vrl/) with `parse_regex!` against the same `^(?P<timestamp>\S+) (?P<level>\S+) (?P<source>\S+) (?P<message>.*)$` pattern, then `split(.source, "::")[0]` for the crate segment.
- **Filebeat/DataDog Agent:** a [Grok processor](https://www.elastic.co/guide/en/elasticsearch/reference/current/grok-processor.html) (Filebeat/Elastic) or a [log-processing pipeline](https://docs.datadoghq.com/logs/log_configuration/parsing/) (DataDog) with an equivalent pattern — `%{TIMESTAMP_ISO8601:timestamp} %{WORD:level} %{NOTSPACE:source} %{GREEDYDATA:message}`, then a second Grok/pipeline stage on `source` to peel off the crate.

None of these are wired into WorldZero itself, tested against its output, or kept in sync as new crates/modules are added — same "here's one way" framing as Promtail above.
