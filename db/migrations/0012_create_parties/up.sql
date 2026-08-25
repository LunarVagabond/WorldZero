-- Real party/group formation (#178) — a small roster of characters, not
-- accounts (matches #142's reconnect-placement logic, which already
-- keys group state off the specific character rather than the account).
-- `party_members.character_id` is UNIQUE, not just part of a composite
-- key: a character is in at most one party at a time, enforced at the
-- storage layer rather than left to application-level discipline.
-- `party_type` names one of the dev-declared entries in
-- `party.schema.yaml` (`character::party_schema::PartySchema`) — set
-- once, at party formation, from whichever type the founding invite
-- requested; immutable afterward (a party's cap doesn't change mid-life
-- just because a later invite names a different type).
CREATE TABLE parties (
    id UUID PRIMARY KEY,
    party_type TEXT NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE TABLE party_members (
    party_id UUID NOT NULL REFERENCES parties(id) ON DELETE CASCADE,
    character_id UUID NOT NULL UNIQUE REFERENCES characters(id) ON DELETE CASCADE,
    joined_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (party_id, character_id)
);
