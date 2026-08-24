//! Realm registry, open/bound character-binding policy, layer assignment, and transfer eligibility.
//!
//! The registry (realm CRUD, zone-to-realm tracking, #47) is real —
//! see [`store`]. `src/bin/realm.rs` (`make realm ARGS="..."`) is a
//! minimal CLI over it — the only way to manage realms today, since
//! nothing wires this crate into `server` yet (tracked as #136). Open/bound
//! policy *enforcement* (#51, [`login_policy`]) is real and tested too,
//! also not wired in yet — see [`login_policy`]'s doc comment. Dynamic
//! layer assignment (#50) is done, but doesn't live here — it turned
//! out to be a `server`-side concern (population-spreading across
//! `world_actor` instances within a zone, `server::zone_registry`), not
//! a `realm-directory` one.
//!
//! Realm population reporting (#137, [`population`]) — character census
//! plus live connection counts — is also real and tested, not wired in.
//!
//! Design: docs/PROPOSAL.md ("Realm & Character Policy Model")
//! and docs/specs/Realm_Character_Policy_Spec.md.

pub mod login_policy;
pub mod population;
pub mod store;

pub use login_policy::LoginPolicy;
pub use population::{RealmPopulation, RealmPresence};
pub use store::{OpenOrBound, Realm, RealmStore};
