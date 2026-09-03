//! `cargo run -p auth --bin role -- <command> [args...]` (or `make role
//! ARGS="..."`) — a minimal CLI over `AccountRoleStore` so a self-hoster
//! can grant/revoke/list account roles without writing Rust or hand-
//! rolling a throwaway Postgres client (docs/specs/Auth_Spec.md, #114/
//! #124). Needs `WZ_POSTGRES_*` (`.env` is loaded automatically by
//! `make`). Mirrors `realm-directory`'s `realm` bin — same hand-rolled
//! positional-args style, no argument-parsing crate.
//!
//! Takes a username, not an account id — `AccountRoleStore` itself is
//! keyed by `AccountId`, but a human operator thinks in usernames, so
//! this resolves via `AccountStore::find_by_username` first.

use std::process::ExitCode;

use auth::{AccountRoleStore, AccountStore, PostgresAccountRoleStore, PostgresAccountStore};
use common::Result;
use common::config::PostgresConfig;
use common::id::AccountId;
use common::pool::{PoolOptions, postgres_pool};

fn usage() -> ! {
    eprintln!(
        "usage: role <command> [args...]\n\
         \n\
         commands:\n\
         \x20\x20grant <username> <role>\n\
         \x20\x20revoke <username> <role>\n\
         \x20\x20list <username>"
    );
    std::process::exit(2);
}

/// Same rationale as `realm`'s own `print_error_chain`: this CLI is the
/// terminal consumer, so it prints the full error chain rather than
/// leaving an operator staring at one crate-prefixed line with no way to
/// see the real Postgres error underneath.
fn print_error_chain(err: &(dyn std::error::Error + 'static)) {
    eprintln!("{err}");
    let mut source = err.source();
    while let Some(e) = source {
        eprintln!("  caused by: {e}");
        source = e.source();
    }
}

async fn resolve_account(accounts: &PostgresAccountStore, username: &str) -> Result<AccountId> {
    match accounts.find_by_username(username).await? {
        Some(account) => Ok(account.id),
        None => {
            eprintln!("no account with username {username:?}");
            std::process::exit(1);
        }
    }
}

#[tokio::main]
async fn main() -> ExitCode {
    common::logging::init();

    let mut args = std::env::args().skip(1);
    let Some(command) = args.next() else { usage() };
    const KNOWN_COMMANDS: &[&str] = &["grant", "revoke", "list"];
    if !KNOWN_COMMANDS.contains(&command.as_str()) {
        usage();
    }

    let config = PostgresConfig::from_env().expect("WZ_POSTGRES_* env vars set");
    let pool = postgres_pool(&config, PoolOptions::default())
        .await
        .expect("failed to connect to Postgres");
    let accounts = PostgresAccountStore::new(pool.clone());
    let roles = PostgresAccountRoleStore::new(pool);

    let result: Result<()> = match command.as_str() {
        "grant" => {
            let (Some(username), Some(role)) = (args.next(), args.next()) else {
                usage()
            };
            let account_id = resolve_account(&accounts, &username).await.unwrap();
            roles.grant_role(account_id, &role).await
        }
        "revoke" => {
            let (Some(username), Some(role)) = (args.next(), args.next()) else {
                usage()
            };
            let account_id = resolve_account(&accounts, &username).await.unwrap();
            roles.revoke_role(account_id, &role).await
        }
        "list" => {
            let Some(username) = args.next() else { usage() };
            let account_id = resolve_account(&accounts, &username).await.unwrap();
            roles.roles_for(account_id).await.map(|mut held| {
                held.sort();
                if held.is_empty() {
                    println!("(no roles)");
                } else {
                    for role in &held {
                        println!("{role}");
                    }
                }
            })
        }
        _ => usage(),
    };

    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            print_error_chain(&e);
            ExitCode::FAILURE
        }
    }
}
