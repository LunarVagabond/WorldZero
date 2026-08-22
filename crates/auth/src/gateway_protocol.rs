//! Wire protocol for authenticating a gateway connection — the
//! login/registration handshake a client performs first, before any
//! other `message_type` is trusted with its claimed identity
//! (docs/specs/Auth_Spec.md, "Gateway handshake"). `message_type` 1 —
//! see docs/specs/Networking_Spec.md's catalog note.
//!
//! JSON payloads, same tradeoff as `chat::gateway_protocol`: readability
//! during development over wire efficiency, appropriate for a
//! low-frequency, connection-setup-only exchange.

use common::id::AccountId;
use common::{Error, Result};
use gateway::Envelope;
use serde::{Deserialize, Serialize};

pub const AUTH_MESSAGE_TYPE: u16 = 1;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind")]
pub enum ClientMessage {
    Register { username: String, password: String },
    Login { username: String, password: String },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind")]
pub enum ServerMessage {
    Authenticated {
        account_id: AccountId,
        username: String,
        session_token: String,
    },
    Error {
        message: String,
    },
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
        .map_err(|e| Error::wrap("auth", "failed to encode gateway auth message", e))?;
    Ok(Envelope::new(AUTH_MESSAGE_TYPE, payload))
}

fn decode<T: for<'de> Deserialize<'de>>(envelope: &Envelope) -> Result<T> {
    if envelope.message_type != AUTH_MESSAGE_TYPE {
        return Err(Error::new(
            "auth",
            format!(
                "expected message_type {AUTH_MESSAGE_TYPE} (auth), got {}",
                envelope.message_type
            ),
        ));
    }
    serde_json::from_slice(&envelope.payload)
        .map_err(|e| Error::wrap("auth", "failed to decode gateway auth message", e))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn client_message_round_trips_through_an_envelope() {
        let message = ClientMessage::Login {
            username: "alice".to_string(),
            password: "hunter2".to_string(),
        };
        let envelope = message.into_envelope().unwrap();
        assert_eq!(envelope.message_type, AUTH_MESSAGE_TYPE);
        let decoded = ClientMessage::from_envelope(&envelope).unwrap();
        assert!(matches!(decoded, ClientMessage::Login { username, .. } if username == "alice"));
    }

    #[test]
    fn decode_rejects_the_wrong_message_type() {
        let envelope = Envelope::new(100, b"{}".to_vec());
        assert!(ClientMessage::from_envelope(&envelope).is_err());
    }
}
