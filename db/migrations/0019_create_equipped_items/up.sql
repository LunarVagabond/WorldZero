-- #277: one row per currently-worn item, keyed by (character_id, slot) —
-- a slot holds at most one item at a time, enforced by the PK itself, not
-- application logic alone. slot/item_type are opaque dev-declared strings
-- validated against equipment.schema.yaml at the call site
-- (character::equipment_schema::EquipmentSchema), same discipline as
-- items.item_type.
CREATE TABLE equipped_items (
    character_id UUID NOT NULL REFERENCES characters(id) ON DELETE CASCADE,
    slot TEXT NOT NULL,
    item_type TEXT NOT NULL,
    equipped_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (character_id, slot)
);
