//! Account role storage — separate from `AccountStore` per #114's decision
//! (a normalized `account_id`-keyed table, not a flat column on
//! `accounts`), so an account can hold more than one role. Scope is
//! global for v0 — see [`crate::postgres_role_store::PostgresAccountRoleStore`]
//! for the real-deployment implementation; `InMemoryAccountRoleStore` here
//! is for tests. A role is an opaque, dev-defined string (e.g. `"admin"`,
//! `"dev"`) — core assigns no meaning to any particular value, the same
//! "core has no privileged notion" discipline as gameplay stat keys.

use std::collections::{HashMap, HashSet};
use std::sync::RwLock;

use async_trait::async_trait;
use common::Result;
use common::id::AccountId;

#[async_trait]
pub trait AccountRoleStore: Send + Sync {
    async fn grant_role(&self, account_id: AccountId, role: &str) -> Result<()>;

    async fn revoke_role(&self, account_id: AccountId, role: &str) -> Result<()>;

    /// Every role currently held by `account_id`, in no particular order.
    /// Empty (not an error) if the account holds none — that's the common
    /// case, not an exceptional one.
    async fn roles_for(&self, account_id: AccountId) -> Result<Vec<String>>;
}

#[derive(Default)]
pub struct InMemoryAccountRoleStore {
    by_account: RwLock<HashMap<AccountId, HashSet<String>>>,
}

#[async_trait]
impl AccountRoleStore for InMemoryAccountRoleStore {
    async fn grant_role(&self, account_id: AccountId, role: &str) -> Result<()> {
        self.by_account
            .write()
            .unwrap()
            .entry(account_id)
            .or_default()
            .insert(role.to_string());
        Ok(())
    }

    async fn revoke_role(&self, account_id: AccountId, role: &str) -> Result<()> {
        if let Some(roles) = self.by_account.write().unwrap().get_mut(&account_id) {
            roles.remove(role);
        }
        Ok(())
    }

    async fn roles_for(&self, account_id: AccountId) -> Result<Vec<String>> {
        Ok(self
            .by_account
            .read()
            .unwrap()
            .get(&account_id)
            .map(|roles| roles.iter().cloned().collect())
            .unwrap_or_default())
    }
}
