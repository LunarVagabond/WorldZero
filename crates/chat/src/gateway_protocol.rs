//! Wire protocol for chat's dev-facing gateway integration — how
//! `bin/demo` and `bin/gateway_server` talk chat over the real `gateway`
//! TCP/TLS transport (docs/specs/Networking_Spec.md's envelope catalog,
//! `message_type` 100 — see docs/specs/Chat_Spec.md, "Gateway demo
//! integration"). JSON payloads, not the leanest possible encoding, but
//! chat traffic is low-frequency and human-typed, so readability during
//! development wins here over wire efficiency.
//!
//! Scope: this is chat's own demo-integration protocol — a fixed, closed
//! set of message kinds `chat` itself defines. It is not a generic
//! "anyone can register a new command" mechanism; see
//! [#95](https://github.com/LunarVagabond/WorldZero/issues/95) for an
//! extensible message-type/command registry a plugin author could hook
//! into instead.

use common::id::{AccountId, ChannelId};
use common::{Error, Result};
use gateway::Envelope;
use serde::{Deserialize, Serialize};

pub const CHAT_MESSAGE_TYPE: u16 = 100;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind")]
pub enum ClientMessage {
    Hello { username: String },
    Join { channel: String },
    Leave { channel: String },
    Send { channel_id: ChannelId, body: String },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind")]
pub enum ServerMessage {
    Welcome {
        account_id: AccountId,
    },
    Joined {
        channel_id: ChannelId,
        channel: String,
    },
    Left {
        channel: String,
    },
    Chat {
        channel_id: ChannelId,
        channel: String,
        sender: String,
        body: String,
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
        .map_err(|e| Error::wrap("chat", "failed to encode gateway chat message", e))?;
    Ok(Envelope::new(CHAT_MESSAGE_TYPE, payload))
}

fn decode<T: for<'de> Deserialize<'de>>(envelope: &Envelope) -> Result<T> {
    if envelope.message_type != CHAT_MESSAGE_TYPE {
        return Err(Error::new(
            "chat",
            format!(
                "expected message_type {CHAT_MESSAGE_TYPE} (chat), got {}",
                envelope.message_type
            ),
        ));
    }
    serde_json::from_slice(&envelope.payload)
        .map_err(|e| Error::wrap("chat", "failed to decode gateway chat message", e))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn client_message_round_trips_through_an_envelope() {
        let message = ClientMessage::Join {
            channel: "trade".to_string(),
        };
        let envelope = message.into_envelope().unwrap();
        assert_eq!(envelope.message_type, CHAT_MESSAGE_TYPE);
        let decoded = ClientMessage::from_envelope(&envelope).unwrap();
        assert!(matches!(decoded, ClientMessage::Join { channel } if channel == "trade"));
    }

    #[test]
    fn decode_rejects_the_wrong_message_type() {
        let envelope = Envelope::new(1, b"{}".to_vec());
        assert!(ClientMessage::from_envelope(&envelope).is_err());
    }
}
