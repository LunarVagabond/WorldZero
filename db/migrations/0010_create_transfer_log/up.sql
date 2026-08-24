-- Transfer audit trail (#55): one row per transfer *attempt*, successful
-- or failed. Append-only -- character/source_realm_id/destination_realm_id
-- are deliberately NOT foreign keys (unlike most tables in this schema):
-- a failed transfer against a nonexistent character or realm must still
-- be logged, and an FK would reject exactly the row that matters most.
-- character_id is still indexed (the query shape #56's admin API needs,
-- "this character's transfer history") even without the FK.
CREATE TABLE transfer_log (
    id UUID PRIMARY KEY,
    character_id UUID NOT NULL,
    source_realm_id UUID,
    destination_realm_id UUID NOT NULL,
    gate_type TEXT,
    initiated_by UUID NOT NULL,
    outcome TEXT NOT NULL CHECK (outcome IN ('success', 'failed')),
    failure_reason TEXT,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE INDEX transfer_log_character_id_idx ON transfer_log (character_id);
