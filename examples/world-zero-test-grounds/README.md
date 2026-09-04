# World Zero Test Grounds

**This is an example, not part of the framework.** It's a real, working Godot
4.8 (C#/.NET) client built against WorldZero's actual wire protocol — copy
this directory out as the starting point for your own client, or delete it
entirely if you don't want it. Nothing under `crates/` depends on it, the
cargo workspace never builds it, and no released artifact bundles it.

A deliberately ugly, disposable manual-integration-test client — not a game
in its own right, but a debug tool for running two clients side by side and
watching WorldZero's real backend systems work (auth, movement, chat, combat,
zones, parties, guilds, crafting) end to end. It's the most complete worked
example in this repo of what a real client integration actually looks like.

Read `PROMPT.md` first — it's the full wire-protocol contract this client was
built against, cited against WorldZero's actual source as of when it was
written (dated, not a living document — treat it as a detailed worked
example, not current API reference; `docs/specs/` is the source of truth for
that). This README only covers running *this* project.

## What's built

- `Net/` — the raw TCP+TLS socket layer: `Envelope.cs` (length-delimited
  framing, byte-for-byte matched against `world_zero/crates/gateway/src/envelope.rs`),
  `GameConnection.cs` (socket + background read loop), `NetworkClient.cs` (the
  autoload that decodes every envelope by `message_type` and dispatches typed
  C# events).
- `Protos/` — the five real `.proto` files copied verbatim from `world_zero`
  (auth/realm/character/chat/session), with `csharp_namespace` options added
  for C# codegen. Regenerated at build time via `Grpc.Tools`' bundled `protoc`
  — no system `protoc` install needed.
- `State/` — `GameState.cs` (everything PROMPT.md §15 says to track: ids,
  roster, party/guild, stats/items/currency, the event log), `EnvConfig.cs`
  (loads `.env`), `SessionStore.cs` (persists the last session token for
  `Resume`).
- `Movement/` — client-side prediction/reconciliation for your own entity
  (`PredictedMovement.cs`, using the real `seq` correlation from `Move`/
  `Moved`/`Rejected`) and interpolation for everyone else
  (`EntityInterpolator.cs`).
- `Scenes/` — `Main.cs` orchestrates the auth → realm-select → character-select
  → world-join flow; `WorldController.cs` is the 3D scene (primitive
  capsules/boxes, WASD movement, click-to-target, the Evil Cube with its
  floating HP label); `Scenes/UI/` has the login form, character picker
  (including a real archetype picker — see "One discovery beyond PROMPT.md"
  below), and the tabbed HUD (Debug/Chat/Party/Guild/Craft/Inventory).
- [`../evil-cube-plugin/`](../evil-cube-plugin) — the small custom WASM plugin
  (PROMPT.md §7.2) backing the Evil Cube NPC: spawns it, applies damage via
  `apply-stat-delta`, tracks HP in zone-scoped plugin state (since
  `apply-stat-delta` still can't return the resulting value), and pushes HP to
  the attacker via the `cube:` text convention below.

All UI is built in code (no hand-authored widget trees in `.tscn` files) —
this was built without an interactive Godot editor session available, so
leaning on code-constructed `Control`/`Node3D` trees was the reliable path.
`Scenes/Main.tscn` is a minimal scene stub that just attaches `Main.cs`;
everything else is built in `_Ready()`.

## One discovery beyond PROMPT.md

`character.proto` (checked directly against `world_zero`'s current source,
not just the prompt doc) has a real `ListCharacterOptions`/`CharacterOptions`/
`ArchetypeOption` request-response and a `CreateCharacter.archetype_key`
field that PROMPT.md §3.3 doesn't mention — it describes class/archetype
selection as necessarily client-side-cosmetic only. That's no longer
accurate: there's a real "ask the server for the declared archetype list,
let the player pick one" flow now (backed by `character.archetypes.yaml`).
`Scenes/UI/CharacterSelectPanel.cs` uses it for a real archetype picker.

## Requirements

- Godot 4.8 (Mono/.NET build) — this project was built/verified against
  `Godot_v4.8-dev3_mono`.
- .NET SDK (8.0 target; the project builds fine under a newer installed SDK
  via `TargetFramework=net8.0`).
- A running World Zero `server` — see "Backend setup" below. **This needs
  Postgres and Redis reachable; if those live on a separate VM, that VM has
  to be up before `server` will start.**

## TLS

`server` generates a self-signed cert for `"localhost"` (PROMPT.md §2.1).
This client uses choice **(b)**: it disables certificate validation entirely
in `GameConnection.cs` (`RemoteCertificateValidationCallback` always accepts).
This is only acceptable because it's a disposable local-dev tool talking to
`localhost` — don't reuse this networking code against anything else.

## Setup

1. Copy `.env.example` to `.env` in this project's root. Realm selection is a
   real in-client picker now (`Scenes/UI/RealmSelectPanel.cs`) — no realm id
   needs to go in `.env`; log in and pick it on screen instead.
2. Open this project in the Godot 4.8 Mono editor, or build headless:
   ```sh
   dotnet build
   # or, via Godot's own build pipeline:
   Godot_v4.8-dev3_mono_linux.x86_64 --headless --path . --build-solutions --quit
   ```
3. Run it (editor Play button, or `Godot_v4.8-dev3_mono_linux.x86_64 --path .`).
   Launch two instances side by side for the two-client tests in
   PROMPT.md §18.

## Backend setup (from the `world_zero` repo root, two directories up) — do this before running the client

**If Postgres/Redis run on a separate VM, start that VM first** — `server`
refuses to start without both reachable.

```sh
cd ../..   # world_zero repo root, if you're starting from this example's directory

# 1. Postgres + Redis (or point WZ_POSTGRES_*/WZ_REDIS_* at your own instances)
make docker-up
cargo run -p common --bin migrate -- up

# 2. Create a realm — no need to save the printed id anywhere, the client
#    lists/selects realms live via RealmSelectPanel after login
make realm ARGS="create MyRealm open"

# 3. Config — content-pack.yaml, character.archetypes.yaml, crafting.schema.yaml,
#    and currency.schema.yaml already exist in this checkout (copies of their
#    .example.yaml). greenwood-forest.yaml already has the evil-cube-01 spawn
#    table added. `make quickstart` copies the rest for a fresh checkout.

# 4. Build and place the Evil Cube plugin
rustup target add wasm32-wasip2
cargo build --manifest-path examples/evil-cube-plugin/Cargo.toml --target wasm32-wasip2 --release
mkdir -p config/plugins/evil-cube-plugin
cp examples/evil-cube-plugin/plugin.toml config/plugins/evil-cube-plugin/
cp examples/evil-cube-plugin/target/wasm32-wasip2/release/evil_cube_plugin.wasm config/plugins/evil-cube-plugin/

# (optional) also drop in the shipped example-plugin for its wolf-pack/
# /wave chat command behavior — capabilities/message_types/chat_commands
# don't collide with evil-cube-plugin, so both can run at once.

# 5. Run
WZ_REALM_ID=<id from step 2> cargo run -p server
```

Look for `worldzero server listening local_addr=127.0.0.1:7900` in the log —
that's the client's connect target (`WZ_TEST_SERVER_HOST`/`_PORT` in `.env`).

## UI layout

The HUD is a bottom-anchored, full-width dock (not a right-side full-height
one) — `Scenes/UI/Hud.cs` puts one subsystem per tab (Debug/Chat/Party/Guild/
Craft, plus Admin once your account announces the `admin` role) in a Godot
`TabContainer`. Only one tab's contents are ever visible at a time, and the
active tab gets the dock's full width, rather than every panel splitting the
width side by side (the previous layout). Login, Realm Select, and Character
Select are grouped into titled sections (`UiHelpers.Section`) rather than one
long unbroken column of fields.

## Admin panel

World Zero has a real, backend-enforced account-roles system
(`docs/specs/Auth_Spec.md`'s "Account roles"), and `Authenticated` now echoes
a connecting client's own roles back on the wire — this project predates
that and still works around both original gaps, left as-is since it's a
harmless belt-and-suspenders approach, not because the workaround is still
strictly necessary:

- `evil-cube-plugin` announces your account's roles to you on zone join as
  an ad-hoc `PluginMessage` (`roles:admin`, or `roles:` if you have none) —
  the client uses this only to decide whether to show the Admin panel at
  all. It is never the actual authorization check.
- Every admin action is a real `caller-role`-gated plugin chat command
  (`/grant`, `/grantcurrency`, `/killcube`, `/respawncube`) — the plugin
  re-checks the account's real role server-side on every single call, so a
  non-admin account is blocked by the backend itself even if it somehow saw
  the panel.
- Granting the role itself: `make role ARGS="grant <username> admin"` from
  the `world_zero` repo root (mirrors `make realm`'s CLI pattern).

`/grant`/`/grantcurrency` are also the only way to test crafting right now
— nothing else in this setup grants any item, so `CraftItem` has no inputs
to consume without them (e.g. grant `wolf-fang` x3 + `iron-ore` x2, then
craft `wolf-fang-dagger` — see `crafting.schema.yaml`).

## The Evil Cube's HP convention

The core has no client-visible push for NPC stats (`StatChanged` only ever
covers a connection's own character — PROMPT.md §4/§7.1). `evil-cube-plugin`
pushes HP to the attacker as a `PluginMessage` body using this convention,
parsed in `WorldController.HandlePluginMessage`:

```
cube:<entity_id>:hp:<current>/<max>
cube:<entity_id>:dead
cube:<entity_id>:respawned:hp:<max>
```

## Verified against a real running server

The full wire protocol has been exercised end to end against a real, locally
running `server` with real Postgres/Redis — Register → Authenticated →
SelectRealm → RealmSelected → CreateCharacter → SelectCharacter → automatic
Joined (with the Evil Cube in the roster) → Move → a deliberately-too-fast
move correctly got `Rejected{reason: "TooFast..."}` → Ping/Pong → Attack →
`PluginMessage` back with `cube:<id>:hp:40/50`, confirming
`evil-cube-plugin`'s damage math. `make status`/`make stop`/`make start` (run
from the `world_zero` repo root) manage the server process; `/readyz` and
`/healthz` (`http://127.0.0.1:9091/{readyz,healthz}`) report dependency
health.

`WorldController.cs` filters the roster on `entity_type == "npc.evil_cube"`
(PROMPT.md §7.2 step 5's original guidance) — this briefly had to fall back
to a looser `"npc"` prefix check against a real server bug (#239) that collapsed every NPC's wire
`entity_type` to the literal `"npc"`, discarding the spawn table's declared
type. Fixed now, so the filter works as originally documented.

## What's still genuinely unverified

No display was available to drive the Godot editor interactively, so nobody
has clicked through the actual UI/3D scene yet. The C# project builds clean
(`dotnet build`, and Godot's own headless `--build-solutions`), all three
autoloads instantiate, and the full scene tree constructs without runtime
errors under `--headless` — but that's static/headless verification, not a
human watching movement prediction, the health bar, or panel layout on
screen. Open this project in the Godot 4.8 Mono editor and run through
PROMPT.md §18's full two-client checklist for that.
