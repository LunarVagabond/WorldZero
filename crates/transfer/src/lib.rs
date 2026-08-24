//! Character transfer execution (#53, [`execute`]), ticket/cash gating
//! (#54, not built yet), and audit trail (#55, not built yet) between
//! bound realms.
//!
//! Design: docs/PROPOSAL.md ("Realm & Character Policy Model") and
//! docs/specs/Realm_Character_Policy_Spec.md ("Transfers (bound realms
//! only)").

pub mod execute;

pub use execute::{TransferExecutor, TransferRequest};

#[cfg(test)]
mod tests;
