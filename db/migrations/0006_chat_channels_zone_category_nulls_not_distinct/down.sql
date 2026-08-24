DROP INDEX chat_channels_zone_category_idx;
CREATE UNIQUE INDEX chat_channels_zone_category_idx
    ON chat_channels (zone_id, category)
    WHERE channel_type = 'zone';
