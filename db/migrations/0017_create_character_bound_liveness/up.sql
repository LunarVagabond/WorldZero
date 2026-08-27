-- Bound-realm connection liveness (#169): parallel to character_sessions
-- (docs/specs/Realm_Character_Policy_Spec.md, "Open realms: concurrency
-- control") but for the case that table explicitly never covers — a
-- bound-realm character has exactly one realm that could ever claim it,
-- so there's no lease contention to arbitrate, only "is this character
-- connected right now." At most one row per currently-connected
-- bound-realm character.
CREATE TABLE character_bound_liveness (
    character_id UUID PRIMARY KEY REFERENCES characters(id) ON DELETE CASCADE,
    realm_id UUID NOT NULL REFERENCES realms(id) ON DELETE CASCADE,
    connected_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    expires_at TIMESTAMPTZ NOT NULL
);

CREATE INDEX character_bound_liveness_expires_at_idx ON character_bound_liveness (expires_at);
