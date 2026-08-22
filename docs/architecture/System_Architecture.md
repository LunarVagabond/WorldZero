# System Architecture

**Status:** mostly placeholder — no system-level diagrams yet. One real section below.

Corresponds to [Architecture Overview](../PROPOSAL.md#architecture-overview) and [Service / Crate Breakdown](../PROPOSAL.md#service--crate-breakdown) in the proposal.

Will hold system-level diagrams and cross-service design notes as they're elaborated beyond the proposal's overview level.

## Optional service toggles

Decided in [#91](https://github.com/LunarVagabond/WorldZero/issues/91), implemented in [#92](https://github.com/LunarVagabond/WorldZero/issues/92): a crate the roadmap documents as optional in the combined `server` process (`chat` first; `transfer`/`plugin-host`/`realm-directory` as later phases bring them online) is toggled at **runtime via config, not a Cargo feature** — `server` always links every crate it supports, and a config flag per optional service decides at startup whether that service's routes/background tasks/DB pool actually start. Core services (auth, character, world, gateway, content) are never part of this toggle.

`common::config::ServicesConfig::from_env()` reads one `WZ_SERVICE_<NAME>_ENABLED` var per optional service (currently `WZ_SERVICE_CHAT_ENABLED`) — unset defaults to `true` (enabled), matching every existing deployment's behavior; a set-but-unparsable value (anything other than `true`/`false`) is a startup error, not a silent fallback.

**Current status:** the config mechanism exists and is tested, but `server` doesn't gate anything with it yet — `chat` itself isn't wired into the combined `server` process at all today (it only runs as its own standalone binary, `crates/chat/src/bin/gateway_server.rs`). Wiring `chat` into `server` and gating that wiring behind `WZ_SERVICE_CHAT_ENABLED` is tracked as a follow-up, not done here.
