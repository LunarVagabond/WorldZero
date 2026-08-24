CREATE TABLE realms (
    id UUID PRIMARY KEY,
    name TEXT NOT NULL,
    open_or_bound TEXT NOT NULL CHECK (open_or_bound IN ('open', 'bound')),
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

-- Which zone-service instance(s) (content manifest zone `id` slugs, not a
-- DB foreign key — same "content-defined, not a database row" reasoning
-- as characters.zone_id, docs/specs/Data_Model_Spec.md) belong to which
-- realm. zone_id is the primary key, not (realm_id, zone_id): a
-- zone-service instance belongs to at most one realm at a time.
CREATE TABLE realm_zones (
    zone_id TEXT PRIMARY KEY,
    realm_id UUID NOT NULL REFERENCES realms(id) ON DELETE CASCADE
);

CREATE INDEX realm_zones_realm_id_idx ON realm_zones (realm_id);
