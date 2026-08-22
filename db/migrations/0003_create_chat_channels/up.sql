CREATE TABLE chat_channels (
    id UUID PRIMARY KEY,
    channel_type TEXT NOT NULL,
    name TEXT,
    zone_id TEXT,
    category TEXT,
    created_by UUID REFERENCES accounts(id) ON DELETE SET NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    CHECK (channel_type IN ('direct', 'group', 'guild', 'zone'))
);

CREATE TABLE chat_channel_members (
    channel_id UUID NOT NULL REFERENCES chat_channels(id) ON DELETE CASCADE,
    account_id UUID NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    joined_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (channel_id, account_id)
);

-- One channel per (zone, category) — ensure_zone_channel relies on this to
-- stay idempotent even under concurrent callers, not just app-side checks.
CREATE UNIQUE INDEX chat_channels_zone_category_idx ON chat_channels (zone_id, category) WHERE channel_type = 'zone';
