-- Dev-declared multi-currency support (#218, implementing #217's
-- decision) — replaces `characters.currency_balance` (a single
-- hardcoded BIGINT, #4's migration) with one row per (character,
-- currency_key). Storage stays a flat integer balance per currency,
-- always in that currency's base unit; a denomination ladder
-- (copper/silver/gold, etc.) is a pure display/conversion concept
-- computed at read time (character::CurrencySchema::breakdown), never a
-- separate stored ledger per denomination — see this ticket's own
-- design note in docs/specs/Data_Model_Spec.md's "Currency" section.
--
-- The `CHECK (balance >= 0)` preserves exactly the invariant the old
-- `currency_balance` column enforced, just per-currency now instead of
-- once per character.
CREATE TABLE character_currency (
    character_id UUID NOT NULL REFERENCES characters(id) ON DELETE CASCADE,
    currency_key TEXT NOT NULL,
    balance BIGINT NOT NULL DEFAULT 0 CHECK (balance >= 0),
    PRIMARY KEY (character_id, currency_key)
);

-- Preserve any pre-existing balance rather than silently dropping it —
-- every character that had ever accumulated a nonzero `currency_balance`
-- gets a matching `character_currency` row under the key `default`, an
-- arbitrary but real key a dev is expected to either declare in their
-- own `currency.schema.yaml` (renaming/reusing it as they see fit) or
-- migrate again to whatever key their real single currency uses. A
-- character that never had any currency (balance 0, the common case for
-- a pre-#218 deployment) gets no row, matching this table's normal "no
-- row means zero" convention (`character::CharacterStore::currency_balance`).
INSERT INTO character_currency (character_id, currency_key, balance)
SELECT id, 'default', currency_balance
FROM characters
WHERE currency_balance != 0;

ALTER TABLE characters DROP COLUMN currency_balance;
