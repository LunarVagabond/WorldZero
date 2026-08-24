//! Realm registry, open/bound character-binding policy, layer assignment, and transfer eligibility.
//!
//! The registry (realm CRUD, zone-to-realm tracking, #47) is real —
//! see [`store`]. `src/bin/realm.rs` (`make realm ARGS="..."`) is a
//! minimal CLI over it — the only way to manage realms today, since
//! nothing wires this crate into `server` yet. Open/bound policy
//! *enforcement* (#51, [`login_policy`]) is real and tested but also not
//! wired into `server` yet — see [`login_policy`]'s doc comment. Layer
//! assignment (#50) is not built yet.
//!
//! Design: docs/PROPOSAL.md ("Realm & Character Policy Model")
//! and docs/specs/Realm_Character_Policy_Spec.md.

pub mod login_policy;
pub mod store;

pub use login_policy::LoginPolicy;
pub use store::{OpenOrBound, Realm, RealmStore};
