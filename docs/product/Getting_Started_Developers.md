# Getting Started for Developers

Corresponds to [The Developer Experience Bar](../PROPOSAL.md#the-developer-experience-bar) in the proposal — the one-command scaffold below is [#43](https://github.com/LunarVagabond/WorldZero/issues/43); the timed clone-to-running-world validation is [#44](https://github.com/LunarVagabond/WorldZero/issues/44), a separate follow-up, not covered here.

## Prerequisites

- Rust, pinned via [`rust-toolchain.toml`](../../rust-toolchain.toml) — `rustup` picks this up automatically once you're in the repo.
- A reachable Postgres and Redis. Copy [`.env.example`](../../.env.example) to `.env` and fill in `WZ_POSTGRES_*`/`WZ_REDIS_*`. This is the one piece of required configuration — everything else below is automatic.

That's it. Point `.env` at whatever Postgres/Redis you already have running (local install, a container you started yourself, a remote dev instance) — or, if you don't have one yet, `make docker-up` below will start one for you.

### Don't have a Postgres/Redis yet? `make docker-up`

[`docker-compose.yml`](../../docker-compose.yml) starts local Postgres/Redis containers for you. This is a convenience for small teams and for sandboxing/testing, not the production story — this repo does not default to Docker in dev, and a real deployment (this project's own included) may need Postgres/Redis split across several machines for scale, which this compose file doesn't attempt.

```sh
cp .env.example .env   # then fill in WZ_POSTGRES_USER/WZ_POSTGRES_PASSWORD/WZ_POSTGRES_DATABASE
make docker-up
```

`make docker-up` runs a preflight check first ([`scripts/docker_preflight.sh`](../../scripts/docker_preflight.sh)): if `.env` is missing, or missing required values, it tells you exactly what's needed and stops before touching Docker, instead of failing later with an opaque connection error. Once the containers are up, set `WZ_POSTGRES_HOST`/`WZ_REDIS_HOST` to `localhost` in `.env` — everything else below (`make quickstart`, `make migrate`, `cargo run -p server`) then works the same as against any other Postgres/Redis. `make docker-down` stops the containers (data persists in a named volume); `make docker-status`/`make docker-logs` for the rest.

## One command

```sh
make quickstart
```

This is the actual thing docs/PROPOSAL.md's Developer Experience Bar asks for: a complete, runnable default game with zero required configuration beyond the Postgres/Redis connection info above. It:

1. Copies every `config/*.example.yaml` this project ships → its real counterpart (`zone.manifest.yaml`, `stats.schema.yaml`, `party.schema.yaml`, `guild.schema.yaml`, `character.archetypes.yaml`, `crafting.schema.yaml`, `currency.schema.yaml`), but only for a file you don't already have your own copy of — it never overwrites a config you've customized. See [`Server_Customization_Guide.md`](Server_Customization_Guide.md) for what each one actually does.
2. Builds [`examples/example-plugin`](../../examples/example-plugin) — the shipped example plugin — for `wasm32-wasip2` (adding that `rustup` target first if you don't have it).
3. Applies pending database migrations.
4. Starts `server` in the foreground, with the example plugin loaded.

Safe to run again any time — every step is a no-op if there's nothing left to do (an existing config file is left alone, an already-applied migration is skipped, rebuilding the plugin is harmless).

## What you get

A `server` process listening on `127.0.0.1:7900`, running one zone ("Greenwood Forest," from the example manifest) with one NPC pack already spawned by the example plugin's `on_zone_loaded` hook — you'll see `spawned NPC from plugin` in the log. The example plugin also has an `on_interact` hook ready to reply to the zone manifest's `forest-entrance` trigger, but wiring a player's live movement into that trigger volume isn't built yet (docs/specs/Plugin_API.md, "Beyond this v0 slice") — what *is* live today, and what the example plugin does demonstrate end to end, is `on_message`: it declares `message_types = [1000]` in its `plugin.toml`, and any client that sends the gateway an envelope with `message_type` 1000 gets a reply straight from the plugin's `on_message` hook, over the same connection as everything else (#95). `crates/server/tests/server_smoke.rs` exercises exactly this.

**There's no GUI client yet** — this project is infrastructure, not a game, and a real client is a native-engine concern outside this repo's scope (docs/PROPOSAL.md, "What This Project Is Not"). To see the server actually respond to something today:

- [`crates/server/tests/server_smoke.rs`](../../crates/server/tests/server_smoke.rs) is a real client speaking the wire protocol end to end (connect, register/login, see the spawned NPC in your join roster, move, interact via a plugin-routed message) — read it as a worked example of exactly what a real client needs to do.
- `make chat-server` + `make chat NAME=<you>` runs chat's own standalone demo server and an interactive terminal client, if you want to see two connections talk to each other without writing any client code yourself.

## Everything else

Ready to turn this into your own game? [`Server_Customization_Guide.md`](Server_Customization_Guide.md) is the step-by-step walkthrough of every crate's own configuration — your stats schema, world tuning, auth, plugins, and more — in the order you'd actually touch them. Writing actual gameplay logic (not just config)? [`Plugin_Development_Guide.md`](Plugin_Development_Guide.md) covers what language plugins are written in, how they're built, and how `server` picks them up — including the "wait, is this a Godot script?" question (no).

Once you're past first boot: [`docs/specs/`](../specs) covers each service's wire protocol and data model in depth, and [`docs/architecture/System_Architecture.md`](../architecture/System_Architecture.md) covers cross-service design (e.g. how optional services like `chat` get toggled). `examples/example-plugin` is a real, if minimal, starting point for writing your own plugin — copy it and go from there.
