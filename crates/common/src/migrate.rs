//! Runs the reversible SQL migrations under `db/migrations/` against a
//! Postgres pool. `sqlx`'s own migration tracking (`_sqlx_migrations`)
//! is the source of truth for what's applied — this just drives it.
//!
//! Layout is one directory per migration — `db/migrations/<version>_<description>/{up,down}.sql`
//! — rather than sqlx's default flat `<version>_<description>.{up,down}.sql`
//! files, so related migrations are easy to find/browse as a unit. sqlx's
//! `Migrator` doesn't support that layout natively, so this builds the
//! `Migration` list by hand and hands it to `Migrator::with_migrations`.

use std::path::Path;

use sqlx::migrate::{Migration, MigrationType, Migrator};
use sqlx::{AssertSqlSafe, PgPool, SqlSafeStr};

use crate::error::{Error, Result};

fn load_migrations(migrations_dir: &Path) -> Result<Vec<Migration>> {
    let mut migrations = Vec::new();

    let entries = std::fs::read_dir(migrations_dir)
        .map_err(|e| Error::wrap("common", "failed to read migrations directory", e))?;

    for entry in entries {
        let entry = entry
            .map_err(|e| Error::wrap("common", "failed to read a migrations directory entry", e))?;
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }

        let dir_name = path.file_name().and_then(|n| n.to_str()).ok_or_else(|| {
            Error::new(
                "common",
                format!("non-UTF-8 migration directory name: {path:?}"),
            )
        })?;

        let (version_str, description) = dir_name.split_once('_').ok_or_else(|| {
            Error::new(
                "common",
                format!("migration directory must be <version>_<description>: {dir_name}"),
            )
        })?;
        let version: i64 = version_str.parse().map_err(|_| {
            Error::new(
                "common",
                format!("migration version is not a number: {dir_name}"),
            )
        })?;

        let up_sql = std::fs::read_to_string(path.join("up.sql"))
            .map_err(|e| Error::wrap("common", format!("failed to read {dir_name}/up.sql"), e))?;
        let down_sql = std::fs::read_to_string(path.join("down.sql"))
            .map_err(|e| Error::wrap("common", format!("failed to read {dir_name}/down.sql"), e))?;

        migrations.push(Migration::new(
            version,
            description.to_string().into(),
            MigrationType::ReversibleUp,
            AssertSqlSafe(up_sql).into_sql_str(),
            false,
        ));
        migrations.push(Migration::new(
            version,
            description.to_string().into(),
            MigrationType::ReversibleDown,
            AssertSqlSafe(down_sql).into_sql_str(),
            false,
        ));
    }

    migrations.sort();
    Ok(migrations)
}

fn migrator(migrations_dir: &Path) -> Result<Migrator> {
    Ok(Migrator::with_migrations(load_migrations(migrations_dir)?))
}

/// Applies every pending migration, in order.
pub async fn migrate_up(pool: &PgPool, migrations_dir: &Path) -> Result<()> {
    migrator(migrations_dir)?
        .run(pool)
        .await
        .map_err(|e| Error::wrap("common", "failed to apply migrations", e))
}

/// Reverts exactly the most recently applied migration (its `down.sql`).
/// A no-op if nothing has been applied yet.
pub async fn migrate_down_one(pool: &PgPool, migrations_dir: &Path) -> Result<()> {
    let migrator = migrator(migrations_dir)?;

    let mut applied: Vec<i64> =
        sqlx::query_scalar("SELECT version FROM _sqlx_migrations WHERE success ORDER BY version")
            .fetch_all(pool)
            .await
            .map_err(|e| Error::wrap("common", "failed to read applied migrations", e))?;

    if applied.pop().is_none() {
        return Ok(());
    }
    let target = applied.pop().unwrap_or(0);

    migrator
        .undo(pool, target)
        .await
        .map_err(|e| Error::wrap("common", "failed to revert migration", e))
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use crate::config::PostgresConfig;
    use crate::pool::{PoolOptions, postgres_pool};

    use super::*;

    fn migrations_dir() -> std::path::PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../db/migrations")
    }

    #[test]
    fn loads_migrations_from_per_migration_subfolders() {
        let migrations = load_migrations(&migrations_dir()).unwrap();

        assert!(
            migrations
                .iter()
                .any(|m| m.version == 1 && m.migration_type == MigrationType::ReversibleUp)
        );
        assert!(
            migrations
                .iter()
                .any(|m| m.version == 1 && m.migration_type == MigrationType::ReversibleDown)
        );
    }

    // Real Postgres, not run in CI — set WZ_POSTGRES_* and run with
    // `-- --ignored`. Runs against the real db/migrations/ directory, so
    // this is idempotent by construction (sqlx tracks what's applied) —
    // safe to run repeatedly against the same dev database.
    #[tokio::test]
    #[ignore]
    async fn up_then_down_round_trips_cleanly() {
        let config = PostgresConfig::from_env().expect("WZ_POSTGRES_* env vars set");
        let pool = postgres_pool(&config, PoolOptions::default())
            .await
            .unwrap();
        let dir = migrations_dir();

        migrate_up(&pool, &dir).await.unwrap();

        let exists: bool = sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM information_schema.tables WHERE table_name = 'accounts')",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert!(exists, "accounts table should exist after migrate_up");

        migrate_down_one(&pool, &dir).await.unwrap();

        let exists_after_undo: bool = sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM information_schema.tables WHERE table_name = 'accounts')",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert!(
            !exists_after_undo,
            "accounts table should be gone after migrate_down_one"
        );

        // Leave the database in the applied state for other tests/dev use.
        migrate_up(&pool, &dir).await.unwrap();
    }
}
