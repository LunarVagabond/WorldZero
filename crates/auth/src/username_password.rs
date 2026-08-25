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
    #[tracing::instrument(skip(self, password), fields(username))]
    pub async fn register(&self, username: &str, password: &str) -> Result<AccountId> {
        let hash = hash_password(password)?;
        self.store.create(username, &hash).await
    }

    /// Resumes a session from a previously-issued `session_token` (#195)
    /// — no password re-entry. Not part of `AuthProvider` for the same
    /// reason `register` isn't: this is session-token verification, a
    /// concern orthogonal to which provider originally checked the
    /// credentials, not a credentials-shaped check itself. Returns the
    /// account id and username `Authenticated`'s wire shape needs;
    /// `SessionManager::resolve` is what actually renews the token's TTL
    /// (see its own doc comment for the deliberate "same token, sliding
    /// expiration" choice) — this method never mints a new one.
    #[tracing::instrument(skip(self, token))]
    pub async fn resume(&self, token: &str) -> Result<(AccountId, String)> {
        let account_id = self
            .sessions
            .resolve(token)
            .await?
            .ok_or_else(|| Error::new("auth", "session token is invalid or has expired"))?;
        let account = self
            .store
            .find_by_id(account_id)
            .await?
            .ok_or_else(|| Error::new("auth", "account no longer exists"))?;
        Ok((account_id, account.username))
    }
}

#[async_trait]
impl AuthProvider for UsernamePasswordProvider {
    #[tracing::instrument(skip_all)]
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

    #[tracing::instrument(skip(self), fields(%account_id))]
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

    // Real Redis, not run in CI — set WZ_REDIS_* and run with `-- --ignored`.
    // `resume` (#195) is the one method here that actually calls through
    // to `SessionManager`, unlike every other test in this module (see
    // `provider()`'s own doc comment on why a disconnected pool is fine
    // for those).
    fn provider_with_real_redis() -> UsernamePasswordProvider {
        let config = common::config::RedisConfig::from_env().expect("WZ_REDIS_* env vars set");
        let redis =
            common::pool::redis_pool(&config, common::pool::PoolOptions::default()).unwrap();
        UsernamePasswordProvider::new(
            Arc::new(InMemoryAccountStore::default()),
            SessionManager::new(redis),
        )
    }

    #[tokio::test]
    #[ignore]
    async fn resume_finds_the_account_and_username_behind_a_real_token() {
        let provider = provider_with_real_redis();
        let account_id = provider.register("alice", "hunter2").await.unwrap();
        let session = provider.issue_session(account_id).await.unwrap();

        let (resumed_id, username) = provider.resume(&session.token).await.unwrap();
        assert_eq!(resumed_id, account_id);
        assert_eq!(username, "alice");
    }

    #[tokio::test]
    #[ignore]
    async fn resume_with_an_unknown_token_is_rejected() {
        let provider = provider_with_real_redis();
        let err = provider.resume("not-a-real-token").await.unwrap_err();
        assert!(err.to_string().contains("invalid or has expired"), "{err}");
    }
}
