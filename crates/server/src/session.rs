//! Per-connection handling for the phase-1 combined `server`: the auth
//! handshake first (docs/specs/Auth_Spec.md, "Gateway handshake"), then
//! load-or-create a character and drive its movement in the zone
//! (docs/PROPOSAL.md, "Phased Roadmap," Phase 1).

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use character::CharacterStore;
use chat::gateway_protocol::CHAT_MESSAGE_TYPE;
use common::id::{AccountId, CharacterId, EntityId, RealmId};
use common::{Error, Result};
use futures_util::{SinkExt, StreamExt};
use gateway::Envelope;
use tokio::sync::mpsc;
use tokio_util::codec::Framed;
use world::EntityKind;

use crate::chat_session::{self, ChatDeps};
use crate::session_protocol::{ClientMessage, RosterEntry, ServerMessage, WORLD_MESSAGE_TYPE};
use crate::zone_registry::ZoneRegistry;

pub type ServerStream =
    Framed<tokio_rustls::server::TlsStream<tokio::net::TcpStream>, gateway::EnvelopeCodec>;
type ServerSink = futures_util::stream::SplitSink<ServerStream, Envelope>;

/// Every connected entity's outgoing channel — how the world actor's
/// tick-outcome broadcast and a newly-joined session's roster reach
/// every other connected client. Locked only for quick, synchronous
/// insert/remove/iterate — never held across an `.await`.
pub type Sessions = Arc<Mutex<HashMap<EntityId, mpsc::UnboundedSender<Envelope>>>>;

/// Which `character` row a connected player entity belongs to — the
/// resolution `plugin_host`'s `apply-stat-delta` needs (a plugin only
/// knows the opaque entity id; the actual stat write is per-character),
/// populated alongside `Sessions` at spawn and removed at disconnect.
/// Never has an NPC entry — NPCs have no backing character row (no NPC
/// stat storage exists yet, see docs/specs/Plugin_API.md's "Beyond this
/// v0 slice").
pub type EntityCharacters = Arc<Mutex<HashMap<EntityId, CharacterId>>>;

/// Which roles (docs/specs/Auth_Spec.md, "Account roles", #114/#124) the
/// account behind a connected player entity holds — populated once at
/// join time (below) and consulted synchronously by `plugin_startup`'s
/// `caller-role` host function, never queried live from `auth`'s role
/// store from inside a sandboxed plugin call (see `wit/plugin.wit`'s
/// `caller-role` doc comment for why: `plugin_host::HostCallbacks` is
/// called synchronously from inside `wasmtime`, while the role store is
/// async-only). Global scope for v0, so a plugin sees the same roles for
/// the life of the connection — a role granted/revoked mid-session isn't
/// reflected until reconnect, an accepted staleness window for v0. Never
/// has an NPC entry, same as `EntityCharacters`.
pub type EntityRoles = Arc<Mutex<HashMap<EntityId, Vec<String>>>>;

pub struct SessionDeps {
    pub auth_provider: Arc<auth::UsernamePasswordProvider>,
    pub character_store: Arc<CharacterStore>,
    pub realm_id: RealmId,
    /// Every zone-service instance this process runs (#45) — a
    /// connection looks up its current zone's `WorldHandle`/`Sessions`
    /// here at join time, and again on every `ZoneChanged` handoff.
    pub zones: Arc<ZoneRegistry>,
    /// Which zone a brand-new character starts in, and the fallback for
    /// an existing character whose persisted `zone_id` no longer names a
    /// zone this content pack declares (a pack that's since dropped a
    /// zone) — never silently drops the connection over a stale zone_id.
    pub default_zone_id: String,
    pub entity_characters: EntityCharacters,
    /// Backs `EntityRoles` population at join time (#124) — `auth` (like
    /// `character`) is always wired in this combined process, so this is
    /// never optional the way `chat`/`metrics` are.
    pub role_store: Arc<dyn auth::AccountRoleStore>,
    pub entity_roles: EntityRoles,
    /// `message_type`s the configured plugin declared in `plugin.toml`
    /// (empty if no plugin is configured) — checked here rather than
    /// only in the world actor so an envelope with an unroutable
    /// `message_type` still gets a clear per-connection error reply
    /// instead of silently vanishing into the actor's command queue
    /// (#95).
    pub plugin_message_types: Vec<u16>,
    /// Chat command names (without the leading `/`) the configured
    /// plugin declared (empty if none) — checked here, before a `Send`
    /// ever reaches `chat_session`, so a matched command is routed to
    /// the plugin instead of published as an ordinary chat message (#57).
    pub plugin_chat_commands: Vec<String>,
    /// `Some` when `ServicesConfig::chat_enabled` — `None` end to end
    /// means chat is disabled and never touched, not just no-op'd (#104).
    pub chat: Option<ChatDeps>,
    /// `Some` when `ServicesConfig::metrics_enabled` — `None` end to end
    /// means metrics are disabled and `worldzero_connection_count` is
    /// never touched, not just excluded from what gets scraped (#48).
    pub metrics: Option<Arc<common::metrics::Metrics>>,
    /// Backs character-scope `plugin-state-get`/`plugin-state-set`
    /// (#149) — hydrated into `plugin_state_cache` at join time, same
    /// "populate a cache at join, never a live DB read from inside a
    /// sandboxed call" shape `entity_roles` already uses.
    pub plugin_state_store: Arc<crate::plugin_state::PluginStateStore>,
    pub plugin_state_cache: crate::plugin_state::PluginStateCache,
    /// Every connected entity's outgoing channel, process-wide, regardless
    /// of which zone it's currently in (#152) — backs the plugin
    /// `send-message` host function, since a plugin instance is now
    /// shared across every zone and needs to reach a target entity no
    /// matter where they are. Distinct from each zone's own `Sessions`
    /// (`zone.sessions`, used for that zone's broadcast/roster) — an
    /// entry here is added once at initial join and removed once at
    /// final disconnect; it's untouched by `ZoneChanged` zone-hops, since
    /// the same connection/`outgoing_tx` carries straight through those.
    pub global_sessions: Sessions,
}

pub async fn handle_session(framed: ServerStream, deps: Arc<SessionDeps>) -> Result<()> {
    let (mut sink, mut stream) = framed.split();
    let (outgoing_tx, mut outgoing_rx) = mpsc::unbounded_channel::<Envelope>();

    let Some(frame) = stream.next().await else {
        return Ok(());
    };
    let envelope = frame.map_err(|e| Error::wrap("server", "connection error", e))?;
    let auth_message = match auth::gateway_protocol::ClientMessage::from_envelope(&envelope) {
        Ok(m) => m,
        Err(e) => {
            send_auth_error(&mut sink, e.to_string()).await?;
            return Ok(());
        }
    };

    let (account_id, username, session) =
        match authenticate(auth_message, &deps.auth_provider).await {
            Ok(result) => result,
            Err(e) => {
                send_auth_error(&mut sink, e.to_string()).await?;
                return Ok(());
            }
        };

    send_auth(
        &mut sink,
        &auth::gateway_protocol::ServerMessage::Authenticated {
            account_id,
            username: username.clone(),
            session_token: session.token,
        },
    )
    .await?;

    if let Some(chat) = &deps.chat {
        chat.usernames
            .write()
            .unwrap()
            .insert(account_id, username.clone());
    }
    let mut joined_channels = chat_session::JoinedChannels::new();

    let character = load_or_create_character(&deps, account_id, &username).await?;
    let character_id = character.id;
    let position = (character.position.0, character.position.1);

    // A character's persisted `zone_id` might name a zone this content
    // pack no longer declares (the pack changed since they last logged
    // in) — fall back to the default rather than failing the connection
    // over it.
    let mut current_zone_id = if deps.zones.contains(&character.zone_id) {
        character.zone_id.clone()
    } else {
        tracing::warn!(
            character_zone_id = character.zone_id,
            default_zone_id = deps.default_zone_id,
            "character's persisted zone no longer exists in this content pack, using the default"
        );
        deps.default_zone_id.clone()
    };
    // Population-balanced layer assignment (#50) happens once, here, at
    // initial join — see `zone_registry`'s doc comment for why a later
    // zone-link transition or mid-connection `ZoneChanged` (below) always
    // lands on layer 0 instead rather than going through this too.
    let mut zone = deps
        .zones
        .assign_layer(&current_zone_id)
        .expect("default_zone_id must always resolve to a real zone in the registry");

    let entity_id = EntityId::new();
    zone.world.spawn(entity_id, EntityKind::Player, position);
    deps.entity_characters
        .lock()
        .unwrap()
        .insert(entity_id, character_id);
    let roles = deps.role_store.roles_for(account_id).await?;
    deps.entity_roles.lock().unwrap().insert(entity_id, roles);

    // Character-scope plugin state (#149), hydrated once here — before
    // this entity can possibly receive a `plugin-state-get` call — same
    // shape as `entity_roles` just above.
    let plugin_state = deps
        .plugin_state_store
        .character_state(character_id)
        .await?;
    if !plugin_state.is_empty() {
        let mut cache = deps.plugin_state_cache.lock().unwrap();
        for (key, value) in plugin_state {
            cache.insert(
                crate::plugin_state::cache_key(
                    &plugin_host::PluginStateScope::Character(entity_id.to_string()),
                    &key,
                ),
                value,
            );
        }
    }

    if let Some(metrics) = &deps.metrics {
        metrics.connection_count.inc();
    }

    // Everything already in the zone, delivered as one `Joined` message
    // rather than `Spawned` plus a separate `EntitySpawned` per entity —
    // a pre-spawned NPC (or another already-connected player) otherwise
    // has no way to become visible to this connection, and a single
    // message keeps the join a single write on a freshly-established
    // connection instead of several in a row.
    let roster: Vec<RosterEntry> = zone
        .world
        .entities_snapshot()
        .await
        .into_iter()
        .filter(|(other_id, ..)| *other_id != entity_id)
        .map(|(other_id, kind, other_position)| RosterEntry {
            entity_id: other_id.to_string(),
            entity_type: entity_type_label(kind),
            x: other_position.0,
            y: other_position.1,
        })
        .collect();

    zone.sessions
        .lock()
        .unwrap()
        .insert(entity_id, outgoing_tx.clone());
    deps.global_sessions
        .lock()
        .unwrap()
        .insert(entity_id, outgoing_tx.clone());

    queue(
        &outgoing_tx,
        &ServerMessage::Joined {
            entity_id: entity_id.to_string(),
            x: position.0,
            y: position.1,
            roster,
        },
    );

    broadcast_except(
        &zone.sessions,
        entity_id,
        ServerMessage::EntitySpawned {
            entity_id: entity_id.to_string(),
            entity_type: entity_type_label(EntityKind::Player),
            x: position.0,
            y: position.1,
        },
    );

    // After roster delivery, so a plugin's own `send-message` call made
    // from inside `on-player-join-zone` reaches a client that's actually
    // ready to receive it (#155).
    zone.world.dispatch_player_join(entity_id);

    loop {
        tokio::select! {
            maybe_frame = stream.next() => {
                let Some(frame) = maybe_frame else { break };
                let Ok(envelope) = frame else { break };
                if envelope.message_type == WORLD_MESSAGE_TYPE {
                    match ClientMessage::from_envelope(&envelope) {
                        Ok(ClientMessage::Move { x, y }) => {
                            zone.world.request_move(entity_id, (x, y));
                        }
                        Ok(ClientMessage::Attack { target_entity_id, stat_key }) => {
                            match target_entity_id.parse::<EntityId>() {
                                Ok(target) => zone.world.dispatch_attack(entity_id, target, stat_key),
                                Err(_) => {
                                    send_world(&mut sink, &ServerMessage::Error {
                                        message: format!("{target_entity_id:?} is not a valid entity id"),
                                    }).await?;
                                }
                            }
                        }
                        Ok(ClientMessage::UseItem { item_type }) => {
                            zone.world.dispatch_use_item(entity_id, item_type);
                        }
                        Ok(ClientMessage::InteractNpc { npc_entity_id }) => {
                            match npc_entity_id.parse::<EntityId>() {
                                Ok(npc) => zone.world.dispatch_interact_npc(npc, entity_id),
                                Err(_) => {
                                    send_world(&mut sink, &ServerMessage::Error {
                                        message: format!("{npc_entity_id:?} is not a valid entity id"),
                                    }).await?;
                                }
                            }
                        }
                        Err(e) => {
                            send_world(&mut sink, &ServerMessage::Error { message: e.to_string() }).await?;
                        }
                    }
                } else if envelope.message_type == CHAT_MESSAGE_TYPE {
                    match &deps.chat {
                        None => {
                            send_world(&mut sink, &ServerMessage::Error {
                                message: "chat is disabled on this server".to_string(),
                            }).await?;
                        }
                        Some(chat) => {
                            let parsed = chat::gateway_protocol::ClientMessage::from_envelope(&envelope);
                            let command_send = match &parsed {
                                Ok(chat::gateway_protocol::ClientMessage::Send { body, .. }) => {
                                    plugin_chat_command(&deps.plugin_chat_commands, body)
                                }
                                _ => None,
                            };
                            if let Some((command, args)) = command_send {
                                // A matched command is consumed here — never
                                // also forwarded to
                                // `chat_session::handle_message`/published as
                                // an ordinary chat message (#57).
                                zone.world.dispatch_chat_command(command, args, entity_id);
                            } else {
                                match parsed {
                                    Ok(message) => {
                                        if let Some(reply) = chat_session::handle_message(
                                            message,
                                            account_id,
                                            chat,
                                            &outgoing_tx,
                                            &mut joined_channels,
                                        ).await {
                                            send_chat(&mut sink, &reply).await?;
                                        }
                                    }
                                    Err(e) => {
                                        send_chat(&mut sink, &chat::gateway_protocol::ServerMessage::Error {
                                            message: e.to_string(),
                                        }).await?;
                                    }
                                }
                            }
                        }
                    }
                } else if deps.plugin_message_types.contains(&envelope.message_type) {
                    // Goes to whichever zone this connection is in right
                    // now — harmless (just an actor-side "no plugin
                    // configured" warning) if that's not the one zone the
                    // configured plugin is attached to (#45's
                    // single-plugin-single-zone scope, see this module's
                    // `SessionDeps`/`zone_registry` doc comments).
                    zone.world.dispatch_plugin_message(
                        envelope.message_type,
                        entity_id,
                        envelope.payload.to_vec(),
                    );
                } else {
                    let message_type = envelope.message_type;
                    send_world(
                        &mut sink,
                        &ServerMessage::Error {
                            message: format!("unrecognized message_type {message_type}"),
                        },
                    )
                    .await?;
                }
            }
            Some(envelope) = outgoing_rx.recv() => {
                // A `ZoneChanged` envelope both goes out to the client
                // (below, same as any other envelope) and tells this
                // task to switch which zone's `WorldHandle`/`Sessions` it
                // talks to from now on — the connection itself never
                // drops for this (#45).
                if envelope.message_type == WORLD_MESSAGE_TYPE
                    && let Ok(ServerMessage::ZoneChanged { zone_id, .. }) = ServerMessage::from_envelope(&envelope)
                {
                    match deps.zones.get(&zone_id) {
                        Some(new_zone) => {
                            current_zone_id = zone_id;
                            zone = new_zone;
                        }
                        None => {
                            tracing::error!(zone_id, "zone transition target vanished from the registry");
                        }
                    }
                }
                let send_result = sink.send(envelope).await;
                if send_result.is_err() {
                    break;
                }
            }
        }
    }

    chat_session::abort_all(joined_channels);
    if let Some(metrics) = &deps.metrics {
        metrics.connection_count.dec();
    }

    // Dispatched before `entity_characters`/`entity_roles` are cleared
    // below, so a plugin's `on-player-leave-zone` handler can still
    // resolve this entity's character if it makes its own host-function
    // calls in response (#155).
    zone.world.dispatch_player_leave(entity_id).await;

    let final_position = zone.world.position_of(entity_id).await;
    zone.world.despawn(entity_id);
    zone.sessions.lock().unwrap().remove(&entity_id);
    deps.global_sessions.lock().unwrap().remove(&entity_id);
    deps.entity_characters.lock().unwrap().remove(&entity_id);
    deps.entity_roles.lock().unwrap().remove(&entity_id);
    // Character-scope (and any leftover entity-scope) cache entries for
    // this connection — keeps the shared process-wide cache from growing
    // unbounded across reconnects (#149). Zone-scope entries are never
    // touched here; they live for the zone's/process's lifetime.
    let character_prefix = format!("character:{entity_id}:");
    let entity_prefix = format!("entity:{entity_id}:");
    deps.plugin_state_cache
        .lock()
        .unwrap()
        .retain(|k, _| !k.starts_with(&character_prefix) && !k.starts_with(&entity_prefix));
    broadcast(
        &zone.sessions,
        ServerMessage::EntityDespawned {
            entity_id: entity_id.to_string(),
        },
    );

    if let Some((x, y)) = final_position {
        deps.character_store
            .update_position_and_zone(character_id, (x, y, 0.0), &current_zone_id)
            .await?;
    }

    Ok(())
}

async fn load_or_create_character(
    deps: &SessionDeps,
    account_id: AccountId,
    username: &str,
) -> Result<character::CharacterSummary> {
    if let Some(existing) = deps
        .character_store
        .find_by_account(account_id, deps.realm_id)
        .await?
    {
        return Ok(existing);
    }

    let id = deps
        .character_store
        .create(account_id, username, deps.realm_id, &deps.default_zone_id)
        .await?;
    Ok(character::CharacterSummary {
        id,
        name: username.to_string(),
        realm_id: deps.realm_id,
        zone_id: deps.default_zone_id.clone(),
        position: (0.0, 0.0, 0.0),
    })
}

/// Matches a chat `Send`'s `body` against `declared_commands` (a
/// plugin's `plugin.toml` `chat_commands`, without leading slashes) —
/// `body` must start with `/`, and everything up to the first space (or
/// the rest of the string if there's no space) is the command name,
/// case-sensitive. Returns the matched command name and the remaining
/// args (trimmed of the one separating space, empty string if none).
fn plugin_chat_command(declared_commands: &[String], body: &str) -> Option<(String, String)> {
    let rest = body.strip_prefix('/')?;
    let (command, args) = match rest.split_once(' ') {
        Some((command, args)) => (command, args),
        None => (rest, ""),
    };
    declared_commands
        .iter()
        .any(|declared| declared == command)
        .then(|| (command.to_string(), args.to_string()))
}

/// Root span for #49's demonstrated cross-service trace path: `gateway`
/// (this connection's own task) → `auth` (`register`/`verify_credentials`,
/// `issue_session`) → Redis (`auth::SessionManager::issue`'s write). A
/// single client action — the connection's very first envelope — nests
/// three crates' worth of spans under one trace, exported as one
/// reconstructable request if `WZ_OTEL_ENDPOINT` is set
/// (`common::logging::init`), otherwise just ordinary nested log context.
#[tracing::instrument(skip_all)]
async fn authenticate(
    message: auth::gateway_protocol::ClientMessage,
    provider: &auth::UsernamePasswordProvider,
) -> Result<(AccountId, String, auth::Session)> {
    use auth::AuthProvider;
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
    };

    let session = provider.issue_session(account_id).await?;
    Ok((account_id, username, session))
}

pub(crate) fn entity_type_label(kind: EntityKind) -> String {
    match kind {
        EntityKind::Player => String::new(),
        EntityKind::Npc => "npc".to_string(),
    }
}

fn queue(outgoing_tx: &mpsc::UnboundedSender<Envelope>, message: &ServerMessage) {
    if let Ok(envelope) = message.into_envelope() {
        let _ = outgoing_tx.send(envelope);
    }
}

fn broadcast(sessions: &Sessions, message: ServerMessage) {
    let Ok(envelope) = message.into_envelope() else {
        return;
    };
    for sender in sessions.lock().unwrap().values() {
        let _ = sender.send(envelope.clone());
    }
}

fn broadcast_except(sessions: &Sessions, exclude: EntityId, message: ServerMessage) {
    let Ok(envelope) = message.into_envelope() else {
        return;
    };
    for (id, sender) in sessions.lock().unwrap().iter() {
        if *id != exclude {
            let _ = sender.send(envelope.clone());
        }
    }
}

async fn send_world(sink: &mut ServerSink, message: &ServerMessage) -> Result<()> {
    let envelope = message.into_envelope()?;
    sink.send(envelope)
        .await
        .map_err(|e| Error::wrap("server", "failed to send to client", e))
}

async fn send_auth(
    sink: &mut ServerSink,
    message: &auth::gateway_protocol::ServerMessage,
) -> Result<()> {
    let envelope = message.into_envelope()?;
    sink.send(envelope)
        .await
        .map_err(|e| Error::wrap("server", "failed to send to client", e))
}

async fn send_chat(
    sink: &mut ServerSink,
    message: &chat::gateway_protocol::ServerMessage,
) -> Result<()> {
    let envelope = message.into_envelope()?;
    sink.send(envelope)
        .await
        .map_err(|e| Error::wrap("server", "failed to send to client", e))
}

async fn send_auth_error(sink: &mut ServerSink, message: String) -> Result<()> {
    send_auth(
        sink,
        &auth::gateway_protocol::ServerMessage::Error { message },
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    fn declared() -> Vec<String> {
        vec!["roll".to_string(), "whisper".to_string()]
    }

    #[test]
    fn a_declared_command_with_args_is_matched() {
        assert_eq!(
            plugin_chat_command(&declared(), "/roll 2d6"),
            Some(("roll".to_string(), "2d6".to_string()))
        );
    }

    #[test]
    fn a_declared_command_with_no_args_is_matched_with_empty_args() {
        assert_eq!(
            plugin_chat_command(&declared(), "/roll"),
            Some(("roll".to_string(), "".to_string()))
        );
    }

    #[test]
    fn an_undeclared_command_is_not_matched() {
        assert_eq!(plugin_chat_command(&declared(), "/unknown foo"), None);
    }

    #[test]
    fn ordinary_chat_without_a_leading_slash_is_not_matched() {
        assert_eq!(plugin_chat_command(&declared(), "hello everyone"), None);
    }
}
