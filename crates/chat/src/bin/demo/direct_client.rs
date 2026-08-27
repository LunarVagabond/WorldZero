//! Direct-mode transport for the demo client: talks straight to
//! Postgres/Redis via `chat::ChannelStore`/`ChatBus`, bypassing the
//! gateway entirely. Selected with `--no-gateway` — useful for iterating
//! on chat's own logic without a `bin/gateway_server` running.

use std::collections::HashMap;
use std::io::Write;

use chat::demo_support::{find_or_create_demo_account, find_or_create_named_channel};
use chat::{ChannelStore, ChatBus};
use common::config::{PostgresConfig, RedisConfig};
use common::id::{AccountId, ChannelId};
use common::pool::{PoolOptions, postgres_pool, redis_pool};
use common::{Error, Result};
use futures_util::StreamExt;
use tokio::io::AsyncBufReadExt;
use tokio::sync::mpsc;

use super::commands::{self, Command};

const DEFAULT_CHANNEL: &str = "demo";

pub async fn run(username: &str) -> Result<()> {
    let pg_config = PostgresConfig::from_env()?;
    let pool = postgres_pool(&pg_config, PoolOptions::default()).await?;
    let redis_config = RedisConfig::from_env()?;
    let redis = redis_pool(&redis_config, PoolOptions::default())?;

    let store = ChannelStore::new(pool.clone());
    // Same independent WZ_CHAT_PERSISTENCE_ENABLED toggle `server`/
    // `bin/gateway_server` read (#174, docs/specs/Chat_Spec.md, "Durable
    // message log") — off by default, this demo tool doesn't force it on.
    let message_log = if chat::persistence_enabled_from_env()? {
        Some(std::sync::Arc::new(chat::MessageLog::new(pool.clone())))
    } else {
        None
    };
    let bus = ChatBus::new(redis, redis_config, message_log);
    let account_id = find_or_create_demo_account(&pool, username).await?;

    println!("Connected directly (no gateway) as {username}.");
    println!("{}", commands::HELP_TEXT);

    let (incoming_tx, mut incoming_rx) = mpsc::unbounded_channel::<(String, String)>();
    let mut joined: HashMap<String, (ChannelId, tokio::task::JoinHandle<()>)> = HashMap::new();

    join(
        DEFAULT_CHANNEL.to_string(),
        account_id,
        &pool,
        &store,
        &bus,
        &incoming_tx,
        &mut joined,
    )
    .await?;
    let mut current = Some(DEFAULT_CHANNEL.to_string());
    println!("joined #{DEFAULT_CHANNEL} (now current)");

    let mut lines = tokio::io::BufReader::new(tokio::io::stdin()).lines();
    prompt();

    loop {
        tokio::select! {
            Some((channel, line)) = incoming_rx.recv() => {
                println!("[{channel}] {line}");
                prompt();
            }
            line = lines.next_line() => {
                let Some(line) = line.map_err(|e| Error::wrap("chat", "failed to read stdin", e))? else {
                    break;
                };
                let Some(command) = commands::parse(&line) else {
                    prompt();
                    continue;
                };

                match command {
                    Command::Send(body) => {
                        match current.as_ref().and_then(|c| joined.get(c).map(|(id, _)| *id)) {
                            Some(channel_id) => {
                                let display = format!("{username}: {body}");
                                if let Err(e) = bus.publish(&store, channel_id, account_id, &display).await {
                                    println!("send failed: {e}");
                                }
                            }
                            None => println!("not in a channel yet — /join <name> first"),
                        }
                    }
                    Command::Join(name) => {
                        if name.is_empty() {
                            println!("usage: /join <name>");
                        } else if joined.contains_key(name) {
                            println!("already joined #{name}");
                        } else {
                            join(name.to_string(), account_id, &pool, &store, &bus, &incoming_tx, &mut joined).await?;
                            current = Some(name.to_string());
                            println!("joined #{name} (now current)");
                        }
                    }
                    Command::Leave(name) => {
                        if name.is_empty() {
                            println!("usage: /leave <name>");
                        } else if let Some((channel_id, handle)) = joined.remove(name) {
                            handle.abort();
                            store.leave(channel_id, account_id).await?;
                            if current.as_deref() == Some(name) {
                                current = joined.keys().next().cloned();
                            }
                            println!("left #{name}");
                        } else {
                            println!("not joined to #{name}");
                        }
                    }
                    Command::Switch(name) => {
                        if name.is_empty() {
                            println!("usage: /switch <name>");
                        } else if joined.contains_key(name) {
                            current = Some(name.to_string());
                            println!("now sending to #{name}");
                        } else {
                            println!("not joined to #{name} — /join it first");
                        }
                    }
                    Command::Who => {
                        for name in joined.keys() {
                            let marker = if current.as_deref() == Some(name.as_str()) { "*" } else { " " };
                            println!("{marker} {name}");
                        }
                    }
                    Command::Help => println!("{}", commands::HELP_TEXT),
                    Command::Unknown(cmd) => println!("unknown command: /{cmd} — try /help"),
                }
                prompt();
            }
        }
    }

    for (_, (_, handle)) in joined {
        handle.abort();
    }
    Ok(())
}

fn prompt() {
    print!("> ");
    std::io::stdout().flush().ok();
}

async fn join(
    name: String,
    account_id: AccountId,
    pool: &sqlx::PgPool,
    store: &ChannelStore,
    bus: &ChatBus,
    incoming_tx: &mpsc::UnboundedSender<(String, String)>,
    joined: &mut HashMap<String, (ChannelId, tokio::task::JoinHandle<()>)>,
) -> Result<()> {
    let channel_id = find_or_create_named_channel(pool, store, account_id, &name).await?;
    store.join(channel_id, account_id).await?;

    let mut incoming = Box::pin(bus.subscribe(channel_id).await?);
    let incoming_tx = incoming_tx.clone();
    let channel_name = name.clone();
    let handle = tokio::spawn(async move {
        while let Some(message) = incoming.next().await {
            if message.sender_account_id != account_id
                && incoming_tx
                    .send((channel_name.clone(), message.body))
                    .is_err()
            {
                break;
            }
        }
    });

    joined.insert(name, (channel_id, handle));
    Ok(())
}
