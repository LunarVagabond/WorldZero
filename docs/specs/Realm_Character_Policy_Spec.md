# Realm & Character Policy Spec

Corresponds to [Realm & Character Policy Model](../PROPOSAL.md#realm--character-policy-model) in the proposal.

## The flag

Open-vs-bound is stored per realm, not per deployment or per character. `realm-directory`'s registry (#47, implemented — `RealmStore` in `crates/realm-directory/src/store.rs`, schema in docs/specs/Data_Model_Spec.md) carries an `open_or_bound` field on every realm record — `open` or `bound` — from the moment realm CRUD exists. Enforcement (#51, `realm-directory::LoginPolicy` in `crates/realm-directory/src/login_policy.rs`) is real and tested, and wired into `server`'s login path as of #136 — see "Managing realms today" below. A deployment can mix models across realm groups; there is no global switch anywhere in `common::config`.

A character's own row does not duplicate this flag. Whether a given character can log into a given realm is derived by looking up that realm's `open_or_bound` value at connect time (`gateway` → `realm-directory`), not stored redundantly on the character. Storing it twice would let the two disagree after a realm's policy changes.

### Managing realms today

`server` resolves the one realm it serves from `WZ_REALM_ID` (#136) but doesn't yet expose any in-game or admin-API flow to create or manage realms themselves — the only way to create/inspect/manage a realm today is `realm-directory`'s own CLI, including creating the realm `WZ_REALM_ID` will point at:

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

## Realm selection (#192)

A connecting client's view of "here are the realm(s) this server serves, pick one" — `server::realm_protocol`, `message_type` 2 (docs/specs/Networking_Spec.md's catalog note), slotted between the auth handshake (`message_type` 1) and world-join (`message_type` 200).

- Right after `Authenticated`, a connection must send `SelectRealm { realm_id }` before anything else is accepted — `ListRealms` (no fields) can be sent any number of times first to discover what to select, using #137's live `character_count`/`live_connection_count` numbers, but it's never required: a client that already knows its realm id (a single-realm game has no reason to build a picker UI at all) can send `SelectRealm` immediately. "Skippable" in this sense — no UI required for a single-realm deployment — not "the network step itself can be omitted."
- `SelectRealm` must name the one realm the `server` process handling this connection actually serves (#136, `WZ_REALM_ID`) — anything else is rejected with a clear `Error` and the connection is closed. A process serving more than one realm at once, where a real choice between distinct realms would exist, is #130's job; the wire shape (`RealmList.realms` is already `repeated`) is ready for that without a protocol change, but today's `server` never has more than one entry to offer.
- A successful `SelectRealm` gets `RealmSelected { realm_id }` back, and only then does `resolve_or_create_character`/`LoginPolicy::authorize_login` (#51/#136) run — the realm choice genuinely gates login-policy resolution, not just a rubber-stamp step before it.
- `ListRealms`/`SelectRealm` are only handled during this pre-join phase — once a connection has joined the world, the same `message_type` is no longer routed (world/chat/plugin traffic owns the connection from there); a real realm-switch mid-session would need a reconnect, same as today's zone-transition-free "one realm per connection" model.
- #137's `RealmPresence` (`character_count`/`live_connection_count`) is registered at world-join (not at `SelectRealm`), renewed alongside #21's lease for the life of the connection, and deregistered on disconnect — see `server::session::handle_session`'s wiring.

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

**Finding the character, not just leasing it (#52, implemented).** The lease alone doesn't solve "which character row" — an open-realm character can be *recorded* against any one of the group's open realms (whichever it happened to be created on), so a lookup scoped to just the realm being connected through (`CharacterStore::find_by_account`, realm-scoped) would miss it entirely on every realm except that one. `CharacterStore::find_by_account_in_open_realms` (`crates/character/src/store.rs`) is the open-realm-aware counterpart — it joins against `realms` and matches any realm flagged `open`, never a bound one. `realm-directory::LoginPolicy::resolve_character` (`crates/realm-directory/src/login_policy.rs`) is the single call site that picks the right lookup for a given target realm's policy, mirroring `authorize_login`'s "one enforcement point" shape. There is no caching layer anywhere in this path — every read goes straight to Postgres — so a write made through one open realm is immediately visible through resolution via any other, with no staleness window to reason about.

## Bound realms: caching

A bound realm's zone-service instance may cache character state more aggressively than an open realm's (per the proposal), since no other realm can ever contend it. The lease mechanism above still is not needed for this — the contention it prevents doesn't exist in the bound model. Cache invalidation on write stays entirely within that one realm's own process; nothing cross-realm to coordinate.

## Transfers (bound realms only)

Transfers only make sense for bound-realm characters — an open-realm character can already log into any realm in the group, so "transfer" has no meaning there. `transfer` (#53) rejects a transfer request for an open-realm character as a validation error, not a no-op.

**Mechanism:** a transfer is a single Postgres transaction against `character`'s tables (not a distributed transaction across processes) that:

1. Requires the character currently hold no active `character_sessions` lease (i.e., not logged in) — a transfer never races a live session.
2. Updates `characters.realm_id` to the destination realm and re-validates the character's `stats` blob against the destination realm's declared attribute schema (`docs/specs/Data_Model_Spec.md`) — a destination game's schema may not declare every key the source did; unrecognized keys are dropped, missing keys fall back to the destination's default, exactly as an ordinary schema-evolution read would.
3. Commits or rolls back atomically. There is no intermediate "exists in both realms" state and no intermediate "exists in neither" state — the row either still belongs to the source realm (transaction rolled back) or now belongs to the destination (transaction committed), and a crash mid-transfer is indistinguishable from a rollback because Postgres itself guarantees that.

This satisfies #53's "no partial-transfer state" and "failed transfer leaves the character usable on the source realm" acceptance criteria directly from ordinary transactional semantics — no bespoke saga/compensation logic needed, because everything being moved (record, inventory, stats) already lives in the same Postgres database as one write path.

**Implemented (#53, wired into `server` as of #225):** `transfer::TransferExecutor` (`crates/transfer/src/execute.rs`) is the mechanism above, real and tested. `character::AttributeSchema::migrate_stats` (`crates/character/src/schema.rs`) is step 2's schema re-validation — exactly `AttributeSchema::resolve_read`'s per-key logic (present → kept, missing → destination default) applied to every key the destination schema declares at once, with anything else dropped. `destination_schema` is a caller-supplied input, same "given, not resolved here" shape #51/#52's `LoginPolicy` already uses for realm resolution — this crate has no opinion on how a deployment maps a realm to its schema file. `server` (`crates/server/src/main.rs`) supplies its own one declared `stats.schema.yaml` for this today, since a combined process only ever declares one schema — a real multi-schema, multi-deployment transfer target needs a schema registry keyed by realm that doesn't exist yet. A player requests a transfer for one of their own characters via `RequestTransfer` on `server::character_protocol` (`message_type` 3), reachable anywhere in the pre-join character-selection phase; a successful `TransferComplete` is effective immediately on the same connection — no reconnect needed, since the transfer only touches which realm the character *belongs to*, not anything about the connection's live state.

**Step 1's gap, closed (#169):** bound-realm characters never write to `character_sessions` at all (see "Bound realms" above), and transfer only ever applies to bound characters — so the "no active lease" check couldn't fire for the case it was meant to guard until real bound-realm liveness tracking existed. `character::BoundRealmLiveness` (`crates/character/src/bound_liveness.rs`) is that tracking: a `character_bound_liveness` table, deliberately parallel to `character_sessions` rather than an extension of it — same expiry-based self-healing shape, but keyed on "is this character connected right now" rather than lease contention between zone-service instances, since a bound-realm character has exactly one realm that could ever claim it. `server::session::handle_session` registers a bound-realm connection live on join (alongside `RealmPresence::connect`), renews it on the same heartbeat interval as #21's lease, and clears it on disconnect (alongside `character_lease.release`). `transfer::TransferExecutor::transfer`'s step-1 check now queries `BoundRealmLiveness::is_live` against the same transaction, so a transfer is genuinely rejected while the source character has an active bound-realm connection.

**Gating (#54, implemented):** `transfer::TransferGateStore` (`crates/transfer/src/gate.rs`) holds a per-`(source_realm_id, destination_realm_id)` gate — open (no row at all, the default), ticket-item, or purchase — a deployment may gate realm A → realm B differently than realm C → realm B. Checked inside `TransferExecutor::transfer` (`crates/transfer/src/execute.rs`), between the lease check and the realm-move. A ticket-item gate's consumption (an inventory write) happens *inside the same transaction* as the realm-move, not as a separate step before it opens — so a transfer that fails after consuming the item is impossible, and the gate check itself is what determines whether the transaction ever reaches the realm-move at all. Purchase gating is a `PurchaseVerifier` trait `TransferExecutor` calls before proceeding — real payment-processor integration is explicitly out of #54's scope, so the default (`DenyAllPurchaseVerifier`) always denies rather than silently letting a purchase-gated transfer through with nothing actually wired up.

**Audit (#55, implemented):** `transfer::TransferAuditLog` (`crates/transfer/src/audit.rs`) writes one row to `transfer_log` per transfer *attempt*, successful or rejected. A success's row is written inside the same transaction as the realm-move — "committed" and "audited" become one atomic fact — while a rejected attempt's row is a best-effort standalone write after the fact, since the transaction that would have carried it already rolled back or never opened; a failure to write the audit record itself never masks the real transfer error returned to the caller. `character_id`/`source_realm_id`/`destination_realm_id` are deliberately *not* foreign keys here, unlike most of this schema — a failed transfer against a nonexistent character or realm still needs to be logged, and an FK would reject exactly the row that matters most. Queryable by character via `TransferAuditLog::history_for_character`, the shape #56's admin API needs. Append-only is enforced at the API surface (no `update`/`delete` method exists) but not at the database level — a known, documented gap, same "not silently glossed over" discipline as the other real gaps this spec tracks.

## Summary: what's decided vs. deferred

| Question | Answer | Where enforced |
|---|---|---|
| Where does open/bound live? | Per-realm field on the `realm-directory` registry | #47 (field exists), #51 (enforced, wired into `server` as of #136) |
| How is open-realm split-brain prevented? | A `character_sessions` lease table, checked at login/reconnect only | `character::CharacterSessionLease`, consumed by `realm-directory::LoginPolicy` (#51) |
| Do bound realms need the lease? | No — skipped entirely | `character` |
| How is an open-realm character found regardless of which open realm it's on? | A join against `realms` matching any `open` realm, never a bound one | `CharacterStore::find_by_account_in_open_realms`, consumed by `LoginPolicy::resolve_character` (#52, wired into `server` as of #136) |
| Is a transfer atomic? | Yes — one Postgres transaction, no distributed/saga logic | #53 (wired into `server` as of #225) |
| Can an open-realm character be transferred? | No — rejected as a validation error | #53 |
| Where does gating happen relative to the transfer transaction? | Before it opens; a denied gate never touches character data | #54 |
| How does a player request a transfer? | `RequestTransfer` on `server::character_protocol`, pre-join phase, own characters only — no admin-initiated path | #225 |
| How is a bound-realm connection's liveness tracked for transfer's "no active session" check? | A parallel `character_bound_liveness` table, joined on connect/renewed on heartbeat/cleared on disconnect | `character::BoundRealmLiveness`, checked by `transfer::TransferExecutor::transfer` (#169) |
