-- Open-realm concurrency lease (docs/specs/Realm_Character_Policy_Spec.md,
-- "Open realms: concurrency control"): at most one row per currently-online
-- character, held by whichever zone-service instance is currently
-- authoritative for it. Bound realms never use this table — the split-brain
-- scenario it exists to prevent can't occur there.
CREATE TABLE character_sessions (
    character_id UUID PRIMARY KEY REFERENCES characters(id) ON DELETE CASCADE,
    realm_id UUID NOT NULL REFERENCES realms(id) ON DELETE CASCADE,
    zone_service_id TEXT NOT NULL,
    leased_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    expires_at TIMESTAMPTZ NOT NULL
);

CREATE INDEX character_sessions_expires_at_idx ON character_sessions (expires_at);
