-- Real guild system (#179) — a durable, account-scoped roster with a
-- dev-declared rank hierarchy (guild::GuildSchema, guild.schema.yaml).
-- `rank_key` names one of the schema's declared entries; rank index 0
-- (the schema's first declared entry) is the founder/leader rank,
-- enforced structurally by `guild::GuildStore`, not by this schema.
--
-- Guilds are single-realm, not cross-realm — `realm_id` is a real FK to
-- `realms`, matching `characters.realm_id` (#170). A cross-realm
-- "community" concept is a plausible future feature, deliberately not
-- designed for here.
--
-- `chat_channel_id` deliberately has no foreign key to `chat_channels`
-- (a table `guild`'s migrations know nothing about) — `chat` is an
-- optional service (`WZ_SERVICE_CHAT_ENABLED`) and a guild must keep
-- working with it disabled. `server::session` is the only place that
-- ever reads or writes this column against a real `chat_channels` row.
CREATE TABLE guilds (
    id UUID PRIMARY KEY,
    name TEXT NOT NULL,
    motd TEXT,
    tag TEXT,
    realm_id UUID NOT NULL REFERENCES realms(id),
    chat_channel_id UUID,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

-- One guild per account, not one per (account, realm) — guilds are
-- single-realm for now (see above), so there's no real case yet for one
-- account needing independent guild memberships on two different realms.
CREATE TABLE guild_members (
    guild_id UUID NOT NULL REFERENCES guilds(id) ON DELETE CASCADE,
    account_id UUID NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    rank_key TEXT NOT NULL,
    joined_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (guild_id, account_id)
);

CREATE UNIQUE INDEX guild_members_one_guild_per_account ON guild_members (account_id);
