# Connecting a Client

You've got `make quickstart` running (see [`Getting_Started_Developers.md`](Getting_Started_Developers.md)) — a `server` process sitting there, listening, with a zone loaded and an example plugin running. This doc is the missing piece between that and your own game client (Unity, UE5, Godot, a terminal test harness, whatever) actually talking to it.

This is a narrative walkthrough, not the wire reference — [`docs/specs/Networking_Spec.md`](../specs/Networking_Spec.md) is the source of truth for every byte layout and message shape; this doc explains *why* each step exists and points you at real, working code for each one. Two reference implementations back every claim here:

- [`crates/server/tests/server_smoke.rs`](../../crates/server/tests/server_smoke.rs) — a real Rust client speaking the full protocol end to end against a real compiled `server` binary. Read this if you want to see the sequence of operations with nothing hidden.
- [`examples/world-zero-test-grounds`](../../examples/world-zero-test-grounds) — a real Godot 4.8 (C#) client. Read this if you're building in an engine and want to see what the socket/framing/dispatch layer looks like in a language other than Rust. Its own `PROMPT.md` is a detailed (but dated — not a living doc) worked example of the same contract.

You don't need Rust or Godot experience to use this doc — both are cited as "here's the real code," not as the only way to do this.

## 1. What you're connecting to

By default, `make quickstart` leaves `server` listening on `127.0.0.1:7900` (`WZ_SERVER_ADDR`, see [`Server_Customization_Guide.md`](Server_Customization_Guide.md#step-4--networking-gateway) Step 4). That's a single TCP socket, upgraded to TLS immediately after connecting — every real message you'll send (auth, realm/character selection, movement, chat, plugin traffic) is multiplexed over this one connection, distinguished only by a `message_type` field.

**TLS and local dev.** `gateway` generates a self-signed certificate under `<config_dir>/certs/` the first time it runs (unless you've pointed `WZ_TLS_CERT_PATH`/`WZ_TLS_KEY_PATH` at a real one) and logs its SHA-256 fingerprint at `INFO` on every startup. There's no CA behind it, so your client has two honest options for local development:

- **Skip certificate validation entirely.** This is what the Godot reference client does — see [`Net/GameConnection.cs`](../../examples/world-zero-test-grounds/Net/GameConnection.cs)'s `AcceptAnyServerCertificate`, which unconditionally returns `true` from the TLS validation callback. Fine for a disposable client that only ever talks to localhost; document it as a known trade-off, don't ship it against a real deployment.
- **Pin the fingerprint.** Trust-on-first-use, the same model SSH/Signal use: read the fingerprint `server` logs at startup out-of-band, and have your client compare the presented certificate against it instead of doing full CA-chain validation. `crates/server/tests/server_smoke.rs`'s `connect()` helper does the equivalent — it loads the exact certificate `gateway::tls::load_or_generate` wrote to disk and adds it to its own trusted root store, so its handshake only succeeds against that specific cert.

For a real deployment, the operator points `WZ_TLS_CERT_PATH`/`WZ_TLS_KEY_PATH` at a real (e.g. Let's Encrypt) certificate, and your client does ordinary CA-chain validation like it would against any other TLS server — no special-casing needed at that point.

**The UDP/DTLS channel — a heads-up, not something you need to build today.** [`docs/specs/Networking_Spec.md`](../specs/Networking_Spec.md#dtls-udp-channel) specs an unreliable UDP channel (DTLS-secured, reusing the same cert/key as TCP) for high-frequency, loss-tolerant traffic. The `gateway` crate has a real, working implementation of it (`crates/gateway/src/udp.rs` — a full DTLS handshake/record-layer driver against a real `tokio::net::UdpSocket`), but **the combined `server` binary doesn't call it** — `crates/server/src/main.rs` never stands up a UDP listener. So today, everything — including `Move` — travels over the single TCP+TLS connection, and that's exactly what both reference clients do (the Godot client's own `GameConnection.cs` comment is explicit: "One TCP+TLS socket, one connection, for the whole session"). Build your client against TCP-only for now; if/when the UDP channel gets wired into `server`, that'll be a documented addition, not a silent one.

## 2. Message framing

Every envelope on the wire has the same shape, regardless of which logical protocol (auth, realm, character, chat, session/world, or a plugin's own) it carries:

```
message_type: u16   -- discriminant; 0-999 is core-reserved, >= 1000 is plugin-declared
payload:      bytes -- message_type-specific, opaque to the framing layer
```

Over TCP, this is length-prefixed: a 4-byte big-endian `u32` giving the byte length of everything that follows (`message_type` + `payload`, not including the length field itself), then that many bytes. This is exactly `tokio_util::codec::LengthDelimitedCodec`'s default framing on the Rust side (`gateway::EnvelopeCodec`), and the Godot client's [`Net/Envelope.cs`](../../examples/world-zero-test-grounds/Net/Envelope.cs) implements the identical layout by hand:

```
[ 4-byte big-endian u32: length of everything that follows ]
[ 2-byte big-endian u16: message_type ]
[ N bytes: protobuf-encoded payload ]
```

Whatever language/engine you're building in, this is the shape to implement first, before you touch any actual message type — a length-prefixed reader/writer over your TLS stream. `Envelope.cs`'s `ReadOne`/`Write` are about 40 lines total and are a reasonable model to copy regardless of target language.

**Payload encoding is protobuf** (`prost` on the Rust side), for every `message_type` in the catalog. Point your engine's own protobuf codegen at the real `.proto` files rather than hand-rolling parsers:

- [`crates/auth/proto/auth.proto`](../../crates/auth/proto/auth.proto) — `message_type` 1
- [`crates/server/proto/realm.proto`](../../crates/server/proto/realm.proto) — `message_type` 2
- [`crates/server/proto/character.proto`](../../crates/server/proto/character.proto) — `message_type` 3
- [`crates/chat/proto/chat.proto`](../../crates/chat/proto/chat.proto) — `message_type` 100
- [`crates/server/proto/session.proto`](../../crates/server/proto/session.proto) — `message_type` 200

[`examples/world-zero-test-grounds/Protos/`](../../examples/world-zero-test-grounds/Protos) has these same five files copied verbatim (with `csharp_namespace` options added for C# codegen) — a useful reference for how to adapt them for your own engine's protobuf plugin. There's no packaged client-SDK/codegen cookbook yet (`docs/specs/Networking_Spec.md`'s "Client-integrator codegen" section is explicit about this gap) — for now, you run your engine's own `protoc` + language plugin (Unity's built-in C# protobuf support, `protoc --cpp_out` for UE5, a community Godot plugin, etc.) against these files directly.

## 3. The handshake: auth → realm → character → join

A fresh connection has to complete a fixed sequence before it can do anything world-related. Every step here is mandatory and ordered — you can't select a character before selecting a realm, and you can't do anything on the session (`message_type` 200) protocol before a character is selected. `server_smoke.rs`'s `register_and_authenticate` helper is the compressed version of this whole sequence; the walkthrough below unpacks it into its four stages.

### 3a. Register or Login (`message_type` 1)

Send an `auth.ClientMessage` wrapping either `Register { username, password }` or `Login { username, password }`. A previously-issued session can also be resumed with `Resume { session_token }` instead of re-entering a password (`AuthClientMessage::Resume` — see [`State/SessionStore.cs`](../../examples/world-zero-test-grounds/State/SessionStore.cs) for how the reference client persists and replays this).

On success, you get back `auth.ServerMessage`'s `Authenticated { account_id, username, session_token, roles }` — `roles` is your account's global-scope role list (empty if you don't have any), useful if your client wants to show admin-only UI without inventing its own convention. On failure (bad credentials, a username already taken on `Register`), you get `Error { message }` instead — check for this before assuming you're through.

### 3b. SelectRealm (`message_type` 2)

Every connection must select a realm before doing anything else, even in a single-realm deployment with no picker UI to build. Send `realm.ClientMessage`'s `ListRealms {}` if you want to show a picker — you'll get back `RealmList { realms: [RealmSummary...] }`, each entry naming its `realm_id`, `name`, `open_or_bound` (`"open"` or `"bound"`), and population counts. Then send `SelectRealm { realm_id }`.

Success is `RealmSelected { realm_id }`, echoing the id back so you have explicit confirmation rather than inferring it from the absence of an error. Rejection happens as `Error { message }` — e.g. naming a realm this `server` process doesn't actually serve (a single `server` process serves exactly one realm today, per [`Server_Customization_Guide.md`](Server_Customization_Guide.md#step-8--realms--transfers-real-but-not-yet-live) Step 8 — `WZ_REALM_ID`). This is also where **bound-realm policy** comes in: see [`docs/specs/Realm_Character_Policy_Spec.md`](../specs/Realm_Character_Policy_Spec.md) for what a bound-realm login rejection actually looks like and how open-realm session leases work — nothing about the message shape changes, but the server's decision to accept or reject can depend on it.

### 3c. CreateCharacter / SelectCharacter (`message_type` 3)

Send `ListCharacters {}` to get `CharacterList { characters: [CharacterSummary...] }` — each with `character_id`, `name`, `zone_id`. If the account has none yet (or you want a new one), send `CreateCharacter { name, archetype_key }`; leaving `archetype_key` empty resolves to the first entry declared in `character.archetypes.yaml` (see [`Server_Customization_Guide.md`](Server_Customization_Guide.md#step-1--your-games-stats-characters-and-social-data-character--guild) Step 1 for what an archetype actually is). You can also call `ListCharacterOptions {}` first to get `CharacterOptions { archetypes: [ArchetypeOption...] }` if you want to build a real archetype picker — see the reference client's [`Scenes/UI/CharacterSelectPanel.cs`](../../examples/world-zero-test-grounds/Scenes/UI/CharacterSelectPanel.cs).

Either way, finish with `SelectCharacter { character_id }`, naming a character your account actually owns (an unowned id is rejected). Success is `CharacterSelected { character_id }`.

### 3d. The automatic `Joined` (`message_type` 200)

You don't request this — the moment `SelectCharacter` succeeds, the server spawns your character into its zone and pushes `session.ServerMessage`'s `Joined` on the same connection, no client action needed:

```proto
message Joined {
  string entity_id = 1;
  double x = 2;
  double y = 3;
  repeated RosterEntry roster = 4;
  uint64 tick = 5;
  double z = 6;
  string zone_id = 7;
}
```

`entity_id` is *your* live entity for this session (not your character id — a character gets a fresh entity id each time it's spawned). `roster` is every other entity already in the zone right now (see §5 below). `zone_id` is where you actually spawned (your realm's default zone for a new character, or wherever this character last was for a returning one). `tick` is the zone's own simulation-step counter at this moment — see §4 for why that matters more than a wall-clock timestamp here.

`server_smoke.rs`'s `connect_register_move_and_persist_across_reconnect` test is the cleanest real trace of steps 3a–3d end to end, including a full reconnect that lands the character back at its last saved position.

## 4. Movement: `Move`, `Moved`/`Rejected`, correlation, and ping

Once joined, send `session.ClientMessage`'s `Move { x, y, z, seq }` to request moving your own entity. Two things make this different from a naive request/response:

**`seq` is yours to assign, monotonically increasing per connection, starting at 1.** The server never interprets it — it queues your move for the next simulation tick and, once resolved, echoes the same `seq` back on whichever of `Moved`/`Rejected` applies. This matters because you can have several moves in flight before hearing back about any of them (no lockstep on this connection), and a tick can resolve more than one queued move for the same entity at once — so match outcomes to requests by `seq`, never by assuming response order matches request order. `seq: 0` is reserved for a move that didn't originate from a real client (e.g. a plugin-driven NPC patrol) — a real client's own sequence always starts at 1.

**`Moved`/`Rejected` carry a `tick: uint64`, not a wall-clock timestamp.** This is the zone's own fixed-rate simulation counter (20 Hz by default, configurable via `WZ_WORLD_TICK_RATE_HZ`, [`Server_Customization_Guide.md`](Server_Customization_Guide.md#step-2--your-world-content--world) Step 2), incremented once per tick — it's what's directly meaningful against the simulation your position is validated inside, and it lets you reason about staleness/ordering without needing clock-synced wall time.

**Rejection reasons** come back as `Rejected { reason, seq, tick }` — never a bare disconnect or silent drop. The real validator is `world`'s server-authoritative movement/speed-cap/collision check (against the zone's real `navmesh_v1` asset as of #280 — see [`Server_Customization_Guide.md`](Server_Customization_Guide.md#step-2--your-world-content--world) for `WZ_WORLD_MAX_SPEED_MPS` and the zone manifest's collision reference), so a rejection is a real "that move wasn't valid," not a protocol-level error.

**Client-side prediction is on you** (the wire protocol just gives you the correlation primitive to build it with). The reference client's [`Movement/PredictedMovement.cs`](../../examples/world-zero-test-grounds/Movement/PredictedMovement.cs) is a real worked example: it predicts your position immediately on send (`RecordPredictedMove`), reconciles forward once a matching `Moved` confirms it (`ReconcileConfirmed`, trimming everything with `seq <= confirmedSeq` from its pending buffer), and on `Rejected` discards the rejected step and everything predicted after it, snapping back to the last authoritative position (`ReconcileRejected` — this particular client snaps rather than replaying discarded inputs against the new baseline, which is enough for a manual test tool; a real game typically replays them). `Joined`/`ZoneChanged` are always a hard snap (`HardSet`), never something to reconcile against — a spawn or zone transition isn't a predicted step that turned out wrong.

**`Ping`/`Pong` is a separate, standalone latency probe** — unrelated to movement or any other traffic. Send `Ping { client_sent_at }` (your own clock, any value, never validated) and you'll get back `Pong { client_sent_at, server_time }` immediately — `client_sent_at` echoed verbatim so you can compute round-trip time against your own clock, `server_time` (server wall-clock, Unix millis) if you also want to estimate clock skew. `server_time` is informational only; it's never fed into any simulation decision on the server side (that's `tick`'s job).

## 5. Seeing the world: roster, spawns, despawns, zone changes

Your join's `roster: [RosterEntry...]` is everyone else already in the zone: `entity_id`, `entity_type` (an NPC's real spawn-table type string, e.g. `"npc.wolf"` — a player's is always `""`), and `x`/`y`/`z`. After that, three server-pushed messages keep it current:

- **`EntitySpawned { entity_id, entity_type, x, y, z }`** — someone/something new just appeared (another player joining, an NPC spawning). Never sent for your own entity — that's what `Joined`/`ZoneChanged` are for.
- **`EntityDespawned { entity_id }`** — that entity is gone (disconnect, or an NPC despawning).
- **`Moved { entity_id, x, y, z, seq, tick }`** — broadcast to every connection in the zone whenever an accepted move actually lands, not just to the mover. This is how you animate everyone else's movement, distinct from your own predicted/reconciled movement in §4. (The reference client's [`Movement/EntityInterpolator.cs`](../../examples/world-zero-test-grounds/Movement/EntityInterpolator.cs) smooths these for entities that aren't your own.)

**Crossing a zone link** (if your content pack declares more than one linked zone — see [`Server_Customization_Guide.md`](Server_Customization_Guide.md#step-2--your-world-content--world) Step 2) sends `ZoneChanged { zone_id, entity_id, x, y, z, roster, tick }` — same shape as `Joined`, but for an in-place handoff: your `z` resets to `0.0` on arrival, and the roster is now the *destination* zone's, with its own independent tick counter.

## 6. Beyond movement: chat, and your own plugin messages

### Chat (`message_type` 100)

Send `chat.ClientMessage`'s `Join { channel }` (a channel name — finds or creates a `group`-scoped channel by that exact name) to get back `Joined { channel_id, channel }`. Then `Send { channel_id, body }` publishes a message; every member (including you) receives `Chat { channel_id, channel, sender, body }` as it's published. `Leave { channel }` drops membership. See [`docs/specs/Chat_Spec.md`](../specs/Chat_Spec.md) for system channels (`chat.yaml`, auto-join-on-zone-entry) and party/guild chat sync — this doc only covers the bare join/send/receive loop. `Server_Customization_Guide.md`'s Step 6 covers the `WZ_SERVICE_CHAT_ENABLED` toggle if you're testing against a server with chat disabled — in that case none of this is reachable at all.

### Sending a message to your own plugin

This is the concrete "connect your own gameplay logic" proof: a plugin declares its own `message_type` values (each `>= 1000` — 0-999 is core-reserved) in its `plugin.toml`'s `message_types` list, and any envelope whose `message_type` matches gets routed straight to that plugin's `on-message` hook instead of core dispatch (`docs/specs/Plugin_API.md`'s hooks table: `on-message: func(zone-id, message-type, sender-entity-id, payload)`). The `payload` is opaque bytes as far as the framing layer and core are concerned — your plugin decides what's inside.

The shipped [`examples/example-plugin`](../../examples/example-plugin) declares `message_types = [1000]` and its `on-message` hook echoes back what it received — this is exactly what `make quickstart` loads by default. The rawest possible demonstration is `server_smoke.rs` sending a bare envelope with no session-protocol wrapper at all:

```rust
stream
    .send(gateway::Envelope::new(1000, b"hello".to_vec()))
    .await
    .unwrap();
```

...and getting back a `session.ServerMessage::PluginMessage { body }` whose `body` contains both `"1000"` and `"hello"` — proof the envelope actually round-tripped gateway → session → world actor → plugin → session and back. In your own client, this means: build the envelope with your custom `message_type` and whatever payload encoding your plugin expects (it doesn't have to be protobuf — the core never looks inside it), send it over the same TCP connection as everything else, and listen for `PluginMessage` (or whatever your plugin sends back via its own `send-message` host-function call) the same way you listen for any other session message.

If you'd rather trigger a plugin via chat instead of a raw message type, `chat_commands` in `plugin.toml` routes a named command (no leading `/`) to `on-chat-command` instead of publishing it as ordinary chat — `test-plugin`'s `/give <item_type>` (exercised throughout `server_smoke.rs`) is a real example of this path, and doesn't require your client to construct a custom envelope at all — just an ordinary `chat.ClientMessage::Send` whose `body` starts with the command name.

## 7. What you have now, and where to go next

At this point your client can: open a TLS connection, register/log in, pick a realm and a character, see the automatic join roster, move (with correlation and, if you build it, prediction), see other entities spawn/move/despawn, chat, and exchange a custom message with your own plugin. That's the full loop this project's wire protocol supports today.

From here, what you'd actually customize lives in [`Server_Customization_Guide.md`](Server_Customization_Guide.md):

- **Character creation/stats** — [Step 1](Server_Customization_Guide.md#step-1--your-games-stats-characters-and-social-data-character--guild): what `archetype_key` values exist, what stats a character has, crafting/currency/equipment.
- **World content** — [Step 2](Server_Customization_Guide.md#step-2--your-world-content--world): zones, links, NPC spawn tables, navmesh-backed collision.
- **A different login provider** — [Step 3](Server_Customization_Guide.md#step-3--authentication-auth): swap `UsernamePasswordProvider` for OAuth/SSO without touching this wire protocol at all.
- **Networking/TLS for a real deployment** — [Step 4](Server_Customization_Guide.md#step-4--networking-gateway): `WZ_SERVER_ADDR`, real certificates.
- **Your own gameplay logic** — [Step 5](Server_Customization_Guide.md#step-5--plugins-your-actual-gameplay-logic-plugin-host) plus the full [`Plugin_Development_Guide.md`](Plugin_Development_Guide.md): every hook, every host function, capability gating, and the write/build/deploy loop for a plugin of your own past the `on-message` echo you just wired up.
- **Realms and character transfer** — [Step 8](Server_Customization_Guide.md#step-8--realms--transfers-real-but-not-yet-live): open-vs-bound policy, what a rejection at `SelectRealm` actually means for your client's error handling.

And when you need the exact byte-level contract for anything above — a field you didn't see mentioned here, an edge case, a message this walkthrough didn't cover (parties, guilds, trading, crafting, equipment, and more all exist on the wire — see `session.proto` for the full list) — [`docs/specs/Networking_Spec.md`](../specs/Networking_Spec.md) is the real reference, and the two reference implementations cited throughout this doc are real, working code for all of it.
