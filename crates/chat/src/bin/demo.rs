//! `cargo run -p chat --bin demo -- <username>` — a tiny interactive chat
//! client. Run it in two terminals with different usernames, both join the
//! same "demo" group channel, and watch messages appear live in each
//! other's terminal. Needs WZ_POSTGRES_*/WZ_REDIS_* set (`.env`).

use std::io::{BufRead, Write};

use chat::{ChannelStore, ChatBus};
use common::config::{PostgresConfig, RedisConfig};
use common::id::AccountId;
use common::pool::{PoolOptions, postgres_pool, redis_pool};
use futures_util::StreamExt;

#[tokio::main]
async fn main() {
    let Some(username) = std::env::args().nth(1) else {
        eprintln!("usage: demo <username>");
        std::process::exit(2);
    };

    let pg_config = PostgresConfig::from_env().expect("WZ_POSTGRES_* env vars set");
    let pool = postgres_pool(&pg_config, PoolOptions::default())
        .await
        .expect("failed to connect to Postgres");
    let redis_config = RedisConfig::from_env().expect("WZ_REDIS_* env vars set");
    let redis =
        redis_pool(&redis_config, PoolOptions::default()).expect("failed to build Redis pool");

    let store = ChannelStore::new(pool.clone());
    let bus = ChatBus::new(redis, redis_config);

    // A stable account per username, so re-running with the same name
    // rejoins the same demo identity instead of creating a new one.
    let account_id = find_or_create_demo_account(&pool, &username).await;
    let channel = find_or_create_demo_channel(&pool, &store, account_id).await;
    store.join(channel, account_id).await.ok(); // already a member if we just created it — fine either way

    println!("Joined #demo as {username}. Type a message and press enter (Ctrl+C to quit).");

    let mut incoming = Box::pin(bus.subscribe(channel).await.expect("failed to subscribe"));
    tokio::spawn(async move {
        while let Some(message) = incoming.next().await {
            if message.sender_account_id != account_id {
                println!("{}", message.body);
            }
        }
    });

    let stdin = std::io::stdin();
    for line in stdin.lock().lines() {
        let Ok(line) = line else { break };
        if line.trim().is_empty() {
            continue;
        }
        let body = format!("{username}: {line}");
        if let Err(e) = bus.publish(&store, channel, account_id, &body).await {
            eprintln!("send failed: {e}");
        }
        print!("> ");
        std::io::stdout().flush().ok();
    }
}

/// `ChannelStore::create_group` isn't idempotent (a player naming a new
/// group channel "demo" twice is meant to create two channels) — this demo
/// wants everyone to land in *the same* channel across separate runs, so
/// it looks one up by name first instead of always creating.
async fn find_or_create_demo_channel(
    pool: &sqlx::PgPool,
    store: &ChannelStore,
    creator: AccountId,
) -> common::id::ChannelId {
    if let Some(id) = sqlx::query_scalar::<_, uuid::Uuid>(
        "SELECT id FROM chat_channels WHERE channel_type = 'group' AND name = 'demo'",
    )
    .fetch_optional(pool)
    .await
    .expect("failed to look up the demo channel")
    {
        return common::id::ChannelId::from_uuid(id);
    }

    store
        .create_group(creator, "demo")
        .await
        .expect("failed to create the demo channel")
}

async fn find_or_create_demo_account(pool: &sqlx::PgPool, username: &str) -> AccountId {
    let demo_username = format!("chat-demo-{username}");

    if let Some(id) =
        sqlx::query_scalar::<_, uuid::Uuid>("SELECT id FROM accounts WHERE username = $1")
            .bind(&demo_username)
            .fetch_optional(pool)
            .await
            .expect("failed to look up demo account")
    {
        return AccountId::from_uuid(id);
    }

    let id = AccountId::new();
    sqlx::query("INSERT INTO accounts (id, username, password_hash) VALUES ($1, $2, 'demo')")
        .bind(id.as_uuid())
        .bind(&demo_username)
        .execute(pool)
        .await
        .expect("failed to create demo account");
    id
}
