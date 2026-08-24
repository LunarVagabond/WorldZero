//! Wire protocol for the phase-1 `server` binary's gateway-connected
//! movement session — how a client moves and sees other entities move
//! once authenticated (docs/PROPOSAL.md, "Phased Roadmap," Phase 1: "one
//! client able to connect, move, and persist state across sessions").
//! `message_type` 200 — see docs/specs/Networking_Spec.md's catalog note.
//! Protobuf payloads (`proto/session.proto`, decision #109, implemented
//! in #123) — see `auth::gateway_protocol`'s doc comment for why the
//! ergonomic `ClientMessage`/`ServerMessage` enums below wrap the
//! generated `proto` module rather than being it.
//!
//! Identity comes from `auth::gateway_protocol`'s handshake first, same
//! as chat's gateway integration — nothing here carries or trusts a
//! client-claimed identity.

use common::{Error, Result};
use gateway::Envelope;

mod proto {
    include!(concat!(env!("OUT_DIR"), "/worldzero.session.rs"));
}

pub const WORLD_MESSAGE_TYPE: u16 = 200;

#[derive(Debug, Clone)]
pub enum ClientMessage {
    /// Requests moving this connection's own entity to `(x, y)` — queued
    /// for the next simulation tick, never applied immediately
    /// (`world::Zone::request_move`).
    Move { x: f64, y: f64 },
}

#[derive(Debug, Clone, PartialEq)]
pub struct RosterEntry {
    pub entity_id: String,
    pub entity_type: String,
    pub x: f64,
    pub y: f64,
}

#[derive(Debug, Clone)]
pub enum ServerMessage {
    /// Sent once, right after this connection's own entity is spawned
    /// into the zone — its assigned `entity_id`, its starting position
    /// (loaded from the character's persisted position, or the zero
    /// position for a newly created character), and every other entity
    /// already in the zone at that moment (a pre-spawned NPC otherwise
    /// has no way to become visible to a client that joins after it
    /// spawned). One message rather than `Spawned` plus N separate
    /// `EntitySpawned`s so a join is a single atomic delivery, not
    /// several sequential writes on a freshly-established connection.
    Joined {
        entity_id: String,
        x: f64,
        y: f64,
        roster: Vec<RosterEntry>,
    },
    /// An entity (never this connection's own — see `Joined` above)
    /// newly exists in the zone — broadcast to already-connected clients
    /// whenever one spawns after they joined (a new player, or an NPC
    /// the plugin host spawns).
    EntitySpawned {
        entity_id: String,
        entity_type: String,
        x: f64,
        y: f64,
    },
    EntityDespawned {
        entity_id: String,
    },
    /// Sent to a connection whose entity just crossed a manifest-declared
    /// zone link (#45) — same shape as `Joined`, but for an in-place
    /// zone handoff rather than the initial connect: the new `zone_id`,
    /// the entity's arrival position in that zone's local coordinate
    /// system, and that zone's current roster (this connection has no
    /// other way to learn who's already there). The connection never
    /// disconnects/reconnects for this — the same TCP session, gateway
    /// handshake, and `entity_id` carry straight through.
    ZoneChanged {
        zone_id: String,
        entity_id: String,
        x: f64,
        y: f64,
        roster: Vec<RosterEntry>,
    },
    /// An accepted movement update — broadcast to every connected client,
    /// not just the mover, so everyone's view of the zone stays current
    /// (phase 1 has no interest management yet, see
    /// docs/PROPOSAL.md's "Spatial Index: A → Z Roadmap").
    Moved {
        entity_id: String,
        x: f64,
        y: f64,
    },
    /// A movement update this connection itself requested was rejected —
    /// only sent back to the mover, never broadcast.
    Rejected {
        reason: String,
    },
    Error {
        message: String,
    },
    /// A message a plugin's `send-message` host call addressed to this
    /// connection's entity — the "make the interaction have a visible
    /// effect" primitive (docs/specs/Plugin_API.md), delivered verbatim
    /// as the plugin wrote it.
    PluginMessage {
        body: String,
    },
}

// Both directions of this protocol define both `into_envelope` and
// `from_envelope` for symmetry — which half is actually exercised
// differs by compilation target (the `server` bin only ever receives a
// `ClientMessage` and sends a `ServerMessage`; `tests/server_smoke.rs`,
// standing in for a real client, does the opposite), so each method is
// "dead" in exactly one of those targets. `#[allow(dead_code)]`
// throughout rather than chasing which half is live per target.

impl ClientMessage {
    #[allow(dead_code, clippy::wrong_self_convention)]
    pub fn into_envelope(&self) -> Result<Envelope> {
        encode(&proto::ClientMessage::from(self))
    }

    #[allow(dead_code)]
    pub fn from_envelope(envelope: &Envelope) -> Result<Self> {
        decode::<proto::ClientMessage>(envelope)?.try_into()
    }
}

impl ServerMessage {
    #[allow(dead_code, clippy::wrong_self_convention)]
    pub fn into_envelope(&self) -> Result<Envelope> {
        encode(&proto::ServerMessage::from(self))
    }

    #[allow(dead_code)]
    pub fn from_envelope(envelope: &Envelope) -> Result<Self> {
        decode::<proto::ServerMessage>(envelope)?.try_into()
    }
}

impl From<&RosterEntry> for proto::RosterEntry {
    fn from(entry: &RosterEntry) -> Self {
        proto::RosterEntry {
            entity_id: entry.entity_id.clone(),
            entity_type: entry.entity_type.clone(),
            x: entry.x,
            y: entry.y,
        }
    }
}

impl From<proto::RosterEntry> for RosterEntry {
    fn from(entry: proto::RosterEntry) -> Self {
        RosterEntry {
            entity_id: entry.entity_id,
            entity_type: entry.entity_type,
            x: entry.x,
            y: entry.y,
        }
    }
}

impl From<&ClientMessage> for proto::ClientMessage {
    fn from(message: &ClientMessage) -> Self {
        use proto::client_message::Kind;
        let kind = match message {
            ClientMessage::Move { x, y } => Kind::Move(proto::Move { x: *x, y: *y }),
        };
        proto::ClientMessage { kind: Some(kind) }
    }
}

impl TryFrom<proto::ClientMessage> for ClientMessage {
    type Error = Error;

    fn try_from(message: proto::ClientMessage) -> Result<Self> {
        use proto::client_message::Kind;
        match message.kind {
            Some(Kind::Move(proto::Move { x, y })) => Ok(ClientMessage::Move { x, y }),
            None => Err(Error::new(
                "server",
                "gateway world message has no kind set",
            )),
        }
    }
}

impl From<&ServerMessage> for proto::ServerMessage {
    fn from(message: &ServerMessage) -> Self {
        use proto::server_message::Kind;
        let kind = match message {
            ServerMessage::Joined {
                entity_id,
                x,
                y,
                roster,
            } => Kind::Joined(proto::Joined {
                entity_id: entity_id.clone(),
                x: *x,
                y: *y,
                roster: roster.iter().map(proto::RosterEntry::from).collect(),
            }),
            ServerMessage::EntitySpawned {
                entity_id,
                entity_type,
                x,
                y,
            } => Kind::EntitySpawned(proto::EntitySpawned {
                entity_id: entity_id.clone(),
                entity_type: entity_type.clone(),
                x: *x,
                y: *y,
            }),
            ServerMessage::EntityDespawned { entity_id } => {
                Kind::EntityDespawned(proto::EntityDespawned {
                    entity_id: entity_id.clone(),
                })
            }
            ServerMessage::ZoneChanged {
                zone_id,
                entity_id,
                x,
                y,
                roster,
            } => Kind::ZoneChanged(proto::ZoneChanged {
                zone_id: zone_id.clone(),
                entity_id: entity_id.clone(),
                x: *x,
                y: *y,
                roster: roster.iter().map(proto::RosterEntry::from).collect(),
            }),
            ServerMessage::Moved { entity_id, x, y } => Kind::Moved(proto::Moved {
                entity_id: entity_id.clone(),
                x: *x,
                y: *y,
            }),
            ServerMessage::Rejected { reason } => Kind::Rejected(proto::Rejected {
                reason: reason.clone(),
            }),
            ServerMessage::Error { message } => Kind::Error(proto::Error {
                message: message.clone(),
            }),
            ServerMessage::PluginMessage { body } => {
                Kind::PluginMessage(proto::PluginMessage { body: body.clone() })
            }
        };
        proto::ServerMessage { kind: Some(kind) }
    }
}

impl TryFrom<proto::ServerMessage> for ServerMessage {
    type Error = Error;

    fn try_from(message: proto::ServerMessage) -> Result<Self> {
        use proto::server_message::Kind;
        match message.kind {
            Some(Kind::Joined(proto::Joined {
                entity_id,
                x,
                y,
                roster,
            })) => Ok(ServerMessage::Joined {
                entity_id,
                x,
                y,
                roster: roster.into_iter().map(RosterEntry::from).collect(),
            }),
            Some(Kind::EntitySpawned(proto::EntitySpawned {
                entity_id,
                entity_type,
                x,
                y,
            })) => Ok(ServerMessage::EntitySpawned {
                entity_id,
                entity_type,
                x,
                y,
            }),
            Some(Kind::EntityDespawned(proto::EntityDespawned { entity_id })) => {
                Ok(ServerMessage::EntityDespawned { entity_id })
            }
            Some(Kind::ZoneChanged(proto::ZoneChanged {
                zone_id,
                entity_id,
                x,
                y,
                roster,
            })) => Ok(ServerMessage::ZoneChanged {
                zone_id,
                entity_id,
                x,
                y,
                roster: roster.into_iter().map(RosterEntry::from).collect(),
            }),
            Some(Kind::Moved(proto::Moved { entity_id, x, y })) => {
                Ok(ServerMessage::Moved { entity_id, x, y })
            }
            Some(Kind::Rejected(proto::Rejected { reason })) => {
                Ok(ServerMessage::Rejected { reason })
            }
            Some(Kind::Error(proto::Error { message })) => Ok(ServerMessage::Error { message }),
            Some(Kind::PluginMessage(proto::PluginMessage { body })) => {
                Ok(ServerMessage::PluginMessage { body })
            }
            None => Err(Error::new(
                "server",
                "gateway world message has no kind set",
            )),
        }
    }
}

fn encode(message: &impl prost::Message) -> Result<Envelope> {
    Ok(Envelope::new(WORLD_MESSAGE_TYPE, message.encode_to_vec()))
}

fn decode<T: prost::Message + Default>(envelope: &Envelope) -> Result<T> {
    if envelope.message_type != WORLD_MESSAGE_TYPE {
        return Err(Error::new(
            "server",
            format!(
                "expected message_type {WORLD_MESSAGE_TYPE} (world), got {}",
                envelope.message_type
            ),
        ));
    }
    T::decode(envelope.payload.clone())
        .map_err(|e| Error::wrap("server", "failed to decode gateway world message", e))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn client_message_round_trips_through_an_envelope() {
        let message = ClientMessage::Move { x: 1.5, y: -2.0 };
        let envelope = message.into_envelope().unwrap();
        assert_eq!(envelope.message_type, WORLD_MESSAGE_TYPE);
        let decoded = ClientMessage::from_envelope(&envelope).unwrap();
        assert!(matches!(decoded, ClientMessage::Move { x, y } if x == 1.5 && y == -2.0));
    }

    #[test]
    fn decode_rejects_the_wrong_message_type() {
        let envelope = Envelope::new(1, b"".to_vec());
        assert!(ClientMessage::from_envelope(&envelope).is_err());
    }

    #[test]
    fn decode_rejects_an_envelope_with_no_kind_set() {
        let envelope = Envelope::new(WORLD_MESSAGE_TYPE, b"".to_vec());
        let err = ClientMessage::from_envelope(&envelope).unwrap_err();
        assert!(err.to_string().contains("no kind set"), "{err}");
    }

    #[test]
    fn joined_round_trips_a_roster() {
        let message = ServerMessage::Joined {
            entity_id: "e1".to_string(),
            x: 1.0,
            y: 2.0,
            roster: vec![RosterEntry {
                entity_id: "e2".to_string(),
                entity_type: "npc.wolf".to_string(),
                x: 3.0,
                y: 4.0,
            }],
        };
        let envelope = message.into_envelope().unwrap();
        let decoded = ServerMessage::from_envelope(&envelope).unwrap();
        assert!(matches!(
            decoded,
            ServerMessage::Joined { roster, .. } if roster.len() == 1 && roster[0].entity_id == "e2"
        ));
    }
}
