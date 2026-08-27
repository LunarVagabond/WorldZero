//! Character transfer execution (#53, [`execute`]), ticket/cash gating
//! (#54, [`gate`]), and the audit trail (#55, [`audit`]) between bound
//! realms.
//!
//! Wired into `server`'s combined process as of #225 —
//! `server::character_protocol`'s `RequestTransfer` message routes to
//! [`execute::TransferExecutor::transfer`], the same "plumbing, not new
//! logic" shape #136 wired `realm-directory` in with.
//!
//! Design: docs/PROPOSAL.md ("Realm & Character Policy Model") and
//! docs/specs/Realm_Character_Policy_Spec.md ("Transfers (bound realms
//! only)").

pub mod audit;
pub mod execute;
pub mod gate;

pub use audit::{TransferAuditLog, TransferLogRecord, TransferOutcome};
pub use execute::{TransferExecutor, TransferRequest};
pub use gate::{DenyAllPurchaseVerifier, PurchaseVerifier, TransferGate, TransferGateStore};

#[cfg(test)]
mod tests;
