-- #287: the central item catalog (docs/specs/Data_Model_Spec.md, "The
-- item catalog") — one row per `item_type` that actually exists in a
-- deployment, the single source of truth `crafting.schema.yaml`/
-- `equipment.schema.yaml`/`DropItem` all validate their own `item_type`
-- references against at load/use time. Named `item_types`, not `items`
-- (the issue's working name) — `items` is already the character
-- inventory table (`character_id`, `item_type`, `quantity`, since
-- #112/0004_add_inventory_and_currency) and Postgres tables share one
-- namespace, so reusing the name would collide with an existing,
-- unrelated table.
--
-- `tags` is an open text array, not a closed enum/lookup table — core
-- code checks for a handful of well-known tags (`craftable_output`,
-- `equippable`, `tradeable`, `drop_only`) but a dev (or a plugin, via
-- `register-item-type`) can add arbitrary tags of their own without a
-- schema change, same "no stat is privileged" philosophy the character
-- stats JSONB column already follows. `metadata` is a narrow JSONB
-- column for genuinely per-item-arbitrary data (icon ref, flavor text)
-- — never a place core logic reads a decision from.
CREATE TABLE item_types (
    item_type TEXT PRIMARY KEY,
    display_name TEXT NOT NULL,
    tags TEXT[] NOT NULL DEFAULT '{}',
    metadata JSONB NOT NULL DEFAULT '{}'::jsonb,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now()
);
