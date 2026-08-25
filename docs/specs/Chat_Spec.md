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
| `group` | Arbitrary (`chat_channel_members` rows) | Explicitly, by a player | Has a `name`. Creator and any member can invite; any member can leave. Empty (zero-member) channels are not automatically deleted — cleanup is future work, not v0 scope. **Not the same thing as a real party (#178, see "Party/group system" below)** — this is an open-membership named chat room; a party's *own* chat channel, if a game wants one, is a consequence of party membership, layered on top of this table, not built here. |
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

**Also wired into the combined `server` process** ([#104](https://github.com/LunarVagabond/WorldZero/issues/104), building on the runtime-toggle mechanism from [#91](https://github.com/LunarVagabond/WorldZero/issues/91)/[#92](https://github.com/LunarVagabond/WorldZero/issues/92)): `server`'s own per-connection session loop (`crates/server/src/session.rs`, `crates/server/src/chat_session.rs`) dispatches the same `chat::gateway_protocol` messages described below over the same authenticated connection that carries world/plugin traffic, gated behind `WZ_SERVICE_CHAT_ENABLED` (default on — set `false` to disable; a disabled server responds to any `message_type` 100 envelope with a clear "chat is disabled on this server" error rather than silently dropping it). `bin/gateway_server` isn't replaced by this — it's still a standalone way to run/exercise chat on its own — but a self-hosted deployment no longer needs a second process just to get chat.

- **Wire protocol** (`chat::gateway_protocol`): a fixed, closed set of JSON-encoded messages carried in the envelope's `message_type` 100 (see docs/specs/Networking_Spec.md's catalog note) — `Join`/`Leave`/`Send` client→server, `Joined`/`Left`/`Chat`/`Error` server→client. This is chat's own protocol, not a generic mechanism other message types share, and it carries no identity of its own — see below.
- **Identity comes from a real login first**, not from anything chat-owned: a connection's first envelope must be `auth::gateway_protocol`'s login/registration handshake (docs/specs/Auth_Spec.md, "Gateway handshake") — real Argon2id-hashed passwords, real Postgres accounts, a real Redis-backed session issued on success. Only after that does `bin/gateway_server` accept any `chat::gateway_protocol` message on the connection, trusting the `account_id` the handshake produced.
- **`bin/gateway_server`** terminates the gateway's TCP+TLS listener and, per connection: runs the auth handshake above, then on each `Join` finds-or-creates a `group` channel by name (`chat::demo_support`), joins it, and spawns a task forwarding that channel's pub/sub traffic back over the connection as `Chat` messages.
- **`bin/demo`** defaults to this gateway-routed mode (`--password <pw>`, `--register` on first use); `--no-gateway` switches back to talking to `ChannelStore`/`ChatBus` directly (no TCP/TLS, no auth — the old `chat-demo-<name>` stable-identity shortcut, see `chat::demo_support`). Either way it supports `/join`, `/leave`, and `/switch` for hopping between channels interactively.
- **Explicitly out of scope even for this demo integration:** zone-channel membership validation; rate limiting/moderation. Still tracked below.

## Party/group system ([#178](https://github.com/LunarVagabond/WorldZero/issues/178))

A real party/group system — invite, accept/decline, membership, leave/disband — lands as a **core `server`/`character` feature**, not `chat`'s `group` channel type and not a starter-game-only (#147) mechanism. Two reasons for landing it as core rather than either alternative:

- **Not `chat`'s job.** A party's *chat channel*, if a game wants one, is one consequence of party membership — not the party itself. Building the real thing on top of `group` channels would conflate "an open-membership named chat room" with "a small, exclusive, game-mechanically-meaningful roster," which are different concerns with different invariants (a party caps at some size and a character is in at most one at a time; a `group` channel does neither).
- **Not starter-game-only.** Party-aware live placement (#142's `ZoneRegistry::join_layer_of`, "who ends up on the same zone layer as whom") is exactly the kind of cross-cutting, netcode-adjacent mechanism this project's core is supposed to own — the same reasoning that put movement validation and zone transitions in core rather than leaving every game to reimplement them. A game-specific *policy* on top (who's allowed to invite whom, whether a game wants roles/loot rules) stays out of scope here, same as core movement validation doesn't decide who's allowed to fight whom.

**Storage: `character::party::PartyStore`** (`parties`/`party_members` tables, `db/migrations/0012_create_parties/`) — a small, durable roster of `CharacterId`s (not `AccountId`s, matching #142's reconnect-placement logic, which already keys group state off the specific character). `party_members.character_id` is `UNIQUE`: a character is in at most one party at a time, enforced at the storage layer. Leaving a party that would drop below two members dissolves it entirely — there's no such thing as a one-member party.

**Party size is dev-declared, not hardcoded** — `character::party_schema::PartySchema` (`party.schema.yaml`, see `config/party.schema.example.yaml`), the same "core enforces generically, dev declares the actual numbers/names" pattern `stats.schema.yaml`/`AttributeSchema` already uses for character stats (docs/specs/Data_Model_Spec.md). A game declares as many named party types as it wants — a 5-man "normal" party, a 3-man "rush" group, an uncapped "raid" (omit `max_members` entirely for no cap) — and a party is founded under whichever type its founding invite named (or the schema's first declared entry if it named none); that type, and its cap, is fixed for the party's whole life. `PartyStore::accept_invite` enforces the cap before a new member is added, returning a clear error once a party is full rather than a raw constraint violation.

**Wire protocol** (`session.proto`, `message_type` 200): `PartyInvite { target_entity_id, party_type }` (client), `PartyInviteResponse { accept }` (client), `PartyLeave {}` (client), `PartyInviteReceived { from_entity_id }` (server), `PartyInviteDeclined { by_entity_id }` (server), `PartyUpdate { members }` (server, sent to every currently-online affected member after any membership change — accept, leave, or dissolve — with that recipient's own current roster, empty meaning "no party"). `target_entity_id`/`other_entity_id` are always live entity ids (same "opaque id" discipline as every other message in this protocol), reachable process-wide via `server::session::global_sessions` regardless of which zone the target is actually in — an invite doesn't require being in the same zone as the invitee, since parties are expected to span zones.

**Composes with #142, for real, not a stand-in.** Accepting an invite calls the exact same placement mechanism `JoinGroupLayer` uses (`server::session::perform_group_layer_move`, itself built on `ZoneRegistry::join_layer_of` and the `spawn_into_layer`/`despawn_from_layer` pair shared with a real zone-link crossing) — if the inviter and the newly-accepted member are already in the same zone, the accepter lands on the inviter's live layer as a direct side effect of accepting, no separate step required. `JoinGroupLayer` itself is now real party-aware: it checks `PartyStore::members_of` before performing a move, rejecting a target that isn't actually a fellow party member (a check #142 deliberately deferred to "whichever ticket builds the real group system" — this is that ticket). Reconnect placement (`server::session::handle_session`'s login path) queries real `PartyStore::members_of` too, trying every currently-online party member in turn (not just one, unlike #142's original pairwise placeholder) until one resolves to a layer in the zone being joined.

## Not this pass

Explicitly out of scope for the current implementation, to keep in mind rather than lose track of:

- **The real, production version of wiring a connected client's messages into/out of these channels** — a demo-scoped version now exists (see "Gateway demo integration" above), including real authenticated identity via `auth`'s gateway handshake. What's still not built: figuring out which channel(s) an authenticated client is actually allowed to publish to (right now any authenticated account can join/send to any named `group` channel), and validating zone membership for `zone` channels.
- **A real guild system** — `guild`-type channels exist in the data model but aren't backed by any actual guild roster/permissions system yet (see the `guild` row above). Tracked as [#179](https://github.com/LunarVagabond/WorldZero/issues/179); expected to follow the same "core `character` storage, dev-declared limits" shape #178's party system just established, not `chat`'s `group`/`guild` channel types.
- **Rate limiting / moderation / profanity filtering** — not designed at all yet.
- **Presence** — the crate's PROPOSAL.md description mentions "presence" alongside channels, but that's a distinct concern (who's online, not what channel they're in) not covered by this spec.
- **Message history/persistence** — explicitly deferred, see "No message persistence" above.
