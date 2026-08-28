# Plugin API

Corresponds to [Plugin System](../PROPOSAL.md#plugin-system) in the proposal.

**Status:** the v0 slice below is real and implemented (`plugin-host`, #37/#38, extended by #95, #116, #57, #124, #149, #155, #154, #153, #152, #194, #197, #211, #214, #216, #218, #168, and #186) — one WIT world with nineteen hooks and fourteen host functions: #38's original NPC-spawn-plus-interaction slice, #116's combat/NPC-patrol/chat-command hooks, #57's inventory/economy hooks and host functions (#112 supplied the core storage these write through), #124's `caller-role` (the account-roles decision, #114), #149's `plugin-state-get`/`plugin-state-set` (the Plugin-Scoped Data Store), #155's `on-player-join-zone`/`on-player-leave-zone`, #154's real client-protocol call sites for `on-damage-calc`/`on-item-use`/`on-npc-interact` plus `report-death`/`report-respawn` (the plugin-owned trigger for `on-death`/`on-respawn`), #153's real capability gating, #152's real multi-plugin support (process-wide loading, per-hook opt-in, `on-zone-loaded`), #194's `on-character-create`/`apply-stat-delta-for-character` (the character-creation extension point — starting stats, archetype/preset systems built as a plugin rather than a core class/race enum), #197's NPC-targetable stats (`apply-stat-delta` now resolves for an NPC entity too, through in-memory storage instead of a character row — see "NPC-targetable stats" below), #211's automatic `StatChanged`/`ItemChanged`/`CurrencyChanged` client pushes (see "Automatic client pushes" below) closing the gap where a successful write never told the connected client anything without the plugin inventing its own convention, #214's real `on-entity-spawn` wiring plus its `spawn-table-id` correlation parameter (see "Entity correlation via on-entity-spawn" below) — `on-entity-spawn` existed in this interface well before #214 but nothing ever actually called it; #214 is what made it live — #216's `on-craft-complete` (implementing #215's generic crafting decision — see "Crafting" below), #218's `currency-key` parameter on `modify-currency` (a dev-declared, possibly multi-currency system replaced the old implicit single balance), #168's `on-tick` (the zone-wide tick hook, wired to a real call site for the first time — see its row below), and #186's `block-zone-channel` (the block/restriction primitive for `server`'s zone-entry chat auto-join, see "Zone-scoped chat auto-join" below). See "Beyond this v0 slice" below for what's still not here.

## Interface technology

WASM Component Model + WIT (docs/PROPOSAL.md, "Interface Technology") — the actual interface lives in `crates/plugin-host/wit/plugin.wit`, this doc explains and indexes it rather than duplicating it verbatim (the `.wit` file is the source of truth for the exact signatures).

- **Host side:** `wasmtime::component::bindgen!` generates Rust bindings from `wit/plugin.wit` directly into `plugin-host` at compile time (`crates/plugin-host/src/bindings.rs`).
- **Guest side:** a plugin author uses `wit-bindgen`'s guest macro (`wit_bindgen::generate!`) for Rust, or the equivalent for any other language `wasmtime`/the Component Model toolchain supports.
- **Target:** `wasm32-wasip2` — this target directly emits a Component Model binary (no separate "componentize a core module" step needed), and comes with the WASI Preview 2 CLI imports a Rust std binary needs to start at all.

## The `plugin` world (v0)

```wit
package worldzero:plugin@0.12.0;

interface host {
    spawn-npc: func(spawn-table-id: string) -> result<string, string>;
    send-message: func(target-entity-id: string, body: string) -> result<_, string>;
    apply-stat-delta: func(entity-id: string, stat-key: string, delta: s64) -> result<_, string>;
    apply-stat-delta-for-character: func(character-id: string, stat-key: string, delta: s64) -> result<_, string>;
    move-entity: func(entity-id: string, x: f64, y: f64) -> result<_, string>;
    grant-item: func(entity-id: string, item-type: string, quantity: s64) -> result<_, string>;
    remove-item: func(entity-id: string, item-type: string, quantity: s64) -> result<_, string>;
    modify-currency: func(entity-id: string, currency-key: string, delta: s64) -> result<_, string>;
    caller-role: func(entity-id: string) -> result<list<string>, string>;

    variant plugin-state-scope {
        character(string),
        entity(string),
        zone(string),
    }
    plugin-state-get: func(scope: plugin-state-scope, key: string) -> result<option<list<u8>>, string>;
    plugin-state-set: func(scope: plugin-state-scope, key: string, value: list<u8>) -> result<_, string>;

    report-death: func(entity-id: string) -> result<_, string>;
    report-respawn: func(entity-id: string) -> result<_, string>;

    block-zone-channel: func(entity-id: string, category: string) -> result<_, string>;
}

interface hooks {
    on-load: func();
    on-unload: func();
    on-zone-loaded: func(zone-id: string);
    on-character-create: func(character-id: string, zone-id: string);
    on-entity-spawn: func(zone-id: string, entity-id: string, entity-type: string, spawn-table-id: string);
    on-player-join-zone: func(zone-id: string, entity-id: string);
    on-player-leave-zone: func(zone-id: string, entity-id: string);
    on-interact: func(zone-id: string, trigger-id: string, actor-entity-id: string);
    on-message: func(zone-id: string, message-type: u16, sender-entity-id: string, payload: list<u8>);
    on-damage-calc: func(zone-id: string, attacker-entity-id: string, target-entity-id: string, stat-key: string, base-amount: s64);
    on-death: func(zone-id: string, entity-id: string);
    on-respawn: func(zone-id: string, entity-id: string);
    on-tick: func(zone-id: string, dt: f64);
    on-npc-tick: func(zone-id: string, entity-id: string, x: f64, y: f64, route-waypoints: list<tuple<f64, f64>>, route-loop: bool, route-speed: f64, dt: f64);
    on-npc-interact: func(zone-id: string, npc-entity-id: string, actor-entity-id: string);
    on-chat-command: func(zone-id: string, command: string, args: string, sender-entity-id: string);
    on-item-acquire: func(zone-id: string, entity-id: string, item-type: string, new-quantity: s64);
    on-item-use: func(zone-id: string, entity-id: string, item-type: string);
    on-craft-complete: func(character-id: string, recipe-key: string);
}

world plugin {
    include wasi:cli/imports@0.2.12;
    import host;
    export hooks;
}
```

`worldzero:plugin@0.12.0` is the actual versioning mechanism (docs/PROPOSAL.md, "Interface Technology": WIT "worlds" give real interface versioning). A breaking change to this interface bumps the package version and/or introduces a new world; a plugin manifest declares which `host_api_version` it targets (`plugin.toml` below), and `plugin-host` refuses to instantiate a plugin declaring a version it doesn't implement (`crates/plugin-host/src/manifest.rs::PluginManifest::check_compatible`) — it never silently links a plugin against an interface shape it wasn't built for. `0.2.0` added `on-message` (#95); `0.3.0` added the combat/NPC-patrol/chat-command hooks and `apply-stat-delta`/`move-entity` (#116); `0.4.0` added the inventory/economy hooks and host functions below (#57); `0.5.0` added `caller-role` (#124); `0.6.0` added `on-player-join-zone`/`on-player-leave-zone` (#155); `0.7.0` added `report-death`/`report-respawn` and real client-protocol call sites for the three hooks below that previously had none (#154); `0.8.0` added `on-zone-loaded` and a `zone-id` parameter to every other zone-specific hook (#152 — see "Multi-plugin support" below for why); `0.9.0` added `on-character-create` and `apply-stat-delta-for-character` (#194 — see "Character creation" below); `0.10.0` bundles three independent breaking changes that landed together — a `spawn-table-id` parameter on `on-entity-spawn`, wiring the hook up for real (it previously existed in this interface with nothing calling it, #214 — see "Entity correlation via on-entity-spawn" below), `on-craft-complete` (#216 — see "Crafting" below), and a `currency-key` parameter on `modify-currency` (#217/#218 — a dev-declared, possibly multi-currency system replaced the old implicit single balance); `0.11.0` added `on-tick` (#168 — see its row below), wiring a hook that previously existed only on paper in this doc's "Beyond this v0 slice" section; `0.12.0` added `block-zone-channel` (#186 — see its row below), the block/restriction primitive a dev calls to keep an entity from auto-joining a zone-scoped chat channel category. A plugin declaring an older version is refused, not silently linked against the new shape.

### Why `include wasi:cli/imports`

A `wasm32-wasip2` Rust binary needs baseline WASI Preview 2 imports (clocks, random, stdio, ...) to start at all — that's a Rust-runtime requirement, not a capability grant. The actual sandboxing comes from what the **host** provides at instantiation, not from omitting WASI: `plugin-host` builds each plugin's `WasiCtx` via `WasiCtxBuilder::new().build()` with nothing else configured — no preopened directories, no sockets, no inherited stdio/env/args. This is WASI Preview 2's own capability-based security model (the same one `wasmtime` documents as its intended sandboxing mechanism), not a hand-rolled restriction layer. Verified directly: `crates/plugin-host/tests/plugin_sandbox.rs`'s `a_plugin_cannot_read_the_filesystem_with_no_preopens_granted` loads a real compiled plugin that attempts `std::fs::read_to_string` and confirms it fails.

### Hooks (host calls into the plugin)

| Hook | Signature | Fires when |
|---|---|---|
| `on-load` | `func()` | The plugin is instantiated, once for the whole `server` process (#152 — see "Multi-plugin support" below). Zero-arg: nothing zone-specific has happened yet. |
| `on-unload` | `func()` | Before the plugin is torn down, once for the whole process. |
| `on-zone-loaded` | `func(zone-id)` | **Live (#152).** Fires once per zone-service as it starts — the per-zone equivalent of `on-load` (e.g. seeding that zone's NPCs via `spawn-npc`), since `on-load` itself no longer has a zone to act on. |
| `on-character-create` | `func(character-id, zone-id)` | **Live (#194).** Fires once, right after a new character row is created (`server::character_protocol`'s `CreateCharacter`, #193) and before the client's `CharacterCreated` acknowledgement — no entity/session exists yet, hence `character-id` rather than an entity id, and why setting starting stats here means calling `apply-stat-delta-for-character` below, not `apply-stat-delta`. `zone-id` is the character's starting zone (the deployment's default). See "Character creation" below. |
| `on-entity-spawn` | `func(zone-id, entity-id, entity-type, spawn-table-id)` | **Live (#214).** Fires back to a plugin right after an NPC it requested via `spawn-npc` is actually spawned (`server::world_actor::spawn_npc_from_table`'s caller) — `entity-type` matches `content::manifest::SpawnTable`'s `entity_type` (e.g. `"npc.wolf"`), `spawn-table-id` is the id that specific `spawn-npc` call passed, the correlation token described in "Entity correlation via on-entity-spawn" below. Only fires back to the requesting plugin, not a fan-out to every loaded plugin (unlike most hooks in this table) — a plugin that spawned nothing has no correlation token to receive. Does **not** fire for a player entity joining a zone; that's `on-player-join-zone` below, a distinct event with distinct timing. |
| `on-player-join-zone` | `func(zone-id, entity-id)` | **Live (#155, #233).** Fires once a player's connection has fully joined `zone-id` — after roster delivery, so a `send-message` call made from inside this hook reaches a client that's actually ready. Fires for every zone a player is ever in: the initial connection, and (as of #233) the arrival side of a mid-session zone-link crossing, not just at session start — regardless of what the plugin actually does with `zone-id`; a zone-service with no plugin configured never calls this at all. |
| `on-player-leave-zone` | `func(zone-id, entity-id)` | **Live (#155, #233).** Fires on a player's clean disconnect from `zone-id`, same entity id — and, as of #233, right before a mid-session zone-link crossing carries the player out of `zone-id` into a different zone, not just at final disconnect. `server::session` awaits this hook (and any pending effects it triggers) actually running before it clears that entity from its own bookkeeping (on disconnect) or switches over to the new zone (on a zone-link crossing), so the plugin can still resolve the departing entity's character (e.g. to `apply-stat-delta`) from inside the handler. |
| `on-interact` | `func(zone-id, trigger-id, actor-entity-id)` | A player's entity enters a trigger volume in `zone-id` whose manifest `event` is an interact-style event (`content::manifest::Trigger`). |
| `on-message` | `func(zone-id, message-type, sender-entity-id, payload)` | The gateway receives an envelope whose `message_type` matches one of this plugin's declared `message_types` (below) — `zone-id` is the sending connection's current zone, `sender-entity-id` is its own entity, `payload` is the envelope's opaque bytes (#95). |
| `on-damage-calc` | `func(zone-id, attacker-entity-id, target-entity-id, stat-key, base-amount)` | **Live (#154).** Fires when a client sends an `Attack` action (`server::session_protocol`) naming another entity — the server confirms the target actually exists in `zone-id` (`world::Zone::kind_of`) before ever calling this, an unknown/vanished target is dropped, never passed through. `base-amount` is always `0` — the core never invents a damage number (docs/PROPOSAL.md, "v0 Hooks": "core has no notion of HP or a death condition"); the plugin owns the whole mitigation formula and must call `apply-stat-delta` itself, this hook alone changes nothing. `stat-key` is whatever the client requested, an opaque game-defined string like every other `stat-key` in this interface. |
| `on-death` | `func(zone-id, entity-id)` | **Live (#154).** Fires once a `report-death` request this plugin made is actually applied (below) — the plugin decided this entity died and reported it; this hook is the host's confirmation callback, not a request for the plugin to decide anything. |
| `on-respawn` | `func(zone-id, entity-id)` | **Live (#154).** Same shape as `on-death`, fired after a `report-respawn` request is applied. |
| `on-tick` | `func(zone-id, dt)` | **Live (#168).** Called once per simulation tick, per zone, regardless of NPC count — the zone-wide counterpart to `on-npc-tick` below (that one fires once per route-NPC; this fires exactly once per zone-service per tick). `server::world_actor` drives this, once per plugin that declared `on-tick` in `hooks` (below). Fires after that tick's queued moves are already applied and after this same tick's `on-npc-tick` fan-out has run for that plugin — a plugin declaring both hooks sees this tick's NPC moves already queued before its own `on-tick` body runs. Intended for time-of-day, periodic world events, or aggregate bookkeeping on a fixed cadence rather than per-entity. |
| `on-npc-tick` | `func(zone-id, entity-id, x, y, route-waypoints, route-loop, route-speed, dt)` | **Live.** Called once per tick, per NPC entity in `zone-id` whose spawn table declared a `route_id` (`content::manifest::SpawnTable`/`Route`) — `world::world_actor` drives this. The host never moves the NPC itself; the plugin decides the next position and calls `move-entity`. |
| `on-npc-interact` | `func(zone-id, npc-entity-id, actor-entity-id)` | **Live (#154).** Fires when a client sends an `InteractNpc` action naming a specific NPC entity in `zone-id` — distinct from the generic trigger-volume `on-interact` above. The server confirms the target actually is a currently-spawned NPC (`world::Zone::kind_of`) before ever calling this. |
| `on-chat-command` | `func(zone-id, command, args, sender-entity-id)` | **Live.** Fires when a chat `Send`'s body starts with `/command` and `command` (without the slash) matches one of this plugin's declared `chat_commands` (`plugin.toml` below) — `server::session` matches before the message ever reaches `chat_session`/gets published as ordinary chat. |
| `on-item-acquire` | `func(zone-id, entity-id, item-type, new-quantity)` | **Live.** Fires after a `grant-item` call this same plugin made is actually applied — `world::world_actor` calls this itself right after a queued grant lands, so (unlike most of this interface) a plugin can treat this as real confirmation, not just "the call was queued." `new-quantity` is the item type's new total, not the delta granted. |
| `on-item-use` | `func(zone-id, entity-id, item-type)` | **Live (#154).** Fires when a client sends a `UseItem` action naming `item-type` (an opaque string). The server never validates ownership itself — the plugin decides what using it does, typically by calling `remove-item` itself in response. |
| `on-craft-complete` | `func(character-id, recipe-key)` | **Live (#216).** Fires once a `CraftItem` request (`server::session_protocol`) successfully consumes its declared inputs and grants its declared output — post-craft notification only, no veto (#215's decision deliberately defaults to post-craft-only for v0). No entity/zone id, same reasoning as `on-character-create`: a craft is character-scoped, not entity-scoped. `recipe-key` is the same key the request named, resolved against the dev-declared `crafting.schema.yaml` (docs/specs/Data_Model_Spec.md's "Crafting" section). See "Crafting" below. |

Every plugin *exports* all nineteen — a WIT world's exports aren't individually optional, so the interface itself has no per-hook opt-out. What a plugin actually wants *called* is a manifest-level concern instead: see `plugin.toml`'s `hooks` field below.

### Host functions (plugin calls out to the host)

| Function | Signature | Does |
|---|---|---|
| `spawn-npc` | `func(spawn-table-id: string) -> result<string, string>` | Requests one NPC spawn from a zone's declared spawn table (`content::manifest::SpawnTable`). The `Ok` string is **not** the real entity id — it's `spawn-table-id` echoed back, since the host can't synchronously assign a real id from inside this sandboxed call (the entity is only created once the caller drains the request afterward). To learn the real id — and correlate it back to this specific call — consume `on-entity-spawn` above; see "Entity correlation via on-entity-spawn" below (#214). |
| `send-message` | `func(target-entity-id: string, body: string) -> result<_, string>` | Sends a visible message to one connected client — the "make the interaction have a visible effect" primitive. |
| `apply-stat-delta` | `func(entity-id: string, stat-key: string, delta: s64) -> result<_, string>` | Adjusts one declared stat by `delta` — works for a player entity (through `character::CharacterStore::set_stat`, durable) or an NPC entity (through `server::world_actor`'s in-memory `npc_stats`, #197) equally; the caller doesn't need to know or care which kind of entity it's targeting. Both paths validate against the same declared `AttributeSchema` (bounds/defaults) — see "NPC-targetable stats" below. A successful write against a player entity also pushes `StatChanged` to that entity's own connection automatically (#211) — see "Automatic client pushes" below; no push for an NPC target, which has no owning connection. |
| `apply-stat-delta-for-character` | `func(character-id: string, stat-key: string, delta: s64) -> result<_, string>` | Same validated-write discipline as `apply-stat-delta`, but identifies the target by `character-id` directly (#194) — the counterpart for `on-character-create`, which fires before any entity/session exists so there's no entity id to pass. Every other function in this interface deliberately treats an id as opaque and never as a character id directly ("Ids are opaque strings" below); this is the one narrow, documented exception. Also pushes `StatChanged` on success (#211), but only if the character happens to already have a live connection at the moment it's called — the ordinary case (firing from `on-character-create`) has none yet, so this is usually a silent no-push. |
| `move-entity` | `func(entity-id: string, x: f64, y: f64) -> result<_, string>` | Queues a move applied and validated on the zone's next tick through the same path a player's own movement goes through (`world::Zone::request_move`) — never a direct position write. |
| `grant-item` | `func(entity-id: string, item-type: string, quantity: s64) -> result<_, string>` | Queues a grant applied through `character::CharacterStore::grant_item` (#112's `items` table) — player entities only, same caveat as `apply-stat-delta`. A successful grant fires `on-item-acquire` back to the plugin, and pushes `ItemChanged` to the owning connection automatically (#211). |
| `remove-item` | `func(entity-id: string, item-type: string, quantity: s64) -> result<_, string>` | Queues a removal applied through `character::CharacterStore::remove_item`. Rejected during the drain (not synchronously) if the character doesn't own enough — same "queued, not confirmed synchronously" caveat as `apply-stat-delta`. A successful removal pushes `ItemChanged` to the owning connection automatically (#211), same as `grant-item`. |
| `modify-currency` | `func(entity-id: string, currency-key: string, delta: s64) -> result<_, string>` | Queues a currency balance adjustment for `currency-key` (one of `currency.schema.yaml`'s dev-declared currencies, #217/#218) applied through `character::CharacterStore::modify_currency`. Rejected during the drain if `currency-key` isn't declared, or if the result would go negative — each currency's balance is independent. A successful adjustment pushes `CurrencyChanged` to the owning connection automatically (#211), carrying `currency-key` alongside the resulting balance. |
| `caller-role` | `func(entity-id: string) -> result<list<string>, string>` | Returns the roles (docs/specs/Auth_Spec.md, "Account roles", #114/#124) held by the account behind `entity-id` — empty list if none, the common case. Global scope for v0. Unlike the seven above, this is answered from an in-memory cache populated at session join, never a live `auth` DB read from inside the sandboxed call — see "Beyond this v0 slice" and docs/specs/Auth_Spec.md for why. Only resolves for a connected player entity. |
| `plugin-state-get` | `func(scope: plugin-state-scope, key: string) -> result<option<list<u8>>, string>` | Reads a previously-stored opaque blob for `scope`/`key`, `none` if nothing's been stored yet — the Plugin-Scoped Data Store (#149, docs/PROPOSAL.md's "Plugin-Scoped Data Store"). Same in-memory-cache-not-live-DB-read constraint as `caller-role` — see below. |
| `plugin-state-set` | `func(scope: plugin-state-scope, key: string, value: list<u8>) -> result<_, string>` | Stores a blob for `scope`/`key`, overwriting whatever was there. Visible to a `plugin-state-get` in the same session immediately; for `character`/`zone` scope, also queued for durable persistence (same "queued, not synchronously confirmed" shape as `apply-stat-delta`). |
| `report-death` | `func(entity-id: string) -> result<_, string>` | Reports that `entity-id` has died (#154) — the plugin decides what "died" means for its own game (docs/PROPOSAL.md: "core has no notion of HP or a death condition") and calls this itself. Queued, applied on the zone's next tick drain, fires `on-death` back to the caller once applied — same "queued, not synchronously confirmed" shape as `apply-stat-delta`. |
| `report-respawn` | `func(entity-id: string) -> result<_, string>` | Same shape as `report-death`, for the respawn case — fires `on-respawn` once applied. |
| `block-zone-channel` | `func(entity-id: string, category: string) -> result<_, string>` | Prevents `entity-id` from auto-joining the zone-scoped chat channel declared under `category` (`chat.yaml`, #186) — see "Zone-scoped chat auto-join" below. Applied immediately to an in-memory, per-entity set; only affects auto-joins from this point forward, never retroactively leaves a channel already auto-joined. |

Eleven of the fourteen are gated by `plugin.toml`'s `capabilities` — see "Capability gating" below. `grant-item`/`remove-item`/`modify-currency` only resolve for player entities — an NPC entity id is rejected, since NPCs have no character-backed storage (no NPC item/currency ownership exists, only players own items/currency).

### Automatic client pushes (#211, implementing #210's decision)

Before #211, a successful `apply-stat-delta`/`apply-stat-delta-for-character`/`grant-item`/`remove-item`/`modify-currency` call landed in Postgres, but the plugin had to separately call `send-message` (and invent its own text convention) if it wanted the connected client to actually learn the new value — the core had no structured notion of "a stat/item/currency just changed." Now every one of those five host functions, on a write that actually applies, automatically pushes a matching `server::session_protocol::ServerMessage` (`StatChanged`/`ItemChanged`/`CurrencyChanged` — docs/specs/Networking_Spec.md's "Automatic stat/item/currency pushes" has the wire-level detail) to the one connection that owns the affected entity/character, with no extra call and no capability beyond whichever of `combat`/`economy` the write itself already required.

**This is a side effect of the write, not a separate host function** — a plugin can't opt out of it, and doesn't need to opt in; a plugin that only ever wanted the old ad-hoc `send-message` convention can still send whatever additional message it likes on top, the two aren't mutually exclusive. No push happens for a write that's rejected (insufficient item quantity, currency going negative, a stat delta overflow) or that has no owning connection to reach (an NPC-targeted `apply-stat-delta`; `apply-stat-delta-for-character` called before the character has any live connection, the ordinary case for `on-character-create`) — see each function's own row above and Networking_Spec.md for the full "when no push happens" list.

### Capability gating (`plugin.toml`'s `capabilities`, #153)

Five named groups, each covering a fixed subset of host functions:

| Capability | Grants |
|---|---|
| `spawning` | `spawn-npc` |
| `movement` | `move-entity` |
| `combat` | `apply-stat-delta`, `apply-stat-delta-for-character`, `report-death`, `report-respawn` |
| `economy` | `grant-item`, `remove-item`, `modify-currency` |
| `messaging` | `send-message`, `block-zone-channel` |

Only `caller-role`, `plugin-state-get`, and `plugin-state-set` are **ungated** regardless of declared capabilities — `caller-role` is a read-only answer from a cache already scoped to the calling connection, and `plugin-state-get`/`-set` only ever touch the plugin's own storage. Every gated function reaches *across* an entity boundary — moving/damaging/granting-to/messaging another entity, spawning a new one — that's the actual dividing line, not an arbitrary split. `send-message` is gated (not folded into "ungated") specifically because it can target *any* connected entity by id, not just the one a hook call was actually about — matching docs/PROPOSAL.md's own "v0 Host Functions" list, which already treats messaging as its own capability group, separate from entity control.

**Enforcement is per-call, not load-time refusal.** `plugin-host::PluginHost::load` wraps every plugin's `HostCallbacks` in a capability-checking layer (`runtime::CapabilityGatedCallbacks`) before instantiation — a call to a host function outside what the manifest declared returns an ordinary `Err` string back to the plugin (never a trap/panic), the same shape every other host-function failure already takes. Per-call rather than refuse-to-load because a plugin's declared hooks/message types are still valid and useful even if one gated call would fail — the plugin (or its author, reading the error during development) decides what to do about it, same as any other host-function error.

**The default is strict.** `capabilities = []` (the same default every manifest already had before this) grants *none* of the five gated groups — not all of them. This is a deliberate v0.7.0 behavior tightening: before #153 the field was parsed and carried but checked against nothing, so every plugin had full access regardless of what it declared; every existing manifest in this repo (`config/plugin.example.toml`, `examples/example-plugin/plugin.toml`, test fixtures) was updated to explicitly list the capabilities it actually uses.

This is the real trust boundary for running a less-trusted, third-party-authored plugin alongside an operator's own trusted "core" plugin: grant the operator's own plugin every capability it needs, restrict a community-authored add-on to just what it actually needs (e.g. `messaging`-only plus whatever `message_types`/`chat_commands` it declares) without needing separate infrastructure. See "Multi-plugin support" below for how more than one plugin actually loads at once (#152).

**`on-craft-complete` is the one exception to "enforcement is per-call."** It has no corresponding host function to gate — a hook fires *into* the plugin, it isn't a call *out* to the host — so #216 gates it at the firing site instead (`server::session::fire_on_craft_complete`): a successful craft only calls the hook at all if the plugin declared both `on-craft-complete` in `hooks` *and* `economy` in `capabilities` (the same capability that already covers `grant-item`/`remove-item`/`modify-currency`, the closest existing grouping). Declaring the hook without the capability (or vice versa) means it simply never fires — no error, same "missing opt-in, not a refusal" shape `hooks` itself already has for every other hook.

### Multi-plugin support (#152)

**One plugin instance, process-wide.** A plugin is loaded exactly once per `server` process — never once per zone-service. Every zone-specific hook (everything in the table above except `on-load`/`on-unload`) takes an explicit `zone-id` first parameter instead: a plugin that only cares about certain zones checks `zone-id` itself inside its own hook body, rather than the host filtering events by zone on the plugin's behalf. `on-zone-loaded(zone-id)` is the per-zone setup hook — e.g. seeding that zone's NPCs — since `on-load` no longer has a zone to act on. This replaced an earlier per-zone-instance design during implementation; a plugin is additive to the engine's flows, not an object the engine attaches to individual zones.

**`plugins_dir` discovery, not a single manifest path.** `WZ_PLUGINS_DIR` (default `<config_dir>/plugins`) names a directory of `<name>/{plugin.toml,*.wasm}` subdirectories — every subdirectory found there is auto-discovered and loaded at startup, replacing the old single `WZ_PLUGIN_MANIFEST_PATH`/`WZ_PLUGIN_WASM_PATH` env-var pair. Each manifest is checked individually (`PluginManifest::check_compatible`) and the whole set collectively (`check_no_collisions`, below) before any plugin is instantiated.

**Per-hook opt-in via `plugin.toml`'s `hooks` field**, not a WIT-level mechanism (the interface's exports aren't individually optional — see above). The host only calls a hook for a plugin whose manifest lists it in `hooks`; every hook not listed is simply never invoked for that plugin. `on-message`/`on-chat-command` are the exception — routed by `message_types`/`chat_commands` membership alone (below), since declaring interest in a specific message type or command name already states the same intent `hooks` would.

**Cross-plugin collision checking.** `plugin_host::check_no_collisions` refuses to start the server if two loaded plugins declare the same `message_type` or the same `chat_command` — there's no arbitration between colliding claims, just a load-time refusal so the conflict is caught before either plugin runs. Every other hook fans out to *every* plugin that declared it, independently, in discovery order — the core never picks a winner.

**A process-wide `send-message` needs a process-wide session map.** Since a plugin instance isn't tied to one zone's connections, `send-message` resolves `target-entity-id` against every connected entity in the process, not just the caller's own zone — an entity stays reachable across a `ZoneChanged` zone-hop without the plugin needing to know it happened.

### Character creation (`on-character-create`, #194)

The intended extension point for a plugin-driven starting-stat/archetype system — per this project's "no game-specific concept, not even HP, is privileged by the core" design principle, class/race/archetype selection is deliberately not a core concept, and this hook is where a game developer's own plugin decides what a freshly created character starts with.

- **Fires once, right after the character row exists, before any entity does.** `server::character_protocol`'s `CreateCharacter` (#193) creates the row, fires this hook (and drains/applies whatever `apply-stat-delta-for-character` calls it made) synchronously, *then* sends the client its `CharacterCreated` acknowledgement — a client that immediately lists/selects/joins never observes the character pre-hook.
- **No entity id, because none exists yet.** The character hasn't spawned into any zone — `zone-id` is given up front (the deployment's default starting zone) since there's no zone-service dispatch context to infer it from; this hook does not fire from inside any particular zone actor's event loop the way `on-damage-calc`/`on-item-use`/etc. do.
- **Setting starting stats means `apply-stat-delta-for-character`, not `apply-stat-delta`** — the character-id-scoped host function exists specifically because this hook has no entity id to pass to the ordinary one.
- **Listing presets *before* a client commits to `CreateCharacter`** (so a UI could show "pick an archetype" ahead of naming a character) is explicitly **out of scope** for this hook — a fire-after-creation hook can't do that; it would need a real request/response mechanism this interface doesn't have yet (every other host function/hook here is either fire-and-forget or a synchronous cache read, never "ask the plugin a question and wait for its answer"). Deferred to a follow-up ticket, not solved here — a v0 plugin wanting archetype selection today has to build it as its own client-visible protocol (e.g. its own declared `message_types`) layered on top of `CreateCharacter`/`SelectCharacter`, picking the starting stats itself once it knows what the client chose, then applying them via this hook or later via `on-player-join-zone`.

### Crafting (`on-craft-complete`, #216, implementing #215's decision)

Core owns the mechanical act of crafting — resolving a `CraftItem{recipe_key}` request against the dev-declared `crafting.schema.yaml` and atomically consuming/granting through it (docs/specs/Data_Model_Spec.md's "Crafting" section, docs/specs/Networking_Spec.md's "Crafting" section). `on-craft-complete` is the extension point for everything genre-specific that core deliberately has no opinion on: quality rolls, XP/profession-leveling, swapping the granted output for a rarer variant, enforcing a profession/skill gate.

- **Fires once, after the exchange already committed.** By the time this hook runs, the inputs are gone and the output is granted — there is no veto. #215's decision explicitly defaults to post-craft-only for v0; a pre-craft veto hook (e.g. for skill-gating before core's exchange runs) was considered and deliberately deferred, not built here.
- **No entity id, same reasoning as `on-character-create`.** A craft is character-scoped, not entity-scoped — the request came from an already-connected player, but the hook still only carries `character-id`, not an entity id, for symmetry with `on-character-create`'s "narrow exception" (see "Ids are opaque strings" below). A plugin wanting to react against the crafting character's stats calls `apply-stat-delta-for-character`, the one host function reachable without an entity id; `grant-item`/`remove-item`/`modify-currency` are all entity-id-scoped and unreachable from inside this hook.
- **Gated behind the `economy` capability, not just `hooks`.** See "Capability gating" above for why this is the one hook enforced that way.

### Zone-scoped chat auto-join (#186)

`server::chat_session::auto_join_zone_channels` auto-joins a connection to every `chat.yaml` category declared `scope: zone` and `auto_join: true` (docs/specs/Chat_Spec.md's "chat.yaml") whenever it enters that zone — initial zone join or a later `ZoneChanged` transition — and auto-leaves the previous zone's auto-joined channels on the way out. Global (`scope: global`) categories are never affected — they're not zone-triggered at all, and `auto_join: true` on one is refused when `chat.yaml` loads.

`block-zone-channel` is the plugin-facing escape hatch: call it (e.g. from `on-player-leave-zone`, ahead of the transition that would trigger the join, or from `on-character-create`) to keep a specific `entity-id` from auto-joining a specific `category` — a city channel gated behind a quest flag, an event channel gated behind an account role (`caller-role` answers the latter). The block is in-memory and connection-scoped: it doesn't survive a reconnect, and it doesn't retroactively remove a channel already auto-joined before the call.

### NPC-targetable stats (#197)

Before this, `apply-stat-delta` only ever resolved for a player entity — it went through `entity_characters` (`server::session`'s map from a connected player's entity id to its `CharacterId`), which an NPC entity id never has an entry in, so the call silently no-op'd (logged as a `WARN`). A plugin wanting real, declared-schema-backed HP on an NPC — "hit the monster, watch its health bar drop, it dies," the single most basic combat scenario an MMO framework needs — had no core mechanism for it.

**Storage: `server::session::NpcStats`, in-memory, process-wide, keyed by entity id** (`Arc<Mutex<HashMap<EntityId, HashMap<String, i64>>>>`) — the NPC counterpart to a `characters` row's `stats` column, populated lazily on first write and cleared when the entity despawns. Deliberately **not** persisted to Postgres. Justification: an NPC's entity id is generated fresh at spawn time (`world_actor::spawn_npc_from_table`), never stable across a zone-service restart the way a character id is — there is nothing meaningful to durably key stored stats against, and a restarted server respawns its NPCs from the same manifest-declared spawn tables at their schema-declared defaults either way. If a future ticket needs NPC state to survive a restart (a persistent world boss, say), that's a different, larger problem than this one — revisit then, not preemptively here.

**Resolution: `apply-stat-delta` decides by entity kind, not by a separate function.** `server::world_actor::apply_plugin_pending_effects` checks `Zone::kind_of(entity)` for each queued stat delta: `Player` resolves through `entity_characters` + `CharacterStore::apply_stat_delta` (durable, async, the existing path); `Npc` resolves through `NpcStats` + a new `apply_npc_stat_delta` helper (in-memory, synchronous — no DB round trip on this path). Both go through the exact same `character::AttributeSchema` (bounds/defaults validation) — the schema instance is loaded once at startup and shared (cloned into an `Arc`) between `CharacterStore` and the NPC path, so there's no risk of the two drifting. No WIT/ABI change was needed: `apply-stat-delta`'s signature (`entity-id`, `stat-key`, `delta`) already treats the target as an opaque entity id, exactly the shape this needed.

**Deciding "dead" still doesn't get a stat read-back.** `apply-stat-delta` stays fire-and-forget/queued (same shape as ever — see its own doc comment above) even for the NPC path, so a plugin's `on-damage-calc` handler never gets a synchronous read of the value it just wrote, whether the target is a player or an NPC. A plugin that wants to decide "this hit killed it" composes `apply-stat-delta` (the durable, schema-validated write) with its own combat-scoped bookkeeping via `plugin-state-get`/`-set`'s `entity` scope (in-memory, read-your-own-write within the same session — see below) to track whatever threshold logic it wants, then calls `report-death` itself once it decides. This is the same "core has no notion of HP or a death condition" discipline every other death decision already follows (#154) — #197 didn't change that, it just made the write half of the story work for an NPC target too. `crates/plugin-host/tests/fixtures/test-plugin`'s `on_damage_calc` demonstrates the full composition (a small per-target hit counter, `report-death` once it reaches zero), exercised end to end by `crates/server/tests/server_smoke.rs`'s `attacking_an_npc_applies_real_stats_and_kills_it_at_zero`.

### Entity correlation via on-entity-spawn (#214)

**The problem `spawn-npc`'s return value can't solve.** `spawn-npc`'s host callback (`crates/server/src/plugin_startup.rs`'s `PluginCallbacks::spawn_npc`) runs inside the sandboxed WASM call — it has no `&mut world::Zone` and no DB handle, so it can't create the real entity or assign it a real id there and then. It only records the request (`pending_spawns`); the real entity is created later, when the caller (`server::world_actor::spawn_requested_npcs`) drains that queue outside the sandboxed call — the same "can't touch `&mut Zone`/the DB from inside a sandboxed sync call" constraint every other `pending_*` field in `PluginCallbacks` exists for (`grant-item`, `move-entity`, `report-death`, ...). Making `spawn-npc` synchronously return a real id would mean giving the sandboxed call itself write access to the zone/DB, a much larger architecture change than this problem needs — see #214's issue discussion for why that was rejected. So `spawn-npc`'s `Ok` value is `spawn-table-id` echoed back, never a real entity id.

**The actual fix: correlate through `on-entity-spawn`, not the return value.** `on-entity-spawn` fires back to the requesting plugin the moment the real entity is actually created — `server::world_actor::spawn_requested_npcs` calls it right after `spawn_npc_from_table` succeeds, passing the real `entity-id` and the `spawn-table-id` that caused it. This existed as a documented hook well before #214, but nothing ever called it — #214 is what wired it up for real and added the `spawn-table-id` parameter.

**Telling repeat calls to the same table apart.** `spawn-table-id` alone doesn't disambiguate two `spawn-npc` calls against the *same* table (e.g. three wolves in a row all carry `"wolf-pack-01"`). What does: `pending_spawns` is a plain `Vec`, so requests are drained, spawned, and fired back to the requesting plugin in exactly the order they were made — a plugin that calls `spawn-npc` more than once for one table can track its own call order (or, more robustly, stash a per-call label via `plugin-state-set` right before each call and consume it inside `on-entity-spawn`) to attribute each arriving `on-entity-spawn` to the right request. `crates/plugin-host/tests/fixtures/test-plugin`'s `spawn-track`/`which-wolf` chat commands do exactly this — `spawn-track <label>` records `label` under zone-scope plugin state immediately before its own `spawn-npc` call, `on_entity_spawn` consumes it (and clears it, so an untracked spawn later never picks up a stale label) and stores the resulting real entity id under `spawn-result-<label>`, and `which-wolf <label>` reads it back — exercised end to end by `crates/server/tests/server_smoke.rs`'s `spawn_npc_correlates_to_the_real_entity_via_on_entity_spawn`, which issues two `spawn-track` calls against the same table back to back and asserts they resolve to two distinct real entity ids.

**Keying entity-scoped state, the original motivating use case.** Once a plugin has the real id from `on-entity-spawn`, it can immediately key its own per-NPC state via `plugin-state-set`'s `entity` scope (`PluginStateScope::Entity(entity_id)`, see below) — e.g. per-entity HP bookkeeping — rather than the zone-scoped-key workaround a plugin previously had to reach for (this is the concrete case the original ticket named, `world-zero-test-grounds/PROMPT.md`'s Evil Cube example). `test-plugin`'s `on_entity_spawn` also demonstrates this directly, storing `spawned-from-table`/`spawned-with-entity-type` under the entity's own scope the moment the real id is known.

**Only fires back to the requesting plugin, and only for a `spawn-npc`-caused spawn.** `on-entity-spawn` does not fan out to every loaded plugin the way most hooks in this interface do (see "Multi-plugin support" below) — a plugin with no pending spawn request of its own has no correlation token to consume, so there's nothing useful to fire back to it. It also does not fire for a player entity joining a zone; despite this hook's name sounding general-purpose, that case is `on-player-join-zone` above, a distinct event with distinct timing (after roster delivery, not "just spawned"). A future ticket wanting a true "any entity, any cause" spawn feed would need a different mechanism — this one is scoped to what #214 actually needed: correlating a plugin's own spawn requests.

### The Plugin-Scoped Data Store (`plugin-state-get`/`plugin-state-set`, #149)

`plugin-state-scope` is a variant carrying both which "bucket" a key lives in and which id it's scoped to:

```wit
variant plugin-state-scope {
    character(string),  // the entity id currently representing this character
    entity(string),
    zone(string),        // a content-manifest zone id
}
```

- **`character`/`zone` scope is durable** (Postgres, `plugin_character_state`/`plugin_zone_state` tables) — hydrated into an in-memory cache once at the right lifecycle point (character scope at session join, zone scope at zone-actor startup, before either could possibly receive a `plugin-state-get` call), so reads never hit the database live from inside the sandboxed call — same constraint `caller-role` documents. Writes update that cache immediately and queue a durable write, applied on the zone's next tick drain, mirroring `apply-stat-delta`/`grant-item`'s "queued, not synchronously confirmed" shape.
- **`entity` scope is transient** — in-memory only, cache-only end to end, no persistence. Nothing survives a restart for this scope, by design; it's for state that only needs to live as long as the entity does.
- The value is an opaque blob to the host in every scope — a plugin author defines its shape (JSON, bincode, whatever) entirely on their own; the core never looks inside it.
- `character` scope's id is an *entity* id, like every other host function's `entity-id` parameter — never a raw `CharacterId`, since a plugin never sees one of those directly in the ordinary case. **Known gap:** `on-character-create`/`on-craft-complete` (#194/#216) are the two hooks that *do* carry a raw `character-id` (see "Ids are opaque strings" below), but `character` scope's durable-persistence drain (`server::world_actor`'s `pending_state_writes` handling) still assumes its id is always resolvable through `entity_characters` — a `plugin-state-set` call from inside either hook only ever updates the in-memory cache, it silently never persists (logged as a `WARN`). A plugin reacting to either hook should use `apply-stat-delta-for-character` instead, which does correctly accept a raw `character-id`.

### Ids are opaque strings

Every id crossing the boundary (`entity-id`, `spawn-table-id`, `trigger-id`, ...) is a plain `string`, not a typed WIT record — deliberately: a plugin has no legitimate reason to construct or inspect one of these ids' internal structure, only to pass one it received right back to a host function. Keeping them opaque avoids coupling the interface to whatever internal id representation the host happens to use (`common::id::Id<T>`'s UUID today) — that's free to change without touching this interface.

## `plugin.toml`: the plugin manifest

Same convention as the content manifest and dev-config files elsewhere in the project — one manifest per plugin, checked *before* the plugin is ever instantiated (`PluginHost::load` calls `PluginManifest::check_compatible` first):

```toml
[plugin]
name = "example-plugin"
host_api_version = "0.12.0"
capabilities = []
message_types = []
chat_commands = []
hooks = []
```

| Field | Type | Notes |
|---|---|---|
| `plugin.name` | string | Free-form, used in error/log messages. |
| `plugin.host_api_version` | string | Must equal `plugin_host::HOST_API_VERSION` (currently `"0.12.0"`, matching the WIT package version above) or the plugin is refused before instantiation. |
| `plugin.capabilities` | list of strings, optional | Gates which host functions this plugin may call (#153) — see "Capability gating" above. Strict default: `[]` grants none of the five gated groups. An unknown capability name, or the same one declared twice, is refused at load time. |
| `plugin.message_types` | list of `u16`, optional | Gateway `message_type` values (docs/specs/Networking_Spec.md) routed to this plugin's `on-message` hook (#95). Each must be `>= 1000` (0-999 is core-reserved) and appear at most once, checked by `PluginManifest::check_compatible` before the plugin is instantiated, and collectively across every loaded plugin by `check_no_collisions` (#152). |
| `plugin.chat_commands` | list of strings, optional | Chat command names, without the leading `/` (#57). Each must be non-empty, have no leading `/`, and appear at most once, checked the same way as `message_types` (including the cross-plugin collision check, #152). A matched command is routed to `on-chat-command` instead of published as ordinary chat. |
| `plugin.hooks` | list of strings, optional | Which of the nineteen hooks (except `on-message`/`on-chat-command`, routed by the two fields above instead) the host should actually call for this plugin (#152) — see "Multi-plugin support" above. Strict default: `[]` means none are called. An unknown hook name is refused at load time (`plugin_host::manifest::KNOWN_HOOKS`). |

## Sandbox guarantees

- **No ambient capability.** A plugin gets nothing beyond the two `host` functions above — no direct DB access, no raw network access, no filesystem access, ever (docs/PROPOSAL.md, "Plugin System"). Verified by `plugin_sandbox.rs`.
- **A trap doesn't crash the host.** A panicking/trapping plugin surfaces as an ordinary `Err` from whichever hook call triggered it (`LoadedPlugin::on_load`/etc. return `common::Result<()>`) — the zone-service keeps running. Verified by `plugin_sandbox.rs`'s `a_plugin_panic_does_not_crash_the_host_process`.
- **One `wasmtime::Engine` for the whole process** (`PluginHost`, constructed once in `main`), shared across every loaded plugin — compiling/loading a component is the expensive part; the engine itself is designed to be shared. This is a natural consequence of #152's process-wide plugin instances, not a separate decision: there's no longer a "per zone-service" to scope an engine to.

## Beyond this v0 slice

Real design from docs/PROPOSAL.md's "Plugin System" section, deliberately not built yet:

- **A true synchronous "query" host function against `character`'s live storage** — `item-quantity`/`currency-balance`-style reads that hand a value straight back to the plugin. Still deliberately not built: `PluginCallbacks` is called synchronously from inside `wasmtime`, while `character::CharacterStore` is async-only (`sqlx`); reads would need either a new blocking-call mechanism (`tokio::task::block_in_place`, not used anywhere else in this codebase) or an eventually-consistent cache, and neither was worth the complexity for v0 — a plugin that needs to know a quantity/balance can track it itself from `on-item-acquire`/its own bookkeeping. `caller-role` (#124) sidesteps this exact constraint for account roles specifically, not by adding blocking DB calls, but by having `server::session` populate an in-memory cache at join time (`session::EntityRoles`) that `caller-role` reads synchronously — a pattern this general query problem could reuse later if a case for it emerges, but wasn't generalized here.
- **Cross-plugin RPC, hot-reload, plugin-defined persistent schema (structured tables beyond the opaque blob store)** — explicit v0 non-goals per the proposal, not accidentally missing. The opaque-blob-store half of that story *is* now in (#149, `plugin-state-get`/`plugin-state-set` above) — the non-goal is specifically anything beyond it (a plugin declaring its own real DB schema). (Plugin-declared gateway message types/custom packets *are* now in — `on-message`, #95, with cross-plugin collision checking since #152.)
- ~~**Account roles for dev/admin-only commands**~~ — decided in #114, implemented in #124: see the `caller-role` host function above and docs/specs/Auth_Spec.md's "Account roles" section.
- ~~**Player session** (`on_player_join_zone`, `on_player_leave_zone`)~~ — implemented in #155: see the `on-player-join-zone`/`on-player-leave-zone` hooks above.
- ~~**Live call sites for `on-damage-calc`/`on-death`/`on-respawn`/`on-npc-interact`/`on-item-use`**~~ — implemented in #154: see each hook's row above and `report-death`/`report-respawn` in the host functions table.
- ~~**Real capability gating**~~ — implemented in #153: see "Capability gating" above.
- ~~**Per-plugin optional hooks / multi-plugin loading**~~ — implemented in #152: see "Multi-plugin support" above.
- ~~**`on_tick(zone, dt)`** (the zone-wide tick hook, distinct from #116's per-NPC `on-npc-tick`)~~ — implemented in #168: see the `on-tick` hook's row above.
