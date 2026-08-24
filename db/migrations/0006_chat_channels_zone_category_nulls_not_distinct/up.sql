-- 0003's unique index on (zone_id, category) WHERE channel_type = 'zone'
-- was meant to make chat::store::ChannelStore::ensure_zone_channel
-- idempotent even under concurrent callers, but standard SQL unique
-- indexes treat every NULL as distinct from every other NULL — so two
-- concurrent global-scope channels (zone_id IS NULL) for the same
-- category never actually conflicted, silently producing duplicate
-- global system channels. NULLS NOT DISTINCT (Postgres 15+) makes NULL
-- zone_id values compare equal to each other for this index, closing
-- the gap; per-zone channels (zone_id NOT NULL) are unaffected.
DROP INDEX chat_channels_zone_category_idx;
CREATE UNIQUE INDEX chat_channels_zone_category_idx
    ON chat_channels (zone_id, category) NULLS NOT DISTINCT
    WHERE channel_type = 'zone';
