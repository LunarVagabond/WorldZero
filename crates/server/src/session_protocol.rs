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
    /// (`world::Zone::request_move`). `seq` is client-assigned and
    /// monotonically increasing per connection (#196, start at `1`) —
    /// the server only ever echoes it back on `Moved`/`Rejected`, never
    /// interprets it, so a client can correlate a specific outcome to
    /// the specific predicted step it corresponds to.
    Move { x: f64, y: f64, seq: u32 },
    /// A latency probe, independent of gameplay/movement traffic (#196)
    /// — `client_sent_at` is opaque to the server, echoed back verbatim
    /// on `Pong` so the client can compute round-trip time against its
    /// own clock.
    Ping { client_sent_at: i64 },
    /// Requests attacking `target_entity_id` — the server confirms the
    /// target actually exists in this zone before ever calling the
    /// configured plugin's `on-damage-calc` hook; never a client-reported
    /// damage amount or outcome, only the intent to attack (#154).
    /// `stat_key` is an opaque, game-defined string, same discipline as
    /// `apply-stat-delta`'s own `stat_key`.
    Attack {
        target_entity_id: String,
        stat_key: String,
    },
    /// Requests using `item_type` from this connection's own inventory —
    /// an opaque string. The server never validates ownership itself;
    /// the configured plugin's `on-item-use` hook decides what using it
    /// does (#154).
    UseItem { item_type: String },
    /// Requests interacting with `npc_entity_id` specifically, distinct
    /// from a generic trigger-volume interaction — the server confirms
    /// the target actually is a currently-spawned NPC before ever
    /// calling the configured plugin's `on-npc-interact` hook (#154).
    InteractNpc { npc_entity_id: String },
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
        /// The zone's server-authoritative simulation-step counter at
        /// the moment this was built (#196) — this connection's own
        /// baseline for reasoning about the ordering/staleness of every
        /// later `Moved`/`Rejected`.
        tick: u64,
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
        /// The *destination* zone's own tick counter (#196) — each
        /// zone-service instance ticks independently, so this is a
        /// fresh baseline for the new zone, not a continuation of the
        /// old one's.
        tick: u64,
    },
    /// An accepted movement update — broadcast to every connected client,
    /// not just the mover, so everyone's view of the zone stays current
    /// (phase 1 has no interest management yet, see
    /// docs/PROPOSAL.md's "Spatial Index: A → Z Roadmap"). `seq` is `0`
    /// for a move that didn't originate from a real client `Move`
    /// request (e.g. an NPC's plugin-driven movement) — a real client's
    /// own `seq` always starts at `1`, so a client correlating its own
    /// moves can simply ignore any `Moved` whose `seq` is `0` or doesn't
    /// match one it sent (#196). `tick` is the simulation step this was
    /// applied on.
    Moved {
        entity_id: String,
        x: f64,
        y: f64,
        seq: u32,
        tick: u64,
    },
    /// A movement update this connection itself requested was rejected —
    /// only sent back to the mover, never broadcast. `seq` echoes the
    /// rejected `Move`'s own `seq` (#196); `tick` is the simulation step
    /// the rejection was decided on.
    Rejected {
        reason: String,
        seq: u32,
        tick: u64,
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
    /// Replies to a `Ping` — `client_sent_at` is echoed back verbatim so
    /// the client can compute round-trip time against its own clock;
    /// `server_time` is the server's own wall-clock (Unix millis) at the
    /// moment it replied, for a client that wants clock-skew estimation
    /// too, not just RTT (#196).
    Pong {
        client_sent_at: i64,
        server_time: i64,
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
            ClientMessage::Move { x, y, seq } => Kind::Move(proto::Move {
                x: *x,
                y: *y,
                seq: *seq,
            }),
            ClientMessage::Ping { client_sent_at } => Kind::Ping(proto::Ping {
                client_sent_at: *client_sent_at,
            }),
            ClientMessage::Attack {
                target_entity_id,
                stat_key,
            } => Kind::Attack(proto::Attack {
                target_entity_id: target_entity_id.clone(),
                stat_key: stat_key.clone(),
            }),
            ClientMessage::UseItem { item_type } => Kind::UseItem(proto::UseItem {
                item_type: item_type.clone(),
            }),
            ClientMessage::InteractNpc { npc_entity_id } => Kind::InteractNpc(proto::InteractNpc {
                npc_entity_id: npc_entity_id.clone(),
            }),
        };
        proto::ClientMessage { kind: Some(kind) }
    }
}

impl TryFrom<proto::ClientMessage> for ClientMessage {
    type Error = Error;

    fn try_from(message: proto::ClientMessage) -> Result<Self> {
        use proto::client_message::Kind;
        match message.kind {
            Some(Kind::Move(proto::Move { x, y, seq })) => Ok(ClientMessage::Move { x, y, seq }),
            Some(Kind::Ping(proto::Ping { client_sent_at })) => {
                Ok(ClientMessage::Ping { client_sent_at })
            }
            Some(Kind::Attack(proto::Attack {
                target_entity_id,
                stat_key,
            })) => Ok(ClientMessage::Attack {
                target_entity_id,
                stat_key,
            }),
            Some(Kind::UseItem(proto::UseItem { item_type })) => {
                Ok(ClientMessage::UseItem { item_type })
            }
            Some(Kind::InteractNpc(proto::InteractNpc { npc_entity_id })) => {
                Ok(ClientMessage::InteractNpc { npc_entity_id })
            }
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
                tick,
            } => Kind::Joined(proto::Joined {
                entity_id: entity_id.clone(),
                x: *x,
                y: *y,
                roster: roster.iter().map(proto::RosterEntry::from).collect(),
                tick: *tick,
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
                tick,
            } => Kind::ZoneChanged(proto::ZoneChanged {
                zone_id: zone_id.clone(),
                entity_id: entity_id.clone(),
                x: *x,
                y: *y,
                roster: roster.iter().map(proto::RosterEntry::from).collect(),
                tick: *tick,
            }),
            ServerMessage::Moved {
                entity_id,
                x,
                y,
                seq,
                tick,
            } => Kind::Moved(proto::Moved {
                entity_id: entity_id.clone(),
                x: *x,
                y: *y,
                seq: *seq,
                tick: *tick,
            }),
            ServerMessage::Rejected { reason, seq, tick } => Kind::Rejected(proto::Rejected {
                reason: reason.clone(),
                seq: *seq,
                tick: *tick,
            }),
            ServerMessage::Error { message } => Kind::Error(proto::Error {
                message: message.clone(),
            }),
            ServerMessage::PluginMessage { body } => {
                Kind::PluginMessage(proto::PluginMessage { body: body.clone() })
            }
            ServerMessage::Pong {
                client_sent_at,
                server_time,
            } => Kind::Pong(proto::Pong {
                client_sent_at: *client_sent_at,
                server_time: *server_time,
            }),
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
                tick,
            })) => Ok(ServerMessage::Joined {
                entity_id,
                x,
                y,
                roster: roster.into_iter().map(RosterEntry::from).collect(),
                tick,
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
                tick,
            })) => Ok(ServerMessage::ZoneChanged {
                zone_id,
                entity_id,
                x,
                y,
                roster: roster.into_iter().map(RosterEntry::from).collect(),
                tick,
            }),
            Some(Kind::Moved(proto::Moved {
                entity_id,
                x,
                y,
                seq,
                tick,
            })) => Ok(ServerMessage::Moved {
                entity_id,
                x,
                y,
                seq,
                tick,
            }),
            Some(Kind::Rejected(proto::Rejected { reason, seq, tick })) => {
                Ok(ServerMessage::Rejected { reason, seq, tick })
            }
            Some(Kind::Error(proto::Error { message })) => Ok(ServerMessage::Error { message }),
            Some(Kind::PluginMessage(proto::PluginMessage { body })) => {
                Ok(ServerMessage::PluginMessage { body })
            }
            Some(Kind::Pong(proto::Pong {
                client_sent_at,
                server_time,
            })) => Ok(ServerMessage::Pong {
                client_sent_at,
                server_time,
            }),
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
        let message = ClientMessage::Move {
            x: 1.5,
            y: -2.0,
            seq: 7,
        };
        let envelope = message.into_envelope().unwrap();
        assert_eq!(envelope.message_type, WORLD_MESSAGE_TYPE);
        let decoded = ClientMessage::from_envelope(&envelope).unwrap();
        assert!(
            matches!(decoded, ClientMessage::Move { x, y, seq } if x == 1.5 && y == -2.0 && seq == 7)
        );
    }

    #[test]
    fn ping_round_trips_through_an_envelope() {
        let message = ClientMessage::Ping {
            client_sent_at: 12345,
        };
        let envelope = message.into_envelope().unwrap();
        let decoded = ClientMessage::from_envelope(&envelope).unwrap();
        assert!(
            matches!(decoded, ClientMessage::Ping { client_sent_at } if client_sent_at == 12345)
        );
    }

    #[test]
    fn pong_round_trips_through_an_envelope() {
        let message = ServerMessage::Pong {
            client_sent_at: 12345,
            server_time: 67890,
        };
        let envelope = message.into_envelope().unwrap();
        let decoded = ServerMessage::from_envelope(&envelope).unwrap();
        assert!(matches!(
            decoded,
            ServerMessage::Pong { client_sent_at, server_time }
                if client_sent_at == 12345 && server_time == 67890
        ));
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
            tick: 42,
        };
        let envelope = message.into_envelope().unwrap();
        let decoded = ServerMessage::from_envelope(&envelope).unwrap();
        assert!(matches!(
            decoded,
            ServerMessage::Joined { roster, .. } if roster.len() == 1 && roster[0].entity_id == "e2"
        ));
    }
}
