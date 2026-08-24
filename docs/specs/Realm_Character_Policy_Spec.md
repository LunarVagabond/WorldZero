# Realm & Character Policy Spec

Corresponds to [Realm & Character Policy Model](../PROPOSAL.md#realm--character-policy-model) in the proposal.

## The flag

Open-vs-bound is stored per realm, not per deployment or per character. `realm-directory`'s registry (#47, implemented — `RealmStore` in `crates/realm-directory/src/store.rs`, schema in docs/specs/Data_Model_Spec.md) carries an `open_or_bound` field on every realm record — `open` or `bound` — from the moment realm CRUD exists. Enforcement (#51, `realm-directory::LoginPolicy` in `crates/realm-directory/src/login_policy.rs`) is real and tested now too, though not yet wired into `server` — see "Managing realms today" below. A deployment can mix models across realm groups; there is no global switch anywhere in `common::config`.

A character's own row does not duplicate this flag. Whether a given character can log into a given realm is derived by looking up that realm's `open_or_bound` value at connect time (`gateway` → `realm-directory`), not stored redundantly on the character. Storing it twice would let the two disagree after a realm's policy changes.

### Managing realms today

`realm-directory` isn't wired into `server` yet (that's #50), so there's no in-game or admin-API flow for this — the only way to create/inspect/manage a realm right now is `realm-directory`'s own CLI:

```sh
make realm ARGS="create MyRealm open"      # prints the new realm's id
make realm ARGS="list"                     # id, policy, name — one per line
make realm ARGS="get <realm-id>"           # realm details + its assigned zones
make realm ARGS="update <realm-id> NewName bound"
make realm ARGS="delete <realm-id>"
make realm ARGS="assign-zone <realm-id> greenwood-forest"
make realm ARGS="unassign-zone greenwood-forest"
```

Needs `WZ_POSTGRES_*` (`.env`, loaded automatically by `make`) and migration `0007_create_realms` applied (`make migrate`). Run `make realm` with no `ARGS` for the full command list. Same hand-rolled-CLI convention as `make migrate`/`content`'s `validate` bin — see `crates/realm-directory/src/bin/realm.rs`.

## Open realms: concurrency control

**The problem this section exists to answer:** `docs/PROPOSAL.md` says open-realm character state needs "appropriate locking/versioning" without saying what that is. This is that design.

An open realm lets one character be reachable from any zone-service instance in the realm group at any time. Two different zone-service instances must never simulate the same character concurrently — that's a split-brain bug (two authoritative positions, two sets of stat writes racing each other), not a performance nuisance.

**Chosen mechanism: a session lease row, not optimistic concurrency on every stat write.**

- A new `character_sessions` table (owned by `character`, alongside the existing `characters` table in `docs/specs/Data_Model_Spec.md`) holds at most one row per currently-online character: `character_id PRIMARY KEY`, `zone_service_id`, `realm_id`, `leased_at`, `expires_at`.
- A zone-service instance acquires the lease with a single conditional insert/update (`INSERT ... ON CONFLICT (character_id) DO UPDATE ... WHERE character_sessions.expires_at < now()`) before it will accept a login for that character. If the row exists and is unexpired, the login is refused with "already logged in elsewhere" — the same user-facing behavior every MMO already has, not a new concept.
- The owning zone-service instance renews the lease periodically (well inside the expiry window) as long as the character is connected, and deletes the row on clean disconnect. An unclean disconnect (crash, network partition) simply lets the lease expire — no explicit cleanup required, no risk of a permanently stuck character.
- Ordinary per-tick stat/position writes (`CharacterStore::set_stat`, `update_position`) do **not** each re-check the lease — that would put a lease check on the hottest path in the server for no real benefit, since the lease already prevents a second instance from ever starting to write concurrently in the first place. The lease is the concurrency boundary; individual writes trust it.
- This is deliberately not row-level optimistic concurrency (a `version` column bumped on every write, rejecting stale writers) on the `characters` table itself. Optimistic concurrency solves "detect a conflicting write after the fact"; a lease solves "prevent a second writer from starting," which is the actual open-realm requirement and is cheaper on the hot path (one lease check at login/reconnect, not one version check per stat write).
- Login flow for open realms becomes: gateway authenticates → `character` attempts the lease → on success, character state is read fresh from Postgres (never assumed cached from a prior session) → zone-service becomes authoritative until disconnect or lease expiry.

Bound realms do not use `character_sessions` at all — a bound-realm character only ever has one realm that could possibly claim it, so the split-brain scenario this table exists to prevent cannot occur. `character` skips the lease step entirely when the target realm's `open_or_bound` is `bound`.

## Bound realms: caching

A bound realm's zone-service instance may cache character state more aggressively than an open realm's (per the proposal), since no other realm can ever contend it. The lease mechanism above still is not needed for this — the contention it prevents doesn't exist in the bound model. Cache invalidation on write stays entirely within that one realm's own process; nothing cross-realm to coordinate.

## Transfers (bound realms only)

Transfers only make sense for bound-realm characters — an open-realm character can already log into any realm in the group, so "transfer" has no meaning there. `transfer` (#53) rejects a transfer request for an open-realm character as a validation error, not a no-op.

**Mechanism:** a transfer is a single Postgres transaction against `character`'s tables (not a distributed transaction across processes) that:

1. Requires the character currently hold no active `character_sessions` lease (i.e., not logged in) — a transfer never races a live session.
2. Updates `characters.realm_id` to the destination realm and re-validates the character's `stats` blob against the destination realm's declared attribute schema (`docs/specs/Data_Model_Spec.md`) — a destination game's schema may not declare every key the source did; unrecognized keys are dropped, missing keys fall back to the destination's default, exactly as an ordinary schema-evolution read would.
3. Commits or rolls back atomically. There is no intermediate "exists in both realms" state and no intermediate "exists in neither" state — the row either still belongs to the source realm (transaction rolled back) or now belongs to the destination (transaction committed), and a crash mid-transfer is indistinguishable from a rollback because Postgres itself guarantees that.

This satisfies #53's "no partial-transfer state" and "failed transfer leaves the character usable on the source realm" acceptance criteria directly from ordinary transactional semantics — no bespoke saga/compensation logic needed, because everything being moved (record, inventory, stats) already lives in the same Postgres database as one write path.

**Gating (#54):** gating (ticket item, real-money purchase, or unrestricted) is a check `transfer` performs *before* opening the transaction above, not part of the transaction itself — a denied gate never touches character data. What "consuming" a ticket item means (an inventory write) happens inside the same transaction as the realm-move, so a gate that requires consuming an item can't succeed while the transfer itself fails.

**Audit (#55):** every attempted transfer (successful or rejected) is expected to write one row to a `transfer_log` table as part of, or immediately following, the same transaction — full design left to #55, but the transaction boundary above is what that log entry's "did this actually commit" column reads back.

## Summary: what's decided vs. deferred

| Question | Answer | Where enforced |
|---|---|---|
| Where does open/bound live? | Per-realm field on the `realm-directory` registry | #47 (field exists), #51 (enforced, not yet wired into `server`) |
| How is open-realm split-brain prevented? | A `character_sessions` lease table, checked at login/reconnect only | `character::CharacterSessionLease`, consumed by `realm-directory::LoginPolicy` (#51) |
| Do bound realms need the lease? | No — skipped entirely | `character` |
| Is a transfer atomic? | Yes — one Postgres transaction, no distributed/saga logic | #53 |
| Can an open-realm character be transferred? | No — rejected as a validation error | #53 |
| Where does gating happen relative to the transfer transaction? | Before it opens; a denied gate never touches character data | #54 |
