//! Wire protocol for authenticating a gateway connection — the
//! login/registration handshake a client performs first, before any
//! other `message_type` is trusted with its claimed identity
//! (docs/specs/Auth_Spec.md, "Gateway handshake"). `message_type` 1 —
//! see docs/specs/Networking_Spec.md's catalog note.
//!
//! Protobuf payloads (`proto/auth.proto`, decision #109, implemented in
//! #123) — `ClientMessage`/`ServerMessage` below are a hand-written,
//! ergonomic Rust enum every other call site in this codebase already
//! matches on; `encode`/`decode` bridge them to/from the generated
//! `proto` module (`build.rs`) rather than exposing the generated
//! `oneof`-shaped types directly, so this migration off `serde_json`
//! didn't need to touch every match site across `auth`/`chat`/`server`.

use common::id::AccountId;
use common::{Error, Result};
use gateway::Envelope;

mod proto {
    include!(concat!(env!("OUT_DIR"), "/worldzero.auth.rs"));
}

pub const AUTH_MESSAGE_TYPE: u16 = 1;

#[derive(Debug, Clone)]
pub enum ClientMessage {
    Register { username: String, password: String },
    Login { username: String, password: String },
}

#[derive(Debug, Clone)]
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
            ClientMessage::Register { username, password } => Kind::Register(proto::Register {
                username: username.clone(),
                password: password.clone(),
            }),
            ClientMessage::Login { username, password } => Kind::Login(proto::Login {
                username: username.clone(),
                password: password.clone(),
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
            Some(Kind::Register(proto::Register { username, password })) => {
                Ok(ClientMessage::Register { username, password })
            }
            Some(Kind::Login(proto::Login { username, password })) => {
                Ok(ClientMessage::Login { username, password })
            }
            None => Err(Error::new("auth", "gateway auth message has no kind set")),
        }
    }
}

impl From<&ServerMessage> for proto::ServerMessage {
    fn from(message: &ServerMessage) -> Self {
        use proto::server_message::Kind;
        let kind = match message {
            ServerMessage::Authenticated {
                account_id,
                username,
                session_token,
            } => Kind::Authenticated(proto::Authenticated {
                account_id: account_id.to_string(),
                username: username.clone(),
                session_token: session_token.clone(),
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
            Some(Kind::Authenticated(proto::Authenticated {
                account_id,
                username,
                session_token,
            })) => Ok(ServerMessage::Authenticated {
                account_id: account_id
                    .parse()
                    .map_err(|e| Error::wrap("auth", "invalid account_id in wire message", e))?,
                username,
                session_token,
            }),
            Some(Kind::Error(proto::Error { message })) => Ok(ServerMessage::Error { message }),
            None => Err(Error::new("auth", "gateway auth message has no kind set")),
        }
    }
}

fn encode(message: &impl prost::Message) -> Result<Envelope> {
    Ok(Envelope::new(AUTH_MESSAGE_TYPE, message.encode_to_vec()))
}

fn decode<T: prost::Message + Default>(envelope: &Envelope) -> Result<T> {
    if envelope.message_type != AUTH_MESSAGE_TYPE {
        return Err(Error::new(
            "auth",
            format!(
                "expected message_type {AUTH_MESSAGE_TYPE} (auth), got {}",
                envelope.message_type
            ),
        ));
    }
    T::decode(envelope.payload.clone())
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
        let envelope = Envelope::new(100, b"".to_vec());
        assert!(ClientMessage::from_envelope(&envelope).is_err());
    }

    #[test]
    fn decode_rejects_an_envelope_with_no_kind_set() {
        let envelope = Envelope::new(AUTH_MESSAGE_TYPE, b"".to_vec());
        let err = ClientMessage::from_envelope(&envelope).unwrap_err();
        assert!(err.to_string().contains("no kind set"), "{err}");
    }

    #[test]
    fn server_message_round_trips_through_an_envelope() {
        let message = ServerMessage::Authenticated {
            account_id: AccountId::new(),
            username: "alice".to_string(),
            session_token: "tok".to_string(),
        };
        let envelope = message.into_envelope().unwrap();
        let decoded = ServerMessage::from_envelope(&envelope).unwrap();
        assert!(
            matches!(decoded, ServerMessage::Authenticated { username, .. } if username == "alice")
        );
    }
}
