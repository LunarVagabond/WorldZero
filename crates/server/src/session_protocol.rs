//! Wire protocol for the phase-1 `server` binary's gateway-connected
//! movement session — how a client moves and sees other entities move
//! once authenticated (docs/PROPOSAL.md, "Phased Roadmap," Phase 1: "one
//! client able to connect, move, and persist state across sessions").
//! `message_type` 200 — see docs/specs/Networking_Spec.md's catalog note.
//! JSON payloads, same tradeoff as `auth`/`chat`'s gateway protocols.
//!
//! Identity comes from `auth::gateway_protocol`'s handshake first, same
//! as chat's gateway integration — nothing here carries or trusts a
//! client-claimed identity.

use common::{Error, Result};
use gateway::Envelope;
use serde::{Deserialize, Serialize};

pub const WORLD_MESSAGE_TYPE: u16 = 200;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind")]
pub enum ClientMessage {
    /// Requests moving this connection's own entity to `(x, y)` — queued
    /// for the next simulation tick, never applied immediately
    /// (`world::Zone::request_move`).
    Move { x: f64, y: f64 },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind")]
pub enum ServerMessage {
    /// Sent once, right after this connection's own entity is spawned
    /// into the zone — its assigned `entity_id` and starting position
    /// (loaded from the character's persisted position, or the zero
    /// position for a newly created character).
    Spawned { entity_id: String, x: f64, y: f64 },
    /// An entity (this connection's own, or another) newly exists in the
    /// zone — sent for every other currently-spawned entity when a
    /// client joins, and broadcast whenever a new one spawns afterward
    /// (including an NPC the plugin host spawns).
    EntitySpawned {
        entity_id: String,
        entity_type: String,
        x: f64,
        y: f64,
    },
    EntityDespawned { entity_id: String },
    /// An accepted movement update — broadcast to every connected client,
    /// not just the mover, so everyone's view of the zone stays current
    /// (phase 1 has no interest management yet, see
    /// docs/PROPOSAL.md's "Spatial Index: A → Z Roadmap").
    Moved { entity_id: String, x: f64, y: f64 },
    /// A movement update this connection itself requested was rejected —
    /// only sent back to the mover, never broadcast.
    Rejected { reason: String },
    Error { message: String },
}

impl ClientMessage {
    pub fn into_envelope(&self) -> Result<Envelope> {
        encode(self)
    }

    pub fn from_envelope(envelope: &Envelope) -> Result<Self> {
        decode(envelope)
    }
}

impl ServerMessage {
    pub fn into_envelope(&self) -> Result<Envelope> {
        encode(self)
    }

    pub fn from_envelope(envelope: &Envelope) -> Result<Self> {
        decode(envelope)
    }
}

fn encode(message: &impl Serialize) -> Result<Envelope> {
    let payload = serde_json::to_vec(message)
        .map_err(|e| Error::wrap("server", "failed to encode gateway world message", e))?;
    Ok(Envelope::new(WORLD_MESSAGE_TYPE, payload))
}

fn decode<T: for<'de> Deserialize<'de>>(envelope: &Envelope) -> Result<T> {
    if envelope.message_type != WORLD_MESSAGE_TYPE {
        return Err(Error::new(
            "server",
            format!(
                "expected message_type {WORLD_MESSAGE_TYPE} (world), got {}",
                envelope.message_type
            ),
        ));
    }
    serde_json::from_slice(&envelope.payload)
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
        let envelope = Envelope::new(1, b"{}".to_vec());
        assert!(ClientMessage::from_envelope(&envelope).is_err());
    }
}
