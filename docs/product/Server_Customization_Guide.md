# Server Customization Guide

This is the guide for turning `make quickstart`'s default game into *your* game — step by step, one crate's configuration surface at a time. It's distinct from two other things you might be looking for:

- [`Getting_Started_Developers.md`](Getting_Started_Developers.md) gets a server running with zero configuration. Read that first if you haven't yet — this guide assumes you already have `make quickstart` working.
- [`docs/specs/`](../specs) is deep per-service reference (wire protocols, full data model, decision rationale). This guide tells you *what to change and why*; the specs tell you everything about *how it works underneath*. Each step below links to the relevant spec for when you need that depth.
- [#60](https://github.com/LunarVagabond/WorldZero/issues/60) (once it lands) will add narrow, outcome-first walkthroughs — "add an NPC," "add a stat," "gate a zone behind a level." This guide is the broader map; those will be worked examples that live inside it.

**How to use this:** the steps are ordered the way you'd actually touch them building a real game — infrastructure first, then your game's data model, then world content, then everything else. Skip any step whose default already fits. Every env var and config file below is real and verified against the current codebase, not aspirational — where something is documented but not yet wired into the combined `server` process, that's called out explicitly rather than glossed over.

A full reference table of everything in this guide is at the bottom if you just want to skim.

**Checking your setup as you go.** `server` logs a real `INFO`-level line for the config that actually took effect, not just what you set: `worldzero server listening` (with the bound address, confirming [Step 4](#step-4--networking-gateway)'s `WZ_SERVER_ADDR`), plus `chat service enabled`/`disabled` and `metrics enabled`/`disabled` for [Step 6](#step-6--optional-services-chat-metrics)'s toggles. If metrics are on, `curl localhost:9090/metrics` (or wherever `WZ_METRICS_ADDR` points) is a quick liveness check. There's no equivalent startup log for a loaded stats schema today, but `server` does log `discovered plugin(s)` with a count at startup for plugins — beyond that, the first real signal is a client interaction actually working (a character loading with your declared stats, a plugin's `on-zone-loaded` NPC appearing).

---

## Step 0 — Infrastructure (`common`)

Every other step assumes this one is done. `common` is the one crate every other crate depends on — it owns Postgres/Redis connection config, structured logging, and the toggles for optional services.

**Required**, no defaults — the server refuses to start without these (copy [`.env.example`](../../.env.example) to `.env`):

| Var | Purpose |
|---|---|
| `WZ_POSTGRES_HOST`, `WZ_POSTGRES_PORT`, `WZ_POSTGRES_USER`, `WZ_POSTGRES_PASSWORD`, `WZ_POSTGRES_DATABASE` | Durable storage — accounts, characters, realms, everything that must survive a restart. |
| `WZ_REDIS_HOST`, `WZ_REDIS_PORT` (+ optional `WZ_REDIS_PASSWORD`) | Ephemeral storage — sessions, chat pub/sub, presence counters. |

**Optional, with defaults:**

| Var | Default | Purpose |
|---|---|---|
| `WZ_CONFIG_DIR` | `./config` | Where every config file in this guide (`stats.schema.yaml`, `zone.manifest.yaml`, `plugin.toml`, …) actually lives. Point this somewhere else if you want your game's config tracked in its own directory/repo. |
| `WZ_SERVICE_CHAT_ENABLED` | `true` | See [Step 6](#step-6--optional-services-chat-metrics). |
| `WZ_SERVICE_METRICS_ENABLED` | `true` | See [Step 6](#step-6--optional-services-chat-metrics). |
| `WZ_OTEL_ENDPOINT` | unset (disabled) | An OTLP gRPC collector address (e.g. `http://localhost:4317`). Unset means distributed tracing export is off entirely — its *presence* is the enable signal, there's no separate on/off flag. |
| `WZ_OTEL_SERVICE_NAME` | `"worldzero"` | The `service.name` every exported trace span carries. One value today since `server` is a single combined process (see [Step 7](#step-7--zone-layering-server) and [Step 8](#step-8--realms--transfers-real-but-not-yet-live) for what's still single-process). |

Nothing to build here — this step is entirely `.env`. See [`docs/specs/Observability_Spec.md`](../specs/Observability_Spec.md) for logging format and tracing details.

---

## Step 1 — Your game's stats (`character`)

This is the single most game-specific piece of configuration in the whole framework, and the one you should expect to spend the most time on. **The core ships with zero stats of its own — not even HP.** What your characters have, and what those stats mean, is entirely up to `<config_dir>/stats.schema.yaml`.

Start from [`config/stats.schema.example.yaml`](../../config/stats.schema.example.yaml):

```yaml
schema_version: 1
stats:
  - key: hp
    type: int
    default: 100
    min: 0
    max: 100
  - key: mana
    type: int
    default: 50
    min: 0
    max: 50
  - key: reputation.ironclad_guild
    type: int
    default: 0
```

Each entry under `stats` is one declared stat: `key` (any string — dotted keys like `reputation.ironclad_guild` are just a naming convention, not a nested structure), `type` (only `int` exists today), `default` (what an existing character gets for a key added *after* they were created — see below), and optional `min`/`max` (enforced on every write, not on read).

**What "declared" actually buys you:**
- A write to a key not in this file is rejected before it ever reaches storage — no silent typos becoming permanent state.
- A write outside `min`/`max` is rejected the same way.
- Reading a key a character's stored data doesn't have yet returns `default`, not an error or `null` — which means **adding a new stat to a live game needs no migration.** Every existing character implicitly already has it, at the default, until something writes a real value.
- Reading a key the schema doesn't declare at all is still rejected — same as a write.

None of this is a database schema change on your end — `stats` is one `JSONB` column, and `character::AttributeSchema` is the validator sitting in front of it. Full mechanism: [`docs/specs/Data_Model_Spec.md`](../specs/Data_Model_Spec.md#statsschemayaml-format).

**Also in this crate:**

| Var | Default | Purpose |
|---|---|---|
| `WZ_INVENTORY_MAX_ITEM_TYPES` | `40` | Caps the number of *distinct* item types a character can hold — the classic "N inventory slots" limit. Granting more of an already-owned type is never blocked by this, only a new stack is. |

---

## Step 2 — Your world (`content` + `world`)

**Zones and links (`content`).** Your zone(s) live in one of two places:

- `<config_dir>/zone.manifest.yaml` — one zone, the quickstart default ([`config/zone.manifest.example.yaml`](../../config/zone.manifest.example.yaml)).
- `<config_dir>/content-pack.yaml` — multiple zones, if present ([`config/content-pack.example.yaml`](../../config/content-pack.example.yaml) + zone files under [`config/example-zones/`](../../config/example-zones)). A player walking through a declared `links[]` edge between two zones crosses live, no reconnect.

A zone manifest declares its bounds, a navmesh collision reference, NPC spawn tables (entity type, population cap, respawn timer, optional patrol route), and interaction triggers. Full field reference: [`docs/specs/Content_Manifest_Spec.md`](../specs/Content_Manifest_Spec.md).

**World simulation tuning (`world`):**

| Var | Default | Purpose |
|---|---|---|
| `WZ_WORLD_TICK_RATE_HZ` | `20` | How often the simulation steps. |
| `WZ_WORLD_GRID_CELL_SIZE_METERS` | `25.0` | Spatial index granularity for broad-phase queries (collision, interest management). |
| `WZ_WORLD_MAX_SPEED_MPS` | `10.0` | Server-enforced movement speed cap — a generous walking/light-jog default. A game with mounts/sprint/dashes overrides this and layers per-ability speed on top at the plugin level. |

All three are read together via `WorldConfig::from_env()` — chunk them as one "simulation feel" pass.

---

## Step 3 — Authentication (`auth`)

No crate-specific env vars — `auth` only needs the `WZ_POSTGRES_*`/`WZ_REDIS_*` from [Step 0](#step-0--infrastructure-common) (accounts and password hashes in Postgres, session tokens in Redis).

The shipped default is `UsernamePasswordProvider` — self-contained accounts, zero external identity dependencies. If you want OAuth/SSO federation (Steam, Discord, Google, …) instead of or alongside it, that's a **provider**, not a fork: `auth::AccountStore`/`auth::AccountRoleStore` are traits, and a new provider implementing them plugs in without touching the session model or any downstream service. See [`docs/specs/Auth_Spec.md`](../specs/Auth_Spec.md) for the provider interface.

---

## Step 4 — Networking (`gateway`)

By default, `gateway` generates a self-signed TLS certificate under `<config_dir>/certs/` the first time it runs, and reuses it on every subsequent run — nothing to configure for local development.

| Var | Default | Purpose |
|---|---|---|
| `WZ_SERVER_ADDR` | `127.0.0.1:7900` | The bind address `gateway` listens on — change this to expose the server beyond localhost (e.g. `0.0.0.0:7900`). |

For a real certificate:

| Var | Purpose |
|---|---|
| `WZ_TLS_CERT_PATH` + `WZ_TLS_KEY_PATH` | Both required together (unset either one and you're back to the self-signed default). PEM files. |

---

## Step 5 — Plugins: your actual gameplay logic (`plugin-host`)

Configuration data models (Steps 1–2) describe *what exists*; plugins are *what happens*. This is where NPC behavior, custom interactions, and your own message types live. See [`Plugin_Development_Guide.md`](Plugin_Development_Guide.md) for the full write-it/build-it/deploy-it workflow (what language, how it gets built, how `server` picks it up) — this section is just the config surface.

A plugin is a `wasmtime`-sandboxed WASM component plus a manifest, [`config/plugin.example.toml`](../../config/plugin.example.toml):

```toml
[plugin]
name = "example-plugin"
host_api_version = "0.9.0"
capabilities = []
message_types = []
chat_commands = []
hooks = []
```

- `capabilities`: gates which host functions your plugin may call (`docs/specs/Plugin_API.md`'s "Capability gating") — `spawning` (`spawn-npc`), `movement` (`move-entity`), `combat` (`apply-stat-delta`/`report-death`/`report-respawn`), `economy` (`grant-item`/`remove-item`/`modify-currency`), `messaging` (`send-message`). **The default is strict**: an empty list grants none of these — `caller-role`/`plugin-state-get`/`plugin-state-set` are the only host functions always available regardless of what's declared. List only what your plugin actually needs; this is the real mechanism for running a less-trusted, third-party-authored plugin alongside your own trusted one.
- `message_types`: which gateway-routed message type IDs get delivered to your plugin's `on-message` hook. **Must each be ≥ 1000** — 0–999 is core-reserved (see [`docs/specs/Networking_Spec.md`](../specs/Networking_Spec.md)'s message catalog). Checked for collisions across every loaded plugin, not just within your own manifest (#152).
- `chat_commands`: command names (no leading `/`) routed to `on-chat-command` instead of published as ordinary chat. Same cross-plugin collision check as `message_types`.
- `hooks`: which of the other hooks (everything except `on-message`/`on-chat-command`, which route on the two fields above instead) you actually want the host to call — `[]` by default, meaning none (#152). See [`docs/specs/Plugin_API.md`](../specs/Plugin_API.md#pluginhooks-func-hooks) for the full list.

Wire it in:

| Var | Purpose |
|---|---|
| `WZ_PLUGINS_DIR` | Default `<config_dir>/plugins`. Every `<name>/{plugin.toml,*.wasm}` subdirectory found there is auto-discovered and loaded at startup — more than one plugin loads just by having more than one subdirectory (#152). A plugin loads exactly once for the whole process, not once per zone; every zone-specific hook takes a `zone-id` argument instead of the plugin being attached to one zone. |

`on-zone-loaded`, `on-message`, `on-interact`, `on-chat-command`, `on-player-join-zone`/`on-player-leave-zone`, `on-npc-tick`, `on-item-acquire`, and (as of #154) `on-damage-calc`/`on-item-use`/`on-npc-interact`/`on-death`/`on-respawn` are all live today — only the zone-wide `on-tick` hook still has no real call site, see [`docs/specs/Plugin_API.md`](../specs/Plugin_API.md)'s "Beyond this v0 slice." [`examples/example-plugin`](../../examples/example-plugin) is a real, minimal, copyable starting point — start there, not from scratch.

**Persistent plugin state (`plugin-state-get`/`plugin-state-set`, #149).** Quest flags, NPC memory, per-guild economy counters — anything your plugin needs to remember belongs here, not in a stat (Step 1 is for character stats the core validates against a schema; this is an opaque blob entirely yours). No config of its own — it's two host functions your plugin calls directly, scoped by a `plugin-state-scope` variant:

| Scope | Durable? | Use it for |
|---|---|---|
| `character(id)` | Yes (Postgres), hydrated at session join | Per-character plugin data — quest progress, unlocked recipes. |
| `zone(id)` | Yes (Postgres), hydrated at zone-actor startup | Per-zone plugin data — a boss's defeated state, a world event's phase. |
| `entity(id)` | No — in-memory only, gone on restart | Short-lived per-entity scratch state — an NPC's current patrol target. |

Reads never hit the database live from inside the call (same cache-hydrated-in-advance constraint as `caller-role`); writes land in the cache immediately and, for the two durable scopes, queue for the next tick's drain. Full mechanism: [`docs/specs/Plugin_API.md`](../specs/Plugin_API.md#the-plugin-scoped-data-store-plugin-state-getplugin-state-set-149).

---

## Step 6 — Optional services (`chat`, metrics)

Optional services are a **runtime config toggle**, not a compile-time feature — `server` always links every crate it supports; config decides at startup which ones actually stand up their routes/tasks/DB pool (see [Decision #91](https://github.com/LunarVagabond/WorldZero/issues/91)).

| Var | Default | Purpose |
|---|---|---|
| `WZ_SERVICE_CHAT_ENABLED` | `true` | Cross-shard messaging/channels. `false` means no `ChannelStore`, no `ChatBus`, no per-connection chat dispatch at all — not a listener left running with nothing to do. |
| `WZ_SERVICE_METRICS_ENABLED` | `true` | Prometheus-compatible `/metrics` endpoint. |
| `WZ_METRICS_ADDR` | `127.0.0.1:9090` | Only consulted when metrics are enabled. |

See [`docs/specs/Observability_Spec.md`](../specs/Observability_Spec.md) for what's actually exposed on `/metrics`.

---

## Step 7 — Zone layering (`server`)

Dynamic layer assignment ([#50](https://github.com/LunarVagabond/WorldZero/issues/50)) spreads a zone's population across parallel copies transparently — a player never sees layering happen. Two knobs, both read directly by `server::main`, not by a crate of their own:

| Var | Default | Purpose |
|---|---|---|
| `WZ_LAYER_ENABLED` | `true` | `false` pins every zone to exactly one layer forever, for deployments that don't want this at all (small player counts, or a game that needs every player in a zone able to see every other). |
| `WZ_LAYER_POPULATION_THRESHOLD` | `200` | Connected sessions per layer before a new one spins up. A small community server might want `10`; a large one `1000+` — that range is exactly why this is a runtime env var, not a hardcoded constant. |

---

## Step 8 — Realms & transfers (real, but not yet live)

`realm-directory` (realm CRUD, open-vs-bound character-binding policy, cross-realm consistency) is wired into the combined `server` process as of [#136](https://github.com/LunarVagabond/WorldZero/issues/136) — `transfer` (moving a character between bound realms, with gating and an audit trail) is a real, fully tested crate too, but **still isn't wired in**; there's no config file or env var for it yet because there's no live code path that would consume one.

**Required, no default** — `server` refuses to start without this:

| Var | Purpose |
|---|---|
| `WZ_REALM_ID` | The one realm this `server` process serves. Create it first — see below — and pass the id `make realm` prints. A single process serving more than one realm at once is [#130](https://github.com/LunarVagabond/WorldZero/issues/130)'s job, not this one's. |

**Optional, with a default:**

| Var | Default | Purpose |
|---|---|---|
| `WZ_REALM_LEASE_TTL_SECS` | `60` | [#21](https://github.com/LunarVagabond/WorldZero/issues/21)'s open-realm session lease TTL — only consulted when `WZ_REALM_ID` names an `open` realm; a `bound` realm never takes a lease. |

Create the realm `WZ_REALM_ID` will point at (and, for a multi-zone deployment, assign your zones to it) with `realm-directory`'s CLI over its real `RealmStore` — still the only way to create/manage realms themselves; there's no in-game or admin-API flow for that yet:

```sh
make realm ARGS="create MyRealm open"      # or "bound" — prints the new realm's id
make realm ARGS="list"
make realm ARGS="get <realm-id>"
make realm ARGS="assign-zone <realm-id> greenwood-forest"
```

See [`docs/specs/Realm_Character_Policy_Spec.md`](../specs/Realm_Character_Policy_Spec.md) for the full open-vs-bound design (including what a bound-realm login rejection looks like and how the open-realm lease works) and [`docs/specs/Data_Model_Spec.md`](../specs/Data_Model_Spec.md) for the schema.

---

## Reference table

| Crate | Env vars | Config file | Key types |
|---|---|---|---|
| `common` | `WZ_CONFIG_DIR`, `WZ_POSTGRES_*`, `WZ_REDIS_*`, `WZ_SERVICE_{CHAT,METRICS}_ENABLED`, `WZ_OTEL_*` | — | `PostgresConfig`, `RedisConfig`, `ServicesConfig` |
| `character` | `WZ_INVENTORY_MAX_ITEM_TYPES` | `stats.schema.yaml` | `AttributeSchema`, `InventoryConfig`, `CharacterStore` |
| `content` | — | `zone.manifest.yaml` or `content-pack.yaml` | `ZoneManifest`, `ContentPack` |
| `world` | `WZ_WORLD_TICK_RATE_HZ`, `WZ_WORLD_GRID_CELL_SIZE_METERS`, `WZ_WORLD_MAX_SPEED_MPS` | — | `WorldConfig` |
| `auth` | (shared Postgres/Redis only) | — | `UsernamePasswordProvider`, `AccountStore`/`AccountRoleStore` |
| `gateway` | `WZ_TLS_CERT_PATH`, `WZ_TLS_KEY_PATH` | — | `CertMaterial` |
| `plugin-host` | — | `plugin.toml` | `PluginManifest` |
| `chat` | (toggle lives in `common`) | — | `ChannelStore`, `ChatBus` |
| `server` | `WZ_SERVER_ADDR`, `WZ_METRICS_ADDR`, `WZ_LAYER_ENABLED`, `WZ_LAYER_POPULATION_THRESHOLD`, `WZ_PLUGINS_DIR`, `WZ_REALM_ID`, `WZ_REALM_LEASE_TTL_SECS` | — | — |
| `realm-directory` | (consumed via `server`'s `WZ_REALM_ID`/`WZ_REALM_LEASE_TTL_SECS` above) | — | `RealmStore`, `LoginPolicy`, `RealmPresence` |
| `transfer` | none (not wired into `server` yet) | — | `TransferExecutor`, `TransferGateStore`, `TransferAuditLog` |

## Where to go next

- [`docs/specs/`](../specs) for depth on any of the above — wire protocols, full data model, decision rationale.
- [`examples/example-plugin`](../../examples/example-plugin) to start writing gameplay logic.
- [`crates/server/tests/server_smoke.rs`](../../crates/server/tests/server_smoke.rs) is a real client speaking the wire protocol end to end — the best worked example of what a real client needs to do, since there's no GUI client shipped with this project (infrastructure, not a game — see `docs/PROPOSAL.md`'s "What This Project Is Not").
