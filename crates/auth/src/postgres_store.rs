//! Postgres-backed [`AccountStore`], reading/writing the `accounts` table
//! from `db/migrations/0001_create_accounts.up.sql`.

use async_trait::async_trait;
use common::id::AccountId;
use common::{Error, Result};
use sqlx::{PgPool, Row};

use crate::store::{Account, AccountStore};

pub struct PostgresAccountStore {
    pool: PgPool,
}

impl PostgresAccountStore {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }
}

#[async_trait]
impl AccountStore for PostgresAccountStore {
    async fn create(&self, username: &str, password_hash: &str) -> Result<AccountId> {
        let id = AccountId::new();

        sqlx::query("INSERT INTO accounts (id, username, password_hash) VALUES ($1, $2, $3)")
            .bind(id.as_uuid())
            .bind(username)
            .bind(password_hash)
            .execute(&self.pool)
            .await
            .map_err(|e| {
                if is_unique_violation(&e) {
                    Error::new("auth", format!("username already taken: {username}"))
                } else {
                    Error::wrap("auth", "failed to create account", e)
                }
            })?;

        Ok(id)
    }

    async fn find_by_username(&self, username: &str) -> Result<Option<Account>> {
        let row =
            sqlx::query("SELECT id, username, password_hash FROM accounts WHERE username = $1")
                .bind(username)
                .fetch_optional(&self.pool)
                .await
                .map_err(|e| Error::wrap("auth", "failed to query account", e))?;

        Ok(row.map(|row| Account {
            id: AccountId::from_uuid(row.get("id")),
            username: row.get("username"),
            password_hash: row.get("password_hash"),
        }))
    }
}

fn is_unique_violation(err: &sqlx::Error) -> bool {
    matches!(err, sqlx::Error::Database(db_err) if db_err.is_unique_violation())
}

#[cfg(test)]
mod tests {
    use common::config::PostgresConfig;
    use common::pool::{PoolOptions, postgres_pool};

    use super::*;

    // Real Postgres — set WZ_POSTGRES_* and run with `-- --ignored`. Uses a
    // random username per run to avoid colliding with previous runs against
    // the same persistent dev database.
    async fn store() -> PostgresAccountStore {
        let config = PostgresConfig::from_env().expect("WZ_POSTGRES_* env vars set");
        let pool = postgres_pool(&config, PoolOptions::default())
            .await
            .unwrap();
        PostgresAccountStore::new(pool)
    }

    fn unique_username(label: &str) -> String {
        format!("{label}-{}", common::id::AccountId::new())
    }

    #[tokio::test]
    #[ignore]
    async fn create_then_find_round_trips() {
        let store = store().await;
        let username = unique_username("create-then-find");

        let id = store.create(&username, "hash").await.unwrap();
        let found = store.find_by_username(&username).await.unwrap().unwrap();

        assert_eq!(found.id, id);
        assert_eq!(found.username, username);
        assert_eq!(found.password_hash, "hash");
    }

    #[tokio::test]
    #[ignore]
    async fn find_missing_username_returns_none() {
        let store = store().await;
        let username = unique_username("does-not-exist");

        assert!(store.find_by_username(&username).await.unwrap().is_none());
    }

    #[tokio::test]
    #[ignore]
    async fn duplicate_username_is_rejected_with_a_specific_error() {
        let store = store().await;
        let username = unique_username("duplicate");

        store.create(&username, "hash-a").await.unwrap();
        let err = store.create(&username, "hash-b").await.unwrap_err();

        assert!(err.to_string().contains("already taken"), "{err}");
    }
}
