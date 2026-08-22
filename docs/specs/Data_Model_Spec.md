# Data Model Spec

Corresponds to [Data Model Extensibility: Declared Attribute Schemas](../PROPOSAL.md#data-model-extensibility-declared-attribute-schemas) in the proposal.

## `characters` table: fixed core columns

The framework-required columns — never a superset or subset per deployment — plus the one flexible `stats` column:

| Column | Type | Notes |
|---|---|---|
| `id` | `UUID PRIMARY KEY` | Identity — `CharacterId`. |
| `account_id` | `UUID NOT NULL REFERENCES accounts(id) ON DELETE CASCADE` | Account linkage. A real foreign key, not a loose reference — deleting an account takes its characters with it. |
| `name` | `TEXT NOT NULL` | Identity — display name. No uniqueness constraint at this layer; whether names must be globally unique is a per-game policy concern, not a core one. |
| `realm_id` | `UUID NOT NULL` | Realm. Not yet a foreign key — there is no `realms` table until `realm-directory`'s registry (#47) exists. Revisit then. |
| `zone_id` | `TEXT NOT NULL` | Which zone the character is currently in, by the content manifest's zone `id` slug (docs/specs/Content_Manifest_Spec.md) — not a DB foreign key, since zones are content-defined, not database rows. |
| `position_x`, `position_y`, `position_z` | `DOUBLE PRECISION NOT NULL DEFAULT 0` | Position, in the zone's own coordinate system. |
| `stats` | `JSONB NOT NULL DEFAULT '{}'` | The declared-attribute-schema column — see below. |
| `created_at`, `updated_at` | `TIMESTAMPTZ NOT NULL DEFAULT now()` | Timestamps. |

No column exists for any specific stat (no `hp`, no `mana`) — that would violate "no stat is ever privileged by the core." A GIN index on `stats` makes it queryable/indexable via native JSONB operators per the proposal's rationale.

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
