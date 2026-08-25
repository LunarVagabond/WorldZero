//! `cargo run -p chat --bin gateway_server` (or `make chat-server`) —
//! standalone dev/demo server that terminates the real `gateway` TCP+TLS
//! transport, authenticates each connection via `auth`'s real
//! username/password provider before trusting its identity
//! (docs/specs/Auth_Spec.md, "Gateway handshake"), and routes decoded
//! chat envelopes into `chat`'s `ChannelStore`/`ChatBus`
//! (docs/specs/Chat_Spec.md, "Gateway demo integration"). Not the phase-1
//! combined `server` binary (crates/server) — chat isn't in that phase
//! yet per its own roadmap note; this is a standalone entry point for
//! exercising auth+gateway+chat together ahead of that.
//!
//! Start this first, then point `bin/demo` clients at it (defaults to
//! gateway mode — `cargo run -p chat --bin demo -- <username> --password
//! <pw> [--register]`, or `make chat NAME=<username>`). Needs
//! WZ_POSTGRES_*/WZ_REDIS_* (`.env`). Listens on `WZ_CHAT_GATEWAY_ADDR`
//! (default `127.0.0.1:7800`).

use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use auth::AuthProvider;
use chat::gateway_protocol::{ClientMessage, ServerMessage};
use chat::{ChannelStore, ChatBus};
use common::config::{PostgresConfig, RedisConfig};
use common::id::{AccountId, ChannelId};
use common::pool::{PoolOptions, postgres_pool, redis_pool};
use common::{Error, Result};
use futures_util::{SinkExt, StreamExt};
use gateway::Envelope;
use tokio::sync::mpsc;
use tokio_util::codec::Framed;

const DEFAULT_ADDR: &str = "127.0.0.1:7800";

/// Display name per connected account, populated once a session's auth
/// handshake succeeds — a forwarded `Chat` message needs the sender's
/// real (authenticated) `accounts.username`, not a re-derived lookup on
/// every message.
type Usernames = Arc<RwLock<HashMap<AccountId, String>>>;

type ChatStream =
    Framed<tokio_rustls::server::TlsStream<tokio::net::TcpStream>, gateway::EnvelopeCodec>;
type ChatSink = futures_util::stream::SplitSink<ChatStream, Envelope>;

#[tokio::main]
async fn main() {
    common::logging::init();

    let addr = std::env::var("WZ_CHAT_GATEWAY_ADDR").unwrap_or_else(|_| DEFAULT_ADDR.to_string());

    let pg_config = PostgresConfig::from_env().expect("WZ_POSTGRES_* env vars set");
    let pool = postgres_pool(&pg_config, PoolOptions::default())
        .await
        .expect("failed to connect to Postgres");
    let redis_config = RedisConfig::from_env().expect("WZ_REDIS_* env vars set");
    let redis =
        redis_pool(&redis_config, PoolOptions::default()).expect("failed to build Redis pool");

    let account_store: Arc<dyn auth::AccountStore> =
        Arc::new(auth::PostgresAccountStore::new(pool.clone()));
    let sessions = auth::SessionManager::new(redis.clone());
    let auth_provider = Arc::new(auth::UsernamePasswordProvider::new(account_store, sessions));

    let store = Arc::new(ChannelStore::new(pool.clone()));
    let bus = Arc::new(ChatBus::new(redis, redis_config));
    let usernames: Usernames = Arc::new(RwLock::new(HashMap::new()));

    let config_dir = common::config::config_dir();
    let cert = gateway::tcp::init_and_log_fingerprint(&config_dir)
        .expect("failed to load/generate the gateway's TLS certificate");
    let acceptor = gateway::tcp::build_tls_acceptor(&cert).expect("failed to build TLS acceptor");

    let (local_addr, incoming) = gateway::tcp::listen(&addr, acceptor)
        .await
        .expect("failed to bind the gateway TCP listener");
    tracing::info!(%local_addr, "chat gateway demo server listening");

    let mut incoming = Box::pin(incoming);
    while let Some(framed) = incoming.next().await {
        let pool = pool.clone();
        let store = store.clone();
        let bus = bus.clone();
        let usernames = usernames.clone();
        let auth_provider = auth_provider.clone();
        tokio::spawn(async move {
            if let Err(e) = handle_session(framed, pool, store, bus, usernames, auth_provider).await
            {
                tracing::warn!(error = %e, "chat gateway session ended with an error");
            }
        });
    }
}

async fn handle_session(
    framed: ChatStream,
    pool: sqlx::PgPool,
    store: Arc<ChannelStore>,
    bus: Arc<ChatBus>,
    usernames: Usernames,
    auth_provider: Arc<auth::UsernamePasswordProvider>,
) -> Result<()> {
    let (mut sink, mut stream) = framed.split();
    let (outgoing_tx, mut outgoing_rx) = mpsc::unbounded_channel::<Envelope>();

    let Some(frame) = stream.next().await else {
        return Ok(());
    };
    let envelope = frame.map_err(|e| Error::wrap("chat", "connection error", e))?;
    let auth_message = match auth::gateway_protocol::ClientMessage::from_envelope(&envelope) {
        Ok(m) => m,
        Err(e) => {
            send_auth(
                &mut sink,
                &auth::gateway_protocol::ServerMessage::Error {
                    message: e.to_string(),
                },
            )
            .await?;
            return Ok(());
        }
    };

    let (account_id, username, session_token) =
        match authenticate(auth_message, &auth_provider).await {
            Ok(result) => result,
            Err(e) => {
                send_auth(
                    &mut sink,
                    &auth::gateway_protocol::ServerMessage::Error {
                        message: e.to_string(),
                    },
                )
                .await?;
                return Ok(());
            }
        };
    usernames
        .write()
        .unwrap()
        .insert(account_id, username.clone());

    send_auth(
        &mut sink,
        &auth::gateway_protocol::ServerMessage::Authenticated {
            account_id,
            username,
            session_token,
        },
    )
    .await?;

    // No auto-join here — the client drives its own initial `Join` for
    // the default channel and waits for the `Joined` confirmation, so
    // there's no window where a client could send before its default
    // channel exists server-side.
    let mut joined: HashMap<String, (ChannelId, tokio::task::JoinHandle<()>)> = HashMap::new();

    loop {
        tokio::select! {
            maybe_frame = stream.next() => {
                let Some(frame) = maybe_frame else { break };
                let Ok(envelope) = frame else { break };
                let message = match ClientMessage::from_envelope(&envelope) {
                    Ok(m) => m,
                    Err(e) => {
                        send(&mut sink, &ServerMessage::Error { message: e.to_string() }).await?;
                        continue;
                    }
                };

                match message {
                    ClientMessage::Join { channel } => {
                        if joined.contains_key(&channel) {
                            send(&mut sink, &ServerMessage::Error {
                                message: format!("already joined #{channel}"),
                            }).await?;
                        } else {
                            match join_channel(&channel, account_id, &pool, &store, &bus, &usernames, &outgoing_tx, &mut joined).await {
                                Ok(()) => {
                                    let channel_id = joined[&channel].0;
                                    send(&mut sink, &ServerMessage::Joined { channel_id, channel }).await?;
                                }
                                Err(e) => {
                                    send(&mut sink, &ServerMessage::Error { message: e.to_string() }).await?;
                                }
                            }
                        }
                    }
                    ClientMessage::Leave { channel } => {
                        if let Some((channel_id, handle)) = joined.remove(&channel) {
                            handle.abort();
                            match store.leave(channel_id, account_id).await {
                                Ok(()) => send(&mut sink, &ServerMessage::Left { channel }).await?,
                                Err(e) => send(&mut sink, &ServerMessage::Error { message: e.to_string() }).await?,
                            }
                        } else {
                            send(&mut sink, &ServerMessage::Error {
                                message: format!("not joined to #{channel}"),
                            }).await?;
                        }
                    }
                    ClientMessage::Send { channel_id, body } => {
                        if let Err(e) = bus.publish(&store, channel_id, account_id, &body).await {
                            send(&mut sink, &ServerMessage::Error { message: e.to_string() }).await?;
                        }
                    }
                }
            }
            Some(envelope) = outgoing_rx.recv() => {
                if sink.send(envelope).await.is_err() {
                    break;
                }
            }
        }
    }

    for (_, (_, handle)) in joined {
        handle.abort();
    }
    Ok(())
}

/// Resolves/joins `channel_name` for `account_id` and spawns a task that
/// forwards every pub/sub message published to it back out over
/// `outgoing_tx` as a `Chat` envelope — the client's own messages are
/// filtered out (it already knows what it sent). Tracked in `joined` so a
/// later `Leave` can cancel the forwarding task.
#[allow(clippy::too_many_arguments)]
async fn join_channel(
    channel_name: &str,
    account_id: AccountId,
    pool: &sqlx::PgPool,
    store: &Arc<ChannelStore>,
    bus: &Arc<ChatBus>,
    usernames: &Usernames,
    outgoing_tx: &mpsc::UnboundedSender<Envelope>,
    joined: &mut HashMap<String, (ChannelId, tokio::task::JoinHandle<()>)>,
) -> Result<()> {
    let channel_id =
        chat::demo_support::find_or_create_named_channel(pool, store, account_id, channel_name)
            .await?;
    store.join(channel_id, account_id).await?;

    let mut incoming = Box::pin(bus.subscribe(channel_id).await?);
    let outgoing_tx = outgoing_tx.clone();
    let usernames = usernames.clone();
    let channel_name_owned = channel_name.to_string();
    let handle = tokio::spawn(async move {
        while let Some(message) = incoming.next().await {
            if message.sender_account_id == account_id {
                continue;
            }
            let sender = usernames
                .read()
                .unwrap()
                .get(&message.sender_account_id)
                .cloned()
                .unwrap_or_else(|| "unknown".to_string());
            let server_message = ServerMessage::Chat {
                channel_id,
                channel: channel_name_owned.clone(),
                sender,
                body: message.body,
            };
            let Ok(envelope) = server_message.into_envelope() else {
                continue;
            };
            if outgoing_tx.send(envelope).is_err() {
                break;
            }
        }
    });

    joined.insert(channel_name.to_string(), (channel_id, handle));
    Ok(())
}

async fn send(sink: &mut ChatSink, message: &ServerMessage) -> Result<()> {
    let envelope = message.into_envelope()?;
    sink.send(envelope)
        .await
        .map_err(|e| Error::wrap("chat", "failed to send to client", e))
}

async fn send_auth(
    sink: &mut ChatSink,
    message: &auth::gateway_protocol::ServerMessage,
) -> Result<()> {
    let envelope = message.into_envelope()?;
    sink.send(envelope)
        .await
        .map_err(|e| Error::wrap("chat", "failed to send to client", e))
}

/// Runs the client's `Register`/`Login`/`Resume` request against the
/// real `auth` provider and, on success, returns the account_id,
/// username, and session_token everything downstream in this connection
/// trusts — `Resume` (#195) reuses the token the client presented
/// (renewed in place by `SessionManager::resolve`) instead of minting a
/// new one, same as `server::session`'s own `authenticate`.
async fn authenticate(
    message: auth::gateway_protocol::ClientMessage,
    provider: &auth::UsernamePasswordProvider,
) -> Result<(AccountId, String, String)> {
    use auth::gateway_protocol::ClientMessage as AuthMessage;

    let (account_id, username) = match message {
        AuthMessage::Register { username, password } => {
            let account_id = provider.register(&username, &password).await?;
            (account_id, username)
        }
        AuthMessage::Login { username, password } => {
            let credentials = auth::Credentials::new(
                serde_json::json!({ "username": username, "password": password }),
            );
            let account_id = provider.verify_credentials(&credentials).await?;
            (account_id, username)
        }
        AuthMessage::Resume { session_token } => {
            let (account_id, username) = provider.resume(&session_token).await?;
            return Ok((account_id, username, session_token));
        }
    };

    let session = provider.issue_session(account_id).await?;
    Ok((account_id, username, session.token))
}
