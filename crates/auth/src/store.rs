//! Account storage the username/password provider needs. See
//! [`crate::postgres_store::PostgresAccountStore`] for the real-deployment
//! implementation; `InMemoryAccountStore` here is for tests.

use std::collections::HashMap;
use std::sync::RwLock;

use async_trait::async_trait;
use common::id::AccountId;
use common::{Error, Result};

#[derive(Debug, Clone)]
pub struct Account {
    pub id: AccountId,
    pub username: String,
    pub password_hash: String,
}

#[async_trait]
pub trait AccountStore: Send + Sync {
    async fn create(&self, username: &str, password_hash: &str) -> Result<AccountId>;

    async fn find_by_username(&self, username: &str) -> Result<Option<Account>>;

    /// Looks up an account by id rather than username — #195's session
    /// resumption needs this: a `Resume{ session_token }` only resolves
    /// an `AccountId` (via `SessionManager::resolve`), never a username,
    /// but `Authenticated`'s wire shape still needs one to reply with.
    async fn find_by_id(&self, account_id: AccountId) -> Result<Option<Account>>;
}

#[derive(Default)]
pub struct InMemoryAccountStore {
    by_username: RwLock<HashMap<String, Account>>,
}

#[async_trait]
impl AccountStore for InMemoryAccountStore {
    async fn create(&self, username: &str, password_hash: &str) -> Result<AccountId> {
        let mut accounts = self.by_username.write().unwrap();
        if accounts.contains_key(username) {
            return Err(Error::new(
                "auth",
                format!("username already taken: {username}"),
            ));
        }

        let id = AccountId::new();
        accounts.insert(
            username.to_string(),
            Account {
                id,
                username: username.to_string(),
                password_hash: password_hash.to_string(),
            },
        );
        Ok(id)
    }

    async fn find_by_username(&self, username: &str) -> Result<Option<Account>> {
        Ok(self.by_username.read().unwrap().get(username).cloned())
    }

    async fn find_by_id(&self, account_id: AccountId) -> Result<Option<Account>> {
        Ok(self
            .by_username
            .read()
            .unwrap()
            .values()
            .find(|account| account.id == account_id)
            .cloned())
    }
}
