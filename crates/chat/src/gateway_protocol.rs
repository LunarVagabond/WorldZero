//! Wire protocol for chat's dev-facing gateway integration — how
//! `bin/demo` and `bin/gateway_server` talk chat over the real `gateway`
//! TCP/TLS transport (docs/specs/Networking_Spec.md's envelope catalog,
//! `message_type` 100 — see docs/specs/Chat_Spec.md, "Gateway demo
//! integration"). Protobuf payloads (`proto/chat.proto`, decision #109,
//! implemented in #123) — see `auth::gateway_protocol`'s doc comment for
//! why the ergonomic `ClientMessage`/`ServerMessage` enums below wrap the
//! generated `proto` module rather than being it.
//!
//! Identity is established *before* any of this — a connection must
//! complete the `auth::gateway_protocol` login/registration handshake
//! first (docs/specs/Auth_Spec.md, "Gateway handshake"); nothing here
//! carries a username or issues an account_id.
//!
//! Scope: this is chat's own demo-integration protocol — a fixed, closed
//! set of message kinds `chat` itself defines. It is not a generic
//! "anyone can register a new command" mechanism; see
//! [#95](https://github.com/LunarVagabond/WorldZero/issues/95) for an
//! extensible message-type/command registry a plugin author could hook
//! into instead.

use common::id::ChannelId;
use common::{Error, Result};
use gateway::Envelope;

mod proto {
    include!(concat!(env!("OUT_DIR"), "/worldzero.chat.rs"));
}

pub const CHAT_MESSAGE_TYPE: u16 = 100;

#[derive(Debug, Clone)]
pub enum ClientMessage {
    Join { channel: String },
    Leave { channel: String },
    Send { channel_id: ChannelId, body: String },
}

#[derive(Debug, Clone)]
pub enum ServerMessage {
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
        encode(&proto::ClientMessage::from(self))
    }

    pub fn from_envelope(envelope: &Envelope) -> Result<Self> {
        decode::<proto::ClientMessage>(envelope)?.try_into()
    }
}

impl ServerMessage {
    pub fn into_envelope(&self) -> Result<Envelope> {
        encode(&proto::ServerMessage::from(self))
    }

    pub fn from_envelope(envelope: &Envelope) -> Result<Self> {
        decode::<proto::ServerMessage>(envelope)?.try_into()
    }
}

impl From<&ClientMessage> for proto::ClientMessage {
    fn from(message: &ClientMessage) -> Self {
        use proto::client_message::Kind;
        let kind = match message {
            ClientMessage::Join { channel } => Kind::Join(proto::Join {
                channel: channel.clone(),
            }),
            ClientMessage::Leave { channel } => Kind::Leave(proto::Leave {
                channel: channel.clone(),
            }),
            ClientMessage::Send { channel_id, body } => Kind::Send(proto::Send {
                channel_id: channel_id.to_string(),
                body: body.clone(),
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
            Some(Kind::Join(proto::Join { channel })) => Ok(ClientMessage::Join { channel }),
            Some(Kind::Leave(proto::Leave { channel })) => Ok(ClientMessage::Leave { channel }),
            Some(Kind::Send(proto::Send { channel_id, body })) => Ok(ClientMessage::Send {
                channel_id: channel_id
                    .parse()
                    .map_err(|e| Error::wrap("chat", "invalid channel_id in wire message", e))?,
                body,
            }),
            None => Err(Error::new("chat", "gateway chat message has no kind set")),
        }
    }
}

impl From<&ServerMessage> for proto::ServerMessage {
    fn from(message: &ServerMessage) -> Self {
        use proto::server_message::Kind;
        let kind = match message {
            ServerMessage::Joined {
                channel_id,
                channel,
            } => Kind::Joined(proto::Joined {
                channel_id: channel_id.to_string(),
                channel: channel.clone(),
            }),
            ServerMessage::Left { channel } => Kind::Left(proto::Left {
                channel: channel.clone(),
            }),
            ServerMessage::Chat {
                channel_id,
                channel,
                sender,
                body,
            } => Kind::Chat(proto::Chat {
                channel_id: channel_id.to_string(),
                channel: channel.clone(),
                sender: sender.clone(),
                body: body.clone(),
            }),
            ServerMessage::Error { message } => Kind::Error(proto::Error {
                message: message.clone(),
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
                channel_id,
                channel,
            })) => Ok(ServerMessage::Joined {
                channel_id: channel_id
                    .parse()
                    .map_err(|e| Error::wrap("chat", "invalid channel_id in wire message", e))?,
                channel,
            }),
            Some(Kind::Left(proto::Left { channel })) => Ok(ServerMessage::Left { channel }),
            Some(Kind::Chat(proto::Chat {
                channel_id,
                channel,
                sender,
                body,
            })) => Ok(ServerMessage::Chat {
                channel_id: channel_id
                    .parse()
                    .map_err(|e| Error::wrap("chat", "invalid channel_id in wire message", e))?,
                channel,
                sender,
                body,
            }),
            Some(Kind::Error(proto::Error { message })) => Ok(ServerMessage::Error { message }),
            None => Err(Error::new("chat", "gateway chat message has no kind set")),
        }
    }
}

fn encode(message: &impl prost::Message) -> Result<Envelope> {
    Ok(Envelope::new(CHAT_MESSAGE_TYPE, message.encode_to_vec()))
}

fn decode<T: prost::Message + Default>(envelope: &Envelope) -> Result<T> {
    if envelope.message_type != CHAT_MESSAGE_TYPE {
        return Err(Error::new(
            "chat",
            format!(
                "expected message_type {CHAT_MESSAGE_TYPE} (chat), got {}",
                envelope.message_type
            ),
        ));
    }
    T::decode(envelope.payload.clone())
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
        let envelope = Envelope::new(1, b"".to_vec());
        assert!(ClientMessage::from_envelope(&envelope).is_err());
    }

    #[test]
    fn decode_rejects_an_envelope_with_no_kind_set() {
        let envelope = Envelope::new(CHAT_MESSAGE_TYPE, b"".to_vec());
        let err = ClientMessage::from_envelope(&envelope).unwrap_err();
        assert!(err.to_string().contains("no kind set"), "{err}");
    }

    #[test]
    fn server_message_chat_round_trips_through_an_envelope() {
        // Only `ClientMessage` got a round-trip test before — `ServerMessage`
        // (Joined/Left/Chat/Error) had zero `into_envelope`/`from_envelope`
        // coverage, unlike `auth::gateway_protocol` which tests both
        // directions. A field added to `Chat` without updating both the
        // `From`/`TryFrom` impls would have gone uncaught here.
        let channel_id = ChannelId::new();
        let message = ServerMessage::Chat {
            channel_id,
            channel: "trade".to_string(),
            sender: "alice".to_string(),
            body: "hello".to_string(),
        };
        let envelope = message.into_envelope().unwrap();
        assert_eq!(envelope.message_type, CHAT_MESSAGE_TYPE);
        let decoded = ServerMessage::from_envelope(&envelope).unwrap();
        assert!(matches!(
            decoded,
            ServerMessage::Chat { channel_id: decoded_id, channel, sender, body }
                if decoded_id == channel_id && channel == "trade" && sender == "alice" && body == "hello"
        ));
    }

    #[test]
    fn server_message_joined_and_error_round_trip_through_an_envelope() {
        let channel_id = ChannelId::new();
        let joined = ServerMessage::Joined {
            channel_id,
            channel: "trade".to_string(),
        };
        let decoded = ServerMessage::from_envelope(&joined.into_envelope().unwrap()).unwrap();
        assert!(matches!(
            decoded,
            ServerMessage::Joined { channel_id: decoded_id, channel }
                if decoded_id == channel_id && channel == "trade"
        ));

        let error = ServerMessage::Error {
            message: "something broke".to_string(),
        };
        let decoded = ServerMessage::from_envelope(&error.into_envelope().unwrap()).unwrap();
        assert!(
            matches!(decoded, ServerMessage::Error { message } if message == "something broke")
        );
    }

    #[test]
    fn send_round_trips_a_channel_id() {
        let channel_id = ChannelId::new();
        let message = ClientMessage::Send {
            channel_id,
            body: "hello".to_string(),
        };
        let envelope = message.into_envelope().unwrap();
        let decoded = ClientMessage::from_envelope(&envelope).unwrap();
        assert!(
            matches!(decoded, ClientMessage::Send { channel_id: decoded_id, body } if decoded_id == channel_id && body == "hello")
        );
    }
}
