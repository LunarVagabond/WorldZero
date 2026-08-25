-- #170: characters.realm_id was UUID NOT NULL but never a real foreign
-- key, even though realms (#47) has existed for a while — flagged as a
-- known gap in docs/specs/Data_Model_Spec.md and left that way
-- deliberately until now (retrofitting this is real, separate cleanup
-- across every character-crate test fixture that built an ad hoc
-- RealmId::new() with no backing row, not part of #47 itself). No
-- ON DELETE behavior specified (defaults to NO ACTION/RESTRICT) —
-- deleting a realm that still has characters pointing at it should be a
-- hard error, not a silent cascade that deletes player data.
ALTER TABLE characters
    ADD CONSTRAINT characters_realm_id_fkey
    FOREIGN KEY (realm_id) REFERENCES realms(id);
