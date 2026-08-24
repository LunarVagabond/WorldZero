-- Per-realm-pair transfer gating (#54): whether a transfer from
-- source_realm_id to destination_realm_id is open, ticket-item-gated, or
-- purchase-gated. Absence of a row means open (docs/PROPOSAL.md's "or
-- left open" default) -- an operator only inserts a row here to add a
-- gate, not to declare the default.
CREATE TABLE transfer_gates (
    source_realm_id UUID NOT NULL REFERENCES realms(id) ON DELETE CASCADE,
    destination_realm_id UUID NOT NULL REFERENCES realms(id) ON DELETE CASCADE,
    gate_type TEXT NOT NULL CHECK (gate_type IN ('open', 'ticket_item', 'purchase')),
    ticket_item_type TEXT,
    purchase_product_id TEXT,
    PRIMARY KEY (source_realm_id, destination_realm_id)
);
