# Plugin Development Guide

How a plugin actually gets from your editor into a running server — what language you write it in, how it's built, and how `server` picks it up. Distinct from [`docs/specs/Plugin_API.md`](../specs/Plugin_API.md), which is the full reference for every hook and host function; this doc is the workflow around that reference, not a replacement for it.

## The short answer

A plugin is a **WASM component**, not a script your game engine loads. You write it in a language that can target the [WASM Component Model](https://component-model.bytecodealliance.org/) against this project's WIT interface, compile it to a `.wasm` file, write a small `plugin.toml` manifest next to it, and drop both into their own subdirectory under `server`'s plugins directory before starting it. There's no client involved at all — a Godot/Unity/UE5 game, if you're building one, is a completely separate project that talks to `server` over the network wire protocol. It never touches plugin code, and plugin code never touches rendering, input, or anything client-side.

## No, not GDScript (or any client-engine scripting language)

This trips people up because "plugin" sounds like it might mean "a script the game engine runs." It doesn't. Your game engine (Godot, Unity, whatever you build a client in) is the **player-facing** half — it renders the world and sends/receives messages over `gateway`'s wire protocol (TCP+TLS). Your **plugin** is the **server-authoritative** half — it runs inside the combined `server` process, sandboxed, and decides things like "does this NPC exist," "how much damage did that hit do," "what happens when someone runs `/give torch`." These are two different codebases, two different toolchains, and they only ever communicate over the network protocol, the same way any client talks to any server. GDScript can't produce a WASM component targeting this project's interface, so it isn't a plugin language here — but a Godot client is exactly how you'd play a game *built* on a WorldZero server, regardless of what language the server-side plugin is written in.

## What language, then?

Today: **Rust**, via [`wit-bindgen`](https://github.com/bytecodealliance/wit-bindgen), targeting `wasm32-wasip2`. That's what [`examples/example-plugin`](../../examples/example-plugin) and the test fixture at `crates/plugin-host/tests/fixtures/test-plugin` are, and it's the only toolchain this repo currently demonstrates or has tooling for.

The WASM Component Model + WIT approach (`docs/PROPOSAL.md`, "Interface Technology") is deliberately not Rust-specific — other languages with a component-model toolchain (C, TinyGo, `componentize-py`, ...) could in principle target the same `wit/plugin.wit` interface. Nobody's built or tested that path in this project yet, so treat it as theoretically possible, not a supported workflow.

## Step by step

1. **Write your plugin against `wit/plugin.wit`.** Copy [`examples/example-plugin`](../../examples/example-plugin) as your starting point rather than starting from scratch — it's a real, working `Cargo.toml` + `wit_bindgen::generate!` setup already wired to this project's interface. Implement the `Guest` trait's hooks you actually need (`on-zone-loaded`, `on-message`, `on-chat-command`, ...) — see [`docs/specs/Plugin_API.md`](../specs/Plugin_API.md) for the full hook/host-function reference and [`Server_Customization_Guide.md`](Server_Customization_Guide.md#step-5--plugins-your-actual-gameplay-logic-plugin-host)'s plugin section for the manifest fields. Your plugin loads exactly once for the whole `server` process, not once per zone (#152) — anything zone-specific (NPC seeding, a zone-scoped effect, ...) takes the hook's `zone-id` parameter and checks it yourself, rather than being attached to a single zone by configuration.

2. **Build it:**
   ```sh
   rustup target add wasm32-wasip2   # once
   cargo build --target wasm32-wasip2 --release
   ```
   This produces a real `.wasm` component at `target/wasm32-wasip2/release/<your_crate_name>.wasm` — this file *is* your plugin, no further packaging step.

3. **Write `plugin.toml`** declaring your plugin's name, the `host_api_version` it targets (must match `plugin_host::HOST_API_VERSION`), which gateway `message_type`s and chat commands you want routed to you, and which hooks (besides `on-message`/`on-chat-command`, routed by the fields above instead) you actually want called via `hooks` (#152). [`config/plugin.example.toml`](../../config/plugin.example.toml) is a real starting point.

4. **Drop both files into `server`'s plugins directory** and (re)start it:
   ```sh
   mkdir -p <config_dir>/plugins/your-plugin-name
   cp plugin.toml <config_dir>/plugins/your-plugin-name/
   cp target/wasm32-wasip2/release/<your_crate_name>.wasm <config_dir>/plugins/your-plugin-name/
   cargo run -p server
   ```
   `<config_dir>/plugins` is the default (override with `WZ_PLUGINS_DIR`) — `server` auto-discovers every `<name>/{plugin.toml,*.wasm}` subdirectory in it at startup, so more than one plugin loads just by having more than one subdirectory. `server` validates each `host_api_version`/`message_types`/`chat_commands`/`hooks` before it ever instantiates a `.wasm` (`PluginManifest::check_compatible`), and checks the whole set collectively for `message_type`/`chat_command` collisions across plugins (`check_no_collisions`) — a mismatch, a malformed manifest, or a collision is a startup-time panic with a clear message, not a silent skip.

That's the entire deploy mechanism today: **drop a `{plugin.toml,*.wasm}` pair into its own subdirectory under the plugins directory on the machine running `server`, restart.**

## What doesn't exist yet (and that's a known, tracked gap)

There's no install command, no registry, and no hot-reload — a code change means rebuilding the `.wasm` and restarting the server process. This isn't an oversight; it's [#24](https://github.com/LunarVagabond/WorldZero/issues/24) (the packaging/distribution decision) still being genuinely open, with the fuller story ([#58](https://github.com/LunarVagabond/WorldZero/issues/58) — versioned updates, possibly a registry) explicitly deferred until after that. Today's directory-discovery mechanism is the real, working baseline everything else builds on, not a placeholder that's about to change out from under you — but don't assume any packaging convention beyond "one subdirectory per plugin" until #24 actually lands.

## Where to go next

- [`docs/specs/Plugin_API.md`](../specs/Plugin_API.md) — every hook and host function, what's live vs. not yet wired to a real call site.
- [`examples/example-plugin`](../../examples/example-plugin) — copy this, don't start from a blank `Cargo.toml`.
- [`crates/plugin-host/tests/plugin_sandbox.rs`](../../crates/plugin-host/tests/plugin_sandbox.rs) — real integration tests against a real compiled plugin; useful as further worked examples of calling every host function.
- [`Server_Customization_Guide.md`](Server_Customization_Guide.md) — where plugins fit alongside every other piece of server configuration.
