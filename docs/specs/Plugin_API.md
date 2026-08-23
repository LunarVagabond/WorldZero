# Plugin API

Corresponds to [Plugin System](../PROPOSAL.md#plugin-system) in the proposal.

**Status:** the v0 slice below is real and implemented (`plugin-host`, #37/#38, extended by #95, #116, and #57) — one WIT world with thirteen hooks and seven host functions: #38's original NPC-spawn-plus-interaction slice, #116's combat/NPC-patrol/chat-command hooks, and #57's inventory/economy hooks and host functions (#112 supplied the core storage these write through). See "Beyond this v0 slice" below for what's still not here.

## Interface technology

WASM Component Model + WIT (docs/PROPOSAL.md, "Interface Technology") — the actual interface lives in `crates/plugin-host/wit/plugin.wit`, this doc explains and indexes it rather than duplicating it verbatim (the `.wit` file is the source of truth for the exact signatures).

- **Host side:** `wasmtime::component::bindgen!` generates Rust bindings from `wit/plugin.wit` directly into `plugin-host` at compile time (`crates/plugin-host/src/bindings.rs`).
- **Guest side:** a plugin author uses `wit-bindgen`'s guest macro (`wit_bindgen::generate!`) for Rust, or the equivalent for any other language `wasmtime`/the Component Model toolchain supports.
- **Target:** `wasm32-wasip2` — this target directly emits a Component Model binary (no separate "componentize a core module" step needed), and comes with the WASI Preview 2 CLI imports a Rust std binary needs to start at all.

## The `plugin` world (v0)

```wit
package worldzero:plugin@0.4.0;

interface host {
    spawn-npc: func(spawn-table-id: string) -> result<string, string>;
    send-message: func(target-entity-id: string, body: string) -> result<_, string>;
    apply-stat-delta: func(entity-id: string, stat-key: string, delta: s64) -> result<_, string>;
    move-entity: func(entity-id: string, x: f64, y: f64) -> result<_, string>;
    grant-item: func(entity-id: string, item-type: string, quantity: s64) -> result<_, string>;
    remove-item: func(entity-id: string, item-type: string, quantity: s64) -> result<_, string>;
    modify-currency: func(entity-id: string, delta: s64) -> result<_, string>;
}

interface hooks {
    on-load: func();
    on-unload: func();
    on-entity-spawn: func(entity-id: string, entity-type: string);
    on-interact: func(trigger-id: string, actor-entity-id: string);
    on-message: func(message-type: u16, sender-entity-id: string, payload: list<u8>);
    on-damage-calc: func(attacker-entity-id: string, target-entity-id: string, stat-key: string, base-amount: s64);
    on-death: func(entity-id: string);
    on-respawn: func(entity-id: string);
    on-npc-tick: func(entity-id: string, x: f64, y: f64, route-waypoints: list<tuple<f64, f64>>, route-loop: bool, route-speed: f64, dt: f64);
    on-npc-interact: func(npc-entity-id: string, actor-entity-id: string);
    on-chat-command: func(command: string, args: string, sender-entity-id: string);
    on-item-acquire: func(entity-id: string, item-type: string, new-quantity: s64);
    on-item-use: func(entity-id: string, item-type: string);
}

world plugin {
    include wasi:cli/imports@0.2.12;
    import host;
    export hooks;
}
```

`worldzero:plugin@0.4.0` is the actual versioning mechanism (docs/PROPOSAL.md, "Interface Technology": WIT "worlds" give real interface versioning). A breaking change to this interface bumps the package version and/or introduces a new world; a plugin manifest declares which `host_api_version` it targets (`plugin.toml` below), and `plugin-host` refuses to instantiate a plugin declaring a version it doesn't implement (`crates/plugin-host/src/manifest.rs::PluginManifest::check_compatible`) — it never silently links a plugin against an interface shape it wasn't built for. `0.2.0` added `on-message` (#95); `0.3.0` added the combat/NPC-patrol/chat-command hooks and `apply-stat-delta`/`move-entity` (#116); `0.4.0` added the inventory/economy hooks and host functions below (#57). A plugin declaring an older version is refused, not silently linked against the new shape.

### Why `include wasi:cli/imports`

A `wasm32-wasip2` Rust binary needs baseline WASI Preview 2 imports (clocks, random, stdio, ...) to start at all — that's a Rust-runtime requirement, not a capability grant. The actual sandboxing comes from what the **host** provides at instantiation, not from omitting WASI: `plugin-host` builds each plugin's `WasiCtx` via `WasiCtxBuilder::new().build()` with nothing else configured — no preopened directories, no sockets, no inherited stdio/env/args. This is WASI Preview 2's own capability-based security model (the same one `wasmtime` documents as its intended sandboxing mechanism), not a hand-rolled restriction layer. Verified directly: `crates/plugin-host/tests/plugin_sandbox.rs`'s `a_plugin_cannot_read_the_filesystem_with_no_preopens_granted` loads a real compiled plugin that attempts `std::fs::read_to_string` and confirms it fails.

### Hooks (host calls into the plugin)

| Hook | Signature | Fires when |
|---|---|---|
| `on-load` | `func()` | The plugin is instantiated for a zone-service. |
| `on-unload` | `func()` | Before the plugin is torn down. |
| `on-entity-spawn` | `func(entity-id, entity-type)` | Any entity enters the zone's simulation (`entity-type` matches `content::manifest::SpawnTable`'s `entity_type`, e.g. `"npc.wolf"`; empty for a player). |
| `on-interact` | `func(trigger-id, actor-entity-id)` | A player's entity enters a trigger volume whose manifest `event` is an interact-style event (`content::manifest::Trigger`). |
| `on-message` | `func(message-type, sender-entity-id, payload)` | The gateway receives an envelope whose `message_type` matches one of this plugin's declared `message_types` (below) — `sender-entity-id` is the sending connection's own entity, `payload` is the envelope's opaque bytes (#95). |
| `on-damage-calc` | `func(attacker-entity-id, target-entity-id, stat-key, base-amount)` | **No live host call site yet** — nothing in `world`/`gateway` has a concept of an attack/damage-causing client action to call this from. The plugin owns the whole mitigation formula and must call `apply-stat-delta` itself; this hook alone changes nothing (docs/PROPOSAL.md, "v0 Hooks"). |
| `on-death` | `func(entity-id)` | Same "no live call site yet" caveat as `on-damage-calc` — a plugin decides what "died" means for its own game; no host-side reporting mechanism exists yet to trigger this. |
| `on-respawn` | `func(entity-id)` | Same caveat as `on-death`. |
| `on-npc-tick` | `func(entity-id, x, y, route-waypoints, route-loop, route-speed, dt)` | **Live.** Called once per tick, per NPC entity whose spawn table declared a `route_id` (`content::manifest::SpawnTable`/`Route`) — `world::world_actor` drives this. The host never moves the NPC itself; the plugin decides the next position and calls `move-entity`. |
| `on-npc-interact` | `func(npc-entity-id, actor-entity-id)` | **No live host call site yet** — a player-targets-an-NPC-specifically client action doesn't exist in `docs/specs/Networking_Spec.md`'s message catalog, only the generic trigger-volume `on-interact`. |
| `on-chat-command` | `func(command, args, sender-entity-id)` | **Live.** Fires when a chat `Send`'s body starts with `/command` and `command` (without the slash) matches one of this plugin's declared `chat_commands` (`plugin.toml` below) — `server::session` matches before the message ever reaches `chat_session`/gets published as ordinary chat. |
| `on-item-acquire` | `func(entity-id, item-type, new-quantity)` | **Live.** Fires after a `grant-item` call this same plugin made is actually applied — `world::world_actor` calls this itself right after a queued grant lands, so (unlike most of this interface) a plugin can treat this as real confirmation, not just "the call was queued." `new-quantity` is the item type's new total, not the delta granted. |
| `on-item-use` | `func(entity-id, item-type)` | **No live host call site yet** — no "use an item" client action exists in `docs/specs/Networking_Spec.md`'s message catalog. A plugin wanting this today routes it through its own `chat_commands`/`message_types` and calls `remove-item` itself. |

Every plugin exports all thirteen — a WIT world's exports aren't individually optional, so there's no per-plugin "I don't implement this hook" declaration in this v0 slice (contrast with the richer, deferred story in "Beyond this v0 slice" below).

### Host functions (plugin calls out to the host)

| Function | Signature | Does |
|---|---|---|
| `spawn-npc` | `func(spawn-table-id: string) -> result<string, string>` | Spawns one NPC from a zone's declared spawn table (`content::manifest::SpawnTable`), returning its new entity id or an error (unknown table, at max population, ...). |
| `send-message` | `func(target-entity-id: string, body: string) -> result<_, string>` | Sends a visible message to one connected client — the "make the interaction have a visible effect" primitive. |
| `apply-stat-delta` | `func(entity-id: string, stat-key: string, delta: s64) -> result<_, string>` | Adjusts one declared stat by `delta`, validated the same way `character::CharacterStore::set_stat` validates any write. Only works for player entities today — an NPC entity id is rejected, since no NPC stat storage exists yet (see #112 note below). |
| `move-entity` | `func(entity-id: string, x: f64, y: f64) -> result<_, string>` | Queues a move applied and validated on the zone's next tick through the same path a player's own movement goes through (`world::Zone::request_move`) — never a direct position write. |
| `grant-item` | `func(entity-id: string, item-type: string, quantity: s64) -> result<_, string>` | Queues a grant applied through `character::CharacterStore::grant_item` (#112's `items` table) — player entities only, same caveat as `apply-stat-delta`. A successful grant fires `on-item-acquire` back to the plugin. |
| `remove-item` | `func(entity-id: string, item-type: string, quantity: s64) -> result<_, string>` | Queues a removal applied through `character::CharacterStore::remove_item`. Rejected during the drain (not synchronously) if the character doesn't own enough — same "queued, not confirmed synchronously" caveat as `apply-stat-delta`. |
| `modify-currency` | `func(entity-id: string, delta: s64) -> result<_, string>` | Queues a currency balance adjustment applied through `character::CharacterStore::modify_currency`. Rejected during the drain if the result would go negative. |

All seven are always available to a v0 plugin — no capability gating exists yet (see "Beyond this v0 slice"). `grant-item`/`remove-item`/`modify-currency` only resolve for player entities — an NPC entity id is rejected, since NPCs have no character-backed storage (no NPC item/currency ownership exists, only players own items/currency).

### Ids are opaque strings

Every id crossing the boundary (`entity-id`, `spawn-table-id`, `trigger-id`, ...) is a plain `string`, not a typed WIT record — deliberately: a plugin has no legitimate reason to construct or inspect one of these ids' internal structure, only to pass one it received right back to a host function. Keeping them opaque avoids coupling the interface to whatever internal id representation the host happens to use (`common::id::Id<T>`'s UUID today) — that's free to change without touching this interface.

## `plugin.toml`: the plugin manifest

Same convention as the content manifest and dev-config files elsewhere in the project — one manifest per plugin, checked *before* the plugin is ever instantiated (`PluginHost::load` calls `PluginManifest::check_compatible` first):

```toml
[plugin]
name = "example-plugin"
host_api_version = "0.4.0"
capabilities = []
message_types = []
chat_commands = []
```

| Field | Type | Notes |
|---|---|---|
| `plugin.name` | string | Free-form, used in error/log messages. |
| `plugin.host_api_version` | string | Must equal `plugin_host::HOST_API_VERSION` (currently `"0.4.0"`, matching the WIT package version above) or the plugin is refused before instantiation. |
| `plugin.capabilities` | list of strings, optional | Parsed and carried, **not yet enforced against anything** — see below. |
| `plugin.message_types` | list of `u16`, optional | Gateway `message_type` values (docs/specs/Networking_Spec.md) routed to this plugin's `on-message` hook (#95). Each must be `>= 1000` (0-999 is core-reserved) and appear at most once, checked by `PluginManifest::check_compatible` before the plugin is instantiated. |
| `plugin.chat_commands` | list of strings, optional | Chat command names, without the leading `/` (#57). Each must be non-empty, have no leading `/`, and appear at most once, checked the same way as `message_types`. A matched command is routed to `on-chat-command` instead of published as ordinary chat. |

## Sandbox guarantees

- **No ambient capability.** A plugin gets nothing beyond the two `host` functions above — no direct DB access, no raw network access, no filesystem access, ever (docs/PROPOSAL.md, "Plugin System"). Verified by `plugin_sandbox.rs`.
- **A trap doesn't crash the host.** A panicking/trapping plugin surfaces as an ordinary `Err` from whichever hook call triggered it (`LoadedPlugin::on_load`/etc. return `common::Result<()>`) — the zone-service keeps running. Verified by `plugin_sandbox.rs`'s `a_plugin_panic_does_not_crash_the_host_process`.
- **One `wasmtime::Engine` per zone-service**, shared across every loaded plugin (`PluginHost`) — compiling/loading a component is the expensive part; the engine itself is designed to be shared.

## Beyond this v0 slice

Real design from docs/PROPOSAL.md's "Plugin System" section, deliberately not built yet:

- **`on_tick(zone, dt)`** (the zone-wide tick hook, distinct from #116's per-NPC `on-npc-tick`) — `world`'s tick loop has the call site (`world::zone::Zone::run`'s `on_tick` callback parameter), just not wired to a real plugin call yet.
- **A true synchronous "query" host function** — `item-quantity`/`currency-balance`-style reads that hand a value straight back to the plugin. Deliberately not built: `PluginCallbacks` is called synchronously from inside `wasmtime`, while `character::CharacterStore` is async-only (`sqlx`); reads would need either a new blocking-call mechanism (`tokio::task::block_in_place`, not used anywhere else in this codebase) or an eventually-consistent cache, and neither was worth the complexity for v0 — a plugin that needs to know a quantity/balance can track it itself from `on-item-acquire`/its own bookkeeping.
- **Player session** (`on_player_join_zone`, `on_player_leave_zone`).
- **Live call sites for `on-damage-calc`/`on-death`/`on-respawn`/`on-npc-interact`/`on-item-use`** — these hooks exist and are callable (#116/#57), but nothing in `world`/`gateway`'s client protocol has an attack action, an entity-targeted interact action, or a use-item action yet to call them from; see each hook's row above.
- **Per-plugin optional hooks** — with thirteen hooks now, most plugins won't want all of them; a real WIT world for that likely needs either per-hook opt-in at the manifest level with the host only calling what's declared, or restructuring hooks as several smaller worlds/interfaces a plugin composes from.
- **Real capability gating** — `plugin.toml`'s `capabilities` field exists today but gates nothing; once host functions are grouped (`economy`, `combat`, ...) per the proposal, the host should refuse to load a plugin declaring a capability the operator hasn't enabled for that deployment.
- **Cross-plugin RPC, hot-reload, plugin-defined persistent schema** — explicit v0 non-goals per the proposal, not accidentally missing. (Plugin-declared gateway message types/custom packets *are* now in — `on-message`, #95 — cross-plugin collision checking for them is still deferred; see docs/specs/Networking_Spec.md.)
- **Account roles for dev/admin-only commands** — filed as #114 (decision, undecided) once #57's chat-command hook made it obvious a plugin has no way to know whether the calling account is privileged.
