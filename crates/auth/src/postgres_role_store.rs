//! Postgres-backed [`AccountRoleStore`], reading/writing the
//! `account_roles` table from
//! `db/migrations/0005_create_account_roles/up.sql`.

use async_trait::async_trait;
use common::id::AccountId;
use common::{Error, Result};
use sqlx::{PgPool, Row};

use crate::roles::AccountRoleStore;

pub struct PostgresAccountRoleStore {
    pool: PgPool,
}

impl PostgresAccountRoleStore {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }
}

#[async_trait]
impl AccountRoleStore for PostgresAccountRoleStore {
    async fn grant_role(&self, account_id: AccountId, role: &str) -> Result<()> {
        sqlx::query(
            "INSERT INTO account_roles (account_id, role) VALUES ($1, $2)
             ON CONFLICT (account_id, role) DO NOTHING",
        )
        .bind(account_id.as_uuid())
        .bind(role)
        .execute(&self.pool)
        .await
        .map_err(|e| Error::wrap("auth", "failed to grant role", e))?;

        Ok(())
    }

    async fn revoke_role(&self, account_id: AccountId, role: &str) -> Result<()> {
        sqlx::query("DELETE FROM account_roles WHERE account_id = $1 AND role = $2")
            .bind(account_id.as_uuid())
            .bind(role)
            .execute(&self.pool)
            .await
            .map_err(|e| Error::wrap("auth", "failed to revoke role", e))?;

        Ok(())
    }

    async fn roles_for(&self, account_id: AccountId) -> Result<Vec<String>> {
        let rows = sqlx::query("SELECT role FROM account_roles WHERE account_id = $1")
            .bind(account_id.as_uuid())
            .fetch_all(&self.pool)
            .await
            .map_err(|e| Error::wrap("auth", "failed to query roles", e))?;

        Ok(rows.into_iter().map(|row| row.get("role")).collect())
    }
}

#[cfg(test)]
mod tests {
    use common::config::PostgresConfig;
    use common::pool::{PoolOptions, postgres_pool};
    use sqlx::Row;

    use super::*;
    use crate::store::AccountStore;

    // Real Postgres — set WZ_POSTGRES_* and run with `-- --ignored`.
    async fn store_and_pool() -> (PostgresAccountRoleStore, PgPool) {
        let config = PostgresConfig::from_env().expect("WZ_POSTGRES_* env vars set");
        let pool = postgres_pool(&config, PoolOptions::default())
            .await
            .unwrap();
        (PostgresAccountRoleStore::new(pool.clone()), pool)
    }

    // account_roles has a real FK into accounts, so every test needs a
    // real account row to hang roles off of — same throwaway-account
    // pattern character's postgres tests use for their own FK.
    async fn throwaway_account(pool: &PgPool, label: &str) -> AccountId {
        let account_store = crate::postgres_store::PostgresAccountStore::new(pool.clone());
        account_store
            .create(&format!("{label}-{}", AccountId::new()), "hash")
            .await
            .unwrap()
    }

    #[tokio::test]
    #[ignore]
    async fn grant_then_roles_for_round_trips() {
        let (store, pool) = store_and_pool().await;
        let account_id = throwaway_account(&pool, "grant-round-trip").await;

        store.grant_role(account_id, "admin").await.unwrap();
        store.grant_role(account_id, "dev").await.unwrap();

        let mut roles = store.roles_for(account_id).await.unwrap();
        roles.sort();
        assert_eq!(roles, ["admin", "dev"]);
    }

    #[tokio::test]
    #[ignore]
    async fn roles_for_an_account_with_no_roles_is_empty() {
        let (store, pool) = store_and_pool().await;
        let account_id = throwaway_account(&pool, "no-roles").await;

        assert!(store.roles_for(account_id).await.unwrap().is_empty());
    }

    #[tokio::test]
    #[ignore]
    async fn granting_the_same_role_twice_does_not_duplicate_it() {
        let (store, pool) = store_and_pool().await;
        let account_id = throwaway_account(&pool, "duplicate-grant").await;

        store.grant_role(account_id, "admin").await.unwrap();
        store.grant_role(account_id, "admin").await.unwrap();

        assert_eq!(store.roles_for(account_id).await.unwrap(), ["admin"]);
    }

    #[tokio::test]
    #[ignore]
    async fn revoke_removes_only_the_named_role() {
        let (store, pool) = store_and_pool().await;
        let account_id = throwaway_account(&pool, "revoke").await;

        store.grant_role(account_id, "admin").await.unwrap();
        store.grant_role(account_id, "dev").await.unwrap();
        store.revoke_role(account_id, "admin").await.unwrap();

        assert_eq!(store.roles_for(account_id).await.unwrap(), ["dev"]);
    }

    #[tokio::test]
    #[ignore]
    async fn deleting_an_account_cascades_to_its_roles() {
        let (store, pool) = store_and_pool().await;
        let account_id = throwaway_account(&pool, "cascade").await;
        store.grant_role(account_id, "admin").await.unwrap();

        sqlx::query("DELETE FROM accounts WHERE id = $1")
            .bind(account_id.as_uuid())
            .execute(&pool)
            .await
            .unwrap();

        let count: i64 =
            sqlx::query("SELECT count(*) AS count FROM account_roles WHERE account_id = $1")
                .bind(account_id.as_uuid())
                .fetch_one(&pool)
                .await
                .unwrap()
                .get("count");
        assert_eq!(count, 0);
    }
}
