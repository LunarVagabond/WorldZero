# System Architecture

**Status:** mostly placeholder — no system-level diagrams yet. One real section below.

Corresponds to [Architecture Overview](../PROPOSAL.md#architecture-overview) and [Service / Crate Breakdown](../PROPOSAL.md#service--crate-breakdown) in the proposal.

Will hold system-level diagrams and cross-service design notes as they're elaborated beyond the proposal's overview level.

## Optional service toggles

Decided in [#91](https://github.com/LunarVagabond/WorldZero/issues/91), implemented in [#92](https://github.com/LunarVagabond/WorldZero/issues/92): a crate the roadmap documents as optional in the combined `server` process (`chat` first; `transfer`/`plugin-host`/`realm-directory` as later phases bring them online) is toggled at **runtime via config, not a Cargo feature** — `server` always links every crate it supports, and a config flag per optional service decides at startup whether that service's routes/background tasks/DB pool actually start. Core services (auth, character, world, gateway, content) are never part of this toggle.

`common::config::ServicesConfig::from_env()` reads one `WZ_SERVICE_<NAME>_ENABLED` var per optional service (currently `WZ_SERVICE_CHAT_ENABLED`) — unset defaults to `true` (enabled), matching every existing deployment's behavior; a set-but-unparsable value (anything other than `true`/`false`) is a startup error, not a silent fallback.

**Current status:** `chat` is wired in ([#104](https://github.com/LunarVagabond/WorldZero/issues/104)) — the combined `server` process's own per-connection session loop (`crates/server/src/session.rs`) dispatches `chat::gateway_protocol`'s `message_type` 100 alongside world (200) and any configured plugin's declared types, all over the same authenticated gateway connection (see docs/specs/Networking_Spec.md's catalog). `chat::bin::gateway_server` still exists as a separate standalone demo entry point — it isn't replaced by this, just no longer the only way to exercise chat over the real gateway transport. When `WZ_SERVICE_CHAT_ENABLED=false`, `server` never constructs a `ChannelStore`/`ChatBus` at all, and message_type 100 gets a clear "chat is disabled on this server" error reply rather than being silently dropped or crashing the connection.
