//! Cross-cutting code shared by every other crate in the workspace.
//!
//! **Scope, deliberately tight — read before adding to this crate:** this exists
//! to avoid duplicating (or worse, letting drift) the handful of things every
//! other crate genuinely needs the same version of:
//!
//! - The shared `tracing` init/formatter setup implementing the
//!   `<TIMESTAMP> <LEVEL> <SOURCE> <MESSAGE>` log format and severity
//!   convention, plus Prometheus-compatible metrics and the `/metrics`
//!   HTTP endpoint, plus the `/healthz`/`/readyz` liveness/readiness
//!   endpoints (see docs/specs/Observability_Spec.md).
//! - Shared error/result types used across crate boundaries.
//! - Config loading (env-var-based connection config for Postgres/Redis, etc.).
//! - Strongly-typed ID types shared across crate/domain boundaries.
//! - Postgres/Redis connection pool builders.
//! - Running the reversible SQL migrations under `db/migrations/`.
//!
//! It is not a general-purpose dumping ground. If something is only used by
//! one or two crates, it belongs in one of them (or gets duplicated on
//! purpose), not here by default.

pub mod config;
pub mod error;
pub mod health;
pub mod id;
pub mod logging;
pub mod metrics;
pub mod migrate;
pub mod pool;

pub use error::{Error, Result, ResultExt};

/// Shared across every test module in this crate that reads or mutates
/// `WZ_POSTGRES_*`/`WZ_REDIS_*` env vars — `std::env` is process-global
/// and `cargo test` runs every test in this crate's binary concurrently
/// by default, so `config`'s tests transiently clearing/resetting these
/// vars can otherwise race `pool`'s/`migrate`'s real-connection tests
/// reading them via `from_env()`. Invisible as long as the latter stayed
/// `#[ignore]`d and never ran alongside `config`'s tests in the same
/// process; surfaced for real the first time CI ran the whole workspace
/// with `--include-ignored` (see the commit that added this).
#[cfg(test)]
pub(crate) mod test_env_lock {
    use std::sync::{Mutex, MutexGuard};

    static LOCK: Mutex<()> = Mutex::new(());

    /// Recovers from a poisoned lock rather than propagating the
    /// poison — this lock only ever guards plain env var
    /// reads/writes, no shared data an earlier panicking test could
    /// have left inconsistent, so one test's unrelated assertion
    /// failure (while it happened to hold this lock) shouldn't cascade
    /// into spurious `PoisonError` failures in every other test that
    /// also uses it.
    pub fn acquire() -> MutexGuard<'static, ()> {
        LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}
