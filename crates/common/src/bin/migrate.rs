//! `cargo run -p common --bin migrate -- up|down` — see the Makefile's
//! `make migrate`/`make migrate-down` targets, which are the intended
//! entry point.

use std::path::{Path, PathBuf};

fn migrations_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../db/migrations")
}

#[tokio::main]
async fn main() {
    common::logging::init();

    let direction = std::env::args().nth(1).unwrap_or_else(|| "up".to_string());

    let config = common::config::PostgresConfig::from_env().expect("WZ_POSTGRES_* env vars set");
    let pool = common::pool::postgres_pool(&config, common::pool::PoolOptions::default())
        .await
        .expect("failed to connect to Postgres");
    let dir = migrations_dir();

    match direction.as_str() {
        "up" => {
            common::migrate::migrate_up(&pool, &dir)
                .await
                .expect("migration failed");
            println!("migrations applied");
        }
        "down" => {
            common::migrate::migrate_down_one(&pool, &dir)
                .await
                .expect("revert failed");
            println!("last migration reverted");
        }
        other => {
            eprintln!("unknown direction: {other:?} — expected \"up\" or \"down\"");
            std::process::exit(1);
        }
    }
}
