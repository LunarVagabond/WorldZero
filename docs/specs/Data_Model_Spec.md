# Data Model Spec

Corresponds to [Data Model Extensibility: Declared Attribute Schemas](../PROPOSAL.md#data-model-extensibility-declared-attribute-schemas) in the proposal.

## `characters` table: fixed core columns

The framework-required columns — never a superset or subset per deployment — plus the one flexible `stats` column:

| Column | Type | Notes |
|---|---|---|
| `id` | `UUID PRIMARY KEY` | Identity — `CharacterId`. |
| `account_id` | `UUID NOT NULL REFERENCES accounts(id) ON DELETE CASCADE` | Account linkage. A real foreign key, not a loose reference — deleting an account takes its characters with it. |
| `name` | `TEXT NOT NULL` | Identity — display name. No uniqueness constraint at this layer; whether names must be globally unique is a per-game policy concern, not a core one. |
| `realm_id` | `UUID NOT NULL` | Realm. Still not a foreign key even though `realms` (below) now exists via #47's registry — retrofitting it needs every existing `character`-crate test fixture that constructs an ad hoc `RealmId::new()` to first create a real realm row via `realm-directory::RealmStore`, which is real but separate cleanup, not part of #47's scope. Tracked as a known gap, not silently glossed over. |
| `zone_id` | `TEXT NOT NULL` | Which zone the character is currently in, by the content manifest's zone `id` slug (docs/specs/Content_Manifest_Spec.md) — not a DB foreign key, since zones are content-defined, not database rows. |
| `position_x`, `position_y`, `position_z` | `DOUBLE PRECISION NOT NULL DEFAULT 0` | Position, in the zone's own coordinate system. |
| `stats` | `JSONB NOT NULL DEFAULT '{}'` | The declared-attribute-schema column — see below. |
| `currency_balance` | `BIGINT NOT NULL DEFAULT 0 CHECK (currency_balance >= 0)` | A single currency balance (#112) — see "Currency: one balance, not a ledger table" below. |
| `created_at`, `updated_at` | `TIMESTAMPTZ NOT NULL DEFAULT now()` | Timestamps. |

No column exists for any specific stat (no `hp`, no `mana`) — that would violate "no stat is ever privileged by the core." A GIN index on `stats` makes it queryable/indexable via native JSONB operators per the proposal's rationale.

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

**Not wired into `server`'s combined process yet.** This registry is real and tested, but nothing in `server::main` reads from it — Phase 1's `placeholder_realm_id()` (a nil UUID) is still what every character gets. Consuming this for real (resolving a connection's realm at login, enforcing `open_or_bound`) is #50 (dynamic layer assignment) and #51 (open/bound enforcement)'s job, not this one. Until then, managing realms is `make realm ARGS="..."` (a small CLI over `RealmStore`, `crates/realm-directory/src/bin/realm.rs`) — see docs/specs/Realm_Character_Policy_Spec.md's "Managing realms today" for full usage.

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

## NPCs and items: same pattern, separate files

The proposal is explicit that this same declared-schema mechanism is the intended answer for NPC and item properties too, not just character stats. Each entity type gets **its own schema file** (`stats.schema.yaml` for characters, and the equivalent — e.g. `npc_stats.schema.yaml`, `item_stats.schema.yaml` — for the others) rather than one shared file covering every entity type. Different entity types genuinely have different stat sets (a sword has no HP), so one shared file would either bloat into an every-entity union or need per-entity-type sections reinventing the same problem. `character` implements the loader/validator for its own file now; `world`'s NPC/item work (later phases) is expected to reuse the identical pattern, not invent a second one.
