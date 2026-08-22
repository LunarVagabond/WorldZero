//! Account storage the username/password provider needs.
//!
//! `InMemoryAccountStore` is what actually ships right now — there is no
//! decided Postgres `account` schema/migration tooling yet (unlike
//! `character`, which has one: docs/specs/Data_Model_Spec.md). A
//! Postgres-backed `AccountStore` is a natural follow-up once that schema
//! is designed, behind this same trait.

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
}
