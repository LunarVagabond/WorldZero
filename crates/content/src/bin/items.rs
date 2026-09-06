//! `cargo run -p content --bin items -- <command> [args...]`
//! (or `make items ARGS="..."`) — a minimal CLI over `ItemCatalogStore`
//! (#287) so a self-hoster/game dev can author the central item catalog
//! without writing Rust (docs/specs/Data_Model_Spec.md, "The item
//! catalog"). Needs `WZ_POSTGRES_*` (`.env` is loaded automatically by
//! `make`).
//!
//! Same hand-rolled positional-args style as `realm-directory`'s `realm`
//! bin and `auth`'s `role` bin — no argument-parsing crate anywhere in
//! this workspace, not worth adding for a handful of subcommands.

use std::process::ExitCode;

use common::Result;
use common::config::PostgresConfig;
use common::pool::{PoolOptions, postgres_pool};
use content::ItemCatalogStore;
use content::items::{DropSource, ItemCatalogEntry};

fn usage() -> ! {
    eprintln!(
        "usage: items <command> [args...]\n\
         \n\
         commands:\n\
         \x20\x20create <item-type> <display-name> [tag,tag,...]\n\
         \x20\x20ensure <item-type> <display-name> [tag,tag,...]\n\
         \x20\x20list\n\
         \x20\x20get <item-type>\n\
         \x20\x20delete <item-type>\n\
         \x20\x20link-drop-source <item-type> <zone-id> <spawn-table-id>\n\
         \x20\x20unlink-drop-source <item-type> <zone-id> <spawn-table-id>\n\
         \x20\x20drop-sources <item-type>"
    );
    std::process::exit(2);
}

fn parse_tags(value: Option<String>) -> Vec<String> {
    value
        .map(|v| {
            v.split(',')
                .map(str::to_string)
                .filter(|t| !t.is_empty())
                .collect()
        })
        .unwrap_or_default()
}

/// Same reasoning as `realm`'s own `print_error_chain`: `common::Error`'s
/// `Display` deliberately only shows its own crate-prefixed message, but
/// this CLI is the terminal consumer, so it prints the full chain.
fn print_error_chain(err: &(dyn std::error::Error + 'static)) {
    eprintln!("{err}");
    let mut source = err.source();
    while let Some(e) = source {
        eprintln!("  caused by: {e}");
        source = e.source();
    }
}

fn print_entry(entry: &ItemCatalogEntry) {
    println!(
        "{}  {:?}  tags=[{}]  metadata={}",
        entry.item_type,
        entry.display_name,
        entry.tags.join(", "),
        entry.metadata
    );
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
        "delete",
        "link-drop-source",
        "unlink-drop-source",
        "drop-sources",
    ];
    if !KNOWN_COMMANDS.contains(&command.as_str()) {
        usage();
    }

    let config = PostgresConfig::from_env().expect("WZ_POSTGRES_* env vars set");
    let pool = postgres_pool(&config, PoolOptions::default())
        .await
        .expect("failed to connect to Postgres");
    let store = ItemCatalogStore::new(pool);

    let result: Result<()> = match command.as_str() {
        // `create`, but idempotent — a second `ensure`/`create` call for
        // the same item_type is an upsert either way
        // (`ItemCatalogStore::register`'s own doc comment); `ensure` only
        // exists as a name so a quickstart-style script's intent reads
        // clearly, same distinction `realm create`/`realm ensure` draws.
        "create" | "ensure" => {
            let (Some(item_type), Some(display_name)) = (args.next(), args.next()) else {
                usage()
            };
            let tags = parse_tags(args.next());
            let entry = ItemCatalogEntry::new(item_type, display_name).with_tags(tags);
            store.register(&entry).await
        }
        "list" => store.list().await.map(|entries| {
            for entry in &entries {
                print_entry(entry);
            }
        }),
        "get" => {
            let Some(item_type) = args.next() else {
                usage()
            };
            match store.get(&item_type).await {
                Ok(Some(entry)) => {
                    print_entry(&entry);
                    Ok(())
                }
                Ok(None) => {
                    eprintln!("no item type {item_type:?} in the catalog");
                    return ExitCode::FAILURE;
                }
                Err(e) => Err(e),
            }
        }
        "delete" => {
            let Some(item_type) = args.next() else {
                usage()
            };
            store.delete(&item_type).await
        }
        "link-drop-source" => {
            let (Some(item_type), Some(zone_id), Some(spawn_table_id)) =
                (args.next(), args.next(), args.next())
            else {
                usage()
            };
            store
                .link_drop_source(&DropSource {
                    item_type,
                    zone_id,
                    spawn_table_id,
                })
                .await
        }
        "unlink-drop-source" => {
            let (Some(item_type), Some(zone_id), Some(spawn_table_id)) =
                (args.next(), args.next(), args.next())
            else {
                usage()
            };
            store
                .unlink_drop_source(&DropSource {
                    item_type,
                    zone_id,
                    spawn_table_id,
                })
                .await
        }
        "drop-sources" => {
            let Some(item_type) = args.next() else {
                usage()
            };
            store
                .drop_sources_for_item(&item_type)
                .await
                .map(|sources| {
                    if sources.is_empty() {
                        println!("(no drop sources declared for {item_type:?})");
                    }
                    for (zone_id, spawn_table_id) in sources {
                        println!("{zone_id}  {spawn_table_id}");
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
