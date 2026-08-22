//! Chat's gateway-routed messages, wired into the combined `server`
//! process's own per-connection session loop (#104 — the wiring #92 left
//! as a documented gap, since #92 only added the runtime-toggle
//! mechanism). Adapted from chat's standalone demo entry point
//! (`crates/chat/src/bin/gateway_server.rs`) to run inside `server`'s
//! single session loop alongside world/plugin message routing, rather
//! than as its own separate listener/process — one gateway connection,
//! one auth handshake, every message_type dispatched from the same loop
//! (`crate::session::handle_session`).

use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use chat::gateway_protocol::{ClientMessage, ServerMessage};
use chat::{ChannelStore, ChatBus};
use common::Result;
use common::id::{AccountId, ChannelId};
use gateway::Envelope;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

/// Display name per connected account — a forwarded `Chat` message needs
/// the sender's real (authenticated) username, not a re-derived lookup on
/// every message. Shared across every session; populated once per
/// connection, right after that connection's (shared, already-happened)
/// auth handshake succeeds.
pub type Usernames = Arc<RwLock<HashMap<AccountId, String>>>;

/// One connection's currently-joined channels — a forwarder task per
/// channel relays pub/sub messages back out over that session's own
/// outgoing envelope channel. Tracked here so `Leave` (or disconnect) can
/// cancel the right task.
pub type JoinedChannels = HashMap<String, (ChannelId, JoinHandle<()>)>;

/// Constructed once at server startup when `ServicesConfig::chat_enabled`
/// — every session that gets one just borrows this shared state, nothing
/// per-connection is constructed. `None` end to end (in `SessionDeps`)
/// when chat is disabled, so a disabled service touches none of this.
#[derive(Clone)]
pub struct ChatDeps {
    pub pool: sqlx::PgPool,
    pub store: Arc<ChannelStore>,
    pub bus: Arc<ChatBus>,
    pub usernames: Usernames,
}

/// Handles one decoded chat envelope for an already-authenticated
/// connection. Returns the direct reply to send back over this
/// connection's own sink, if any — a successful `Send` has none (the
/// sender doesn't get an echo of their own message; `Join`'s forwarder
/// task filters out messages this account itself sent, matching the
/// standalone demo server's behavior).
pub async fn handle_message(
    message: ClientMessage,
    account_id: AccountId,
    chat: &ChatDeps,
    outgoing_tx: &mpsc::UnboundedSender<Envelope>,
    joined: &mut JoinedChannels,
) -> Option<ServerMessage> {
    match message {
        ClientMessage::Join { channel } => {
            if joined.contains_key(&channel) {
                return Some(ServerMessage::Error {
                    message: format!("already joined #{channel}"),
                });
            }
            match join_channel(&channel, account_id, chat, outgoing_tx, joined).await {
                Ok(()) => {
                    let channel_id = joined[&channel].0;
                    Some(ServerMessage::Joined {
                        channel_id,
                        channel,
                    })
                }
                Err(e) => Some(ServerMessage::Error {
                    message: e.to_string(),
                }),
            }
        }
        ClientMessage::Leave { channel } => {
            if let Some((channel_id, handle)) = joined.remove(&channel) {
                handle.abort();
                match chat.store.leave(channel_id, account_id).await {
                    Ok(()) => Some(ServerMessage::Left { channel }),
                    Err(e) => Some(ServerMessage::Error {
                        message: e.to_string(),
                    }),
                }
            } else {
                Some(ServerMessage::Error {
                    message: format!("not joined to #{channel}"),
                })
            }
        }
        ClientMessage::Send { channel_id, body } => {
            match chat
                .bus
                .publish(&chat.store, channel_id, account_id, &body)
                .await
            {
                Ok(()) => None,
                Err(e) => Some(ServerMessage::Error {
                    message: e.to_string(),
                }),
            }
        }
    }
}

/// Aborts every forwarder task for a connection that's shutting down —
/// harmless (and a no-op) if chat was never used on this connection.
pub fn abort_all(joined: JoinedChannels) {
    for (_, (_, handle)) in joined {
        handle.abort();
    }
}

/// Resolves/joins `channel_name` for `account_id` and spawns a task that
/// forwards every pub/sub message published to it back out over
/// `outgoing_tx` as a `Chat` envelope — the client's own messages are
/// filtered out (it already knows what it sent).
async fn join_channel(
    channel_name: &str,
    account_id: AccountId,
    chat: &ChatDeps,
    outgoing_tx: &mpsc::UnboundedSender<Envelope>,
    joined: &mut JoinedChannels,
) -> Result<()> {
    let channel_id = chat::demo_support::find_or_create_named_channel(
        &chat.pool,
        &chat.store,
        account_id,
        channel_name,
    )
    .await?;
    chat.store.join(channel_id, account_id).await?;

    let mut incoming = Box::pin(chat.bus.subscribe(channel_id).await?);
    let outgoing_tx = outgoing_tx.clone();
    let usernames = chat.usernames.clone();
    let channel_name_owned = channel_name.to_string();
    let handle = tokio::spawn(async move {
        use futures_util::StreamExt;

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
