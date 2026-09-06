-- #287: the item <-> spawn-table many-to-many ("where does this item
-- drop from") — a join table rather than embedding drop info on either
-- side, since one item can drop from many spawn tables and one spawn
-- table can drop many items. `zone_id`/`spawn_table_id` are plain,
-- unvalidated-by-FK strings, same as every other cross-reference into
-- `zone.manifest.yaml` content in this codebase (e.g.
-- `realm_zones.zone_id`, #47) — zone manifests are files, not database
-- rows, so there's no table to foreign-key against.
CREATE TABLE item_drop_sources (
    item_type TEXT NOT NULL REFERENCES item_types(item_type) ON DELETE CASCADE,
    zone_id TEXT NOT NULL,
    spawn_table_id TEXT NOT NULL,
    PRIMARY KEY (item_type, zone_id, spawn_table_id)
);

CREATE INDEX item_drop_sources_zone_spawn_table_idx
    ON item_drop_sources (zone_id, spawn_table_id);

-- Seeds the shipped example zone's spawn table (`config/zone.manifest.yaml`'s
-- `wolf-pack-01` in `greenwood-forest`) as a real drop source for
-- `wolf-fang` — the crafting example (`config/crafting.schema.yaml`'s
-- `wolf-fang-dagger` recipe) already consumes it, so this is the
-- "where does this item drop from" answer for that same item, not an
-- arbitrary example. Requires `wolf-fang` to already exist in
-- `item_types` — see 0022_seed_example_item_catalog.
