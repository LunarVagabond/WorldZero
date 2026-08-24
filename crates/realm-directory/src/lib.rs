//! Realm registry, open/bound character-binding policy, layer assignment, and transfer eligibility.
//!
//! The registry (realm CRUD, zone-to-realm tracking, #47) is real —
//! see [`store`]. Layer assignment (#50) and open/bound policy
//! *enforcement* (#51) are not built yet; this crate only carries the
//! `open_or_bound` value today, per docs/specs/Realm_Character_Policy_Spec.md's
//! "The flag".
//!
//! Design: docs/PROPOSAL.md ("Realm & Character Policy Model")
//! and docs/specs/Realm_Character_Policy_Spec.md.

pub mod store;

pub use store::{OpenOrBound, Realm, RealmStore};
