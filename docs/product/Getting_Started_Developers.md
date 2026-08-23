# Getting Started for Developers

Corresponds to [The Developer Experience Bar](../PROPOSAL.md#the-developer-experience-bar) in the proposal — the one-command scaffold below is [#43](https://github.com/LunarVagabond/WorldZero/issues/43); the timed clone-to-running-world validation is [#44](https://github.com/LunarVagabond/WorldZero/issues/44), a separate follow-up, not covered here.

## Prerequisites

- Rust, pinned via [`rust-toolchain.toml`](../../rust-toolchain.toml) — `rustup` picks this up automatically once you're in the repo.
- A reachable Postgres and Redis. Copy [`.env.example`](../../.env.example) to `.env` and fill in `WZ_POSTGRES_*`/`WZ_REDIS_*`. This is the one piece of required configuration — everything else below is automatic.

That's it. No docker-compose ships with this repo today; point `.env` at whatever Postgres/Redis you already have running (local install, a container you started yourself, a remote dev instance).

## One command

```sh
make quickstart
```

This is the actual thing docs/PROPOSAL.md's Developer Experience Bar asks for: a complete, runnable default game with zero required configuration beyond the Postgres/Redis connection info above. It:

1. Copies [`config/zone.manifest.example.yaml`](../../config/zone.manifest.example.yaml) → `config/zone.manifest.yaml` and [`config/stats.schema.example.yaml`](../../config/stats.schema.example.yaml) → `config/stats.schema.yaml`, but only if you don't already have your own — it never overwrites a config you've customized.
2. Builds [`examples/example-plugin`](../../examples/example-plugin) — the shipped example plugin — for `wasm32-wasip2` (adding that `rustup` target first if you don't have it).
3. Applies pending database migrations.
4. Starts `server` in the foreground, with the example plugin loaded.

Safe to run again any time — every step is a no-op if there's nothing left to do (an existing config file is left alone, an already-applied migration is skipped, rebuilding the plugin is harmless).

## What you get

A `server` process listening on `127.0.0.1:7900`, running one zone ("Greenwood Forest," from the example manifest) with one NPC pack already spawned by the example plugin's `on_load` hook — you'll see `spawned NPC from plugin` in the log. The example plugin also has an `on_interact` hook ready to reply to the zone manifest's `forest-entrance` trigger, but wiring a player's live movement into that trigger volume isn't built yet (docs/specs/Plugin_API.md, "Beyond this v0 slice") — what *is* live today, and what the example plugin does demonstrate end to end, is `on_message`: it declares `message_types = [1000]` in its `plugin.toml`, and any client that sends the gateway an envelope with `message_type` 1000 gets a reply straight from the plugin's `on_message` hook, over the same connection as everything else (#95). `crates/server/tests/server_smoke.rs` exercises exactly this.

**There's no GUI client yet** — this project is infrastructure, not a game, and a real client is a native-engine concern outside this repo's scope (docs/PROPOSAL.md, "What This Project Is Not"). To see the server actually respond to something today:

- [`crates/server/tests/server_smoke.rs`](../../crates/server/tests/server_smoke.rs) is a real client speaking the wire protocol end to end (connect, register/login, see the spawned NPC in your join roster, move, interact via a plugin-routed message) — read it as a worked example of exactly what a real client needs to do.
- `make chat-server` + `make chat NAME=<you>` runs chat's own standalone demo server and an interactive terminal client, if you want to see two connections talk to each other without writing any client code yourself.

## Everything else

Once you're past first boot: [`docs/specs/`](../specs) covers each service's wire protocol and data model in depth, and [`docs/architecture/System_Architecture.md`](../architecture/System_Architecture.md) covers cross-service design (e.g. how optional services like `chat` get toggled). `examples/example-plugin` is a real, if minimal, starting point for writing your own plugin — copy it and go from there.
