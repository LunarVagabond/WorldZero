-- Durable, write-only chat message log (#174) -- separate from
-- ChatBus's Redis pub/sub (delivery only, never durable). Operator-side
-- analytics/moderation/disputes, not client-facing history replay --
-- nothing in this codebase reads this table back yet, on purpose.
-- sender_account_id is nullable with ON DELETE SET NULL (mirroring
-- chat_channels.created_by) so deleting an account doesn't erase the
-- log entries that might matter most for a moderation dispute.
CREATE TABLE chat_messages (
    id UUID PRIMARY KEY,
    channel_id UUID NOT NULL REFERENCES chat_channels(id) ON DELETE CASCADE,
    sender_account_id UUID REFERENCES accounts(id) ON DELETE SET NULL,
    body TEXT NOT NULL,
    sent_at TIMESTAMPTZ NOT NULL
);

CREATE INDEX chat_messages_channel_id_sent_at_idx ON chat_messages (channel_id, sent_at DESC);
