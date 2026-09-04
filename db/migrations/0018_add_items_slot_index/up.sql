-- #276: a nullable slot position for player-directed inventory ordering.
-- NULL means "unsorted" — a newly granted item stays unsorted until the
-- player explicitly places it (character::CharacterStore::move_item_to_slot).
-- No uniqueness constraint on (character_id, slot_index): swap semantics
-- are enforced by move_item_to_slot's own transaction, not the schema.
ALTER TABLE items
    ADD COLUMN slot_index INTEGER;
