//! Accounts, credentials, and session tokens, behind a pluggable provider interface.
//!
//! Design: docs/PROPOSAL.md ("Auth Provider Architecture") and docs/specs/Auth_Spec.md.

pub mod gateway_protocol;
pub mod password;
pub mod postgres_role_store;
pub mod postgres_store;
pub mod provider;
pub mod roles;
pub mod session;
pub mod store;
pub mod username_password;

pub use postgres_role_store::PostgresAccountRoleStore;
pub use postgres_store::PostgresAccountStore;
pub use provider::{AuthProvider, Credentials, Session};
pub use roles::{AccountRoleStore, InMemoryAccountRoleStore};
pub use session::SessionManager;
pub use store::{Account, AccountStore, InMemoryAccountStore};
pub use username_password::UsernamePasswordProvider;
