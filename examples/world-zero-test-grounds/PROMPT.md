# World Zero Test Grounds — Implementation Prompt

> **This document is historical.** It's the original build brief this client was implemented against, dated 2026-08-28, kept as a detailed worked example of the wire protocol (every claim cited against real source) — not a living document, and not updated as WorldZero's API evolves. It also predates this client living inside the `world_zero` repo itself (`examples/world-zero-test-grounds/`) — where it says "the backend lives at a sibling directory `../world_zero`," read that as "the backend is two directories up." Treat `docs/specs/` as the current source of truth for anything that looks like it might have drifted.

> **Re-passed again (2026-08-28) against WorldZero's actual shipped state.** The previous re-pass's one open caveat is resolved: [#179](https://github.com/LunarVagabond/WorldZero/issues/179) (real guild system) merged to `main` via [PR #209](https://github.com/LunarVagabond/WorldZero/pull/209) — §12 is accurate against a fresh checkout with no caveat now. This pass found several other claims had gone stale since the last re-pass (not because they were wrong when written, just because `main` kept moving):
>
> - §1's architecture table said `transfer` was "NOT wired into server yet" — it now is (#225). Doesn't change this document's test flow (transfer is inter-*realm*, and this test grounds only ever stands up one realm), but the claim itself was wrong.
> - §1 said the only HTTP endpoint anywhere was `/metrics` — `/healthz`/`/readyz` liveness/readiness endpoints exist now too (#181, `WZ_HEALTH_ADDR`, default `127.0.0.1:9091`). Still nothing a game client itself calls, just corrected for accuracy.
> - **§4/§13's "biggest gap" was wrong as of this pass — read the new text, don't skim past this one.** `StatChanged`/`ItemChanged`/`CurrencyChanged` (#211/#218) are real, structured, server-pushed wire messages now for a connection's **own player-owned** entity. The "invent a text convention because there's no structured push" framing is gone for player stats/inventory/currency. It is **not** gone for NPCs (the Evil Cube, §7) — an NPC has no owning connection to push to, so that half of the old workaround is still exactly as necessary as before. Sections §4, §7, §13, §17 below are corrected to reflect this split.
> - §9.2's setup steps were missing three now-mandatory config files: `character.archetypes.yaml`, `crafting.schema.yaml`, `currency.schema.yaml` — `server::main` panics at startup without any one of them (unconditional, same as `party.schema.yaml`/`guild.schema.yaml`). Following the old steps as written would not have gotten `server` running at all. Fixed below.
> - §10/§17's "`chat.yaml` system channels are parsed but never wired into `server::main`" is now closed (#234) — system channels declared in `chat.yaml` actually get created at startup now.
> - §17's "no symmetric `on-zone-left`/`on-player-leave-zone`-adjacent signal" gap is now closed (#233) — `on-player-join-zone`/`on-player-leave-zone` fire on every real mid-session zone crossing, not just session bookends. This is a plugin-side hook, not a new client wire message, so it doesn't change anything in §5/§6's client-facing zone-transition contract — noted for completeness since §17 tracked it as an open gap.
>
> A new, real, entirely unmentioned-until-now system also landed on `main`: **crafting** (#216, `CraftItem` in `session.proto`, `crafting.schema.yaml`). It's cheap to exercise given `ItemChanged` already pushes the result — added to §13 and the checklist (§18) rather than skipped.
>
> As before: every claim below cites the real file it came from, and this was a full re-pass against current source, not a diff-patch.

You are building `world-zero-test-grounds`: a deliberately ugly, disposable, manual integration-test client for **World Zero**, an open-source self-hostable MMO server framework written in Rust. This is **not a real game** — it exists so a human can run two clients side by side and manually verify that World Zero's real, already-implemented backend systems actually work end to end: auth, world simulation, movement, chat, combat, zone transitions, reconnection, parties, guilds.

**You are the game-engine agent. You do not need to guess how the backend works.** This document is the complete, verified client-facing contract, written by inspecting the actual World Zero source (not its aspirational docs) as of this writing. Every claim below cites the real file it came from. Where World Zero does not yet support something the desired test flow calls for, that is stated explicitly, not papered over — see "Known Gaps" near the end. Do not invent APIs to fill those gaps; work around them the way this document tells you to, or skip that part of the flow.

The World Zero backend source lives at a sibling directory: `../world_zero` (relative to this project). You have local filesystem access to it — read it if anything here is unclear, but everything you need to build the client is already extracted into this document.

**Project scaffold:** this is already a Godot 4.8 project (`project.godot` — Forward Plus rendering, Jolt Physics 3D, a `[dotnet]` section present). **Strong recommendation: build this in C# (Godot's .NET workflow), not GDScript.** The reason is entirely about the wire protocol (below): World Zero's protocols are **Protobuf**, and Protobuf's C# codegen (`protoc --csharp_out` + the official `Google.Protobuf` NuGet package) is first-party and rock solid. There is no first-party or well-maintained GDScript protobuf implementation — you'd be depending on an obscure community plugin for the single most load-bearing part of this client. This is a recommendation, not a backend requirement; if you have a strong reason to use GDScript instead (e.g. a mature protobuf addon you already trust), that's your call, but budget real time for wire-format debugging if so.

---

## 1. Architecture at a glance

```
world_zero/                      <- the backend, a Rust cargo workspace
  crates/
    auth/          - accounts, login/register/resume, sessions (Redis-backed tokens)
    character/     - character row, stats (JSONB, dev-declared schema), inventory, real party system
    realm-directory/ - realm CRUD + zone-to-realm tracking (wired into `server`, #136)
    guild/         - real, account-scoped guild system: roster, dev-declared rank hierarchy (#179, pending merge — see banner above)
    content/       - zone manifest / content-pack YAML parsing
    world/         - the authoritative tick-based zone simulation (movement, collision)
    gateway/       - TCP+TLS transport, envelope framing/codec
    chat/          - channels (direct/group/guild/zone), Redis pub/sub delivery
    transfer/      - inter-realm character transfer, wired into `server` (#225) — irrelevant to this test grounds, which only ever stands up one realm
    plugin-host/   - sandboxed WASM plugin runtime (wasmtime), the WIT interface
    server/        - the runnable combined-process binary — THIS is what you connect to
    common/        - shared id types, config loading, logging
  examples/example-plugin/  - the one shipped example WASM plugin
  config/          - example YAML configs you'll copy from
```

`server` is one binary that runs auth + character + world + gateway + content + chat + realm-directory (+ a plugin-host slice) as a single combined process. There is no separate "API server" vs "realtime server" — **one TCP+TLS connection carries everything**: auth handshake, realm select, character select, world/movement, chat, and any plugin-custom messages, multiplexed by a `message_type` field on every message.

**There is no REST/HTTP API for gameplay.** The only HTTP endpoints in the whole system are a Prometheus `/metrics` text endpoint (`WZ_METRICS_ADDR`, default `127.0.0.1:9090`) and `/healthz`/`/readyz` liveness/readiness endpoints (#181, `WZ_HEALTH_ADDR`, default `127.0.0.1:9091`) — none of these are something a game client calls. Everything gameplay-related is the TCP+TLS protocol below.

---

## 2. The wire protocol — read this section carefully, it underlies everything

Source of truth: `world_zero/crates/gateway/src/envelope.rs`, and the `.proto` files:
- `world_zero/crates/auth/proto/auth.proto` (auth)
- `world_zero/crates/server/proto/realm.proto` (realm discovery/selection)
- `world_zero/crates/server/proto/character.proto` (character list/create/select)
- `world_zero/crates/chat/proto/chat.proto` (chat)
- `world_zero/crates/server/proto/session.proto` (world/movement/combat/party/guild)

### 2.1 Transport

- **Plain TCP wrapped in TLS.** One socket, one connection, for the whole session (auth + realm + character + world + chat + plugin messages all multiplexed over it).
- Default listen address: `127.0.0.1:7900` (`WZ_SERVER_ADDR`, configurable).
- **There is a UDP/DTLS transport module in the `gateway` crate (`udp.rs`) but `server`'s `main.rs` never wires it up.** The runnable server today is TCP-only. Do not build a UDP path — there is nothing listening on it.
- **TLS certificate:** by default `server` generates a self-signed cert for `"localhost"` and caches it at `<config_dir>/certs/self_signed.cert.der` (`world_zero/crates/gateway/src/tls.rs`). Your client needs to either (a) trust that exact certificate (read the same `.der` file `server` generated, add it as a trusted root — this is exactly what World Zero's own Rust integration test client does, see `world_zero/crates/server/tests/server_smoke.rs`'s `connect()`), or (b) disable certificate validation entirely for local dev (acceptable for a disposable test client talking to `localhost`). Either is fine; document which one you chose in this project's own README.

### 2.2 Framing (byte layout)

Every message on the wire is:

```
[ 4-byte big-endian u32: length of everything that follows ]
[ 2-byte big-endian u16: message_type ]
[ N bytes: protobuf-encoded payload ]
```

This is `tokio_util::codec::LengthDelimitedCodec` (default config: big-endian u32 length prefix, length field covers only what follows it, not itself) wrapping a 2-byte `message_type` + raw protobuf bytes (`Envelope::encode_to`/`decode_from` in `envelope.rs`). Implement this by hand in Godot using `StreamPeerTCP`/`StreamPeerTLS` (`get_u32()` reads big-endian by default in Godot's `StreamPeer`, matching this framing) — there is no length-delimited-frame helper built into Godot, you'll write a small read-loop that buffers until a full frame is available, mirroring what `EnvelopeCodec` does. Verify your framing against `world_zero/crates/gateway/src/envelope.rs`'s own round-trip tests before debugging anything higher up the stack — a framing bug looks like "the server never responds" and will waste your time if you assume it's something else.

### 2.3 `message_type` catalog (`docs/specs/Networking_Spec.md` in world_zero)

| `message_type` | Protocol | `.proto` file | Purpose |
|---|---|---|---|
| `1` | `auth::gateway_protocol` | `auth.proto` | Register/Login/Resume — must be the very first envelope on a new connection |
| `2` | `server::realm_protocol` | `realm.proto` | Realm discovery/selection (#136/#192) — required right after auth succeeds, before anything else is accepted |
| `3` | `server::character_protocol` | `character.proto` | Character list/create/select (#193) — required right after realm selection, before world-join |
| `100` | `chat::gateway_protocol` | `chat.proto` | Join/leave/send chat, gated behind auth |
| `200` | `server::session_protocol` | `session.proto` | Move, attack, use item, interact, party, guild — zone/world traffic, gated behind having a selected character |
| `>= 1000` | plugin-declared, opaque | none (whatever the plugin defines) | Routed to a specific loaded plugin's `on-message` hook — see §7 |
| everything else in `0-999` not listed above | core-reserved | — | Don't use — this range is reserved for future core message types |

**Codegen:** run `protoc` yourself against the `.proto` files above with the C# plugin (`protoc --csharp_out=...`) and add `Google.Protobuf` via NuGet. World Zero does not ship pre-generated client bindings for any non-Rust engine — you are the first Godot integration, per `docs/specs/Networking_Spec.md`'s own "Client-integrator codegen" note. Check the `.proto` files into this repo (or generate at build time) so the contract stays traceable.

### 2.4 Auth handshake — must happen first, every connection

`auth.proto` / `world_zero/crates/auth/src/gateway_protocol.rs`:

```protobuf
message ClientMessage {
  oneof kind {
    Register register = 1;   // { string username, string password }
    Login login = 2;          // { string username, string password }
    Resume resume = 3;        // { string session_token }
  }
}

message ServerMessage {
  oneof kind {
    Authenticated authenticated = 1;  // { string account_id, string username, string session_token }
    Error error = 2;                   // { string message }
  }
}
```

Flow:
1. Open the TCP+TLS connection.
2. Send `message_type = 1`, `ClientMessage{Register{username,password}}` (first time), `ClientMessage{Login{username,password}}`, or `ClientMessage{Resume{session_token}}` — this **must be the very first envelope** sent on the connection.
3. Server replies `message_type = 1`, either `Authenticated{account_id, username, session_token}` or `Error{message}`.
4. On `Error`, **the server closes the connection** — no retry on the same socket; reconnect and try again.
5. `account_id` (a UUIDv7, wire-formatted as its plain string text form, e.g. `"01912a3b-..."`) is now this connection's trusted identity for everything else on it.

**Session resumption is real now (#195).** `Resume{session_token}` reconnects using the `session_token` an earlier `Authenticated` reply issued, instead of re-entering a password — replied to exactly the same way `Register`/`Login` are (a fresh `Authenticated` with the same token back, or an `Error` you should treat as "fall back to `Login`"). This is a real, useful shortcut, but it is **not required** — `Login{username,password}` still works identically to before on every reconnect if you'd rather not track the token. There is still no logout message — disconnect just means closing the socket. Recommended client behavior: store the `session_token` from the last successful auth; on reconnect, try `Resume` first and fall back to `Login` on `Error`.

### 2.5 After auth: realm select, then character select — both now mandatory, neither automatic

**This is the single biggest structural change since the previous version of this document.** There used to be no realm/character step at all — auth success went straight to world-join. That's gone. The real flow today (`server::session::handle_session` in `world_zero/crates/server/src/session.rs`) is a strict three-stage handshake:

1. **Auth** (§2.4, `message_type 1`) → `Authenticated`.
2. **Realm selection** (§3.1, `message_type 2`) — mandatory. The connection is not accepted for anything else until it sends a valid `SelectRealm`.
3. **Character selection** (§3.2, `message_type 3`) — mandatory. The connection is not accepted for anything else (including world traffic) until it sends a valid `SelectCharacter` (or `CreateCharacter` followed by `SelectCharacter`).
4. **Only then**, automatically, the world-join happens: the selected character is spawned into its last-known zone (or the realm's default zone, for a brand-new character) at its last-known position, and `session.proto`'s `Joined` message (§6) arrives unprompted. There's nothing to send to trigger this — it happens the instant `SelectCharacter` succeeds.

There is still no explicit "enter world" message at stage 4 — that part of the old behavior is unchanged, it's just now gated behind two new mandatory stages instead of firing directly off auth.

---

## 3. Realm select, character select/create, classes, races

Unlike the previous version of this document, realm selection and character selection are now **fully real, wired, and mandatory** (§2.5). Classes/races are still **not a backend concept at all**.

### 3.1 Realm selection (`realm.proto`, `message_type 2`, #136/#192)

```protobuf
message ClientMessage {
  oneof kind {
    ListRealms list_realms = 1;   // {}
    SelectRealm select_realm = 2;  // { string realm_id }
  }
}
message ServerMessage {
  oneof kind {
    RealmList realm_list = 1;      // { repeated RealmSummary realms }
    RealmSelected realm_selected = 2;  // { string realm_id }
    Error error = 3;
  }
}
message RealmSummary {
  string realm_id = 1;
  string name = 2;
  string open_or_bound = 3;        // "open" or "bound"
  int64 character_count = 4;
  uint64 live_connection_count = 5;
}
```

`ListRealms{}` can be sent any number of times (useful for a picker UI, using the live `character_count`/`live_connection_count` numbers) but is never required — a client that already knows its realm id can send `SelectRealm` immediately with no picker UI at all. **A `server` process today only ever serves exactly one realm** (`WZ_REALM_ID`, set at server startup — see §9.2, this is now a *required* env var, not optional) — a process serving more than one realm at once is a separate, unbuilt feature (#130). `SelectRealm{realm_id}` naming anything other than that one realm is rejected with `Error` and the connection is closed. On success you get `RealmSelected{realm_id}` back, echoing what you selected.

For a single-realm test client, the simplest correct implementation is: skip the picker entirely, and immediately send `SelectRealm{realm_id: <the one realm id you configured your test server with>}`. You still have to send it — it's just that you don't need a real UI in front of it.

### 3.2 Character selection (`character.proto`, `message_type 3`, #193)

```protobuf
message ClientMessage {
  oneof kind {
    ListCharacters list_characters = 1;  // {}
    CreateCharacter create_character = 2; // { string name }
    SelectCharacter select_character = 3; // { string character_id }
  }
}
message ServerMessage {
  oneof kind {
    CharacterList character_list = 1;      // { repeated CharacterSummary characters }
    CharacterCreated character_created = 2; // { string character_id }
    CharacterSelected character_selected = 3; // { string character_id }
    Error error = 4;
  }
}
message CharacterSummary {
  string character_id = 1;
  string name = 2;
  string zone_id = 3;
}
```

An account can now have **multiple characters per realm** (`WZ_CHARACTER_MAX_PER_ACCOUNT`, a real per-account cap enforced server-side, not just per-realm assumed-one-character the way it used to be). Real flow: `ListCharacters{}` → `CharacterList{characters}` (empty on a brand-new account) → if you want a new one, `CreateCharacter{name}` → `CharacterCreated{character_id}` → `SelectCharacter{character_id}` → `CharacterSelected{character_id}`, which is what actually unblocks world-join (§2.5 step 4). `character_id` is a real UUIDv7 string and, unlike the previous version of this document's claim, **does cross the wire now** — track it, it's a real, stable identifier your client can hold onto (see the updated table in §14).

### 3.3 Classes / races — still not a backend concept

WorldZero's core still has **zero concept of class or race**, and `CreateCharacter` still takes only a `name` — no class/race/appearance parameter anywhere in its shape. What *did* change (#194): there is now a real plugin hook, `on-character-create(character-id, zone-id)` (`world_zero/crates/plugin-host/wit/plugin.wit`), that fires right after a character row is created and before any entity/session exists for it — a plugin can use this to assign differentiated starting stats via `apply-stat-delta-for-character` (a character-id-addressed variant of the usual stat-delta call, since no entity id exists yet at this point). **This is still not client-selectable** — there is no request/response "ask the plugin what archetypes exist, let the player pick one" mechanism; the WIT file's own comment on `on-character-create` explicitly notes a real "ask a plugin for a list of options" request/response is deferred to a future ticket. So: a plugin *can* make two characters end up with different starting stats (e.g. based on `name`, or just always the same preset), but a client cannot present real choices and have the server honor them. If you want a class-flavored creation UI, it can only ever be entirely client-side cosmetic (e.g. a locally-remembered "look" with zero backend backing) — be honest in the UI that this isn't real, exactly as before.

---

## 4. Character stats (what exists, and the remaining display gap)

`config/stats.schema.example.yaml` (copy this as `stats.schema.yaml` — see §9 for the full local setup):

```yaml
schema_version: 1
stats:
  - key: hp
    type: int
    default: 100
    min: 0
    max: 100
  - key: mana
    type: int
    default: 50
    min: 0
    max: 50
  - key: reputation.ironclad_guild
    type: int
    default: 0
```

A freshly created character gets `hp: 100`, `mana: 50` by default (whatever the schema declares, possibly steered per-character by a plugin's `on-character-create` hook, §3.3). Stats live in a JSONB column, written only through `apply-stat-delta`/`apply-stat-delta-for-character` (**plugin-only** host functions — a client never writes its own stats).

**This used to be a real gap; it's closed now for player-owned entities (#211).** `session.proto` has a real, structured `StatChanged{stat_key, value}` server message (`message_type = 200`, one of `ServerMessage`'s `oneof kind` variants alongside `Moved`/`Joined`/etc.) — `value` is the resulting stat value after the delta, not the delta itself. `server::session`/`server::world_actor` push this automatically to a connection whenever `apply-stat-delta` actually writes against *that connection's own* character (`crates/server/src/session.rs`, `crates/server/src/world_actor.rs`). Track `StatChanged` messages in your local state exactly like you'd track `Moved` for position — no invented text convention needed for your own HP/mana/etc. anymore.

**The gap survives, unchanged, for NPCs.** `StatChanged`'s own doc comment in `session.proto` is explicit: "Never sent for an NPC-targeted `apply-stat-delta` — an NPC has no owning connection to push to." There is still no "read a stat" host function either (`apply-stat-delta` itself still returns only `result<_, string>`, no value, confirmed against current `plugin.wit`). So the Evil Cube's HP (§7) — an NPC, not a player character — still has no structural path to the client at all; you still need the invented `PluginMessage`/`send-message` text-convention workaround for it specifically. Don't assume `StatChanged` covers NPCs just because it exists now.

---

## 5. Entering, zones, and zone geometry

### 5.1 What a "zone" is

Source: `world_zero/crates/content/src/manifest.rs`, `docs/specs/Content_Manifest_Spec.md`. A zone is a YAML manifest: an `id` (a plain string slug, e.g. `"greenwood-forest"` — **not a UUID**, see §14), a 2D polygon boundary in meters, optional NPC spawn tables + patrol routes, optional trigger volumes, and optional `links[]` to other zones for live transitions.

**Everything in World Zero's world simulation is 2D — `(x, y)` as `f64`, meters, no `z` in gameplay logic.** Zone boundaries are 2D polygons; movement validation, collision, and the wire protocol's `Move`/`Moved` messages are `(x, y)` only. **`character` rows do persist a `position_z` column** (`world_zero/crates/character/src/store.rs`), but the live simulation and every wire message ignore it — it's read back as part of a character's last position but never validated, broadcast, or used for anything gameplay-authoritative. Since you're building this as a real 3D scene (per your own direction — full 3D like WoW): treat the server's `(x, y)` as your ground-plane X/Z (or X/Y — your call), and drive visual height/verticality (terrain following, jumping, camera) entirely client-side; the server has no opinion about vertical position and will never validate or reject anything about it. This is a real architectural limitation of World Zero today, not a misunderstanding on your part — call it out plainly in your test UI/README rather than pretending the server is 3D-aware.

### 5.2 Ready-made two-zone content — use this, don't invent your own

World Zero already ships exactly the two-zone setup this test grounds needs, linked to each other:

- `config/content-pack.example.yaml` — declares two zones: `greenwood-forest` and `stonebridge-village`.
- `config/example-zones/greenwood-forest.yaml` — 500×500m open area, a wolf NPC spawn table + patrol route, an interact trigger, and a `links[]` entry to `stonebridge-village` along its eastern edge (`x=500`).
- `config/example-zones/stonebridge-village.yaml` — a smaller 200×200m zone, linked back to `greenwood-forest` along its western edge (`x=0`).

**Use these as-is** for your "open-world test area" + "second zone for transition testing" requirement — copy `content-pack.example.yaml` and both zone files into your running `server`'s config dir (exact steps in §9). You'll add one more spawn table to `greenwood-forest.yaml` for the Evil Cube (§7) — that's the only content edit you need to make.

### 5.3 Zone transitions — fully automatic, no client message

There is **no "enter zone" or "request zone transition" client message.** A zone transition is a side effect of ordinary movement: you send `Move{x,y,seq}` like always; if the server's authoritative movement resolution finds that the accepted move segment crosses a manifest-declared `links[]` edge, it despawns you from the old zone and respawns you into the target zone automatically, sending you a `ZoneChanged` message (below) instead of the usual `Moved`. **Your client does not decide when a zone transition happens — it only reacts to receiving `ZoneChanged`.** (Source: `world::crossed_link` in `world_zero/crates/world/src/links.rs`, `Zone::tick` in `world_zero/crates/world/src/zone.rs`, `handle_tick_outcomes`/`complete_zone_transition` in `world_zero/crates/server/src/main.rs`.)

The same live-handoff mechanism (spawn/despawn/roster/`ZoneChanged`) is also reused for **live same-zone layer reassignment** when a party forms across layers — see §8/§11.

---

## 6. Movement — the full contract

Source: `session.proto`, `world_zero/crates/world/src/movement.rs`, `world_zero/crates/world/src/zone.rs`, `world_zero/crates/server/src/main.rs`'s `handle_tick_outcomes`.

### 6.1 Client → server

```protobuf
message Move {
  double x = 1;
  double y = 2;
  uint32 seq = 3;
}
message Ping {
  int64 client_sent_at = 1;
}
```
Sent as `ClientMessage{Move{x,y,seq}}`, `message_type = 200`. This is a request for **your own entity** to move to `(x, y)` — there is no entity id field, the server always resolves it to whichever entity this authenticated connection owns. **`seq` is new (#196)**: client-assigned, monotonically increasing per connection (start at `1`) — the server never interprets it beyond echoing it back on `Moved`/`Rejected`, so you can correlate a specific outcome to the specific predicted step it corresponds to (§6.4 below is a full rewrite from the old version of this document, which had no correlation mechanism at all).

`Ping{client_sent_at}` is also new (#196) — a latency probe independent of movement traffic, replied to with `Pong` (§6.3). `client_sent_at` is opaque to the server (send whatever timestamp convention you like, e.g. Unix millis), echoed back verbatim.

### 6.2 What the server does with `Move`

- **Queued, not applied immediately.** A `Move` is queued (`Zone::request_move`) and only resolved on the **next simulation tick** — a fixed **20 Hz** tick rate (`WZ_WORLD_TICK_RATE_HZ`, default `20`, `world_zero/crates/world/src/config.rs`). So there is inherent per-move latency of up to `1/20s = 50ms` even with a perfect network, before you'll see any outcome.
- **Speed cap:** `attempted_distance > max_speed_meters_per_second * dt` is rejected as `TooFast` (default `10.0` m/s, `WZ_WORLD_MAX_SPEED_MPS`). `dt` is the fixed tick interval (`1/tick_rate_hz`), **not** a client-supplied delta — the server computes it from its own tick timing. A move is judged solely against "how far is this from your last accepted position, is that reachable in one fixed tick at the speed cap."
- **Bounds check:** destination must fall inside the zone's manifest boundary polygon, else `OutOfBounds`.
- **Collision:** a tight 0.5m point-collision radius against every other entity in the zone (`COLLISION_RADIUS_METERS`) — landing within 0.5m of another entity (player or NPC) is rejected as `Blocked{blocking_entity}`. This is not a real hitbox/physics system, just "don't let two entities occupy the same point."
- **Link crossing:** if the accepted destination crosses a declared `links[]` edge, this becomes a zone transition (§5.3) instead of an ordinary move.

### 6.3 Server → client

```protobuf
message Moved {           // an accepted move, BROADCAST to every connected client in the same zone+layer
  string entity_id = 1;
  double x = 2;
  double y = 3;
  uint32 seq = 4;
  uint64 tick = 5;
}
message Rejected {        // a move YOU requested was rejected — sent ONLY back to you, never broadcast
  string reason = 1;
  uint32 seq = 2;
  uint64 tick = 3;
}
message Pong {
  int64 client_sent_at = 1;
  int64 server_time = 2;
}
```

- **`Moved` is broadcast to everyone in the same zone (and same layer — §8), including yourself**, for every accepted move — yours and everyone else's. This is your only source of truth for "where is entity X right now."
- **`Moved.seq`/`Rejected.seq` now echo the originating `Move.seq` (#196)** — this is the correlation mechanism the old version of this document said didn't exist. `Moved.seq` is `0` for a move that didn't originate from a real client `Move` (e.g. an NPC's plugin-driven movement, or another player's own broadcast reaching you) — a real client's own `seq` always starts at `1`, so you can distinguish "this Moved is about my own pending request" from "this is just someone else's broadcast" by checking both `entity_id == my own` and `seq != 0`.
- **`tick` is the server's authoritative simulation-step counter** at the moment the message was built (also on `Joined`/`ZoneChanged`, §5.3/§6) — your baseline for reasoning about ordering/staleness across messages.
- **`Rejected.reason` is still a raw Rust `Debug`-formatted string** of the rejection enum, e.g. literally the text `"OutOfBounds"`, `"TooFast { attempted_distance: 12.3, max_allowed: 10.0 }"`, or `"Blocked { blocking_entity: <uuid> }"`. Treat this as an opaque debug string for your on-screen debug console (§16), not a stable machine-parseable enum.
- `Pong` replies to `Ping` — `client_sent_at` echoed verbatim for your own RTT computation; `server_time` is the server's wall-clock (Unix millis) at reply time, if you want a clock-skew estimate too.
- NPC movement (patrol routes driven by a plugin's `on-npc-tick` hook, §7) goes through the exact same `Moved` broadcast — there is no separate "NPC moved" message type.

### 6.4 Client-side prediction / reconciliation — now has a real correlation mechanism

The old version of this document said this "cannot be implemented precisely" because there was no way to match a `Moved`/`Rejected` back to the `Move` that caused it. That's fixed (#196): every `Move` carries a client-assigned `seq`, and every `Moved`/`Rejected` echoes it back.

Recommended approach:
- Send `Move{x,y,seq}` with a monotonically incrementing `seq` on your own client-side cadence (throttling to roughly the 20Hz tick rate is enough — sending faster just wastes bandwidth, since only the latest queued move per entity per tick resolves). Keep a small local buffer of `(seq, predicted_position)` for moves you've sent but not yet had confirmed.
- Predict your own entity's movement locally as soon as you send it (classic client-side prediction) — you have the input, you get to guess.
- When a `Moved` for your own `entity_id` arrives, match its `seq` against your buffer: everything up to and including that `seq` is now confirmed, drop it from the buffer, and reconcile (snap or smoothly correct) to the confirmed position if your prediction had drifted.
- When a `Rejected` for your own moves arrives, match its `seq` the same way: discard that predicted step and everything predicted after it (since they were all predicted forward from a step that turned out invalid), and re-predict forward from your last confirmed `Moved` position instead.
- **For every other entity**, you still have no input signal — just periodic `Moved` snapshots — so purely **interpolate** other entities between the last two `Moved` positions you received for them; don't try to predict them. `seq`/correlation only matters for your own entity's moves.
- Teleports/zone changes are still a different message entirely (`ZoneChanged`, §5.3, or the initial `Joined`) — never a `Moved`. Treat receipt of `Joined`/`ZoneChanged` as "hard-set position, no interpolation," never blend it as if it were a normal move.

---

## 7. NPCs and combat — the "Evil Cube" test

This is the part of the test grounds that requires you to touch the **backend** repo too, not just this Godot project — combat/NPC behavior is entirely plugin-owned (World Zero's core "has no notion of HP or a death condition," by design), so an Evil Cube with a health bar needs a small custom WASM plugin. This is expected and normal — see `world_zero/docs/product/Plugin_Development_Guide.md` if you want the general background, but everything you need is below.

### 7.1 What the core actually gives you — NPC stats are real now (#197), client-side stat display is still not

- `session.proto`'s `Attack`: `ClientMessage{Attack{target_entity_id, stat_key}}` — "I want to attack this entity, aimed at this stat." The server confirms `target_entity_id` is actually a spawned entity in your zone before doing anything; an unknown/vanished target is silently dropped. **The client never reports a damage amount or outcome — only the intent to attack.** `stat_key` is an arbitrary string your plugin defines the meaning of (e.g. `"hp"`).
- This routes to the loaded plugin's `on-damage-calc(zone_id, attacker_entity_id, target_entity_id, stat_key, base_amount)` hook — `base_amount` is **always `0`**; the core never invents a damage number. **The plugin's own code decides how much damage happens and whether the target dies.**
- **Changed since the previous version of this document: `apply-stat-delta(entity_id, stat_key, delta)` now works against NPC entities too, for real**, not just players (#197). The core resolves which storage to write against by entity kind — a player's declared stats still go to its `characters` row; an NPC's now go to a real, schema-validated, in-memory-per-entity stat map (`server::session::NpcStats`, cleared automatically when the NPC despawns), using the exact same `AttributeSchema` bounds/defaults/clamping real character stats use (`server::world_actor::apply_npc_stat_delta`). Before #197 this silently no-op'd on an NPC id; now it's a real write. **You should call `apply-stat-delta` for the cube's HP directly** — you no longer need to fake NPC stat tracking as a workaround for a broken core primitive.
- **One real nuance that survives #197: `apply-stat-delta` still returns no value** (`result<_, string>` in `plugin.wit`, no current-stat-value in the success case), and there is still no "read a stat" host function. So while damage now genuinely persists server-side, **your plugin still can't learn "did this hit bring HP to 0" purely from the return of `apply-stat-delta`.** The practical approach is unchanged from before: track the cube's HP in your own plugin state (`plugin-state-set`/`plugin-state-get`, `entity` or `zone` scope) *in parallel* with the real `apply-stat-delta` write, so your plugin can compare against zero and decide when to call `report-death`. Both writes should use the same delta amount each hit so they stay in sync.
- When HP hits zero (by your own tracked-state check), the plugin calls **`report-death(entity_id)`**, which fires `on-death(zone_id, entity_id)` back — the plugin's own confirmation that "yes, this thing is now dead." `report-death`/`report-respawn` take **any entity id string, player or NPC** — no character-row requirement.
- The cube's health is still not sent to the client structurally. §4's gap is now closed for *player* stats (`StatChanged`, #211) — but that message is explicitly never sent for NPC-targeted `apply-stat-delta` (no owning connection to push to), and #197 never touched that half either — #197 only made the underlying NPC stat *write* real, not client-visible. **The plugin must `send-message` the cube's current/updated HP to the attacking (and ideally all nearby) clients as an ad-hoc string** — you're defining this convention yourself, it's not a WorldZero-native concept. A suggested minimal convention (feel free to adjust, just be consistent and document it in this project's README):
  ```
  "cube:<entity_id>:hp:<current>/<max>"     e.g. "cube:01912...:hp:35/50"
  "cube:<entity_id>:dead"
  "cube:<entity_id>:respawned:hp:<max>"
  ```
  These arrive to the client as `session.proto`'s `PluginMessage{body}` (`message_type = 200`) — parse the string convention above client-side.
- Death/respawn on this NPC does not despawn/respawn it from the zone automatically — **`max_population`/`respawn_seconds` fields exist in the zone manifest schema but are still not read or enforced by any running code today** (verified against current source — still parsed, stored, and otherwise dead config). Your plugin's own `on-death` handler is responsible for whatever "the cube comes back" behavior you want (e.g. on the *next* `on-damage-calc` hit after death, or on a subsequent `on-npc-interact`, treat it as a respawn and reset both your tracked state and a fresh `apply-stat-delta` back up to max — or just leave the corpse and don't auto-respawn for this test, and note that gap too).

### 7.2 What to actually build (server-side)

1. **Write a new WASM plugin crate** (Rust, `wit-bindgen`, `wasm32-wasip2` target) — copy `world_zero/examples/example-plugin` as your structural starting point, same as any WorldZero plugin author would (`Plugin_Development_Guide.md`, step 1). Do not reuse `example-plugin` as-is; write a new one (e.g. `evil-cube-plugin`) since `example-plugin`'s `on-damage-calc` targets a *player* stat, not an NPC one, and it never calls `report-death`.
2. **Add a spawn table** to your copy of `config/example-zones/greenwood-forest.yaml` (§5.2), e.g.:
   ```yaml
   spawn_tables:
     - id: evil-cube-01
       entity_type: npc.evil_cube
       points: [[250, 250]]
       max_population: 1
       respawn_seconds: 30   # still unenforced by the core, see above — informational only
   ```
   No `route_id` — the cube should be stationary (no `on-npc-tick` movement needed for it).
3. In your plugin's `plugin.toml`: declare `capabilities = ["combat", "spawning", "messaging"]` (`report-death`/`report-respawn` are gated by `combat`; `spawn-npc` by `spawning`; `send-message` by `messaging` — `plugin_host::manifest`/`docs/specs/Plugin_API.md`'s "Capability gating"), and `hooks = ["on-zone-loaded", "on-damage-calc", "on-death", "on-respawn"]` (only hooks you actually implement need to be listed — undeclared hooks are simply never called).
4. In `on_zone_loaded(zone_id)`: call `spawn_npc("evil-cube-01")`, then `plugin_state_set(PluginStateScope::Zone(zone_id), "evil-cube-hp", <max_hp as bytes>)` to seed your own tracked HP.
5. **Known plugin-API rough edge, unchanged: `spawn-npc`'s return value is not the real entity id** — the plugin does not synchronously learn the new NPC's real `entity_id` at spawn time. Your client will learn the cube's real `entity_id` the normal way: via the **`roster`** field on the `Joined`/`ZoneChanged` message (`RosterEntry{entity_id, entity_type, x, y}` — filter for `entity_type == "npc.evil_cube"`). Your plugin should key its own tracked HP by **zone-scoped state** with a fixed key (e.g. `PluginStateScope::Zone(zone_id)`, key `"evil-cube-hp"`) rather than trying to key by entity id, for the same reason as before — works because you'll only ever have one cube in this zone.
6. In `on_damage_calc(zone_id, attacker_id, target_id, stat_key, _base_amount)`: check whether `target_id` matches your cube (compare against the roster, or just treat any `on-damage-calc` call in this zone as "the cube," since it's the only attackable NPC you're adding); call `apply_stat_delta(target_id, "hp", -10)` (or whatever amount) for real, **and** decrement your own zone-scoped tracked HP by the same amount so you can check for death; `send_message` the attacker (the host `send-message` function only targets one entity id per call, so "everyone nearby" means calling it once per target — for a manual 2-client test, just message the attacker) using the string convention in §7.1. If your tracked HP hits 0, call `report_death(target_id)`.
7. In `on_death(zone_id, entity_id)`: `send_message` the `"cube:...:dead"` convention to whoever needs to know.

### 7.3 What to build (client-side, Godot)

- Render the cube as any primitive `MeshInstance3D` (a `BoxMesh` is literally "Evil Cube," lean into it) at the position from its `RosterEntry` in your zone's roster.
- A world-space (or screen-space, your call) health bar UI element above it, driven by parsing `PluginMessage` bodies matching your `cube:` convention (§7.1).
- "Targeting" is **entirely client-side state** — there is no server concept of a current target. When the player clicks/selects the cube, remember its `entity_id` locally; "Attack" sends `Attack{target_entity_id: <that remembered id>, stat_key: "hp"}`.
- Rewards/XP/loot: **not supported at all** — there is no loot table, XP, or reward concept anywhere in World Zero's core or in `example-plugin`. Skip this entirely; don't build a fake local-only reward system and present it as if it were real.

---

## 8. Multiplayer visibility — realm / zone / layer (no shard, no instance)

World Zero's actual hierarchy, from `docs/PROPOSAL.md`'s glossary and `world_zero/crates/server/src/zone_registry.rs`:

**Realm → Zone → Layer.** That's it. **"Shard" is not a World Zero concept at all.** **"Instance"** in World Zero's own vocabulary just means "a realm is one independently-operable instance of the game world" (like a classic MMO "server") — it does **not** mean WoW-style instanced dungeons; no per-group private zone copies exist.

- **Realm:** now real and reachable — see §3.1. A `server` process serves exactly one realm today, chosen at startup (`WZ_REALM_ID`, see §9.2); two test clients pointed at the same running `server` are automatically in the same realm.
- **Zone:** the `zone_id` string from the manifest (§5.1). Two clients only see each other if they're in the same zone.
- **Layer:** within a zone, `server` can run more than one parallel simulated copy ("layer") of that zone once population crosses a threshold (`WZ_LAYER_POPULATION_THRESHOLD`, default `200` connected sessions; `WZ_LAYER_ENABLED`, default `true`). Layer assignment happens automatically, server-side, at initial join (`ZoneRegistry::assign_layer`) — **there is no layer identifier anywhere in the wire protocol; the client never learns or chooses its layer.** `Moved`/`EntitySpawned`/`EntityDespawned` broadcasts only reach sessions in the same zone **and layer**.
- **Live cross-layer reassignment is now real (#142, plus #178's party system built on top of it).** `ZoneRegistry::join_layer_of(zone_id, entity_id)` resolves which layer another already-spawned entity currently occupies, and `session.proto`'s `JoinGroupLayer{other_entity_id}` (or, more usefully, accepting a real party invite — §11) moves your own entity onto that layer live, reusing the exact same spawn/despawn/roster mechanism a zone-link crossing uses (`ZoneChanged`, §5.3). `other_entity_id` must actually be a fellow party member (checked against the real `character::PartyStore`) — this used to be an unconditional, unchecked move; it's real membership-gated now.

For your manual two-client test: with the default threshold of 200, two clients will always land on the same layer 0 without you doing anything. If you want a real cross-layer test (now meaningful given party formation is real, per the note above), lower `WZ_LAYER_POPULATION_THRESHOLD` for a 3-4 client run — that will force clients onto different layers initially, and you can confirm a party invite pulls a member onto its inviter's layer live. Setting `WZ_LAYER_ENABLED=false` still removes the variable entirely if you want deterministic same-layer behavior instead. Both are env vars on the **backend** process, not something your Godot client controls.

**What determines "A sees B":** same `zone_id` + same layer (invisible, auto-assigned unless moved live via a party) + both currently spawned. Confirmed via: A's `Joined`/`ZoneChanged` roster includes B if B was already there; B receives `EntitySpawned` for A the moment A joins; both receive each other's `Moved` broadcasts thereafter; both receive `EntityDespawned` when the other disconnects or leaves the zone.

**Identifiers the client needs:** `entity_id` (assigned by the server at spawn time, a fresh UUIDv7 per connection/session — **not stable across reconnects**, a new one is minted every time you join). `character_id`, `account_id`, `zone_id`, `realm_id` are the other ids in play (see §14's full table).

---

## 9. Local dev setup — exact steps, real values

This section is what your generated `.env.example` (see §9.3) should be based on, and what your README should tell a developer to actually run.

### 9.1 Required local services

- **PostgreSQL** and **Redis** — `server` needs both, no exceptions (`world_zero/crates/common/src/config.rs`).
- Fastest path: from the `world_zero` repo root, `make docker-up` starts both via `docker-compose.yml` (small-team/dev-only, not the production story, but exactly right for this test grounds). It runs a preflight check for the required `WZ_POSTGRES_*`/`WZ_REDIS_*` vars in `world_zero/.env` first.
- `world_zero/.env.example` (copy to `world_zero/.env` and fill in — this is the **backend's** own env file, separate from anything in this project):
  ```
  WZ_POSTGRES_HOST=
  WZ_POSTGRES_PORT=5432
  WZ_POSTGRES_USER=
  WZ_POSTGRES_PASSWORD=
  WZ_POSTGRES_DATABASE=

  WZ_REDIS_HOST=
  WZ_REDIS_PORT=6379
  WZ_REDIS_PASSWORD=
  ```
  If using `make docker-up`, set both hosts to `localhost` once the containers are up.

### 9.2 Standing up `server` itself with the two-zone content + your plugin

Run these from the `world_zero` repo root, **in order**:

1. `make docker-up` (or point `WZ_POSTGRES_*`/`WZ_REDIS_*` at your own instances) — Postgres/Redis up first.
2. `cargo run -p common --bin migrate -- up` — applies DB migrations (or `make migrate`).
3. **Create a realm — this is new and required (#136).** `server` no longer runs against a hardcoded placeholder realm; it needs a real one to exist first and its id passed in:
   ```sh
   make realm ARGS="create MyRealm open"    # prints the new realm's id — save it
   ```
   (`open` vs `bound` policy doesn't matter for this test grounds — either works; see `docs/specs/Realm_Character_Policy_Spec.md` if you're curious about the difference.)
4. Copy config into `world_zero/config/` (create `WZ_CONFIG_DIR` if you're pointing elsewhere instead — default is `./config` relative to wherever you run `server` from):
   - `cp config/stats.schema.example.yaml config/stats.schema.yaml`
   - `cp config/party.schema.example.yaml config/party.schema.yaml` — **required**, `server` panics at startup without it (backs the real party system, §11).
   - `cp config/guild.schema.example.yaml config/guild.schema.yaml` — **required**, `server` panics at startup without it (backs the real guild system, §12).
   - `cp config/character.archetypes.example.yaml config/character.archetypes.yaml` — **required**, `server` panics at startup without it (#213/#212 — validated against `stats.schema.yaml`; see §3.3 for what it actually does, which is not client-selectable).
   - `cp config/crafting.schema.example.yaml config/crafting.schema.yaml` — **required**, `server` panics at startup without it (backs the real crafting system, #216 — see §13).
   - `cp config/currency.schema.example.yaml config/currency.schema.yaml` — **required**, `server` panics at startup without it (backs the dev-declared multi-currency system, #217/#218 — see §13).
   - `cp config/content-pack.example.yaml config/content-pack.yaml`
   - Your edited copy of `config/example-zones/greenwood-forest.yaml` (with the Evil Cube spawn table added, §7.2) + the unmodified `config/example-zones/stonebridge-village.yaml`, at whatever paths `content-pack.yaml`'s `zones[].path` entries point to (relative to the content-pack file's own directory — check `content::content_pack.rs` if you move things).
   - **Do not** also create `config/zone.manifest.yaml` — its presence is only a single-zone fallback path; `content-pack.yaml` being present is what activates the multi-zone/link-transition code path.
5. Build and place your plugin: `rustup target add wasm32-wasip2` (once), then build your plugin crate with `cargo build --target wasm32-wasip2 --release`, then:
   ```
   mkdir -p config/plugins/evil-cube-plugin
   cp <your-plugin-crate>/plugin.toml config/plugins/evil-cube-plugin/
   cp <your-plugin-crate>/target/wasm32-wasip2/release/<crate_name>.wasm config/plugins/evil-cube-plugin/
   ```
   `WZ_PLUGINS_DIR` (default `<config_dir>/plugins`) is auto-scanned at startup — every `<name>/{plugin.toml,*.wasm}` subdirectory loads, no further wiring needed. You can drop `example-plugin` in alongside yours too if you want its wolf-pack/interact-trigger/chat-command (`/wave`) behavior in the test grounds as well — nothing stops you from running both plugins at once; just make sure their declared `message_types`/`chat_commands` don't collide (`plugin_host::check_no_collisions` will refuse to start if they do).
6. `WZ_REALM_ID=<the id you got from step 3> cargo run -p server` — also reads `WZ_CONFIG_DIR` (default `./config`), `WZ_SERVER_ADDR` (default `127.0.0.1:7900`), `WZ_SERVICE_CHAT_ENABLED` (default `true`), `WZ_LAYER_ENABLED`/`WZ_LAYER_POPULATION_THRESHOLD`, `WZ_METRICS_ADDR`, `WZ_CHARACTER_MAX_PER_ACCOUNT`. On success you'll see `INFO server worldzero server listening local_addr=127.0.0.1:7900` in the log — that's your connect target.

### 9.3 What `world-zero-test-grounds/.env.example` should contain

This project's own env file — for the Godot client, not the backend (the backend's own `.env` lives in the `world_zero` repo and is separate). No real secrets belong in either — everything below is local dev config:

```
# Where the World Zero `server` process (run separately, see PROMPT.md §9.2) is listening.
WZ_TEST_SERVER_HOST=127.0.0.1
WZ_TEST_SERVER_PORT=7900

# Path to server's self-signed TLS cert (world_zero/config/certs/self_signed.cert.der,
# generated on server's first run) if you're trusting it explicitly rather than disabling
# cert validation for local dev. Leave blank if you chose to disable validation instead.
WZ_TEST_TLS_CERT_PATH=

# The one realm id your test server was started with (PROMPT.md §9.2 step 3/6) — the
# client sends this back as SelectRealm right after auth (§3.1); there's no discovery
# UI needed for a single-realm test setup.
WZ_TEST_REALM_ID=

# A default account to auto-fill the login form with during manual testing, purely a
# convenience — leave blank to always type credentials by hand.
WZ_TEST_DEFAULT_USERNAME=
WZ_TEST_DEFAULT_PASSWORD=
```

Adjust names/shape to fit however Godot's C#/.NET tooling conventionally loads env files in your setup — the point is: no hardcoded connection info baked into a scene, everything above is developer-local and gitignored (`.env` itself, not `.env.example`).

### 9.4 CORS / origin requirements

**None.** This is a raw TCP+TLS socket protocol, not HTTP — there is no CORS concept anywhere in this stack. Ignore any instinct to configure origins/headers; there's nothing to configure.

---

## 10. Chat

Source: `chat.proto`, `world_zero/crates/server/src/chat_session.rs`, `world_zero/crates/chat/src/store.rs`, `docs/specs/Chat_Spec.md`.

```protobuf
message ClientMessage {
  oneof kind {
    Join join = 1;    // { string channel }       -- a NAME, not an id
    Leave leave = 2;   // { string channel }
    Send send = 3;     // { string channel_id, string body }   -- an ID, from the Joined reply
  }
}
message ServerMessage {
  oneof kind {
    Joined joined = 1;  // { string channel_id, string channel }
    Left left = 2;       // { string channel }
    Chat chat = 3;        // { string channel_id, string channel, string sender, string body }
    Error error = 4;
  }
}
```

`message_type = 100`, gated behind auth (same connection, same handshake as everything else — chat does not additionally require realm/character selection to have completed, unlike world traffic). Flow: send `Join{channel: "some-name"}` → server finds-or-creates a channel by that exact name and replies `Joined{channel_id, channel}` (or `Error` if you're already joined to that name) → remember `channel_id` → `Send{channel_id, body}` to talk → other joined members receive `Chat{channel_id, channel, sender, body}` (you never get an echo of your own sent message) → `Leave{channel}` to leave.

**Important, verified behavior, unchanged since the previous version of this document:** `server::chat_session::join_channel`'s name resolution (`chat::demo_support::find_or_create_named_channel`) **only ever finds-or-creates `group`-type channels**, regardless of what name you send. This is still your "form/join a group" mechanic for casual, unmanaged chat rooms — but see §11: **real parties are no longer the same thing as a `group` chat channel.** They're a separate, real system now, layered on top of this chat mechanism only in that a party doesn't automatically get its own chat channel (that's still up to you — see §11). Guild chat channels, by contrast, *are* now really synced to a real roster (§12) — a structural change from the previous "guild is unreachable" state.

**World Zero's own pre-declared "system channels" (`chat.yaml`) are now wired into `server::main` (#234)** — a declared `global`- or `zone`-scope category actually gets its channel created at startup, unlike the earlier version of this document's claim that this was dead config. This is optional and not required for the test grounds (an ad-hoc `chat.yaml` isn't part of §9.2's setup steps) — if you want to exercise it, declare a category in `chat.yaml` and confirm the channel exists (e.g. by `Join`ing it by the name you declared) right after `server` starts, with no client having done anything yet. For the test grounds' own basic chat panel: implement a simple "type a channel name, join it, send/receive text" flow (§10 above) for casual group chat — that's still the real, fully-working mechanic for that use case, `chat.yaml` or not.

---

## 11. Parties — real now, not a chat-channel workaround (#178)

**This section is a full rewrite.** The previous version of this document said "a group is exactly a chat channel of type `group`," with no invite/accept, no membership updates, and no real cap. That's gone — there is now a real, durable party system, backed by `character::PartyStore`, with a real invite/accept/decline flow and dev-declared size limits. Source: `docs/specs/Chat_Spec.md`'s "Party/group system" section, `session.proto`.

```protobuf
message PartyInvite {           // client
  string target_entity_id = 1;
  string party_type = 2;
}
message PartyInviteResponse {   // client
  bool accept = 1;
}
message PartyLeave {}           // client

message PartyInviteReceived {   // server
  string from_entity_id = 1;
}
message PartyInviteDeclined {   // server
  string by_entity_id = 1;
}
message PartyUpdate {           // server
  repeated string members = 1;  // every OTHER member's live entity id; empty = "no party"
}
```

Flow: send `PartyInvite{target_entity_id, party_type}` to any currently-connected player's live entity id, in any zone — this creates a new party if you're not already in one, or grows your existing party. `party_type` names one of the dev-declared entries in `party.schema.yaml` (`config/party.schema.example.yaml` — copy this, §9.2); an empty string resolves to the schema's first declared type, and it only matters when this invite *founds* a new party — joining an inviter's already-existing party always uses whatever type it was actually founded under. The target gets `PartyInviteReceived{from_entity_id}` and answers with `PartyInviteResponse{accept}` — declining notifies you via `PartyInviteDeclined{by_entity_id}` and nothing is committed; accepting is a real, durable storage write and both sides get `PartyUpdate{members}` reflecting the new roster. `PartyLeave{}` leaves your current party — if that would drop it to a single remaining member, the whole party dissolves (both sides get `PartyUpdate` again, empty for the one who left). At most one pending invite per invitee — a later invite before the first is answered just replaces it.

**Live layer composition (§8):** if you and the inviter are already in the same zone when an invite is accepted, you're moved onto their live layer as a direct side effect — no separate step, see §8's `join_layer_of` note.

There is still no "list a party you're not in" or party-browsing concept — you invite someone you already know the entity id of (from your local roster, §15), same as targeting anything else in this protocol.

## 12. Guilds — real now (#179, merged via PR #209)

**This section is also a full rewrite.** The previous version of this document said guilds had zero wire-protocol path at all. There is now a real, persistent, account-scoped guild system with a dev-declared rank hierarchy and permissions, in a dedicated `guild` crate — source: `docs/specs/Chat_Spec.md`'s "Guild system" section, `session.proto`.

```protobuf
message GuildCreate       { string name = 1; }                                    // client
message GuildInvite       { string target_entity_id = 1; }                        // client
message GuildInviteResponse { bool accept = 1; }                                  // client
message GuildLeave        {}                                                      // client
message GuildDisband      {}                                                      // client
message GuildKick         { string target_entity_id = 1; }                        // client
message GuildPromote      { string target_entity_id = 1; string rank_key = 2; }    // client
message GuildDemote       { string target_entity_id = 1; string rank_key = 2; }    // client
message GuildSetMotd      { string motd = 1; }                                    // client
message GuildSetTag       { string tag = 1; }                                     // client

message GuildInviteReceived { string from_entity_id = 1; }                        // server
message GuildInviteDeclined { string by_entity_id = 1; }                          // server
message GuildMember       { string entity_id = 1; string rank_key = 2; }
message GuildUpdate {                                                             // server
  string guild_id = 1;
  string name = 2;
  string motd = 3;
  string tag = 4;
  repeated GuildMember members = 5;   // the WHOLE roster (including you), unlike PartyUpdate
}
message GuildDisbanded {}                                                         // server
```

Key differences from parties (§11), worth understanding before you build a UI:

- **Guild membership is account-scoped, not character-scoped.** A guild persists independent of which character you're logged in as, or even whether any of its members are currently connected at all — unlike a party, which is inherently a live, connected-players thing.
- **Ranks are dev-declared, not hardcoded**, via `guild.schema.yaml` (`config/guild.schema.example.yaml` — copy this, §9.2, and it's required at server startup). A dev names as many ranks as they want, each carrying a set of permissions from a fixed core list: `invite`, `kick`, `promote`, `demote`, `edit_motd`, `edit_tag`, `rename`. Whoever creates a guild is placed at the schema's first-declared rank (the "founder" rank) — only that rank may disband the guild or promote/demote anyone into or out of it, regardless of how a dev otherwise assigns permissions. A fresh accepted invite joins at the schema's *last*-declared rank.
- **Every action targets *your own* guild implicitly** — there's no `guild_id` parameter on any client message; the server resolves it from your account's current membership (an account can only be in one guild at a time).
- **Real chat sync, unlike parties**: creating a guild (when chat is enabled server-side, `WZ_SERVICE_CHAT_ENABLED`) creates a real backing chat channel, and every membership change (accept, leave, kick, disband) keeps that channel's membership in sync automatically — you don't have to build your own "announce join" convention the way parties/groups still require. If chat is disabled, guilds still work fully; there's just no synced channel.
- **`GuildUpdate.members` is the whole roster including yourself**, unlike `PartyUpdate`'s "everyone but me" — a guild UI is meant to show ranks for everyone. A member's `entity_id` in that list is empty if they're not currently connected (a guild roster can include offline members; a party's can't, since a party only ever exists among currently-known live entity ids in the first place).
- **Acting on an offline member isn't supported by this wire protocol** — `GuildKick`/`GuildPromote`/`GuildDemote` all target a live `entity_id`, same discipline as `GuildInvite`/`PartyInvite`. You can't manage an offline guildmate today.

Flow example: `GuildCreate{name}` → `GuildUpdate` with a one-member roster (you, at the founder rank) → `GuildInvite{target_entity_id}` to another connected player → they get `GuildInviteReceived{from_entity_id}` → `GuildInviteResponse{accept: true}` → both sides get a fresh `GuildUpdate` with the two-member roster. `GuildLeave{}`/`GuildDisband{}`/`GuildKick{target_entity_id}` all produce fresh `GuildUpdate`s for whoever's still affected and connected; a departed/kicked/disbanded-out member specifically gets an empty `GuildUpdate` (`guild_id` empty, no members) rather than the stale old roster.

---

## 13. Inventory / equipment / currency / crafting

- **Items** are `(item_type: string, quantity: i64)` stacks per character — that's the entire equipment model. **There is no equipment/gear-slot system at all** — no "equip a weapon," no armor slots, nothing structural beyond quantity stacks. `item_type` is an opaque plugin-defined string (like `"wolf-fang"` in `example-plugin`).
- **Currency is now a separate, dev-declared, possibly-multi-balance system (#217/#218)**, not a single implicit balance folded into `character::inventory` the way an earlier version of this document described — see `currency.schema.yaml` (§9.2, required at startup). Every declared `currency_key` (`config/currency.schema.example.yaml`) carries its own independent `i64` balance.
- All writes (`grant-item`/`remove-item`/`modify-currency`) are **plugin-only**, same as stats — a client never directly manages its own inventory or currency.
- **This used to be a real gap; it's closed now for your own inventory/currency (#211/#218), same story as §4's stats.** `session.proto` has real, structured `ItemChanged{item_type, quantity}` and `CurrencyChanged{currency_key, balance}` server messages (`message_type = 200`) — both carry the *resulting* value, not the delta, same convention as `StatChanged`. Pushed automatically to a connection whenever `grant-item`/`remove-item`/`modify-currency` actually writes against that connection's own character. There's still no "get my current inventory/balance" query — you have to have received every relevant push since connecting (or since the character last loaded) to have a complete picture, same as you'd track position from `Moved` — but the push itself is real now, not an invented convention.
- **Crafting is a real system now (#216), previously unmentioned in this document.** `session.proto`'s `CraftItem{recipe_key}` (`message_type = 200`) requests a craft against the caller's own character — `recipe_key` names an entry in `crafting.schema.yaml` (§9.2, required at startup; `config/crafting.schema.example.yaml`). Rejected (unknown `recipe_key`, or insufficient input items) with nothing consumed — check your debug console's event log (§16) for the `Error`/lack of any `ItemChanged`, there's no dedicated success/failure reply beyond that. On success, no dedicated reply either: the resulting change (every consumed input, then the granted output) arrives as ordinary `ItemChanged` pushes, one per item type that changed. Build a minimal "pick a known `recipe_key`, send `CraftItem`, watch `ItemChanged` land" flow — no need for a real recipe-browsing UI, there's no "list known recipes" query any more than there's a "list my inventory" one.
- For the test grounds: an inventory/currency panel can now be built against real structural pushes (`ItemChanged`/`CurrencyChanged`) — track them the same way you track `StatChanged`/`Moved`. This is a real change from an earlier version of this document, which said this could only ever be a client-side reconstruction from ad-hoc plugin text.

---

## 14. IDs — what's a real UUID vs. a plain string, and what the client should hold onto

| Concept | Wire representation | Assigned by | Stable across reconnect? |
|---|---|---|---|
| `account_id` | UUIDv7 text string | server, at registration | yes (permanent) |
| `session_token` | opaque random string | server, at login/register (now real for reconnection too, §2.4) | no — reissued every login/resume |
| `realm_id` | UUIDv7 text string | server, at realm creation (`make realm ARGS="create..."`) | yes — **now reachable by a client**, required as part of the mandatory `SelectRealm` step (§3.1/§2.5) |
| `character_id` | UUIDv7 text string | server, at character creation | yes — **now crosses the wire for real** (`character.proto`'s `CreateCharacter`/`CharacterList`/`SelectCharacter`, §3.2), unlike the previous version of this document's claim that it never appears on the wire |
| `entity_id` | UUIDv7 text string | server, fresh **every time you join/spawn** | **no** — new one every connection |
| `zone_id` | **plain manifest-declared string slug** (e.g. `"greenwood-forest"`), never a UUID | content-pack/manifest author (you, when authoring zone YAML) | yes (it's just config) |
| `channel_id` | UUIDv7 text string | server, at channel creation | yes, but only discoverable via a fresh `Join` reply — no lookup-by-name-only-if-cached |
| `guild_id` | UUIDv7 text string | server, at guild creation (#179, §12) | yes — carried on every `GuildUpdate`, empty string meaning "no guild" |

Every id crosses the wire as a plain `string` (protobuf `string` fields) — never a typed/structured field — by deliberate design (`docs/specs/Plugin_API.md`'s "Ids are opaque strings" principle, applied identically to the client protocol). Treat every id field as an opaque string; don't try to parse UUID internals client-side. There is still no `party_id` on the wire — a party is only ever addressed implicitly, through its members' entity ids (§11).

**Worth knowing for later (see the follow-up ticket, §17):** `common::id` (`world_zero/crates/common/src/id.rs`) actually declares a `ZoneId` UUID type — but grepping the entire codebase confirms **it's still never used anywhere**. Every real zone reference, everywhere, uses the plain string slug instead. This is exactly the "should be a PK, is actually a name" pattern flagged for investigation.

---

## 15. Client SDK internal state — recommendations, not a redesign

World Zero has no packaged client SDK for any engine today — you're writing the thin client layer yourself. Based on everything above, track at minimum:

- Current `account_id` / logged-in username, `session_token` (for `Resume`, §2.4)
- Current `realm_id` (from a successful `RealmSelected`, §3.1)
- Current `character_id` (from a successful `CharacterSelected`, §3.2 — this is now a real, trackable id, unlike before)
- Current `zone_id` (from the last `Joined`/`ZoneChanged`)
- Current `entity_id` (from the last `Joined`/`ZoneChanged` — changes every reconnect)
- **Layer/shard/instance:** nothing to track directly — never sent, never knowable client-side (§8) beyond inferring "I got moved live" from an unprompted `ZoneChanged` to the same `zone_id` you were already in
- Authoritative position (last `Moved`/`Joined`/`ZoneChanged` for your own `entity_id`) **and** locally-predicted position, kept separate, plus your small in-flight-`seq` buffer (§6.4)
- A local roster: `entity_id → {entity_type, last known x/y, last update tick/time}`, seeded from `Joined.roster`/`ZoneChanged.roster`, updated by `EntitySpawned`/`EntityDespawned`/`Moved`
- Connection state (connected / authenticating / realm-select / character-select / in-world / disconnected) — entirely your own state machine, nothing server-driven beyond message arrival/socket closure
- Joined chat channels: `name → channel_id` (§10) — the server gives you no way to re-discover this after the fact except re-`Join`ing
- Current party: the last `PartyUpdate.members` you received (§11)
- Current guild: the last `GuildUpdate` you received — `guild_id`/`name`/`motd`/`tag`/`members` (§12)
- Current target: purely local UI state (§7.3)
- Your own character's last known stats/items/currency: seeded from nothing (no query exists), built up purely from `StatChanged`/`ItemChanged`/`CurrencyChanged` pushes received since connecting (§4/§13) — the NPC-specific text-convention state (e.g. the Evil Cube's HP, §7) is separate local state, parsed from `PluginMessage` bodies, not from these structured pushes
- Last received event/hook, for your debug console (§16)

None of this needs to be a formal "SDK" for this task — a plain autoload/singleton in Godot holding these fields, updated as messages arrive, is enough. If a future real SDK gets built, this is the shape it should probably have — note that in the follow-up ticket rather than over-engineering it now.

---

## 16. Make the client observable — required for this to be useful as a test tool

Build a debug/dashboard overlay (a simple `Control` UI is enough, doesn't need to be pretty) showing, live:

- `account_id`, `realm_id`, `character_id`, `entity_id`, `zone_id`, layer (say "layer: not exposed by protocol" — be honest rather than omitting it)
- Connected/visible player+NPC count in current zone (derived from your local roster, §15 — there's no server-pushed count)
- Your authoritative server position vs. your predicted local position, both numerically and ideally as two markers in the 3D view
- **Ping/latency: now a real number, not an approximation.** Send `Ping{client_sent_at}` periodically and show the RTT from the matching `Pong` (§6.3) — this is a real backend-provided mechanism now, unlike the previous version of this document, which had to estimate RTT off `Move`/`Moved` timing as a rough stand-in.
- Connection status (per your own state machine, §15)
- Current party roster (if any), current guild + rank (if any), current target entity id (if any)
- Your own character's last known stats/items/currency, from `StatChanged`/`ItemChanged`/`CurrencyChanged` pushes (§4/§13) — separately from the Evil Cube's HP, which is NPC state parsed from `PluginMessage` text (§7)
- **An event/message log console** — every envelope you decode, its `message_type`, and a one-line summary of its contents, newest at top or bottom, scrollable. This is the single most useful thing for the human running this tool to actually watch World Zero's real lifecycle during manual testing — don't skip it or make it minimal.

---

## 17. Hooks/events audit + IDs/PKs + client SDK — file this as a follow-up ticket, don't solve it now

While building this handoff, several concrete gaps surfaced that are worth a real backend investigation, but are explicitly **out of scope to fix as part of building the test grounds**. Filed as [`LunarVagabond/WorldZero#191`](https://github.com/LunarVagabond/WorldZero/issues/191) (`[Decision] client hook / SDK contract evaluation`) to track — still open as of this re-pass, and not part of the `Frontend Ready` milestone's blocking list, so it stays open by design:

- **Mutable state events** — e.g. a respawn point changing as a player progresses — have no hook or wire message today; worth deciding if/how that should surface.
- **IDs/PKs vs. names** (§14) — `zone_id` is still a plain string slug everywhere, never the declared-but-unused `ZoneId` UUID type. Realm/character/channel/guild ids are now real UUIDs crossing the wire (§14) — the question is really just `zone_id` at this point, not the broader set it used to be.
- **What a real client SDK should track internally** (§15) — this document's recommendations are a reasonable starting point but were produced by one integration effort, not a design pass.
- **`max_population`/`respawn_seconds` are declared in zone manifests but never enforced anywhere** (§7.1) — still true, unchanged.
- **NPC stats still have no client-visible push** (§4, §7) — `StatChanged` (#211) closed this for player-owned entities, but its own doc comment explicitly excludes NPC-targeted writes ("no owning connection to push to"). This is the narrowed, still-real remainder of what used to be a much broader "no structured stat/inventory push at all" gap (#211/#218 closed the inventory/currency half and the player-stat half). If World Zero ever wants the Evil Cube-style NPC health bar to work without an invented text convention, this is the concrete piece left to design — e.g. broadcasting NPC `StatChanged`-equivalents to everyone in the zone rather than to "an owning connection" that doesn't exist for an NPC.

Five items from earlier versions of this list are now resolved and removed from it: "guild system has no wire path at all" (closed by #179, §12), "no ping/heartbeat message + no sequence number/timestamp on `Move`/`Moved`/`Rejected`" (closed by #196, §6/§16), "no structured stat/inventory push to the client" for player-owned entities specifically (closed by #211/#218, §4/§13 — the NPC-specific remainder is the new bullet above), "`chat.yaml`/system channels parsed but never wired into `server::main`" (closed by #234, §10), and "no symmetric `on-zone-left`/`on-player-leave-zone`-adjacent signal" (closed by #233 — a plugin-side hook fix, not a new client wire message, so it doesn't change §5/§6's client contract, but it closes the underlying gap this bullet tracked).

This is an open discussion issue, not a mandate — do not attempt to resolve any of it as part of building the test grounds. If you notice further gaps while implementing, add them as a comment on #191 rather than expanding scope here.

---

## 18. What to build — the concrete manual test checklist

Build the smallest possible client that lets a human manually verify, running two client instances side by side:

1. Register a new account (`ClientMessage{Register}`, §2.4)
2. Log in (`ClientMessage{Login}`)
3. Select a realm (`SelectRealm`, §3.1) — real now; can be automatic (no picker UI needed for a single-realm test server) but the message must actually be sent
4. List/create a character (`ListCharacters`/`CreateCharacter`, §3.2) — real now; build at least a minimal "create if none exists, else pick the first" flow, though a real picker is more useful for testing multi-character support
5. Select the character (`SelectCharacter`, §3.2) — real now, and this is what actually triggers world-join
6. ~~Classes/races~~ — still not supported (§3.3); skip, or client-side-cosmetic only
7. Confirm the world-join happens automatically right after character selection (`Joined`, §2.5/§6)
8. Walk around (`Move{x,y,seq}`, §6) with client-side prediction of your own entity (using real `seq` correlation, §6.4) and interpolation of others
9. Confirm movement is validated by the server (trigger a `Rejected` on purpose — e.g. try to walk out of bounds — and confirm you see it, with its echoed `seq`, in the debug console, §16)
10. Launch a second client/account, repeat steps 1-7 (same realm — it's the only one your test server serves)
11. Confirm both land in `greenwood-forest` and see each other (`EntitySpawned`, roster, `Moved`, §8)
12. Chat between them (§10)
13. Send a real party invite from one to the other, accept it, confirm both get a correct `PartyUpdate`, and (if they weren't already) confirm the accepter lands on the inviter's live layer (§8/§11) — then have one leave and confirm both sides' `PartyUpdate` reflects it
14. Create a guild, invite the second client, accept, confirm both get a correct `GuildUpdate` including your assigned ranks, edit the MOTD/tag and confirm it propagates, then have the invited member leave and the founder disband (§12) — if chat is enabled, also confirm the guild's synced chat channel gained/lost members correctly
15. Fight the Evil Cube (§7) — attack, see health bar update via your `cube:` message convention (still required — the cube is an NPC, `StatChanged` doesn't cover it), see it die, note whether/how it respawns
15a. Confirm your own character's stat/inventory/currency pushes are real and structural (§4/§13, no text-convention needed here): trigger something that changes your own HP (e.g. take a hit that `on-damage-calc` routes back at you, if your plugin does that) and confirm a `StatChanged` lands in your debug console; send `CraftItem{recipe_key}` for a recipe declared in `crafting.schema.yaml` and confirm `ItemChanged` (and `CurrencyChanged`, if the recipe touches currency) lands for both consumed inputs and the granted output
16. Walk from `greenwood-forest` into `stonebridge-village` across the linked edge and confirm a live `ZoneChanged` (no reconnect) — walk back too
17. Disconnect (close the client) and reconnect — try `Resume{session_token}` first (§2.4), confirm it works; also test the `Login` fallback path. Either way, confirm your character is back where you left it (persistence, applied at clean disconnect only — note in your debug console if you kill the client instead of a graceful disconnect, since position won't have been flushed for that case)
18. Ping/latency: confirm a real RTT number shows up in your debug console (§16), not an approximation
19. Watch the debug/event console (§16) throughout — this is as much the point of the exercise as the 3D scene is

Primitive geometry throughout (boxes/capsules), no art requirements. The 3D scene should feel like a barely-dressed debug tool with a floor and some boxes, not a game — that's correct, not a shortfall.
