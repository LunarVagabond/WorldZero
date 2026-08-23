//! Cross-cutting code shared by every other crate in the workspace.
//!
//! **Scope, deliberately tight — read before adding to this crate:** this exists
//! to avoid duplicating (or worse, letting drift) the handful of things every
//! other crate genuinely needs the same version of:
//!
//! - The shared `tracing` init/formatter setup implementing the
//!   `<TIMESTAMP> <LEVEL> <SOURCE> <MESSAGE>` log format and severity
//!   convention, plus Prometheus-compatible metrics and the `/metrics`
//!   HTTP endpoint (see docs/specs/Observability_Spec.md).
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
pub mod id;
pub mod logging;
pub mod metrics;
pub mod migrate;
pub mod pool;

pub use error::{Error, Result, ResultExt};
