ALTER TABLE characters
    ADD COLUMN currency_balance BIGINT NOT NULL DEFAULT 0 CHECK (currency_balance >= 0);

-- Best-effort restore of the single-balance column from whatever
-- 'default'-keyed rows `up.sql` created — a character with balances
-- under other currency keys (created after this migration ran) has no
-- lossless way back into one BIGINT column, so those are dropped along
-- with the rest of `character_currency`, same "down.sql is a best-effort
-- rollback, not guaranteed lossless" stance every other migration in
-- this repo already takes.
UPDATE characters
SET currency_balance = character_currency.balance
FROM character_currency
WHERE character_currency.character_id = characters.id
  AND character_currency.currency_key = 'default';

DROP TABLE character_currency;
