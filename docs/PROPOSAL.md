# Project Proposal: An Open-Source MMO Server Framework

**Status:** Draft — living document, revised as design decisions are made
**Last updated:** 2026-08-21

**Contents:** [Executive Summary](#executive-summary) · [The Problem](#the-problem) · [The Solution](#the-solution-in-one-sentence) · [Design Principles](#design-principles-non-negotiables) · [Terminology](#core-concepts--terminology) · [Architecture](#architecture-overview) · [Crate Breakdown](#service--crate-breakdown) · [Realm & Character Policy](#realm--character-policy-model) · [World Content](#world-content-maps-npcs-and-routes) · [Networking](#networking) · [Plugin System](#plugin-system) · [Data Model Extensibility](#data-model-extensibility-declared-attribute-schemas) · [Observability](#observability--operations) · [Why Not SpacetimeDB](#why-not-spacetimedb-or-similar-all-in-one-platforms) · [Tech Stack](#technology-stack-current-thinking) · [Roadmap](#phased-roadmap) · [Non-Goals](#what-this-project-is-not-non-goals) · [Prior Art](#prior-art--positioning) · [Sustainability](#sustainability--business-model-considerations) · [Licensing](#licensing--attribution) · [Glossary](#glossary)

*New here? Read [Executive Summary](#executive-summary)–[Design Principles (Non-Negotiables)](#design-principles-non-negotiables) first — that's the whole pitch in five minutes. Everything after [Core Concepts & Terminology](#core-concepts--terminology) is the technical backing for it.*

---

## Executive Summary

Building an MMO used to require a small studio's worth of backend engineering before a single player ever saw the game: authentication, character persistence, world simulation, netcode, sharding, chat, matchmaking — all before "is this game fun" could even be tested. That cost is a large part of why the last decade produced very few new MMOs outside a handful of big-studio releases, even though the genre still has a large, underserved audience and indie/solo developers now have the tools to build everything *except* this layer.

This project is an open-source, self-hostable MMO server framework. It does for MMO backends roughly what a game engine does for rendering and physics: it owns the hard, genre-defining infrastructure — realms, sharding, world state, netcode, persistence, cross-server character policy — and exposes a plugin surface so a game developer can bring their own game logic without forking or fighting the core.

**The test for success:** not "a skilled team could use this" — several existing projects already clear that bar ([Prior Art & Positioning](#prior-art--positioning)). The actual test is a developer's gut reaction in the first few minutes: *"thank goodness I don't have to build this part, I just need to do XYZ,"* rather than *"I could just do this myself."* That second reaction is the real competitor, more than any other project's missing feature — see [The Developer Experience Bar](#the-developer-experience-bar) for what this demands concretely.

This is not a game. It is infrastructure for games, released as OSS, with an optional hosted/managed offering as a future sustainability path (see [Sustainability & Business Model Considerations](#sustainability--business-model-considerations)).

---

## The Problem

- **The backend is not fun to build, and it's genuinely hard to build well.** State sync, authoritative movement validation, cross-shard chat, session management, and character persistence are all deceptively complex, and getting them wrong produces the exploits and desync bugs that kill a young MMO's reputation permanently.
- **Existing options don't fit the gap.** General-purpose multiplayer backends (e.g. Nakama, Colyseus) are excellent for lobby-based or session-based multiplayer games but aren't opinionated about persistent-world, realm/shard, and cross-server character concerns that define the MMO genre specifically. Full game engines with networking layers (SpatialOS-style) tend to be heavyweight, commercially licensed, or defunct. Nothing occupies "MMO-specific, OSS, self-hostable, bring-your-own-game-logic."
- **The genre has gone quiet.** Without infrastructure that removes this cost, only well-funded studios can afford to attempt MMOs, and the genre's growth stalls.

---

## The Solution, In One Sentence

A modular, self-hostable server framework — auth, character/persistence, realm & shard/layer topology, world simulation, netcode, and a sandboxed plugin system — that a developer configures and extends rather than builds from scratch.

---

## Design Principles (Non-Negotiables)

1. **Control and flexibility over convenience shortcuts.** We will not adopt an all-in-one platform (e.g. an embedded reactive database/runtime) that trades away our ability to define the realm/sharding/persistence model ourselves. See [Why Not SpacetimeDB (or similar all-in-one platforms)](#why-not-spacetimedb-or-similar-all-in-one-platforms) for the SpacetimeDB evaluation and why it was rejected.
2. **The server owns simulation and truth; it does not own art.** Maps, meshes, textures, and audio are client-side or CDN-delivered concerns. The server only ever consumes a gameplay-only representation (collision, spawn tables, triggers, nav data).
3. **Policy, not hardcoding.** Anywhere the genre has two legitimate traditions (e.g. OSRS-style open realms vs. WoW-style realm-locked characters), the framework supports both as configuration, not as a fork-the-codebase decision. This extends to gameplay data itself: the core has no opinion on whether a game uses HP/Mana/Stamina, a single "vitality" pool, or something with no precedent yet — see [Data Model Extensibility: Declared Attribute Schemas](#data-model-extensibility-declared-attribute-schemas).
4. **Plugins are sandboxed, not trusted.** Game-specific logic runs in an isolated runtime with an explicit host API, so a third-party plugin cannot destabilize or compromise the core server or other games hosted on the same operator's infrastructure.
5. **Small, correct core; wide, optional edges.** The core ships the minimum viable set of authoritative systems. Everything genre- or game-specific is a plugin, not a core feature request.
6. **Boring, proven data infrastructure.** Postgres and Redis, not novel or unproven storage systems, for the parts of the stack where correctness and operational maturity matter most.
7. **Approachability beats architectural purity.** The bar isn't "a capable team could figure this out" — the competitive research in [Prior Art & Positioning](#prior-art--positioning) shows several projects already clear that bar. The bar is that a developer evaluating whether to hand-roll their own backend concludes it's not worth their time within minutes of looking at this project, not that they *could* eventually get there by reading enough docs. Any design decision that adds friction between clone and a running world is working against adoption, regardless of how architecturally sound it is — see [The Developer Experience Bar](#the-developer-experience-bar).

---

## Core Concepts & Terminology

| Term | Meaning |
|---|---|
| **Realm** | A named, independently operable instance of the game world (a "server" in classic MMO terms). Realms can be configured as *open* or *bound* (see [Realm & Character Policy Model](#realm--character-policy-model)). |
| **Shard** | A horizontal partition of a realm's world simulation — e.g. splitting a large continent across multiple simulation processes. |
| **Layer** | A dynamically-assigned parallel copy of a zone within a shard, used to spread player load without changing the logical world (OSRS-style). Transparent to the player. |
| **Zone** | The smallest unit of world simulation — one loaded map, owned by one zone-service instance at a time. |
| **Character binding** | The policy governing whether a character can log in through any realm (open) or is locked to the realm it was created on (bound). |
| **Transfer** | A governed, optionally ticket/cash-gated move of a character from one bound realm to another. |
| **Plugin** | Sandboxed, game-specific logic (combat rules, quest scripts, economy hooks, NPC behavior, etc.) running in the WASM plugin host against a defined API surface. |
| **Declared attribute schema** | A dev-authored definition of an entity's custom stat/property keys, types, and bounds — the mechanism that keeps gameplay data flexible without ever changing the core database schema (see [Data Model Extensibility: Declared Attribute Schemas](#data-model-extensibility-declared-attribute-schemas)). |

---

## Architecture Overview

```
                         ┌─────────────────────┐
                         │   Client (native)     │
                         │  Unity / UE5 / Godot   │
                         └──────────┬───────────┘
                                    │ TCP (reliable) + UDP (unreliable)
                         ┌──────────▼───────────┐
                         │       Gateway         │  connection termination,
                         │                       │  auth handoff, routing
                         └──────────┬───────────┘
              ┌─────────────────────┼─────────────────────┐
              │                     │                     │
      ┌───────▼──────┐     ┌────────▼───────┐     ┌───────▼───────┐
      │     Auth      │     │   Character    │     │  Realm Directory│
      │ (accounts,    │     │ (persistence,  │     │ (open/bound     │
      │  sessions)    │     │  inventory)    │     │  policy, layering,
      │               │     │                │     │  transfer rules) │
      └───────┬──────┘     └────────┬───────┘     └───────┬───────┘
              │                     │                     │
              └─────────────────────┼─────────────────────┘
                                    │
                       ┌────────────▼────────────┐
                       │     World / Zone         │  simulation, tick loop,
                       │  (one instance per zone/  │  spatial index, collision,
                       │   layer)                  │  authoritative movement
                       └────────────┬────────────┘
                                    │
                       ┌────────────▼────────────┐
                       │      Plugin Host          │  sandboxed WASM runtime,
                       │  (game-specific logic)     │  host API surface
                       └───────────────────────────┘

      Cross-cutting: Chat (pub/sub via Redis), Postgres (durable state),
      Redis (ephemeral/hot state, presence, queues)
```

---

## Service / Crate Breakdown

Framing in Rust terms (implementation language locked, see [Technology Stack (current thinking)](#technology-stack-current-thinking) — the module boundaries hold regardless):

| Crate | Responsibility |
|---|---|
| `auth` | Accounts, credentials, session tokens, cross-realm SSO — behind a pluggable provider interface (see [Auth Provider Architecture](#auth-provider-architecture)) |
| `character` | Character records, inventory, declared-schema stats (see [Data Model Extensibility: Declared Attribute Schemas](#data-model-extensibility-declared-attribute-schemas)) — the durable "who is this player" data |
| `realm-directory` | Realm registry, open/bound policy enforcement, layer assignment, transfer eligibility |
| `gateway` | Client connection termination (TCP + UDP), protocol framing, request routing to backing services |
| `world` | Per-zone simulation: tick loop, spatial index (see [Spatial Index: A → Z Roadmap](#spatial-index-a--z-roadmap)), authoritative movement/collision |
| `chat` | Cross-shard messaging, presence, channels |
| `transfer` | Character transfer execution, ticket/cash gating, audit trail |
| `plugin-host` | WASM runtime, sandboxing, host API surface exposed to plugins |
| `content` | Map/NPC/route manifest loading, versioning, and validation (see [World Content: Maps, NPCs, and Routes](#world-content-maps-npcs-and-routes)) |
| `common` | Tightly-scoped cross-cutting code shared by every other crate: logging setup, shared error/result types, config loading. Deliberately not a general-purpose dumping ground — see [Observability & Operations](#observability--operations) for the logging piece. |

Each is an independently deployable service; small self-hosted deployments can run several in one process, larger operators can scale them independently.

### Auth Provider Architecture

`auth` defines a **provider interface** rather than hardcoding a single identity model — the same "policy, not hardcoding" principle ([Design Principles (Non-Negotiables)](#design-principles-non-negotiables)) applied to identity. The core ships one concrete provider (self-contained username/password accounts) as the default, so the Phase 1 vertical slice has zero external identity dependencies. OAuth/SSO federation (Steam, Discord, Google, etc.) is then just another provider implementing the same interface, addable by the core team or the community without touching the rest of the auth crate, session model, or any downstream service. This trades a small amount of up-front design work for never having to retrofit federation into a hardcoded account model later.

### Spatial Index: A → Z Roadmap

`world` defines a `SpatialIndex` abstraction rather than committing to one data structure — plugins and future core work can swap implementations without touching simulation, collision, or interest-management code built against the interface. This mirrors the auth provider pattern: a thin, stable abstraction with a pluggable implementation behind it.

Rather than starting at the simplest possible thing and hoping to get to something polished, the roadmap states both ends up front:

- **A (baseline, Phase 1/2):** a hybrid index — the zone graph handles macro partitioning (which zone-service owns what), and a uniform grid handles micro spatial queries *within* a zone (broad-phase collision, interest management/who-sees-what). This is deliberately more capable than a bare zone-graph-only approach, since grid-based broad-phase is simple, well-understood, and good enough for real early load — not a placeholder that needs replacing before the framework is usable.
- **Z (target, Phase 3+):** a density-adaptive index (quadtree/octree, or an off-the-shelf spatial crate such as `rstar`) swapped in behind the same `SpatialIndex` trait once real deployments produce load data that justifies it — dense towns vs. sparse wilderness, dynamic entity counts, etc. External crates are acceptable here with pinned versions; this is not a place to reinvent well-solved computational geometry.

The point of stating both ends now: the grid-based baseline is never a dead end, it's a deliberate, documented step on a path the abstraction already supports.

---

## Realm & Character Policy Model

This is the most consequential design decision in the project, because it determines where character state authority lives.

- **Open realms** (OSRS-style): a character can log into any realm the operator runs. Character state must be globally authoritative — Postgres is the source of truth, no single realm process owns a character's data, and realm processes read/write through `character` with appropriate locking/versioning.
- **Bound realms** (WoW-style): a character belongs to exactly one realm. That realm may cache and own character state more aggressively, since it is never contended by another realm.
- **This is a per-realm-group configuration flag**, not a codebase fork. A single deployment could run a mix, though most operators will pick one model for their whole game.
- **Transfers** between bound realms are a distinct, explicitly governed operation (via the `transfer` crate) — never an implicit side effect of login. Transfers can be gated by an in-game ticket item, a real-money purchase, or left open, per operator configuration.

---

## World Content: Maps, NPCs, and Routes

The server never consumes renderable art. It consumes a **gameplay-only content manifest** — a small, versioned, canonical format the `content` crate loads at zone-service startup. This is deliberately similar to a database migration system: content is versioned, applied, and rollback-able, and the schema is treated as a stable public interface from day one (breaking it later is as costly as breaking a save-file format).

The manifest describes, per zone:
- **Boundaries** — the zone's collision/walkable geometry and its polygon or grid extent
- **Zone links** — portals/connections to neighboring zones (and, where relevant, other realms)
- **Spawn tables** — NPC types, spawn points, respawn timers, population caps
- **Routes** — NPC patrol/patrol-graph or waypoint data, consumed by plugin-driven AI rather than hardcoded in the core (the core provides the data structure and movement validation; behavior is a plugin concern)
- **Trigger volumes** — named regions the plugin host can bind event handlers to (enter/exit/interact)

**Authoring workflow:** developers use existing, familiar tools (Tiled for 2D/tile worlds, glTF-derived data for 3D collision/nav) and the project ships importers that convert those exports into the canonical manifest — the framework does not invent a new authoring tool or become an asset pipeline. Visual assets stay entirely client-side/CDN-delivered and never enter the server's content model.

**NPCs specifically:** the manifest defines *what exists and where* (spawn tables, routes); *how an NPC behaves* (combat AI, dialogue, quest logic) is plugin code bound to that NPC's type, keeping the core agnostic to game-specific NPC behavior while still owning the authoritative simulation of their position and state.

### Manifest Format & Example

One manifest file per zone (`zone.manifest.yaml`), plus a `content-pack.yaml` bundling many zones for a given game, versioned as a unit. Concrete shape:

```yaml
schema_version: 1          # manifest schema version — a framework compatibility contract
id: greenwood-forest
display_name: "Greenwood Forest"

bounds:
  shape: polygon
  coordinate_system: { units: meters, origin: [0, 0] }
  points: [[0,0], [500,0], [500,500], [0,500]]

collision:
  asset_ref: sha256:9f2a...        # content-addressed, imported from Tiled/glTF source
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

**Versioning & validation:** `schema_version` is the framework's own compatibility contract on the manifest format (bumped only on breaking changes to the format itself, independent of any one game's content). The `content` crate refuses to start a zone-service against a manifest schema version it doesn't understand, rather than guessing. A standalone `validate` CLI command ships as part of the toolchain so a dev can check manifests in their own CI before ever deploying — validation failures should be a local, fast feedback loop, not a runtime surprise.

**Content-addressing:** binary assets referenced by a manifest (collision/nav data) are referenced by content hash, not path — the same import producing the same hash means CDN/cache-friendly delivery and no duplicate storage across zones that happen to reuse geometry.

---

## Networking

Plain TCP (reliable) + plain UDP (unreliable), as two ordinary sockets — not QUIC.

- **Target clients are native game engines** (Unity, UE5, Godot, and similar), not browsers. That single fact drives this decision: it removes the reason QUIC was originally attractive (WebTransport gave browser clients a path to the same protocol as native ones) while leaving its cost intact — none of the target engines have QUIC built in, so every adopting developer would need to vendor a third-party QUIC library before their client could even connect. That's real friction against Design Principle #7 (approachability beats architectural purity).
- **TCP** carries auth, chat, inventory, trade, guild operations — anything where a dropped or reordered packet is unacceptable. TLS layers onto it trivially, and every target engine has native TCP+TLS support with zero extra dependencies.
- **UDP** carries high-frequency, loss-tolerant traffic — position updates, combat ticks. Also natively available in every target engine's socket API. **Encrypted via DTLS**, not sent in the clear. This is a security floor, not a nice-to-have: an operator self-hosting this framework is trusting it not to hand their players' game to network-level attackers — an unencrypted UDP channel is trivially sniffable and spoofable (forged movement packets, replay attacks, session hijacking), and "the docs mention that's a known gap" is not an acceptable answer to give someone running a real game on this. DTLS handles confidentiality/integrity/authenticity of the wire; it does *not* replace server-side authoritative validation of the data itself (§ [World Content](#world-content-maps-npcs-and-routes), the `world` crate) — a malicious *client's own* packets can still be encrypted and still be cheating, so both layers stay: DTLS stops outside tampering with the channel, authoritative server-side validation stops a bad actor lying through a legitimate connection.
- **What this gives up, honestly:** QUIC's per-stream independence (a stalled TCP byte-stream can hold up unrelated data the way independent QUIC streams wouldn't), 0-RTT reconnect, and connection migration. Real properties, just not worth the per-engine integration tax given who's actually adopting this.
- **Worth naming as a genre precedent, not just a technical convenience:** this is closer to how WoW's client protocol actually works than the earlier QUIC-based design was — a persistent reliable connection as the backbone, not a from-scratch netcode architecture. (WoW itself is close to all-TCP; the split here keeps a UDP channel available for games that want faster-than-TCP movement/combat updates, since not every game built on this framework will be WoW-style tab-target.)

Revisit trigger: if a specific game built on the framework needs QUIC's properties badly enough to justify per-engine integration work, or if QUIC support becomes native/trivial in Unity, UE5, and Godot later, this is the section to reopen.

---

## Plugin System

- Plugins run in a sandboxed WASM runtime (`wasmtime`), not as native code loaded into the server process. This means a third-party plugin cannot corrupt memory, crash the host process, or access anything outside an explicit host API surface.
- Plugin authors can write in any language that compiles to WASM (Rust, AssemblyScript, C/C++, etc.), lowering the barrier for adoption.
- Two clean, typed boundaries define the whole system: **hooks** (host calls into the plugin — "a player entered this trigger volume") and **host functions** (plugin calls out to the host — "apply 12 damage to this entity"). Nothing crosses that boundary except through an explicit, versioned interface — no direct DB access, no raw network access, no filesystem access, ever.
- This is the layer where "cool game devs make cool MMOs" actually happens: the core team ships infrastructure, the community ships game logic.

### Interface Technology

The plugin boundary is defined using the **WASM Component Model + WIT** (WebAssembly Interface Types), not a hand-rolled flat ABI. WIT generates typed bindings for every target language (Rust, AssemblyScript, C, etc.) from one interface definition, and — the part that matters for a project meant to last years — it gives real interface versioning via WIT "worlds" instead of hand-maintained ABI compatibility by convention. `wasmtime` has first-class support for this. A simpler flat C-style ABI (à la early Extism) would be faster to stand up but pushes the versioning/typing discipline onto us to maintain by hand indefinitely; not worth the short-term speed given how central this interface is.

### Plugin Manifest & Capability Declaration

Every plugin ships a manifest (`plugin.toml`, sibling to the content manifest pattern in [World Content: Maps, NPCs, and Routes](#world-content-maps-npcs-and-routes)) declaring: the host API version it targets, which hooks it implements, and which optional host-function capability groups it needs (e.g. `economy`, `combat`). The host only calls into hooks a plugin actually declares — plugins are not required to stub out event handlers they don't care about, and the host can refuse to load a plugin that requests a capability the operator hasn't enabled.

### v0 Hooks (host calls into the plugin)

The minimum set needed for a real, complete game to be built — not exhaustive, but nothing on the roadmap requires a category not listed here:

| Hook group | Examples | Ties to |
|---|---|---|
| Lifecycle | `on_load`, `on_unload` | Plugin startup/teardown per zone-service instance |
| World tick | `on_tick(zone, dt)` | `world` crate's simulation loop |
| Entity lifecycle | `on_entity_spawn`, `on_entity_despawn` | Generic entity model (covers players, NPCs, items) |
| Player session | `on_player_join_zone`, `on_player_leave_zone` | `gateway` / `world` handoff |
| Trigger volumes | `on_trigger_enter`, `on_trigger_exit`, `on_interact` | Content manifest trigger volumes ([World Content: Maps, NPCs, and Routes](#world-content-maps-npcs-and-routes)) |
| Combat | `on_damage_calc`, `on_death`, `on_respawn` | **Generic events over declared stats ([Data Model Extensibility: Declared Attribute Schemas](#data-model-extensibility-declared-attribute-schemas)).** Core has no notion of HP or a death condition — plugin logic decides both, and explicitly drives the corresponding host function (e.g. `apply_stat_delta`, `set_entity_state`) to make it happen. |
| NPC behavior | `on_npc_tick`, `on_npc_interact` | Route/spawn-table data from [World Content: Maps, NPCs, and Routes](#world-content-maps-npcs-and-routes); behavior itself is plugin-owned |
| Inventory/economy | `on_item_use`, `on_item_acquire` | `character` inventory data |
| Chat | `on_chat_command(name, args)` | `chat` crate, registered command names |

### v0 Host Functions (plugin calls out to the host)

What a plugin is actually allowed to *do*, grouped by capability (see [Plugin Manifest & Capability Declaration](#plugin-manifest--capability-declaration)):

- **Entity control:** query nearby entities (bounded by the `world` spatial index, [Spatial Index: A → Z Roadmap](#spatial-index-a--z-roadmap)), read/write declared entity stats ([Data Model Extensibility: Declared Attribute Schemas](#data-model-extensibility-declared-attribute-schemas)) — including a convenience `apply_stat_delta` helper for the common "reduce a plugin-specified stat by N" case — move/teleport an entity within authoritative bounds, spawn/despawn NPCs from a declared spawn table.
- **Messaging:** send a message to a specific client, broadcast to a zone or channel, emit a chat message.
- **Inventory/economy:** grant/remove items, query/modify currency — always through this API, never a direct write to `character`'s storage.
- **Scheduling:** register a callback N ticks or milliseconds in the future (cooldowns, DOT effects, timed respawns) — plugins never get their own thread or timer, everything is driven by the host's tick loop.
- **Plugin-scoped data store** ([Plugin-Scoped Data Store](#plugin-scoped-data-store)).
- **Logging/diagnostics:** structured log output surfaced through core observability tooling ([Observability & Operations](#observability--operations)), not raw stdout.

### Plugin-Scoped Data Store

A recurring problem for any plugin system: a plugin needs to persist *its own* custom data (quest progress, custom NPC state, a faction reputation score) without every plugin requiring a core schema migration. v0 solves this with a host function pair — `plugin_state_get(scope, key)` / `plugin_state_set(scope, key, value)` — where `scope` is `character`, `entity`, or `zone`, and the value is an opaque blob from the host's perspective. Backed by Postgres for character/zone scope (durable) and Redis for transient entity scope, but plugins never see or care which. This keeps the core schema stable indefinitely while plugins accumulate arbitrary state. [Data Model Extensibility: Declared Attribute Schemas](#data-model-extensibility-declared-attribute-schemas) generalizes this same idea into the primary mechanism for core gameplay stats, not just plugin bookkeeping.

### Explicit v0 Non-Goals

Deferred deliberately, not forgotten — revisit once real plugins exist and pressure-test v0:

- Cross-plugin RPC (plugins calling other plugins directly)
- Custom network packet types / raw protocol access
- Hot-reloading a plugin into a running zone-service without a restart
- Plugin-defined persistent schema (structured tables) beyond the opaque blob store in [Plugin-Scoped Data Store](#plugin-scoped-data-store)

---

## Data Model Extensibility: Declared Attribute Schemas

**The problem:** no two MMOs agree on what a character even *is*, statistically. One game wants HP/Mana/Stamina. Another wants a single "vitality" pool. Another has no health stat at all and tracks something the core team will never anticipate. A framework that hardcodes any of this — even something as apparently universal as HP — has quietly re-introduced lock-in through the back door, which directly contradicts Design Principle #3 ([Design Principles (Non-Negotiables)](#design-principles-non-negotiables)).

**The rejected approach: config-driven DDL.** The naive fix is to let a dev hand over a YAML file and have the framework literally generate `CREATE TABLE`/`ALTER TABLE` statements from it, giving every deployment its own bespoke schema. Rejected — this looks maximally flexible but is an operational trap: every deployment becomes structurally unique, core framework upgrades have to reason about arbitrary schema drift instead of one known shape, Rust's compile-time-checked query tooling (a real correctness advantage we want to keep) stops working once the schema isn't static, admin/ops tooling can't assume anything about the data it's looking at, and dynamically generating DDL/SQL from external config is a meaningfully larger security surface than it needs to be.

**The chosen approach: fixed core schema + declared attribute schema over a flexible column.** The `character` (and more generally, entity) table keeps a small, permanently fixed set of framework-required columns — identity, account linkage, realm, position, timestamps — because the framework itself depends on those existing in every deployment. Alongside that sits one `stats JSONB` column, which is schemaless from the database's point of view. The dev's YAML file is not DDL — it's a **declared attribute schema**: the list of valid stat keys, their types, defaults, and bounds for their specific game.

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

- `character` (and the equivalent for NPCs/items) validates every read/write against this declared schema at the API boundary — invalid keys or out-of-bounds values are rejected before they ever touch storage — but the underlying column shape never changes, in any deployment, ever.
- Postgres `JSONB` is genuinely well-suited here: it's indexable (GIN indexes) and queryable via native JSON operators, so this isn't "give up performance for flexibility" — it's a well-trodden pattern, not an exotic one.
- This is the exact same mechanism as the plugin-scoped data store in [Plugin-Scoped Data Store](#plugin-scoped-data-store), applied to the framework's own core gameplay data instead of only plugin bookkeeping — one consistent extensibility pattern used everywhere the framework would otherwise have to guess at a game's data model, rather than a different bolt-on invented per problem.
- **No stat is ever privileged by the core.** HP is not a special column — it's just a common key a dev's schema happens to define. Combat/death hooks ([v0 Hooks (host calls into the plugin)](#v0-hooks-host-calls-into-the-plugin)) are generic events over whatever stats a game declares; the framework provides the plumbing (validated read/write, event firing) and the plugin supplies all meaning (what counts as damage, what counts as death).
- **Evolving the schema over time** is low-risk by construction: adding a new stat key needs no migration (existing rows simply fall back to the declared default when a key is missing); removing or renegotiating a stat's meaning is the game developer's own concern, not a framework-level schema migration.
- This same declared-schema pattern is the intended answer for item properties and NPC properties too, not just character stats — documented here once as the general pattern rather than three separate ad hoc systems.

**Also considered and rejected: an EAV table** (`character_stat(character_id, key, value_type, value)`). Fully relational, real foreign key, feels more "DB-proper" than a JSON blob. Rejected because getting a character's *entire* stat bag — which happens on essentially every combat tick, the hottest read path in the server — costs N row reads or a pivot query under EAV versus one row fetch under JSONB, and EAV's "type safety" is mostly illusory in practice since it still needs a `value_type` discriminator interpreted in application code, the same soft-typing JSONB has.

**Note on the "isn't this just Mongo" concern:** it isn't, meaningfully. The critiques that make Mongo-as-primary-store risky — weak cross-document transactions, awkward joins, schema drift *everywhere* because nothing in the whole app enforces structure, vendor/licensing risk — don't apply here. The relational backbone (accounts, characters, realms, inventory, guilds, transfers) stays fully typed with real foreign keys and real transactions; JSONB is scoped to exactly one column, for exactly the one thing that's genuinely unknowable to the core team in advance. The sloppiness schemaless stores are (rightly) criticized for comes from undisciplined write paths, not the storage format — and there is exactly one write path into this column (the `character` crate's validated stat API), never raw SQL from any other service or plugin.

---

## Observability & Operations

What ships in core vs. what's left to the operator, stated explicitly so this doesn't drift into "the framework tries to also be a monitoring platform" scope creep (Design Principle #5, [Design Principles (Non-Negotiables)](#design-principles-non-negotiables)).

**Ships in core:**
- **Structured logging** (Rust's `tracing` ecosystem) uniformly across every service, including plugin log calls ([v0 Host Functions (plugin calls out to the host)](#v0-host-functions-plugin-calls-out-to-the-host)) routed through the same pipeline rather than raw stdout. Standard five levels — `TRACE`/`DEBUG`/`INFO`/`WARN`/`ERROR` — in a fixed `<TIMESTAMP> <LEVEL> <SOURCE> <MESSAGE>` line format across every service. `ERROR` is reserved for the core framework's own oncall-worthy failures (the level future alerting pages a human on); `WARN` is for things that can wait until morning. This severity discipline applies to core services, not plugin log calls — a plugin author can log however they want without it carrying the same operational weight. Full convention and rationale in `docs/specs/Observability_Spec.md`.
- **Metrics**, Prometheus-compatible, per service — tick duration, entity counts, connection counts, queue depths. Boring and standard on purpose: most self-hosters already have Prometheus/Grafana experience or tooling.
- **Distributed tracing**, OpenTelemetry-compatible spans across service boundaries. Given the multi-service architecture ([Architecture Overview](#architecture-overview)/[Service / Crate Breakdown](#service--crate-breakdown)), a single player action can cross `gateway` → `auth`/`character` → `world` → `plugin-host`; being able to follow one request across all of them is an operational necessity, not a nice-to-have, once something goes wrong in production.
- **Health/readiness endpoints** per service, for orchestration platforms (Kubernetes, or Agones specifically for game-server lifecycle — see [Prior Art & Positioning](#prior-art--positioning)).
- **A minimal admin/introspection API** — active zones, loaded plugin versions, connected player counts, recent transfer audit entries ([Realm & Character Policy Model](#realm--character-policy-model)). An API, not a bundled dashboard product.

**Explicitly left to the operator/community:**
- Any full dashboard UI — a reference Grafana dashboard JSON is a reasonable Phase 5 documentation deliverable, but building/maintaining a dashboard product is out of scope.
- Alerting rules and pipelines.
- Log aggregation infrastructure (ship structured logs in a standard format; don't ship an ELK stack).
- Player-behavior/business analytics beyond the basic counts above — that's a downstream concern for whoever operates the game, not the framework's job.

---

## Why Not SpacetimeDB (or similar all-in-one platforms)

Seriously evaluated and explicitly rejected for this project, documented here so the decision doesn't get re-litigated without cause:

SpacetimeDB (and platforms like it) solve real-time state synchronization — arguably the single hardest part of MMO netcode — by running game logic as transactional reducers inside the database itself, with clients reactively subscribed to state changes. That's a genuine capability advantage and would reduce initial build time substantially.

It was rejected because:
1. It is a replacement for this entire stack, not a supplement to Postgres/Redis — adopting it means designing the realm/character/transfer model *inside* its data and subscription model rather than freely.
2. Its horizontal-scaling story for sharding a single world across nodes is less mature than what can be built directly on Postgres + custom partitioning — and sharding/layering is a core requirement here, not a nice-to-have.
3. It would couple this OSS project's long-term fate to a single upstream vendor's roadmap, pricing, and licensing — an unacceptable risk for infrastructure other people's businesses will depend on.
4. It conflicts with Design Principle #1 ([Design Principles (Non-Negotiables)](#design-principles-non-negotiables)): control and flexibility over convenience.

Revisit trigger: if SpacetimeDB's scaling/self-hosting story matures significantly and a future maintainer wants to re-evaluate, this section is the place to start.

---

## Technology Stack (current thinking)

| Layer | Choice | Rationale |
|---|---|---|
| Implementation language | **Rust (locked)** | Performance, memory safety without a GC pause budget, first-class WASM tooling for the plugin host, mature async ecosystem (tokio) |
| Durable storage | PostgreSQL | Accounts, characters, inventory, guilds — anywhere correctness and transactions matter |
| Ephemeral/hot storage | Redis | Presence, session cache, matchmaking queues, cross-shard pub/sub |
| Transport | Plain TCP (reliable) + plain UDP (unreliable) | Native to every target engine (Unity, UE5, Godot) with zero extra dependencies — no per-engine QUIC library integration required |
| Plugin runtime | WASM (`wasmtime`) | Sandboxing, language-agnostic plugin authoring |
| Content format | Custom canonical manifest (JSON/TOML) + importers from Tiled/glTF | Keeps server gameplay-only; avoids becoming an asset pipeline |

Rust is locked as of 2026-08-21. The crate/service boundaries above were designed to hold regardless of language, which is part of why the decision was low-risk to make early.

---

## Phased Roadmap

The single biggest risk to this project is scope creep — this genre of ambitious backend project has a graveyard of predecessors that tried to do everything before anyone could use anything. The roadmap is deliberately conservative early on.

**Phase 0 — Design lock**
Finalize core data model (auth, character, content manifest schema, declared attribute schema), transport choice, plugin host API v0.

**Phase 1 — Vertical slice (single realm, no layering, no cross-realm transfer)**
Auth → character persistence → one zone-service instance running one map → minimal plugin hook (e.g. NPC spawn + one interaction) → one client able to connect, move, and persist state across sessions. This is the milestone that proves the core loop end-to-end.

**Phase 2 — Multi-zone & sharding**
Multiple zone instances, zone links/transitions, spatial partitioning at scale, realm-directory service goes live.

**Phase 3 — Layering & realm policy**
Dynamic layer assignment, open vs. bound realm configuration, cross-realm character visibility for open realms.

**Phase 4 — Transfers & governance**
Ticket/cash-gated character transfer between bound realms, audit trail, admin tooling.

**Phase 5 — Plugin ecosystem maturity**
Expanded host API surface, plugin packaging/distribution story, reference observability dashboards, documentation and examples aimed at lowering the barrier for a solo developer.

**Ongoing, from Phase 1 onward:** documentation quality, example game(s) built on the framework as a dogfooding proof point, and community onboarding — these are not "later," they're what makes the project actually get used.

### The Developer Experience Bar

The competitive research in [Prior Art & Positioning](#prior-art--positioning) found several architecturally solid alternatives (Redwood, OpenCoreMMO, Worldforge). None of them are approachable enough to change a developer's default instinct, which is to build their own backend. That instinct is the real thing being competed against — more than any specific missing feature. If a developer has to read an architecture document before they can see anything running, the project has already lost them to their own decision to hand-roll it instead.

This reframes what "done" means for Phase 1 — not just "the core loop works," but the following, treated as exit criteria with the same weight as the technical milestones above:

- **One command from clone to a running world.** A scaffold/init command producing a complete, runnable default game — one zone, a default declared stat schema, one example plugin — with zero required configuration. First impression is "it already works," not "now configure these twelve things."
- **A stated, measured time budget:** clone → two players able to see each other move in a locally running world, well under 30 minutes, ideally under 10. This is a hard product constraint on Phase 1, not an aspiration — a design decision that threatens this number gets reconsidered, not shipped anyway.
- **Docs written outcome-first** ("add a new NPC," "add a new stat," "gate a zone behind a level"), not architecture-first. This document is for people extending the framework's internals; it is explicitly not what a new adopter should have to read first.
- **The shipped example/starter game is not a toy — it's the thing most devs copy and modify.** Most adopters will never read [Architecture Overview](#architecture-overview)–[Technology Stack (current thinking)](#technology-stack-current-thinking) in depth; they'll clone the starter game and start replacing pieces. That path has to work well, because for most users it effectively *is* the framework.
- **Validation and error messages are a first-class UX surface, not an afterthought.** The `validate` CLI ([Manifest Format & Example](#manifest-format--example)) and plugin manifest loader ([Plugin Manifest & Capability Declaration](#plugin-manifest--capability-declaration)) need actionable, specific errors — a cryptic failure at minute two is exactly the moment a developer reaches for "I'll just build my own."

---

## What This Project Is Not (Non-Goals)

- Not a game engine — no rendering, no client-side physics, no asset pipeline.
- Not an authoring tool — content is authored in existing tools (Tiled, glTF-producing 3D tools) and imported.
- Not a matchmaking-only or lobby-based multiplayer backend — the target is persistent, shared, authoritative worlds, which is a different (harder) problem than session-based multiplayer.
- Not, at least initially, a hosted SaaS — this ships as self-hostable OSS first; a managed offering is a possible later sustainability path, not the initial product.
- Not a dashboard/observability product — core ships metrics, logs, and traces; visualization is left to standard tooling ([Observability & Operations](#observability--operations)).

---

## Prior Art & Positioning

Before committing further design effort, we deliberately asked "does this already exist?" and researched it rather than assumed. Findings below, including the one genuine near-competitor.

| Project | What it is | Why this project is different |
|---|---|---|
| Nakama | OSS game backend (matchmaking, leaderboards, social) | Session/lobby-oriented, not persistent-world/realm-oriented |
| Colyseus | OSS room-based multiplayer server (JS/TS) | Room-based abstraction, not built around realms/shards/layers or persistent world state |
| SpatialOS | Cloud-based distributed simulation platform | Commercial, heavyweight, largely defunct as an indie option |
| Agones | Kubernetes game server orchestration | Solves *deployment/scaling of server processes*, not game state/world model — complementary, not competing; could sit underneath this project's ops story |
| SpacetimeDB | Reactive database + module runtime | Solves state sync elegantly but replaces this whole stack; see [Why Not SpacetimeDB (or similar all-in-one platforms)](#why-not-spacetimedb-or-similar-all-in-one-platforms) |
| **Redwood** (redwoodmmo.com) | Self-hosted MMO backend with automatic sharding/layering, Kubernetes/Pulumi deployment. Epic MegaGrant-funded, actively developed — the closest real competitor found. | **Not actually OSS** — backend source is gated behind a commercial EULA with revenue-royalty obligations (from $295, full source is a separate paid tier), and tooling is Unreal Engine-specific. This project's core differentiators against it are exactly those two gaps: genuinely free/Apache-2.0 with no royalty, and engine/client-agnostic rather than Unreal-first. |
| OpenCoreMMO | OSS MMORPG server with a Lua plugin system, Postgres-backed | Rooted in the OTServer/Tibia emulator lineage ("emulator," "Revscript") — modernizes an existing genre/protocol rather than being a blank-slate framework for an arbitrary game a dev brings their own client to |
| Worldforge | OSS, generic, engine-agnostic MMO framework with Python-scripted world logic — running since 1998 | The closest philosophical predecessor, and a genuine cautionary tale: 25+ years old and never reached mainstream adoption. Its longevity says the idea isn't crazy; its lack of traction says execution and scope discipline are what actually matter — directly why [Phased Roadmap](#phased-roadmap)'s roadmap leads with an aggressively narrow vertical slice rather than a from-scratch generic engine. |

**Verdict:** nothing occupies the specific intersection this project targets — truly free OSS (no royalty/EULA gate), engine/client-agnostic, generic across arbitrary games, with realm/shard/layer semantics as first-class concepts, and a sandboxed plugin system. Redwood is evidence there's real commercial appetite for this category (worth studying its architecture docs directly before Phase 1 design lock). Worldforge is evidence that the idea alone isn't enough — disciplined scope and a working example game matter as much as the engineering.

**The sharper differentiator, on reflection, isn't license or engine-agnosticism — it's approachability.** Redwood and OpenCoreMMO are both architecturally solid. That's not the gap. The gap is that looking at either one, a capable developer's honest first reaction is plausibly "I could just build this myself" — not "thank goodness I don't have to." Competing on features or architecture against solid prior art is a slow, uncertain fight; competing on *"a developer never seriously considers hand-rolling this instead"* is a sharper, more defensible position, and it's a UX/product commitment as much as an engineering one. See [The Developer Experience Bar](#the-developer-experience-bar) for what that requires concretely.

---

## Sustainability & Business Model Considerations

The framework itself is and remains OSS — that's core to the adoption thesis (Joe Smoe reads the proposal, clones the repo, is playing within a weekend, no license negotiation). Possible future sustainability paths, none of which compromise the OSS core, to be revisited once there's real adoption:

- **Hosted/managed control plane** — operators who don't want to run their own Postgres/Redis/orchestration can pay for a managed version, similar to how many OSS databases monetize.
- **Plugin marketplace** — a discovery/distribution layer for community plugins, potentially with a revenue share for paid plugins, analogous to asset stores in commercial engines.
- **Support/enterprise contracts** for studios building on the framework who need SLAs.

None of these should shape core architecture decisions this early — flagged here so the option exists later without needing an architecture rewrite.

---

## Licensing & Attribution

**Core license: Apache-2.0.**

The requirement was: permissive enough that anyone can freely use, modify, maintain, and change the framework, with attribution as the only real condition. That is precisely what Apache-2.0 (and MIT) already guarantee — both legally require the original copyright and license notice to be preserved in any redistribution, including forks, which is enforceable attribution at the source level. Apache-2.0 was chosen over MIT specifically for the explicit patent grant it adds (protects both contributors and adopters from patent claims — meaningfully de-risks adoption by companies with legal review processes).

**No separate `NOTICE` file.** Apache-2.0 supports one (§4(d): if a NOTICE file exists, its attribution text must be carried forward in redistributions), but it's optional, not required — the license's only hard requirement is including a copy of LICENSE itself. A NOTICE file was carried for a while as a deliberate attribution mechanism, then dropped in favor of keeping the repo root leaner; LICENSE's own copyright line covers the same ground with less to maintain. Revisit if a concrete case for the extra §4(d) enforcement shows up later.

Rejected: copyleft (AGPL-3.0) and source-available/BSL-style licenses. Both were considered because they more aggressively protect against a third party taking the project and closing it off — but both directly conflict with the adoption thesis in [Executive Summary](#executive-summary) ("Joe Smoe clones it and is playing within a weekend, no friction"). AGPL's network-use compliance burden and BSL's competing-use restriction both introduce exactly the kind of legal friction that stops a solo developer from adopting something on a Saturday afternoon.

**Visible/in-product attribution** (e.g. a "Powered by [Project]" credit players might see) is a different concern from the code license and is handled separately as a **trademark and branding policy**, not baked into the license terms:
- The project name and logo are protected as a trademark, independent of the Apache-2.0 grant on the code itself (Apache-2.0 explicitly does not grant trademark rights).
- A "Powered by" badge/credit convention is documented and encouraged for operators, but not legally mandated — keeping the code license itself frictionless while still giving the project visible attribution in the wild.
- Full trademark policy text is a Phase 1 documentation deliverable, not a blocker for writing code.

---

## Glossary

See [Core Concepts & Terminology](#core-concepts--terminology) for core terminology. Additional terms will be appended here as they're introduced during design work, so this stays the single reference point for anyone new to the project.
