# Content Manifest Spec

Corresponds to [World Content: Maps, NPCs, and Routes](../PROPOSAL.md#world-content-maps-npcs-and-routes) and [Manifest Format & Example](../PROPOSAL.md#manifest-format--example) in the proposal.

## `zone.manifest.yaml`: field by field

```yaml
schema_version: 1
id: greenwood-forest
display_name: "Greenwood Forest"

bounds:
  shape: polygon
  coordinate_system: { units: meters, origin: [0, 0] }
  points: [[0,0], [500,0], [500,500], [0,500]]

collision:
  asset_ref: sha256:9f2a...
  format: navmesh_v1

links:
  - target_zone: stonebridge-village
    edge: [[500,200], [500,260]]
    bidirectional: true

spawn_tables:
  - id: wolf-pack-01
    entity_type: npc.wolf
    points: [[120,80], [140,95]]
    max_population: 6
    respawn_seconds: 45
    route_id: wolf-patrol-01

routes:
  - id: wolf-patrol-01
    waypoints: [[110,70],[150,70],[150,110],[110,110]]
    loop: true
    speed: 1.4

triggers:
  - id: forest-entrance
    shape: { type: circle, center: [10,10], radius: 5 }
    event: on_trigger_enter
    one_shot: false
```

| Field | Type | Required | Validation |
|---|---|---|---|
| `schema_version` | integer | yes | Must equal the framework build's supported version (`1` for now). Wrong version fails immediately — the loader does not attempt to parse the rest. |
| `id` | string | yes | Non-empty. The zone's slug — this is the same string `character.zone_id` stores (docs/specs/Data_Model_Spec.md), and what `links[].target_zone` references. |
| `display_name` | string | yes | Non-empty. Human-readable only, never parsed for meaning. |
| `bounds.shape` | string | yes | `"polygon"` only for v0 — no other shape is implemented or accepted. |
| `bounds.coordinate_system.units` | string | yes | `"meters"` only for v0. |
| `bounds.coordinate_system.origin` | `[f64, f64]` | yes | The `(0,0)` point of this zone's local coordinate space. |
| `bounds.points` | `[[f64, f64], ...]` | yes | At least 3 points (a valid polygon). |
| `collision.asset_ref` | string | yes | `sha256:<64 lowercase hex chars>` — see "Content-addressing" below. Format is validated; the loader does not fetch or verify the asset itself (no asset store exists yet). |
| `collision.format` | string | yes | `"navmesh_v1"` only for v0. |
| `links[].target_zone` | string | no (array may be empty) | Non-empty string; cross-zone existence is a content-pack-level concern (see below), not checked by a single manifest in isolation. |
| `links[].edge` | `[[f64,f64], [f64,f64]]` | yes, if a link is present | Exactly 2 points (a line segment). |
| `links[].bidirectional` | bool | yes, if a link is present | — |
| `spawn_tables[].id` | string | yes, if a spawn table is present | Unique among `spawn_tables` in this manifest. |
| `spawn_tables[].entity_type` | string | yes | Opaque to `content`/core — a plugin-defined identifier (e.g. `npc.wolf`), not validated against any core enum. |
| `spawn_tables[].points` | `[[f64,f64], ...]` | yes | At least 1 point. |
| `spawn_tables[].max_population` | integer | yes | — |
| `spawn_tables[].respawn_seconds` | integer | yes | — |
| `spawn_tables[].route_id` | string | no | If present, must match a `routes[].id` in the same manifest. |
| `routes[].id` | string | yes, if a route is present | Unique among `routes` in this manifest. |
| `routes[].waypoints` | `[[f64,f64], ...]` | yes | At least 2 points. |
| `routes[].loop` | bool | yes | — |
| `routes[].speed` | f64 | yes | Must be `> 0`. |
| `triggers[].id` | string | yes, if a trigger is present | Unique among `triggers` in this manifest. |
| `triggers[].shape.type` | string | yes | `"circle"` only for v0. |
| `triggers[].shape.center` | `[f64, f64]` | yes | — |
| `triggers[].shape.radius` | f64 | yes | Must be `> 0`. |
| `triggers[].event` | string | yes | Opaque to `content` — matched against plugin hook names (docs/PROPOSAL.md, "v0 Hooks") by the plugin host, not validated here. |
| `triggers[].one_shot` | bool | yes | — |

## `content-pack.yaml`

Bundles many zones for one game, versioned as a unit — only named in passing in the proposal, specified here for the first time:

```yaml
schema_version: 1
id: my-game
display_name: "My Game"
version: "0.1.0"
zones:
  - id: greenwood-forest
    path: zones/greenwood-forest/zone.manifest.yaml
  - id: stonebridge-village
    path: zones/stonebridge-village/zone.manifest.yaml
```

| Field | Type | Notes |
|---|---|---|
| `schema_version` | integer | Same framework compatibility contract as a zone manifest's, checked the same way. |
| `id` | string | The game's own slug. |
| `display_name` | string | Human-readable only. |
| `version` | string | The **game's own content version** — freeform (e.g. semver), never interpreted by the framework. Not to be confused with `schema_version`, which is the manifest *format's* version. |
| `zones[].id` | string | Must match that zone's own `zone.manifest.yaml` `id` field — the pack loader cross-checks this and fails if they disagree. |
| `zones[].path` | string | Path to the zone's manifest file, relative to the content-pack file's own directory. |

Cross-zone validation that only makes sense at the pack level happens here, not in a single zone manifest: every `links[].target_zone` in every bundled zone must resolve to another `zones[].id` in the same pack, or the pack fails to validate.

**Where this file lives:** same convention as `character`'s `stats.schema.yaml` (docs/specs/Data_Model_Spec.md) — a dev drops `content-pack.yaml` (and the `zones/` directory it references) into their config directory (`common::config::config_dir()`, `WZ_CONFIG_DIR` or `./config`), loaded via `ContentPack::from_config_dir()`.

## Content-addressing

`collision.asset_ref` (and any future binary-asset reference) is `sha256:` followed by the lowercase hex-encoded SHA-256 digest of the asset file's bytes — 64 hex characters, always lowercase, no other encoding. Two zones referencing the same imported geometry produce the same hash and therefore the same `asset_ref`, which is what makes this CDN/cache-friendly and dedup-free-by-construction (proposal, "Content-addressing"). The manifest loader validates the **shape** of the string (`sha256:` prefix + exactly 64 lowercase hex characters) since no asset store/importer pipeline exists yet to fetch and verify the referenced bytes against the hash — that check is future work once one does.

## `schema_version` semantics

Both `zone.manifest.yaml` and `content-pack.yaml` carry a `schema_version` — the manifest *format's* own compatibility contract, bumped only on a breaking change to the format itself, independent of any one game's content version. The loader checks this **before** attempting to parse anything else: an unrecognized version fails immediately with a clear "unsupported schema_version" error, never a best-effort partial parse. This is what "the `content` crate refuses to start a zone-service against a manifest schema version it doesn't understand, rather than guessing" (proposal) means concretely.

## `validate` CLI

- Runs against either a single `zone.manifest.yaml` or a whole `content-pack.yaml` (auto-detected by filename, or explicit) — no server process involved.
- Reuses the exact same parsing/validation code the runtime loader uses (`content::manifest`/`content::content_pack`) — never a second, parallel implementation that could silently drift from what the loader actually accepts.
- **Collects every problem it finds, not just the first one** — a dev fixing a manifest with three mistakes shouldn't have to run `validate` three times. Each line names the file, the field (dotted path, e.g. `spawn_tables[0].route_id`), and the specific problem.
- Exit code `0` on a clean validation, nonzero on any failure — usable directly as a CI gate, per the Developer Experience Bar's explicit callout that validation output quality here is the point of the ticket, not a nice-to-have.
