//! Character transfer execution (#53, [`execute`]) and ticket/cash
//! gating (#54, [`gate`]) between bound realms. Audit trail (#55) not
//! built yet.
//!
//! Design: docs/PROPOSAL.md ("Realm & Character Policy Model") and
//! docs/specs/Realm_Character_Policy_Spec.md ("Transfers (bound realms
//! only)").

pub mod execute;
pub mod gate;

pub use execute::{TransferExecutor, TransferRequest};
pub use gate::{DenyAllPurchaseVerifier, PurchaseVerifier, TransferGate, TransferGateStore};

#[cfg(test)]
mod tests;
