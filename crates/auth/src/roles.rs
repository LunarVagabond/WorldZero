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

#[cfg(test)]
mod tests {
    use super::*;

    // `InMemoryAccountRoleStore` exists for tests (server-side callers
    // always use `PostgresAccountRoleStore`) — this exercises it against
    // the same `AccountRoleStore` contract `postgres_role_store.rs`'s
    // ignored suite verifies against real Postgres, so the in-memory
    // fake can't silently drift from what it's meant to stand in for.

    #[tokio::test]
    async fn grant_then_roles_for_round_trips() {
        let store = InMemoryAccountRoleStore::default();
        let account_id = AccountId::new();

        store.grant_role(account_id, "admin").await.unwrap();
        store.grant_role(account_id, "dev").await.unwrap();

        let mut roles = store.roles_for(account_id).await.unwrap();
        roles.sort();
        assert_eq!(roles, ["admin", "dev"]);
    }

    #[tokio::test]
    async fn roles_for_an_account_with_no_roles_is_empty() {
        let store = InMemoryAccountRoleStore::default();
        assert!(store.roles_for(AccountId::new()).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn granting_the_same_role_twice_does_not_duplicate_it() {
        let store = InMemoryAccountRoleStore::default();
        let account_id = AccountId::new();

        store.grant_role(account_id, "admin").await.unwrap();
        store.grant_role(account_id, "admin").await.unwrap();

        assert_eq!(store.roles_for(account_id).await.unwrap(), ["admin"]);
    }

    #[tokio::test]
    async fn revoke_removes_only_the_named_role() {
        let store = InMemoryAccountRoleStore::default();
        let account_id = AccountId::new();

        store.grant_role(account_id, "admin").await.unwrap();
        store.grant_role(account_id, "dev").await.unwrap();
        store.revoke_role(account_id, "admin").await.unwrap();

        assert_eq!(store.roles_for(account_id).await.unwrap(), ["dev"]);
    }

    #[tokio::test]
    async fn revoking_a_role_from_an_account_with_none_is_a_harmless_no_op() {
        let store = InMemoryAccountRoleStore::default();
        let account_id = AccountId::new();
        store.revoke_role(account_id, "admin").await.unwrap();
        assert!(store.roles_for(account_id).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn roles_are_scoped_per_account() {
        let store = InMemoryAccountRoleStore::default();
        let account_a = AccountId::new();
        let account_b = AccountId::new();

        store.grant_role(account_a, "admin").await.unwrap();

        assert_eq!(store.roles_for(account_a).await.unwrap(), ["admin"]);
        assert!(store.roles_for(account_b).await.unwrap().is_empty());
    }
}
