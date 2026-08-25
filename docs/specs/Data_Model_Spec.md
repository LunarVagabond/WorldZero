# Data Model Spec

Corresponds to [Data Model Extensibility: Declared Attribute Schemas](../PROPOSAL.md#data-model-extensibility-declared-attribute-schemas) in the proposal.

## `characters` table: fixed core columns

The framework-required columns — never a superset or subset per deployment — plus the one flexible `stats` column:

| Column | Type | Notes |
|---|---|---|
| `id` | `UUID PRIMARY KEY` | Identity — `CharacterId`. |
| `account_id` | `UUID NOT NULL REFERENCES accounts(id) ON DELETE CASCADE` | Account linkage. A real foreign key, not a loose reference — deleting an account takes its characters with it. |
| `name` | `TEXT NOT NULL` | Identity — display name. No uniqueness constraint at this layer; whether names must be globally unique is a per-game policy concern, not a core one. |
| `realm_id` | `UUID NOT NULL REFERENCES realms(id)` | Realm. A real foreign key as of #170 (`db/migrations/0013_add_character_realm_fk/`) — no `ON DELETE` behavior specified (defaults to `NO ACTION`/`RESTRICT`), so deleting a realm that still has characters pointing at it is a hard error, not a silent cascade that deletes player data. |
| `zone_id` | `TEXT NOT NULL` | Which zone the character is currently in, by the content manifest's zone `id` slug (docs/specs/Content_Manifest_Spec.md) — not a DB foreign key, since zones are content-defined, not database rows. |
| `position_x`, `position_y`, `position_z` | `DOUBLE PRECISION NOT NULL DEFAULT 0` | Position, in the zone's own coordinate system. |
| `stats` | `JSONB NOT NULL DEFAULT '{}'` | The declared-attribute-schema column — see below. |
| `currency_balance` | `BIGINT NOT NULL DEFAULT 0 CHECK (currency_balance >= 0)` | A single currency balance (#112) — see "Currency: one balance, not a ledger table" below. |
| `created_at`, `updated_at` | `TIMESTAMPTZ NOT NULL DEFAULT now()` | Timestamps. |

No column exists for any specific stat (no `hp`, no `mana`) — that would violate "no stat is ever privileged by the core." A GIN index on `stats` makes it queryable/indexable via native JSONB operators per the proposal's rationale.

## Character list/create/select (#193)

Phase 1's login path was fully automatic and silent: an account got exactly one character per realm, auto-created on first connect using the account's username as the character name, with no client input. `character::CharacterStore::find_by_account`'s "just pick the most recent" behavior (its own doc comment already flagged this as a placeholder) is no longer what determines which character a session uses — `server::character_protocol` (`message_type` 3, docs/specs/Networking_Spec.md's catalog note) replaces it with a real client-driven flow, mandatory right after realm selection (#192) and before world-join:

- **`ListCharacters`** → **`CharacterList`**: every character the account owns that's reachable from the already-selected realm — `realm-directory::LoginPolicy::list_characters` (`crates/realm-directory/src/login_policy.rs`) picks the scoping the same way `resolve_character` (#52) already does for the single-character case: `character::CharacterStore::list_by_account` for a bound realm (never crosses into another realm), `list_by_account_in_open_realms` for an open one (spans the whole open-realm group).
- **`CreateCharacter { name }`** → **`CharacterCreated { character_id }`** or an `Error`: reserves a new character. Rejected once the account already owns `WZ_CHARACTER_MAX_PER_ACCOUNT` characters on this realm (default `5`) — a `server`-side policy value (`crates/server/src/main.rs`), not enforced inside `character::CharacterStore::create` itself, which stays a raw insert with no policy opinion of its own. A rejected creation doesn't close the connection; the client can still select one of its existing characters.
- **`SelectCharacter { character_id }`** → **`CharacterSelected { character_id }`** or an `Error`: must name a character `character::CharacterStore::get_for_account` confirms is actually owned by this account (ownership-checked at the query itself, not a separate check after an unscoped lookup). A successful selection is what feeds `realm-directory::LoginPolicy::authorize_login` (#51/#136) — the same bound-realm-mismatch/open-realm-lease enforcement #136 already does, just triggered by this explicit choice instead of an automatic one. Unlike a rejected realm selection (#192), a rejected character selection (e.g. this specific character already leased elsewhere on an open realm) does **not** close the connection — the account may own other, uncontended characters worth trying instead.

**Deliberately not part of this protocol:** class/race/archetype selection or any other starting-stat decision — per this project's "no game-specific concept is privileged by the core" design principle, a hardcoded class/race enum in core would be wrong. `CreateCharacter` is strictly "reserve a name, nothing else"; a character-creation plugin hook (a separate, later ticket) is the intended extension point for archetype/preset choices and setting starting stats, and this protocol's shape — `CreateCharacter` and `SelectCharacter` as two distinct steps, not one combined "create and spawn" call — leaves room for that hook to run in between without a redesign.

## `items` table: one row per owned item-type stack

```sql
CREATE TABLE items (
    id UUID PRIMARY KEY,
    character_id UUID NOT NULL REFERENCES characters(id) ON DELETE CASCADE,
    item_type TEXT NOT NULL,
    quantity BIGINT NOT NULL CHECK (quantity > 0),
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE (character_id, item_type)
);
```

Same "fixed core schema, framework never interprets the meaning" discipline as `stats` — the framework knows an `item_type` string and a `quantity` exist; what an item *type* actually does (a potion heals, a key unlocks a door) is entirely plugin-owned, never core logic. `item_type` is an opaque string the framework doesn't parse, the same pattern `content::manifest::SpawnTable`'s `entity_type` already uses (e.g. `"npc.wolf"`).

**One row per `(character_id, item_type)`, not one row per physical item.** A character's 5 torches are one row with `quantity = 5`, not 5 rows — the `UNIQUE` constraint enforces this. `character::CharacterStore::grant_item` upserts (creates the stack or adds to it); `remove_item` subtracts and deletes the row outright once it reaches zero, rather than leaving a `quantity = 0` row around. Removing more than a character owns is rejected, not silently clamped to zero — same "reject before it reaches storage" discipline as an out-of-bounds stat write.

**Not slot-based.** This table has no notion of inventory "slots," equipment positions, or item instances with individual properties (durability, enchantments, a unique instance id) — that's explicitly out of scope for the core (per #112's "out of scope": item *effects* stay plugin-owned) and, if a game needs per-instance item state, is exactly what the plugin-scoped data store (`docs/PROPOSAL.md`'s "Plugin-Scoped Data Store") is for, keyed by a plugin-chosen id.

**Capacity is enforced but configurable.** `character::inventory::InventoryConfig::max_distinct_item_types` (default 40, override via `WZ_INVENTORY_MAX_ITEM_TYPES`) caps the number of *distinct* `item_type` stacks a character can hold — granting more of an already-owned type is never blocked by this, only a brand-new stack is. This is a soft, configurable UX limit (the classic "N inventory slots" game mechanic), not a hard architectural ceiling — same "solid default everywhere, never a wall for the dev" spirit as every other configurable bound in this crate, and consistent with `AttributeSchema`'s dev-declared per-stat bounds. Enforced with a plain read-then-write count check, not a transaction — acceptable because it's a soft limit, not a data-integrity boundary (see the module doc on `character::inventory` for the full reasoning).

## `realms`/`realm_zones` tables: the realm registry (#47)

```sql
CREATE TABLE realms (
    id UUID PRIMARY KEY,
    name TEXT NOT NULL,
    open_or_bound TEXT NOT NULL CHECK (open_or_bound IN ('open', 'bound')),
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE TABLE realm_zones (
    zone_id TEXT PRIMARY KEY,
    realm_id UUID NOT NULL REFERENCES realms(id) ON DELETE CASCADE
);
```

`realm-directory::RealmStore` (`crates/realm-directory/src/store.rs`) is the one write path — realm CRUD (`create`/`get`/`list`/`update`/`delete`) plus zone-to-realm tracking (`assign_zone`/`unassign_zone`/`zones_for_realm`/`realm_for_zone`). `open_or_bound` is carried on every realm from the moment this registry exists, per docs/specs/Realm_Character_Policy_Spec.md's "The flag" — no per-realm-group column added later, even though enforcement of the flag is #51, not this table.

**`realm_zones.zone_id` is its own primary key, not `(realm_id, zone_id)`** — a zone-service instance (identified the same way everywhere else in this codebase, `content::manifest::ZoneManifest.id`, a content-defined slug, never a DB foreign key) belongs to at most one realm at a time. Reassigning an already-assigned zone moves it (`ON CONFLICT (zone_id) DO UPDATE`) rather than erroring or creating a second mapping.

**Wired into `server`'s combined process (#136).** `server::main` resolves `WZ_REALM_ID` against `RealmStore::get` at startup — a real realm, not Phase 1's `placeholder_realm_id()` (removed) — and every login on that process goes through #51's `realm-directory::LoginPolicy` (below) before the connection is allowed to join the world. A process serving more than one realm at once is #130's job, not this one's; today's `server` process serves exactly the one realm `WZ_REALM_ID` names. Managing realms (creating one to point `WZ_REALM_ID` at, assigning zones to it) is still `make realm ARGS="..."` (a small CLI over `RealmStore`, `crates/realm-directory/src/bin/realm.rs`) — see docs/specs/Realm_Character_Policy_Spec.md's "Managing realms today" for full usage; there's no in-game or admin-API flow for this yet.

## `character_sessions` table: the open-realm concurrency lease (#21, enforced by #51)

```sql
CREATE TABLE character_sessions (
    character_id UUID PRIMARY KEY REFERENCES characters(id) ON DELETE CASCADE,
    realm_id UUID NOT NULL REFERENCES realms(id) ON DELETE CASCADE,
    zone_service_id TEXT NOT NULL,
    leased_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    expires_at TIMESTAMPTZ NOT NULL
);
```

`character::CharacterSessionLease` (`crates/character/src/session_lease.rs`) is the one write path — `acquire`/`renew`/`release`, per docs/specs/Realm_Character_Policy_Spec.md's "Open realms: concurrency control". At most one row per currently-online character; a second zone-service instance can't acquire an unexpired lease, and letting a lease's `expires_at` lapse (a crash, not a clean disconnect) is itself the failure-detection mechanism — no separate liveness check. Bound realms never write to this table at all, since the split-brain scenario it exists to prevent can't occur there — see the spec's "Bound realms" section.

`realm-directory::LoginPolicy` (`crates/realm-directory/src/login_policy.rs`) is #51's single enforcement point: given a character's home realm and a target realm, it rejects a bound-realm mismatch outright, and for an open realm, acquires this lease as part of the same authorization call.

`LoginPolicy::resolve_character` is #52's counterpart on the read side: finding *which* character a login resolves to, using `CharacterStore::find_by_account` (realm-scoped) for a bound target realm or `CharacterStore::find_by_account_in_open_realms` (a join against `realms` matching any `open` realm) for an open one — so an open-realm character is found regardless of which specific open realm it was created on, and a bound-realm character never leaks into an open lookup. No caching layer sits in front of either query, so a write made through one realm is immediately visible through resolution via any other.

## Currency: one balance, not a ledger table

`characters.currency_balance` is a single `BIGINT`, not a `currencies` table or a per-currency ledger — deliberately, for v0: no game requirement for multiple named currencies (gold *and* gems *and* faction tokens, each independently tracked) has driven this yet, and a single balance is the overwhelmingly common case. `character::CharacterStore::modify_currency` applies a signed delta and rejects (storage untouched) any change that would take the balance negative — the same invariant the column's own `CHECK (currency_balance >= 0)` enforces at the database level, checked in `character` first so a caller gets a clear crate error instead of a raw constraint-violation from `sqlx`.

**This is additive, not a wall.** If a real game needs multiple independently-tracked currencies later, that's a new `character_currency(character_id, currency_key, balance)` table added alongside this column — not a breaking redesign of it. `currency_balance` stays as the default/primary balance either way, the same way adding a new declared stat key never requires a migration for existing `stats` data.

## `stats.schema.yaml` format

A game developer's declared attribute schema — not DDL, just the list of valid keys for their game:

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

- `schema_version` — the schema format's own compatibility contract, same pattern as the content manifest's `schema_version` (docs/specs/Content_Manifest_Spec.md). `1` for now.
- `key` — any string, including dotted namespacing (`reputation.ironclad_guild`) — the framework doesn't parse structure into it, it's just a JSONB object key.
- `type` — **`int` only for v0.** The proposal's own example never uses anything else, and there's no real game requirement driving more types yet. Revisit trigger: an actual game built on the framework needs a non-integer stat (float, bool, string) badly enough to justify it — don't speculatively add types nobody's asked for.
- `default` — required. Returned on read when a character's stored `stats` blob is missing this key (see "Missing-key read behavior" below).
- `min`, `max` — optional. When present, a write outside `[min, max]` is rejected. Absent means unbounded in that direction (see `reputation.ironclad_guild` above, which has neither).

**Where this file lives:** a dev drops `stats.schema.yaml` into their config directory — `common::config::config_dir()` (`WZ_CONFIG_DIR` env var, or `./config` by default) — and loads it with `AttributeSchema::from_config_dir()`. No digging through crate source paths; every dev-provided file this framework expects lives in that one directory, under a filename each crate defines for itself.

## Validation at the API boundary

Every stat read/write goes through `character`'s API, validated against the schema **before** touching the `stats` JSONB column — never a direct write from anywhere else (`world`, a plugin host function, etc. all go through this same boundary).

- **Write, key not in the declared schema:** rejected with `common::Error` naming the crate (`character`) and the unrecognized key. The write never reaches storage.
- **Write, value outside `[min, max]`:** rejected with `common::Error` naming the key and which bound it violated. The write never reaches storage.
- **Read, key missing from the stored `stats` blob:** returns the schema's declared `default` — not `null`, not an error. This is what makes adding a new stat key to a live game a zero-migration change (proposal, "Evolving the schema over time"): every existing character implicitly already has the new key, at its default, until it's written.
- **Read, key not in the declared schema:** rejected the same way an invalid write is — there's nothing sensible to return for a key the game never declared.

## Realm population reporting (#137)

Two independent numbers, both exposed as plain counts — no `low`/`med`/`high` bucketing computed by the core, that's left to whoever displays them:

- **Character census** — `character::CharacterStore::count_for_realm` (`crates/character/src/store.rs`), a plain `COUNT(*)` against `characters.realm_id`. Durable, Postgres-backed.
- **Live connections** — `realm-directory::RealmPresence` (`crates/realm-directory/src/population.rs`), a Redis sorted set per realm (`realm:<realm_id>:connections`, member = a per-connection id, score = expiry). `connect`/`disconnect` are the write path; re-calling `connect` for the same connection id is also the heartbeat/renewal call. `count` prunes expired members before counting, so a crashed connection (never cleanly disconnected) self-heals out of the count the next time anything asks — same TTL-expiry discipline as `character::CharacterSessionLease` (#21), and a deliberately separate mechanism from it: `character_sessions` only ever holds *open*-realm rows, but a live-connection count needs to work for bound realms too.

`RealmPresence::population` combines both into one `RealmPopulation { character_count, live_connections }` for callers that want both numbers together. Real and tested, still not wired into `server` (nothing calls `connect`/`disconnect` from a real connection lifecycle yet) — unlike `RealmStore`/`LoginPolicy` above (#136), this one has no caller until the realm-list wire message that will actually display these numbers exists (#192).

## NPCs and items: same pattern, separate files

The proposal is explicit that this same declared-schema mechanism is the intended answer for NPC and item properties too, not just character stats. Each entity type gets **its own schema file** (`stats.schema.yaml` for characters, and the equivalent — e.g. `npc_stats.schema.yaml`, `item_stats.schema.yaml` — for the others) rather than one shared file covering every entity type. Different entity types genuinely have different stat sets (a sword has no HP), so one shared file would either bloat into an every-entity union or need per-entity-type sections reinventing the same problem. `character` implements the loader/validator for its own file now; `world`'s NPC/item work (later phases) is expected to reuse the identical pattern, not invent a second one.

**NPC-targetable stats (#197) reuses `stats.schema.yaml` itself, not a separate `npc_stats.schema.yaml`, as a deliberate v0 scope call** — not the eventual shape this section describes above. #197 needed NPC combat targets (an NPC with real, validated HP) to work at all; introducing a second schema file, its own loader, and a story for what happens when a key means different things in two files was a larger problem than that ticket needed to solve, and would have been designed blind (no second real entity-type schema exists yet to design *against*). A plugin declaring an NPC-only stat with no character-facing meaning (`aggro-range`, say) still has to route it through the character schema today — a real limitation, not a hidden one. Revisit with a real `npc_stats.schema.yaml` once an actual NPC-only stat need shows up, rather than building the general mechanism speculatively now.

## NPC stat storage: `npc_stats`, in-memory only (#197)

Distinct problem from the schema-file question above: *where a value lives*, not *which schema validates it*. A `characters` row's `stats` column has an obvious durable home (the character persists across sessions/restarts); an NPC entity does not — its entity id is generated fresh every time `world_actor::spawn_npc_from_table` spawns it from a manifest-declared spawn table, never stable across a zone-service restart. There is nothing meaningful to durably key a stored NPC stat against, and a restarted server respawns its NPCs at their schema-declared defaults either way (the manifest is the source of truth for what an NPC starts as, not a database row).

So `server::session::NpcStats` (`Arc<Mutex<HashMap<EntityId, HashMap<String, i64>>>>`) is **in-memory only, process-wide, never written to Postgres** — populated lazily on an NPC's first stat write, validated against the same `character::AttributeSchema` real character stats use (see above), and cleared when the entity despawns (`WorldCommand::Despawn`). No new table, no migration. If a future ticket introduces an NPC that genuinely needs to survive a restart with its stats intact (a persistent world boss with a multi-day HP pool, say), that's a different, larger problem — a stable id for that specific NPC, and a real durability story for it — not something this in-memory map should grow into by accident.

See docs/specs/Plugin_API.md's "NPC-targetable stats" for how `apply-stat-delta` resolves against this storage vs. a player's `characters` row.

## `parties`/`party_members` tables: real party/group formation (#178)

A small, durable roster of `characters.id`s (not accounts) — `parties(id, party_type, created_at)` plus `party_members(party_id, character_id, joined_at)`, `db/migrations/0012_create_parties/`. `party_members.character_id` is `UNIQUE`: a character is in at most one party at a time, enforced at the storage layer rather than left to application-level discipline. `parties.party_type` names one of the dev-declared entries in `party.schema.yaml` (`character::party_schema::PartySchema`) — set once at formation from whichever type the founding invite named, immutable afterward, and used to enforce that type's declared `max_members` cap (or no cap at all, if the dev omitted one) on every later invite accepted into that party. Full design, wire protocol, and how this composes with #142's live layer placement: docs/specs/Chat_Spec.md's "Party/group system".
