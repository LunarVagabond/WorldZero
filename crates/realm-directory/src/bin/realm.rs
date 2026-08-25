//! `cargo run -p realm-directory --bin realm -- <command> [args...]`
//! (or `make realm ARGS="..."`) — a minimal CLI over `RealmStore` so a
//! self-hoster can create/inspect/manage realms without writing Rust
//! (docs/specs/Realm_Character_Policy_Spec.md's "The flag",
//! docs/specs/Data_Model_Spec.md's `realms`/`realm_zones` tables).
//! Needs `WZ_POSTGRES_*` (`.env` is loaded automatically by `make`).
//!
//! Same hand-rolled positional-args style as `common`'s `migrate` bin and
//! `content`'s `validate` bin — no argument-parsing crate anywhere in
//! this workspace yet, not worth adding for a handful of subcommands.

use std::process::ExitCode;

use common::Result;
use common::config::PostgresConfig;
use common::id::RealmId;
use common::pool::{PoolOptions, postgres_pool};
use realm_directory::{OpenOrBound, Realm, RealmStore};

fn usage() -> ! {
    eprintln!(
        "usage: realm <command> [args...]\n\
         \n\
         commands:\n\
         \x20\x20create <name> <open|bound>\n\
         \x20\x20ensure <name> <open|bound>\n\
         \x20\x20list\n\
         \x20\x20get <realm-id>\n\
         \x20\x20update <realm-id> <name> <open|bound>\n\
         \x20\x20delete <realm-id>\n\
         \x20\x20assign-zone <realm-id> <zone-id>\n\
         \x20\x20unassign-zone <zone-id>"
    );
    std::process::exit(2);
}

fn parse_policy(value: &str) -> OpenOrBound {
    match value {
        "open" => OpenOrBound::Open,
        "bound" => OpenOrBound::Bound,
        other => {
            eprintln!("policy must be \"open\" or \"bound\", got {other:?}");
            std::process::exit(2);
        }
    }
}

fn parse_realm_id(value: &str) -> RealmId {
    value.parse().unwrap_or_else(|_| {
        eprintln!("{value:?} is not a valid realm id (expected a UUID)");
        std::process::exit(2);
    })
}

/// `common::Error`'s `Display` deliberately only shows its own
/// crate-prefixed message, not the wrapped source's — that's the right
/// call for it (an ordinary `{e}` inside another `Error::wrap` shouldn't
/// duplicate the whole chain every layer down), but this CLI is the
/// actual terminal consumer, so it prints the full chain itself rather
/// than leaving an operator staring at "failed to list realms" with no
/// way to see the real Postgres error underneath.
fn print_error_chain(err: &(dyn std::error::Error + 'static)) {
    eprintln!("{err}");
    let mut source = err.source();
    while let Some(e) = source {
        eprintln!("  caused by: {e}");
        source = e.source();
    }
}

fn print_realm(realm: &Realm) {
    let policy = match realm.open_or_bound {
        OpenOrBound::Open => "open",
        OpenOrBound::Bound => "bound",
    };
    println!("{}  {policy}  {}", realm.id, realm.name);
}

#[tokio::main]
async fn main() -> ExitCode {
    common::logging::init();

    let mut args = std::env::args().skip(1);
    let Some(command) = args.next() else { usage() };
    const KNOWN_COMMANDS: &[&str] = &[
        "create",
        "ensure",
        "list",
        "get",
        "update",
        "delete",
        "assign-zone",
        "unassign-zone",
    ];
    if !KNOWN_COMMANDS.contains(&command.as_str()) {
        usage();
    }

    let config = PostgresConfig::from_env().expect("WZ_POSTGRES_* env vars set");
    let pool = postgres_pool(&config, PoolOptions::default())
        .await
        .expect("failed to connect to Postgres");
    let store = RealmStore::new(pool);

    let result: Result<()> = match command.as_str() {
        "create" => {
            let (Some(name), Some(policy)) = (args.next(), args.next()) else {
                usage()
            };
            store.create(&name, parse_policy(&policy)).await.map(|id| {
                println!("{id}");
            })
        }
        // `create`, but idempotent by name — `make quickstart` (#136) needs
        // a "give me a realm to point WZ_REALM_ID at, reusing the same one
        // on every re-run" primitive, and `create` alone always mints a
        // new realm. Matches by name only (not policy) — a realm found
        // under `name` is returned as-is even if its policy no longer
        // matches `policy`, same as every other idempotent-if-exists tool
        // in this codebase (e.g. `zone.manifest.yaml` is only copied if
        // missing, never overwritten to match a changed example).
        "ensure" => {
            let (Some(name), Some(policy)) = (args.next(), args.next()) else {
                usage()
            };
            match store.list().await {
                Ok(realms) => match realms.into_iter().find(|r| r.name == name) {
                    Some(existing) => {
                        println!("{}", existing.id);
                        Ok(())
                    }
                    None => store.create(&name, parse_policy(&policy)).await.map(|id| {
                        println!("{id}");
                    }),
                },
                Err(e) => Err(e),
            }
        }
        "list" => store.list().await.map(|realms| {
            for realm in &realms {
                print_realm(realm);
            }
        }),
        "get" => {
            let Some(realm_id) = args.next() else { usage() };
            let realm_id = parse_realm_id(&realm_id);
            match store.get(realm_id).await {
                Ok(Some(realm)) => {
                    print_realm(&realm);
                    match store.zones_for_realm(realm_id).await {
                        Ok(zones) if zones.is_empty() => {
                            println!("  (no zones assigned)");
                            Ok(())
                        }
                        Ok(zones) => {
                            for zone in zones {
                                println!("  zone: {zone}");
                            }
                            Ok(())
                        }
                        Err(e) => Err(e),
                    }
                }
                Ok(None) => {
                    eprintln!("no realm with id {realm_id}");
                    return ExitCode::FAILURE;
                }
                Err(e) => Err(e),
            }
        }
        "update" => {
            let (Some(realm_id), Some(name), Some(policy)) =
                (args.next(), args.next(), args.next())
            else {
                usage()
            };
            store
                .update(parse_realm_id(&realm_id), &name, parse_policy(&policy))
                .await
        }
        "delete" => {
            let Some(realm_id) = args.next() else { usage() };
            store.delete(parse_realm_id(&realm_id)).await
        }
        "assign-zone" => {
            let (Some(realm_id), Some(zone_id)) = (args.next(), args.next()) else {
                usage()
            };
            store.assign_zone(parse_realm_id(&realm_id), &zone_id).await
        }
        "unassign-zone" => {
            let Some(zone_id) = args.next() else { usage() };
            store.unassign_zone(&zone_id).await
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
