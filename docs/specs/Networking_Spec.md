# Networking Spec

Corresponds to [Networking](../PROPOSAL.md#networking) in the proposal, which covers the TCP-vs-UDP-vs-QUIC decision and what traffic goes on which channel. This spec covers the actual wire format and TLS/DTLS setup that decision didn't get into.

## TLS (TCP channel)

**Crate: `tokio-rustls`** — pure Rust, no OpenSSL to vendor or link, consistent with the rest of the stack's crypto choices (`argon2` for passwords, `rustls` already pulled in transitively via `sqlx`'s `tls-rustls` feature for Postgres).

**Cert handling for self-hosters — there is no CA relationship by default, so:**

- **Zero-config default:** `gateway` generates a self-signed certificate/key pair on first run if none is configured, stored under `<config_dir>/certs/` (gitignored — generated, not authored, so it doesn't belong in a dev's own config the way `stats.schema.yaml` does). The certificate's SHA-256 fingerprint is logged at `INFO` on every startup. This is the same trust model SSH/Signal use (trust-on-first-use + out-of-band fingerprint verification) — it satisfies "one command from clone to a running world" (docs/PROPOSAL.md, Developer Experience Bar) without pretending self-signed certs are a real production security boundary against a MITM who's never seen the fingerprint before.
- **Real deployments:** an operator points `WZ_TLS_CERT_PATH`/`WZ_TLS_KEY_PATH` at a real certificate (e.g. Let's Encrypt-issued) — same `WZ_*` env var convention as `common::config`'s existing Postgres/Redis config. When both are set, `gateway` uses them instead of generating anything.
- **An invalid/unparseable configured cert fails startup immediately** with a clear error naming the problem — never a silent fallback to a freshly generated self-signed one (that would silently downgrade a production deployment's trust story).

## DTLS (UDP channel)

**Crate: `webrtc-dtls`** (part of the `webrtc-rs` project) — the only actively maintained pure-Rust DTLS 1.2 implementation. The alternative, OpenSSL's DTLS support via the `openssl` crate, was rejected for the same reason `argon2` over a C library was: this project already has zero non-Rust crypto dependencies, and introducing OpenSSL just for the UDP leg would mean two different crypto stacks (and two different build/vendoring stories) for what's conceptually one security requirement.

**Reuses the same certificate/key as TLS** — one keypair, one fingerprint an operator manages, not a second cert path to configure. Same self-signed-by-default / `WZ_TLS_CERT_PATH`/`WZ_TLS_KEY_PATH`-if-set behavior as above.

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
