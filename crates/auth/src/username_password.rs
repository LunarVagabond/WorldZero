//! The self-contained username/password default `AuthProvider`
//! (docs/specs/Auth_Spec.md, "Default provider: username/password").

use std::sync::Arc;

use async_trait::async_trait;
use common::id::AccountId;
use common::{Error, Result};
use serde::Deserialize;

use crate::password::{hash_password, verify_password};
use crate::provider::{AuthProvider, Credentials, Session};
use crate::session::SessionManager;
use crate::store::AccountStore;

#[derive(Deserialize)]
struct UsernamePasswordCredentials {
    username: String,
    password: String,
}

pub struct UsernamePasswordProvider {
    store: Arc<dyn AccountStore>,
    sessions: SessionManager,
}

impl UsernamePasswordProvider {
    pub fn new(store: Arc<dyn AccountStore>, sessions: SessionManager) -> Self {
        Self { store, sessions }
    }

    /// Not part of `AuthProvider` — registration isn't universal across
    /// providers (an OAuth provider has no separate register step), so it
    /// lives only on the provider that actually needs it.
    pub async fn register(&self, username: &str, password: &str) -> Result<AccountId> {
        let hash = hash_password(password)?;
        self.store.create(username, &hash).await
    }
}

#[async_trait]
impl AuthProvider for UsernamePasswordProvider {
    async fn verify_credentials(&self, credentials: &Credentials) -> Result<AccountId> {
        let creds: UsernamePasswordCredentials = serde_json::from_value(credentials.0.clone())
            .map_err(|e| {
                Error::wrap(
                    "auth",
                    "credentials do not match username/password shape",
                    e,
                )
            })?;

        // Same generic error for "no such user" and "wrong password" —
        // don't let a caller distinguish which one was wrong.
        let account = self
            .store
            .find_by_username(&creds.username)
            .await?
            .ok_or_else(|| Error::new("auth", "invalid credentials"))?;

        if verify_password(&creds.password, &account.password_hash)? {
            Ok(account.id)
        } else {
            Err(Error::new("auth", "invalid credentials"))
        }
    }

    async fn issue_session(&self, account_id: AccountId) -> Result<Session> {
        self.sessions.issue(account_id).await
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::store::InMemoryAccountStore;

    fn provider() -> UsernamePasswordProvider {
        // issue_session isn't exercised by these tests, so a pool that's
        // never connected to is fine here — see session.rs for the
        // real-Redis session-issuance test.
        let redis = deadpool_redis::Config::from_url("redis://127.0.0.1:0")
            .create_pool(Some(deadpool_redis::Runtime::Tokio1))
            .unwrap();
        UsernamePasswordProvider::new(
            Arc::new(InMemoryAccountStore::default()),
            SessionManager::new(redis),
        )
    }

    #[tokio::test]
    async fn registration_then_login_succeeds() {
        let provider = provider();
        let registered = provider.register("alice", "hunter2").await.unwrap();

        let credentials = Credentials::new(json!({ "username": "alice", "password": "hunter2" }));
        let verified = provider.verify_credentials(&credentials).await.unwrap();

        assert_eq!(registered, verified);
    }

    #[tokio::test]
    async fn wrong_password_is_rejected() {
        let provider = provider();
        provider.register("alice", "hunter2").await.unwrap();

        let credentials = Credentials::new(json!({ "username": "alice", "password": "wrong" }));
        assert!(provider.verify_credentials(&credentials).await.is_err());
    }

    #[tokio::test]
    async fn nonexistent_username_is_rejected() {
        let provider = provider();

        let credentials = Credentials::new(json!({ "username": "nobody", "password": "hunter2" }));
        assert!(provider.verify_credentials(&credentials).await.is_err());
    }

    #[tokio::test]
    async fn duplicate_registration_is_rejected_with_a_specific_error() {
        let provider = provider();
        provider.register("alice", "hunter2").await.unwrap();

        let err = provider
            .register("alice", "different-password")
            .await
            .unwrap_err();
        assert!(err.to_string().contains("already taken"), "{err}");
    }

    #[tokio::test]
    async fn wrong_and_missing_username_return_the_same_error_message() {
        let provider = provider();
        provider.register("alice", "hunter2").await.unwrap();

        let wrong_password = provider
            .verify_credentials(&Credentials::new(
                json!({ "username": "alice", "password": "wrong" }),
            ))
            .await
            .unwrap_err();
        let missing_user = provider
            .verify_credentials(&Credentials::new(
                json!({ "username": "nobody", "password": "hunter2" }),
            ))
            .await
            .unwrap_err();

        assert_eq!(wrong_password.to_string(), missing_user.to_string());
    }
}
