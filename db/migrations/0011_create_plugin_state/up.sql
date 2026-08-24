-- Durable halves of the Plugin-Scoped Data Store (#149,
-- docs/PROPOSAL.md's "Plugin-Scoped Data Store"): opaque blobs a plugin
-- persists without needing its own core schema migration. `entity`
-- scope is deliberately not here at all -- it's transient/in-memory
-- only, per the design's own scope split.
CREATE TABLE plugin_character_state (
    character_id UUID NOT NULL REFERENCES characters(id) ON DELETE CASCADE,
    key TEXT NOT NULL,
    value BYTEA NOT NULL,
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (character_id, key)
);

-- zone_id is a content-manifest zone slug, not a database row -- same
-- "content-defined, not a DB foreign key" reasoning as
-- realm_zones.zone_id and characters.zone_id (docs/specs/Data_Model_Spec.md).
CREATE TABLE plugin_zone_state (
    zone_id TEXT NOT NULL,
    key TEXT NOT NULL,
    value BYTEA NOT NULL,
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (zone_id, key)
);
