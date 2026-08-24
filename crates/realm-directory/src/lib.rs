//! Realm registry, open/bound character-binding policy, layer assignment, and transfer eligibility.
//!
//! The registry (realm CRUD, zone-to-realm tracking, #47) is real —
//! see [`store`]. `src/bin/realm.rs` (`make realm ARGS="..."`) is a
//! minimal CLI over it — the only way to manage realms today, since
//! nothing wires this crate into `server` yet. Layer assignment (#50)
//! and open/bound policy *enforcement* (#51) are not built yet; this
//! crate only carries the `open_or_bound` value today, per
//! docs/specs/Realm_Character_Policy_Spec.md's "The flag" (which also
//! has the CLI's full usage under "Managing realms today").
//!
//! Design: docs/PROPOSAL.md ("Realm & Character Policy Model")
//! and docs/specs/Realm_Character_Policy_Spec.md.

pub mod store;

pub use store::{OpenOrBound, Realm, RealmStore};
