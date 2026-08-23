ALTER TABLE characters
    ADD COLUMN currency_balance BIGINT NOT NULL DEFAULT 0 CHECK (currency_balance >= 0);

CREATE TABLE items (
    id UUID PRIMARY KEY,
    character_id UUID NOT NULL REFERENCES characters(id) ON DELETE CASCADE,
    item_type TEXT NOT NULL,
    quantity BIGINT NOT NULL CHECK (quantity > 0),
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE (character_id, item_type)
);

CREATE INDEX items_character_id_idx ON items (character_id);
