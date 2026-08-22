# Plugin API

Corresponds to [Plugin System](../PROPOSAL.md#plugin-system) in the proposal.

**Status:** the v0 slice below is real and implemented (`plugin-host`, #37/#38, extended by #95) — one WIT world with five hooks and two host functions, deliberately just enough for "an NPC spawns, a player interacts with it, a plugin can own its own gateway message types." The full v0 hook/host-function list from the proposal's "Plugin System" section — combat, inventory/economy, chat commands, NPC patrol, per-plugin optional hooks, gated capability groups — is real design, not yet implemented; see "Beyond this v0 slice" below.

## Interface technology

WASM Component Model + WIT (docs/PROPOSAL.md, "Interface Technology") — the actual interface lives in `crates/plugin-host/wit/plugin.wit`, this doc explains and indexes it rather than duplicating it verbatim (the `.wit` file is the source of truth for the exact signatures).

- **Host side:** `wasmtime::component::bindgen!` generates Rust bindings from `wit/plugin.wit` directly into `plugin-host` at compile time (`crates/plugin-host/src/bindings.rs`).
- **Guest side:** a plugin author uses `wit-bindgen`'s guest macro (`wit_bindgen::generate!`) for Rust, or the equivalent for any other language `wasmtime`/the Component Model toolchain supports.
- **Target:** `wasm32-wasip2` — this target directly emits a Component Model binary (no separate "componentize a core module" step needed), and comes with the WASI Preview 2 CLI imports a Rust std binary needs to start at all.

## The `plugin` world (v0)

```wit
package worldzero:plugin@0.2.0;

interface host {
    spawn-npc: func(spawn-table-id: string) -> result<string, string>;
    send-message: func(target-entity-id: string, body: string) -> result<_, string>;
}

interface hooks {
    on-load: func();
    on-unload: func();
    on-entity-spawn: func(entity-id: string, entity-type: string);
    on-interact: func(trigger-id: string, actor-entity-id: string);
    on-message: func(message-type: u16, sender-entity-id: string, payload: list<u8>);
}

world plugin {
    include wasi:cli/imports@0.2.12;
    import host;
    export hooks;
}
```

`worldzero:plugin@0.2.0` is the actual versioning mechanism (docs/PROPOSAL.md, "Interface Technology": WIT "worlds" give real interface versioning). A breaking change to this interface bumps the package version and/or introduces a new world; a plugin manifest declares which `host_api_version` it targets (`plugin.toml` below), and `plugin-host` refuses to instantiate a plugin declaring a version it doesn't implement (`crates/plugin-host/src/manifest.rs::PluginManifest::check_compatible`) — it never silently links a plugin against an interface shape it wasn't built for. `0.2.0` added `on-message` (#95); a `0.1.0`-targeting plugin is refused, not silently linked against the new shape.

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

Every plugin exports all four — a WIT world's exports aren't individually optional, so there's no per-plugin "I don't implement this hook" declaration in this v0 slice (contrast with the richer, deferred story in "Beyond this v0 slice" below).

### Host functions (plugin calls out to the host)

| Function | Signature | Does |
|---|---|---|
| `spawn-npc` | `func(spawn-table-id: string) -> result<string, string>` | Spawns one NPC from a zone's declared spawn table (`content::manifest::SpawnTable`), returning its new entity id or an error (unknown table, at max population, ...). |
| `send-message` | `func(target-entity-id: string, body: string) -> result<_, string>` | Sends a visible message to one connected client — the "make the interaction have a visible effect" primitive. |

Both are always available to a v0 plugin — no capability gating exists yet for these two (see "Beyond this v0 slice").

### Ids are opaque strings

Every id crossing the boundary (`entity-id`, `spawn-table-id`, `trigger-id`, ...) is a plain `string`, not a typed WIT record — deliberately: a plugin has no legitimate reason to construct or inspect one of these ids' internal structure, only to pass one it received right back to a host function. Keeping them opaque avoids coupling the interface to whatever internal id representation the host happens to use (`common::id::Id<T>`'s UUID today) — that's free to change without touching this interface.

## `plugin.toml`: the plugin manifest

Same convention as the content manifest and dev-config files elsewhere in the project — one manifest per plugin, checked *before* the plugin is ever instantiated (`PluginHost::load` calls `PluginManifest::check_compatible` first):

```toml
[plugin]
name = "example-plugin"
host_api_version = "0.2.0"
capabilities = []
message_types = []
```

| Field | Type | Notes |
|---|---|---|
| `plugin.name` | string | Free-form, used in error/log messages. |
| `plugin.host_api_version` | string | Must equal `plugin_host::HOST_API_VERSION` (currently `"0.2.0"`, matching the WIT package version above) or the plugin is refused before instantiation. |
| `plugin.capabilities` | list of strings, optional | Parsed and carried, **not yet enforced against anything** — see below. |
| `plugin.message_types` | list of `u16`, optional | Gateway `message_type` values (docs/specs/Networking_Spec.md) routed to this plugin's `on-message` hook (#95). Each must be `>= 1000` (0-999 is core-reserved) and appear at most once, checked by `PluginManifest::check_compatible` before the plugin is instantiated. |

## Sandbox guarantees

- **No ambient capability.** A plugin gets nothing beyond the two `host` functions above — no direct DB access, no raw network access, no filesystem access, ever (docs/PROPOSAL.md, "Plugin System"). Verified by `plugin_sandbox.rs`.
- **A trap doesn't crash the host.** A panicking/trapping plugin surfaces as an ordinary `Err` from whichever hook call triggered it (`LoadedPlugin::on_load`/etc. return `common::Result<()>`) — the zone-service keeps running. Verified by `plugin_sandbox.rs`'s `a_plugin_panic_does_not_crash_the_host_process`.
- **One `wasmtime::Engine` per zone-service**, shared across every loaded plugin (`PluginHost`) — compiling/loading a component is the expensive part; the engine itself is designed to be shared.

## Beyond this v0 slice

Real design from docs/PROPOSAL.md's "Plugin System" section, deliberately not built yet — Phase 1's job (docs/PROPOSAL.md, "Phased Roadmap") is proving the sandboxed boundary works end to end for the simplest real case, not the full surface:

- **The rest of the v0 hook list**: `on_tick(zone, dt)` (the `world` tick loop already has the call site for this — `world::zone::Zone::run`'s `on_tick` callback parameter — just not wired to a real plugin call yet), combat (`on_damage_calc`, `on_death`, `on_respawn`), NPC behavior (`on_npc_tick`, `on_npc_interact`), inventory/economy (`on_item_use`, `on_item_acquire`), chat commands (`on_chat_command`), player session (`on_player_join_zone`, `on_player_leave_zone`).
- **The rest of the v0 host functions**: querying nearby entities, reading/writing declared entity stats (`apply_stat_delta`), teleport-within-bounds, inventory/currency grants, scheduling (N-tick-future callbacks), the plugin-scoped data store (`plugin_state_get`/`plugin_state_set`), structured logging.
- **Per-plugin optional hooks** — once the full hook list exists, most plugins won't want all of them; a real WIT world for that likely needs either per-hook opt-in at the manifest level with the host only calling what's declared, or restructuring hooks as several smaller worlds/interfaces a plugin composes from.
- **Real capability gating** — `plugin.toml`'s `capabilities` field exists today but gates nothing; once host functions are grouped (`economy`, `combat`, ...) per the proposal, the host should refuse to load a plugin declaring a capability the operator hasn't enabled for that deployment.
- **Cross-plugin RPC, hot-reload, plugin-defined persistent schema** — explicit v0 non-goals per the proposal, not accidentally missing. (Plugin-declared gateway message types/custom packets *are* now in — `on-message`, #95 — cross-plugin collision checking for them is still deferred; see docs/specs/Networking_Spec.md.)
