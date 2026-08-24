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

    // Real Postgres — set WZ_POSTGRES_* and run with `-- --include-ignored`
    // (CI does this against its own service container; run locally with
    // `-- --ignored`). Deliberately non-destructive: `migrate_down_one`
    // against the real db/migrations directory is exercised manually
    // (`make migrate-down` / `make migrate`), not here — sqlx's migrator
    // cross-checks the *entire* applied-migration history in
    // `_sqlx_migrations` (shared per-database, not per-directory) against
    // whatever migration source you hand it, so there's no clean way to
    // sandbox an up/down round trip in an isolated directory without
    // racing `character`/`auth`'s tests, which depend on this same real
    // schema and run concurrently under `cargo test --workspace --
    // --include-ignored` — safe in practice since `migrate_up` is
    // idempotent and additive, never dropping/altering what those tests
    // depend on.
    //
    // The `from_env()` call is scoped/guarded/dropped-before-`.await` the
    // same way `pool.rs`'s ignored tests are — see `test_env_lock`'s doc
    // comment.
    #[tokio::test]
    #[ignore]
    async fn migrate_up_is_idempotent_against_the_real_migrations() {
        let config = {
            let _guard = crate::test_env_lock::acquire();
            PostgresConfig::from_env().expect("WZ_POSTGRES_* env vars set")
        };
        let pool = postgres_pool(&config, PoolOptions::default())
            .await
            .unwrap();
        let dir = migrations_dir();

        migrate_up(&pool, &dir).await.unwrap();
        migrate_up(&pool, &dir).await.unwrap();
    }
}
