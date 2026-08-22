//! Per-connection handling for the phase-1 combined `server`: the auth
//! handshake first (docs/specs/Auth_Spec.md, "Gateway handshake"), then
//! load-or-create a character and drive its movement in the zone
//! (docs/PROPOSAL.md, "Phased Roadmap," Phase 1).

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use character::CharacterStore;
use common::id::{AccountId, EntityId, RealmId};
use common::{Error, Result};
use futures_util::{SinkExt, StreamExt};
use gateway::Envelope;
use tokio::sync::mpsc;
use tokio_util::codec::Framed;
use world::EntityKind;

use crate::session_protocol::{ClientMessage, RosterEntry, ServerMessage, WORLD_MESSAGE_TYPE};
use crate::world_actor::WorldHandle;

pub type ServerStream =
    Framed<tokio_rustls::server::TlsStream<tokio::net::TcpStream>, gateway::EnvelopeCodec>;
type ServerSink = futures_util::stream::SplitSink<ServerStream, Envelope>;

/// Every connected entity's outgoing channel — how the world actor's
/// tick-outcome broadcast and a newly-joined session's roster reach
/// every other connected client. Locked only for quick, synchronous
/// insert/remove/iterate — never held across an `.await`.
pub type Sessions = Arc<Mutex<HashMap<EntityId, mpsc::UnboundedSender<Envelope>>>>;

pub struct SessionDeps {
    pub auth_provider: Arc<auth::UsernamePasswordProvider>,
    pub character_store: Arc<CharacterStore>,
    pub realm_id: RealmId,
    pub zone_id: String,
    pub world: WorldHandle,
    pub sessions: Sessions,
    /// `message_type`s the configured plugin declared in `plugin.toml`
    /// (empty if no plugin is configured) — checked here rather than
    /// only in the world actor so an envelope with an unroutable
    /// `message_type` still gets a clear per-connection error reply
    /// instead of silently vanishing into the actor's command queue
    /// (#95).
    pub plugin_message_types: Vec<u16>,
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

    let character = load_or_create_character(&deps, account_id, &username).await?;
    let character_id = character.id;
    let position = (character.position.0, character.position.1);

    let entity_id = EntityId::new();
    deps.world.spawn(entity_id, EntityKind::Player, position);

    // Everything already in the zone, delivered as one `Joined` message
    // rather than `Spawned` plus a separate `EntitySpawned` per entity —
    // a pre-spawned NPC (or another already-connected player) otherwise
    // has no way to become visible to this connection, and a single
    // message keeps the join a single write on a freshly-established
    // connection instead of several in a row.
    let roster: Vec<RosterEntry> = deps
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

    deps.sessions
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
        &deps.sessions,
        entity_id,
        ServerMessage::EntitySpawned {
            entity_id: entity_id.to_string(),
            entity_type: entity_type_label(EntityKind::Player),
            x: position.0,
            y: position.1,
        },
    );

    loop {
        tokio::select! {
            maybe_frame = stream.next() => {
                let Some(frame) = maybe_frame else { break };
                let Ok(envelope) = frame else { break };
                if envelope.message_type == WORLD_MESSAGE_TYPE {
                    match ClientMessage::from_envelope(&envelope) {
                        Ok(ClientMessage::Move { x, y }) => {
                            deps.world.request_move(entity_id, (x, y));
                        }
                        Err(e) => {
                            send_world(&mut sink, &ServerMessage::Error { message: e.to_string() }).await?;
                        }
                    }
                } else if deps.plugin_message_types.contains(&envelope.message_type) {
                    deps.world.dispatch_plugin_message(
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
                if sink.send(envelope).await.is_err() {
                    break;
                }
            }
        }
    }

    let final_position = deps.world.position_of(entity_id).await;
    deps.world.despawn(entity_id);
    deps.sessions.lock().unwrap().remove(&entity_id);
    broadcast(
        &deps.sessions,
        ServerMessage::EntityDespawned {
            entity_id: entity_id.to_string(),
        },
    );

    if let Some((x, y)) = final_position {
        deps.character_store
            .update_position(character_id, (x, y, 0.0))
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
        .create(account_id, username, deps.realm_id, &deps.zone_id)
        .await?;
    Ok(character::CharacterSummary {
        id,
        name: username.to_string(),
        zone_id: deps.zone_id.clone(),
        position: (0.0, 0.0, 0.0),
    })
}

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

fn entity_type_label(kind: EntityKind) -> String {
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

async fn send_auth_error(sink: &mut ServerSink, message: String) -> Result<()> {
    send_auth(
        sink,
        &auth::gateway_protocol::ServerMessage::Error { message },
    )
    .await
}
