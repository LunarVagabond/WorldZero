# Chat Spec

Corresponds to the `chat` row in docs/PROPOSAL.md's Service / Crate Breakdown ("Cross-shard messaging, presence, channels") and the channel model decided in [Decision #82](https://github.com/LunarVagabond/WorldZero/issues/82).

`chat` isn't placed in any Phased Roadmap phase yet — this spec covers the crate's own data model and pub/sub delivery, not when it ships relative to other phases.

## No message persistence

A chat message is delivered to whoever is subscribed to its channel at the moment it's sent — nothing is stored. No `chat_messages` table, no scrollback. This matches how most live MMOs actually behave (you see chat from while you were logged in, not history from before). Revisit if a real, demonstrated need shows up (e.g. a companion mobile client wanting scrollback) — the schema below doesn't preclude adding history later, it's just not built now.

## Channel types

One `chat_channels` table, one `channel_type` per row:

| Type | Members | Created | Notes |
|---|---|---|---|
| `direct` | Exactly 2 (`chat_channel_members` rows) | Implicitly, on first message between two accounts that don't already share a `direct` channel | No `name` — a direct channel is identified by its members, not a label. |
| `group` | Arbitrary (`chat_channel_members` rows) | Explicitly, by a player | Has a `name`. Creator and any member can invite; any member can leave. Empty (zero-member) channels are not automatically deleted — cleanup is future work, not v0 scope. |
| `guild` | Arbitrary (`chat_channel_members` rows) | Explicitly, by a player | **Structurally identical to `group` for now.** There is no guild crate/table anywhere in the crate breakdown (docs/PROPOSAL.md, Service / Crate Breakdown) — this is *not* wired to a real guild roster. It's a distinct `channel_type` value so the distinction exists in the data model, ready to be connected to a real guild system once one exists, not a functioning guild feature today. |
| `zone` | Implicit — no `chat_channel_members` rows | Lazily, the first time a category/scope pair is needed, or up front at zone-service startup | Membership is "whichever characters currently have this `zone_id`" (`character.zone_id`, docs/specs/Data_Model_Spec.md) — tracking a membership row per player per zone channel would just duplicate data `character` already owns. `chat` itself does **not** validate that a sender is actually in the zone before publishing; that's the `gateway`/`world` integration's job (out of scope here — see "Not this pass" below). |

## `chat_channels` / `chat_channel_members` schema

```sql
CREATE TABLE chat_channels (
    id UUID PRIMARY KEY,
    channel_type TEXT NOT NULL,          -- 'direct' | 'group' | 'guild' | 'zone'
    name TEXT,                           -- required for group/guild/zone, NULL for direct
    zone_id TEXT,                        -- set only for channel_type = 'zone'
    category TEXT,                       -- set only for channel_type = 'zone' — e.g. "trade", "lfg", "local"
    created_by UUID REFERENCES accounts(id) ON DELETE SET NULL,  -- NULL for system-created zone channels
    created_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE TABLE chat_channel_members (
    channel_id UUID NOT NULL REFERENCES chat_channels(id) ON DELETE CASCADE,
    account_id UUID NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    joined_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (channel_id, account_id)
);
```

`zone` channels never get rows in `chat_channel_members` — see the table above.

## `chat.yaml`: dev-configured system channels

Same convention as `character`'s `stats.schema.yaml` and `content`'s `content-pack.yaml` — lives in the dev's config directory (`common::config::config_dir()`, docs/specs/Data_Model_Spec.md), loaded via `SystemChannelConfig::from_config_dir()`:

```yaml
schema_version: 1
system_channels:
  - category: trade
    scope: global
  - category: lfg
    scope: global
  - category: local
    scope: zone
```

| Field | Type | Notes |
|---|---|---|
| `schema_version` | integer | Same compatibility-contract pattern as the other declared-schema files. `1` for now. |
| `system_channels[].category` | string | Freeform label (`trade`, `lfg`, `local`, ...) — becomes the `chat_channels.category` value and, combined with scope, the channel's name. |
| `system_channels[].scope` | string | `"global"` — one channel for the whole deployment, `chat_channels.zone_id` is `NULL`. `"zone"` — one channel per zone, created (lazily or at startup) per `zone_id` that needs it, named after that zone. |

This is the dev-facing lever for "enable/disable/name system channels" — adding, removing, or renaming a category is a config change, not a code change.

## Redis pub/sub delivery

Every channel, regardless of type, publishes to Redis pub/sub topic `chat:<channel_id>` (the channel's own `UUID`, no type-specific naming needed since the id is always present). A message is a JSON-encoded:

```jsonc
{
  "channel_id": "...",
  "sender_account_id": "...",
  "body": "...",
  "sent_at": "2026-08-22T15:00:00Z"
}
```

Publishing and subscribing are the only two operations — no message is ever written to Postgres, per "No message persistence" above.

## Gateway demo integration

A dev-facing, demo-scoped piece of "wiring a connected client's messages into/out of these channels" (see "Not this pass" below for what's still explicitly deferred): `bin/gateway_server` and `bin/demo`, both in the `chat` crate, exercise the real `gateway` TCP+TLS transport end to end rather than talking to Postgres/Redis in-process.

- **Wire protocol** (`chat::gateway_protocol`): a fixed, closed set of JSON-encoded messages carried in the envelope's `message_type` 100 (see docs/specs/Networking_Spec.md's catalog note) — `Join`/`Leave`/`Send` client→server, `Joined`/`Left`/`Chat`/`Error` server→client. This is chat's own protocol, not a generic mechanism other message types share, and it carries no identity of its own — see below.
- **Identity comes from a real login first**, not from anything chat-owned: a connection's first envelope must be `auth::gateway_protocol`'s login/registration handshake (docs/specs/Auth_Spec.md, "Gateway handshake") — real Argon2id-hashed passwords, real Postgres accounts, a real Redis-backed session issued on success. Only after that does `bin/gateway_server` accept any `chat::gateway_protocol` message on the connection, trusting the `account_id` the handshake produced.
- **`bin/gateway_server`** terminates the gateway's TCP+TLS listener and, per connection: runs the auth handshake above, then on each `Join` finds-or-creates a `group` channel by name (`chat::demo_support`), joins it, and spawns a task forwarding that channel's pub/sub traffic back over the connection as `Chat` messages.
- **`bin/demo`** defaults to this gateway-routed mode (`--password <pw>`, `--register` on first use); `--no-gateway` switches back to talking to `ChannelStore`/`ChatBus` directly (no TCP/TLS, no auth — the old `chat-demo-<name>` stable-identity shortcut, see `chat::demo_support`). Either way it supports `/join`, `/leave`, and `/switch` for hopping between channels interactively.
- **Explicitly out of scope even for this demo integration:** zone-channel membership validation; rate limiting/moderation. Still tracked below.

## Not this pass

Explicitly out of scope for the current implementation, to keep in mind rather than lose track of:

- **The real, production version of wiring a connected client's messages into/out of these channels** — a demo-scoped version now exists (see "Gateway demo integration" above), including real authenticated identity via `auth`'s gateway handshake. What's still not built: figuring out which channel(s) an authenticated client is actually allowed to publish to (right now any authenticated account can join/send to any named `group` channel), and validating zone membership for `zone` channels.
- **A real guild system** — `guild`-type channels exist in the data model but aren't backed by any actual guild roster/permissions system yet (see the `guild` row above).
- **Rate limiting / moderation / profanity filtering** — not designed at all yet.
- **Presence** — the crate's PROPOSAL.md description mentions "presence" alongside channels, but that's a distinct concern (who's online, not what channel they're in) not covered by this spec.
- **Message history/persistence** — explicitly deferred, see "No message persistence" above.
