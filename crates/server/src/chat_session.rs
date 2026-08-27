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
    /// Zone-scoped categories declared `auto_join: true` in `chat.yaml`
    /// (`chat::SystemChannelConfig::auto_join_zone_categories`, #186) —
    /// what `auto_join_zone_channels` below iterates on every zone entry.
    /// Empty (not `None`) when `chat.yaml` declares none, matching
    /// `SystemChannelConfig::from_config_dir_or_default`'s "no file means
    /// no declarations" default.
    pub auto_join_categories: Arc<Vec<String>>,
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

    let handle = spawn_forwarder(channel_id, channel_name, account_id, chat, outgoing_tx).await?;
    joined.insert(channel_name.to_string(), (channel_id, handle));
    Ok(())
}

/// Subscribes to `channel_id`'s pub/sub traffic and spawns a task
/// forwarding it back out over `outgoing_tx` as a `Chat` envelope labeled
/// `display_name` — the piece `join_channel` and `auto_join_zone_channels`
/// (#186) both need, factored out since neither differs in *how* a
/// connection consumes a channel once resolved, only in how the channel
/// id and membership got established in the first place (an explicit
/// `Join` vs. an implicit zone entry).
async fn spawn_forwarder(
    channel_id: ChannelId,
    display_name: &str,
    account_id: AccountId,
    chat: &ChatDeps,
    outgoing_tx: &mpsc::UnboundedSender<Envelope>,
) -> Result<JoinHandle<()>> {
    let mut incoming = Box::pin(chat.bus.subscribe(channel_id).await?);
    let outgoing_tx = outgoing_tx.clone();
    let usernames = chat.usernames.clone();
    let display_name = display_name.to_string();
    Ok(tokio::spawn(async move {
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
                channel: display_name.clone(),
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
    }))
}

/// Reserved prefix for `auto_join_zone_channels`' entries in
/// `JoinedChannels` (#186) — distinguishes an automatic zone-channel join
/// from a user's own explicit `Join` by name, so `auto_leave_zone_channels`
/// only ever tears down what auto-join itself set up, never a channel the
/// player joined on purpose. A player naming an explicit `group` channel
/// with this exact prefix would collide in principle; in practice this is
/// the same "reserved, not blocked at parse time" convention
/// `chat::demo_support`'s `chat-demo-` username prefix already uses.
const ZONE_AUTO_JOIN_KEY_PREFIX: &str = "#zone-auto:";

fn zone_auto_join_key(category: &str) -> String {
    format!("{ZONE_AUTO_JOIN_KEY_PREFIX}{category}")
}

/// Auto-joins `account_id` to every zone-scoped channel category declared
/// `auto_join: true` in `chat.yaml` for `zone_id` (#186) — called by
/// `server::session` on initial zone join and on each `ZoneChanged`
/// transition, never by an explicit client `Join`. Skips (silently, not
/// an error — this is normal, expected behavior, not a failure) any
/// category listed in `blocked`, the in-memory set a plugin populated via
/// `block-zone-channel` (`wit/plugin.wit`).
///
/// Deliberately never calls `ChannelStore::join`/`leave` — a `zone`
/// channel's membership is implicit via `character.zone_id`
/// (docs/specs/Chat_Spec.md's channel-types table: "`zone` channels never
/// get rows in `chat_channel_members`"), so this only resolves the
/// channel (idempotently, via `ensure_zone_channel`) and starts the same
/// pub/sub forwarder an explicit `join_channel` starts — see
/// `spawn_forwarder`.
pub async fn auto_join_zone_channels(
    zone_id: &str,
    account_id: AccountId,
    chat: &ChatDeps,
    outgoing_tx: &mpsc::UnboundedSender<Envelope>,
    joined: &mut JoinedChannels,
    blocked: &std::collections::HashSet<String>,
) {
    for category in chat.auto_join_categories.iter() {
        if blocked.contains(category) {
            continue;
        }
        let key = zone_auto_join_key(category);
        if joined.contains_key(&key) {
            continue;
        }
        match auto_join_one(
            zone_id,
            category,
            &key,
            account_id,
            chat,
            outgoing_tx,
            joined,
        )
        .await
        {
            Ok(()) => {}
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    zone_id,
                    category,
                    "failed to auto-join zone chat channel"
                );
            }
        }
    }
}

async fn auto_join_one(
    zone_id: &str,
    category: &str,
    key: &str,
    account_id: AccountId,
    chat: &ChatDeps,
    outgoing_tx: &mpsc::UnboundedSender<Envelope>,
    joined: &mut JoinedChannels,
) -> Result<()> {
    let name = format!("{category} — {zone_id}");
    let channel_id = chat
        .store
        .ensure_zone_channel(Some(zone_id), category, &name)
        .await?;

    let handle = spawn_forwarder(channel_id, category, account_id, chat, outgoing_tx).await?;
    joined.insert(key.to_string(), (channel_id, handle));

    let announce = ServerMessage::Joined {
        channel_id,
        channel: category.to_string(),
    };
    if let Ok(envelope) = announce.into_envelope() {
        let _ = outgoing_tx.send(envelope);
    }
    Ok(())
}

/// Leaves every zone channel `auto_join_zone_channels` joined — the
/// counterpart called right before a `ZoneChanged` transition leaves the
/// old zone (#186). Only tears down `ZONE_AUTO_JOIN_KEY_PREFIX`-keyed
/// entries; a user's own explicit `Join`s are untouched (they don't
/// track which zone they were made in, and aren't this function's
/// business). No `ChannelStore::leave` call, mirroring
/// `auto_join_zone_channels`'s own "zone membership is implicit" doc
/// comment — there is no membership row to remove.
pub fn auto_leave_zone_channels(
    joined: &mut JoinedChannels,
    outgoing_tx: &mpsc::UnboundedSender<Envelope>,
) {
    let keys: Vec<String> = joined
        .keys()
        .filter(|k| k.starts_with(ZONE_AUTO_JOIN_KEY_PREFIX))
        .cloned()
        .collect();
    for key in keys {
        if let Some((_, handle)) = joined.remove(&key) {
            handle.abort();
            let category = key
                .strip_prefix(ZONE_AUTO_JOIN_KEY_PREFIX)
                .unwrap_or(&key)
                .to_string();
            if let Ok(envelope) = (ServerMessage::Left { channel: category }).into_envelope() {
                let _ = outgoing_tx.send(envelope);
            }
        }
    }
}
