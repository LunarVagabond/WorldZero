//! The `AuthProvider` trait every identity provider implements
//! (docs/specs/Auth_Spec.md, "Provider trait").

use async_trait::async_trait;
use common::Result;
use common::id::AccountId;
use serde_json::Value;

/// A provider-agnostic credentials bag. Each provider deserializes the
/// shape it expects out of the inner value and rejects anything else with
/// a normal error, not a panic.
#[derive(Debug, Clone)]
pub struct Credentials(pub Value);

impl Credentials {
    pub fn new(value: Value) -> Self {
        Self(value)
    }
}

#[derive(Debug, Clone)]
pub struct Session {
    pub token: String,
    pub account_id: AccountId,
    pub expires_at: time::OffsetDateTime,
}

/// Object-safe on purpose — held as `Box<dyn AuthProvider>`/`Arc<dyn
/// AuthProvider>` so which provider a deployment uses is a runtime/config
/// choice, not a compile-time one.
#[async_trait]
pub trait AuthProvider: Send + Sync {
    async fn verify_credentials(&self, credentials: &Credentials) -> Result<AccountId>;

    async fn issue_session(&self, account_id: AccountId) -> Result<Session>;
}
