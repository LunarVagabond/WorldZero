# Networking Spec

Corresponds to [Networking](../PROPOSAL.md#networking) in the proposal, which covers the TCP-vs-UDP-vs-QUIC decision and what traffic goes on which channel. This spec covers the actual wire format and TLS/DTLS setup that decision didn't get into.

## TLS (TCP channel)

**Crate: `tokio-rustls`** — pure Rust, no OpenSSL to vendor or link, consistent with the rest of the stack's crypto choices (`argon2` for passwords, `rustls` already pulled in transitively via `sqlx`'s `tls-rustls` feature for Postgres).

**Cert handling for self-hosters — there is no CA relationship by default, so:**

- **Zero-config default:** `gateway` generates a self-signed certificate/key pair on first run if none is configured, stored under `<config_dir>/certs/` (gitignored — generated, not authored, so it doesn't belong in a dev's own config the way `stats.schema.yaml` does). The certificate's SHA-256 fingerprint is logged at `INFO` on every startup. This is the same trust model SSH/Signal use (trust-on-first-use + out-of-band fingerprint verification) — it satisfies "one command from clone to a running world" (docs/PROPOSAL.md, Developer Experience Bar) without pretending self-signed certs are a real production security boundary against a MITM who's never seen the fingerprint before.
- **Real deployments:** an operator points `WZ_TLS_CERT_PATH`/`WZ_TLS_KEY_PATH` at a real certificate (e.g. Let's Encrypt-issued) — same `WZ_*` env var convention as `common::config`'s existing Postgres/Redis config. When both are set, `gateway` uses them instead of generating anything.
- **An invalid/unparseable configured cert fails startup immediately** with a clear error naming the problem — never a silent fallback to a freshly generated self-signed one (that would silently downgrade a production deployment's trust story).

## DTLS (UDP channel)

**Crate: `rtc-dtls`** (part of the `webrtc-rs` project's sans-IO `rtc` rewrite) — pure Rust, no OpenSSL, consistent with the rest of the stack's crypto choices. Originally scoped to the older `webrtc-dtls` (also pure Rust), but that crate's home repo has been archived in favor of this sans-IO successor, which is under active development — `rtc-dtls` was picked instead to avoid landing on a crate already headed for unmaintained status. The tradeoff: `rtc-dtls` owns no socket or clock, so `gateway` drives the handshake/record layer itself against a real `tokio::net::UdpSocket` (`gateway::udp`) rather than getting a batteries-included connection type — more integration code, but on the actively-maintained path. The alternative, OpenSSL's DTLS support via the `openssl` crate, was rejected for the same reason `argon2` over a C library was: this project already has zero non-Rust crypto dependencies, and introducing OpenSSL just for the UDP leg would mean two different crypto stacks (and two different build/vendoring stories) for what's conceptually one security requirement.

**Reuses the same certificate/key as TLS** — one keypair, one fingerprint an operator manages, not a second cert path to configure. Same self-signed-by-default / `WZ_TLS_CERT_PATH`/`WZ_TLS_KEY_PATH`-if-set behavior as above.

**Verification model:** like TCP's TLS, this is fingerprint-pinning, not CA-chain validation — DTLS handshakes with `insecure_skip_verify` on the client side (the channel is still fully encrypted; skipping verification only removes protection against a MITM who's never seen the operator-logged fingerprint, the same trade already made for self-signed TLS above).

**Scope reminder (docs/PROPOSAL.md, Networking):** DTLS is transport security only — confidentiality/integrity/authenticity of the wire. It does not, and is not meant to, validate that the *data inside* an encrypted UDP packet is legitimate (a client can encrypt a lie just as easily as the truth). That's `world`'s authoritative movement validation (#33), a completely separate concern from this spec.

## Message framing

**One shared envelope, two different delimiting mechanisms** — TCP and UDP carry the same logical message shape, but a stream and a datagram need different boundary handling:

```
envelope:
  message_type: u16   -- opaque discriminant; the catalog of what each value
                          means is defined incrementally as features wire in
                          (chat, movement, etc.) — not enumerated by this spec
  payload:      bytes  -- message_type-specific, opaque to the framing layer
```

- **TCP:** the envelope is length-prefixed — a 4-byte big-endian `u32` giving the envelope's total byte length, followed by that many bytes. This is exactly what `tokio_util::codec::LengthDelimitedCodec` implements; used directly rather than hand-rolled. Handles a message split across multiple TCP packets by construction (the codec buffers until it has a complete frame) — that's the whole reason a length prefix is needed on a stream in the first place.
- **UDP:** the envelope *is* the datagram payload — no length prefix, since a UDP datagram already has a natural boundary (`recv` returns exactly one datagram's worth of bytes, never more, never a partial one). **No message is ever reassembled across multiple UDP datagrams** — if a message wouldn't fit in one datagram, it doesn't belong on the UDP channel at all (matches the proposal's "high-frequency, loss-tolerant traffic" scope for this channel — position updates and combat ticks are small by nature).
- Both channels use the same `message_type` numbering — a given type means the same thing regardless of which channel carried it, so a handler doesn't need to know which transport a message arrived over to interpret it.

**Catalog so far** (each owned/defined by the crate that uses it, not by this spec — see that crate's own module for the actual message shapes):

| `message_type` | Owner | Payload encoding | Notes |
|---|---|---|---|
| 1 | `auth` (`auth::gateway_protocol`) | Protobuf (`crates/auth/proto/auth.proto`) | Login/registration handshake a connection performs first — docs/specs/Auth_Spec.md, "Gateway handshake". |
| 100 | `chat` (`chat::gateway_protocol`) | Protobuf (`crates/chat/proto/chat.proto`) | Join/leave/send over the same connection as world/plugin traffic, gated behind the auth handshake above — wired into the combined `server` process per [#104](https://github.com/LunarVagabond/WorldZero/issues/104) (`WZ_SERVICE_CHAT_ENABLED`, default on), also still reachable via chat's own standalone demo entry point — docs/specs/Chat_Spec.md, "Gateway demo integration". |
| 200 | `server` (`server::session_protocol`) | Protobuf (`crates/server/proto/session.proto`) | The phase-1 combined server's movement/action session — join a zone, move, see other entities move, and (#154) `Attack`/`UseItem`/`InteractNpc` client actions that route to the configured plugin's `on-damage-calc`/`on-item-use`/`on-npc-interact` hooks (docs/specs/Plugin_API.md) — gated behind the same auth handshake. |

**Plugin-declared message types (#95):** `message_type` 0-999 is core-reserved (the catalog above and any future core addition). A plugin declares its own values at `>= 1000` via `plugin.toml`'s `message_types` list (docs/specs/Plugin_API.md); `plugin-host` refuses to load a manifest declaring one below the floor, or the same value twice, before the plugin is ever instantiated. An envelope whose `message_type` matches a declared value is routed to that plugin's `on-message` hook instead of core dispatch — see docs/specs/Plugin_API.md's hooks table for the exact signature.

Cross-plugin collision checking doesn't exist yet — deliberately deferred, not an oversight: the server only ever loads one plugin at a time today (docs/PROPOSAL.md, "Phased Roadmap"), so there's no second declared set to check a new plugin's `message_types` against. Worth revisiting once multi-plugin loading is real.

## Wire schema (decision: #109, implemented: #123)

Every `message_type` in the catalog above is **protobuf** (Rust side via `prost`), decided in #109 and migrated off `serde_json` in #123. Protobuf was chosen over staying with JSON or moving to FlatBuffers/Cap'n Proto for its codegen coverage across all three target engines (Unity/C#, UE5/C++, Godot via a `protoc` plugin) and because the protocol surface was still small enough to migrate cheaply. This is a deliberately separate concern from the plugin ABI's WASM Component Model/WIT choice — no shared IDL between the two.

**Source of truth:** each `.proto` file (`crates/auth/proto/auth.proto`, `crates/chat/proto/chat.proto`, `crates/server/proto/session.proto`) — not this table, and not the hand-written Rust `ClientMessage`/`ServerMessage` enums each crate's `gateway_protocol.rs`/`session_protocol.rs` exposes. Those Rust enums are a deliberately ergonomic wrapper: every other call site in the codebase already matches on them field-by-field (`server::session`, `server::chat_session`, `chat`'s demo binaries, ...), so rather than exposing the generated `prost` `oneof`-shaped types directly (and rewriting every match site in the migration), each module's `encode`/`decode` bridge to/from the generated types via `From`/`TryFrom` impls. An id crossing the wire (`account_id`, `channel_id`, `entity_id`, `zone_id`) is always a plain `string` (a UUID's text form, or the manifest-declared zone id string), never a typed field — same "opaque id" discipline docs/specs/Plugin_API.md documents for the plugin ABI's ids.

**Rust codegen:** each of the three crates has a `build.rs` that runs `prost-build` against its `.proto` file into `OUT_DIR` at every `cargo build` — nothing generated is checked in. `protoc-bin-vendored` supplies a prebuilt `protoc` binary so building this project never depends on the host having one installed system-wide, keeping the "clone to a running world" DX bar (#44) intact.

**Client-integrator codegen (Unity/UE5/Godot):** a non-Rust client is **not** covered by the Rust-side `build.rs` above — it needs its own `protoc` install plus the language-specific plugin for its engine (`protoc-gen-csharp`/Unity's built-in protobuf support for C#, `protoc --cpp_out` for UE5/C++, a community `protoc-gen-gdscript`-style plugin for Godot) run against the same three checked-in `.proto` files. No client SDK/cookbook doc packaging this step exists yet — tracked as future work (docs/specs/Plugin_API.md-style "Beyond this v0 slice" note, not solved here); for now, an integrator points their engine's protobuf codegen directly at `crates/{auth,chat,server}/proto/*.proto`.
