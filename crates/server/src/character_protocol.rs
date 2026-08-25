//! Wire protocol for character list/create/select (#193) — a connecting
//! client's view of "here are your characters on this realm, pick one or
//! make a new one," slotted between `server::realm_protocol`'s realm
//! selection and `server::session_protocol`'s world-join.
//! `message_type` 3 — see docs/specs/Networking_Spec.md's catalog note.
//! Protobuf payloads (`proto/character.proto`) — see
//! `auth::gateway_protocol`'s doc comment for why the ergonomic
//! `ClientMessage`/`ServerMessage` enums below wrap the generated
//! `proto` module rather than being it.
//!
//! Class/race/archetype selection is deliberately not part of this
//! protocol — that's plugin territory (a future character-creation
//! plugin hook), not core, per this project's "no game-specific concept
//! is privileged by the core" design principle. `CreateCharacter` is
//! strictly "reserve a name," nothing else.

use common::{Error, Result};
use gateway::Envelope;

mod proto {
    include!(concat!(env!("OUT_DIR"), "/worldzero.character.rs"));
}

pub const CHARACTER_MESSAGE_TYPE: u16 = 3;

#[derive(Debug, Clone)]
pub enum ClientMessage {
    /// Lists this account's characters on the already-selected realm
    /// (`realm_protocol::SelectRealm`'s policy-aware scoping — every
    /// character on this realm for a bound realm, every character across
    /// the whole open-realm group for an open one).
    ListCharacters,
    /// Reserves a new character with `name` — does not itself select or
    /// spawn it; a separate `SelectCharacter` is still required.
    CreateCharacter { name: String },
    /// Picks which of this account's characters to spawn into the world
    /// with — must actually be owned by this account.
    SelectCharacter { character_id: String },
}

#[derive(Debug, Clone, PartialEq)]
pub struct CharacterSummary {
    pub character_id: String,
    pub name: String,
    pub zone_id: String,
}

#[derive(Debug, Clone)]
pub enum ServerMessage {
    CharacterList { characters: Vec<CharacterSummary> },
    CharacterCreated { character_id: String },
    CharacterSelected { character_id: String },
    Error { message: String },
}

impl ClientMessage {
    #[allow(dead_code, clippy::wrong_self_convention)]
    pub fn into_envelope(&self) -> Result<Envelope> {
        encode(&proto::ClientMessage::from(self))
    }

    pub fn from_envelope(envelope: &Envelope) -> Result<Self> {
        decode::<proto::ClientMessage>(envelope)?.try_into()
    }
}

impl ServerMessage {
    #[allow(clippy::wrong_self_convention)]
    pub fn into_envelope(&self) -> Result<Envelope> {
        encode(&proto::ServerMessage::from(self))
    }

    #[allow(dead_code)]
    pub fn from_envelope(envelope: &Envelope) -> Result<Self> {
        decode::<proto::ServerMessage>(envelope)?.try_into()
    }
}

impl From<&CharacterSummary> for proto::CharacterSummary {
    fn from(summary: &CharacterSummary) -> Self {
        proto::CharacterSummary {
            character_id: summary.character_id.clone(),
            name: summary.name.clone(),
            zone_id: summary.zone_id.clone(),
        }
    }
}

impl From<proto::CharacterSummary> for CharacterSummary {
    fn from(summary: proto::CharacterSummary) -> Self {
        CharacterSummary {
            character_id: summary.character_id,
            name: summary.name,
            zone_id: summary.zone_id,
        }
    }
}

impl From<&ClientMessage> for proto::ClientMessage {
    fn from(message: &ClientMessage) -> Self {
        use proto::client_message::Kind;
        let kind = match message {
            ClientMessage::ListCharacters => Kind::ListCharacters(proto::ListCharacters {}),
            ClientMessage::CreateCharacter { name } => {
                Kind::CreateCharacter(proto::CreateCharacter { name: name.clone() })
            }
            ClientMessage::SelectCharacter { character_id } => {
                Kind::SelectCharacter(proto::SelectCharacter {
                    character_id: character_id.clone(),
                })
            }
        };
        proto::ClientMessage { kind: Some(kind) }
    }
}

impl TryFrom<proto::ClientMessage> for ClientMessage {
    type Error = Error;

    fn try_from(message: proto::ClientMessage) -> Result<Self> {
        use proto::client_message::Kind;
        match message.kind {
            Some(Kind::ListCharacters(proto::ListCharacters {})) => {
                Ok(ClientMessage::ListCharacters)
            }
            Some(Kind::CreateCharacter(proto::CreateCharacter { name })) => {
                Ok(ClientMessage::CreateCharacter { name })
            }
            Some(Kind::SelectCharacter(proto::SelectCharacter { character_id })) => {
                Ok(ClientMessage::SelectCharacter { character_id })
            }
            None => Err(Error::new(
                "server",
                "gateway character message has no kind set",
            )),
        }
    }
}

impl From<&ServerMessage> for proto::ServerMessage {
    fn from(message: &ServerMessage) -> Self {
        use proto::server_message::Kind;
        let kind = match message {
            ServerMessage::CharacterList { characters } => {
                Kind::CharacterList(proto::CharacterList {
                    characters: characters
                        .iter()
                        .map(proto::CharacterSummary::from)
                        .collect(),
                })
            }
            ServerMessage::CharacterCreated { character_id } => {
                Kind::CharacterCreated(proto::CharacterCreated {
                    character_id: character_id.clone(),
                })
            }
            ServerMessage::CharacterSelected { character_id } => {
                Kind::CharacterSelected(proto::CharacterSelected {
                    character_id: character_id.clone(),
                })
            }
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
            Some(Kind::CharacterList(proto::CharacterList { characters })) => {
                Ok(ServerMessage::CharacterList {
                    characters: characters.into_iter().map(CharacterSummary::from).collect(),
                })
            }
            Some(Kind::CharacterCreated(proto::CharacterCreated { character_id })) => {
                Ok(ServerMessage::CharacterCreated { character_id })
            }
            Some(Kind::CharacterSelected(proto::CharacterSelected { character_id })) => {
                Ok(ServerMessage::CharacterSelected { character_id })
            }
            Some(Kind::Error(proto::Error { message })) => Ok(ServerMessage::Error { message }),
            None => Err(Error::new(
                "server",
                "gateway character message has no kind set",
            )),
        }
    }
}

fn encode(message: &impl prost::Message) -> Result<Envelope> {
    Ok(Envelope::new(
        CHARACTER_MESSAGE_TYPE,
        message.encode_to_vec(),
    ))
}

fn decode<T: prost::Message + Default>(envelope: &Envelope) -> Result<T> {
    if envelope.message_type != CHARACTER_MESSAGE_TYPE {
        return Err(Error::new(
            "server",
            format!(
                "expected message_type {CHARACTER_MESSAGE_TYPE} (character), got {}",
                envelope.message_type
            ),
        ));
    }
    T::decode(envelope.payload.clone())
        .map_err(|e| Error::wrap("server", "failed to decode gateway character message", e))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn list_characters_round_trips_through_an_envelope() {
        let message = ClientMessage::ListCharacters;
        let envelope = message.into_envelope().unwrap();
        assert_eq!(envelope.message_type, CHARACTER_MESSAGE_TYPE);
        assert!(matches!(
            ClientMessage::from_envelope(&envelope).unwrap(),
            ClientMessage::ListCharacters
        ));
    }

    #[test]
    fn create_character_round_trips_through_an_envelope() {
        let message = ClientMessage::CreateCharacter {
            name: "Aria".to_string(),
        };
        let envelope = message.into_envelope().unwrap();
        let decoded = ClientMessage::from_envelope(&envelope).unwrap();
        assert!(matches!(decoded, ClientMessage::CreateCharacter { name } if name == "Aria"));
    }

    #[test]
    fn select_character_round_trips_through_an_envelope() {
        let message = ClientMessage::SelectCharacter {
            character_id: "abc".to_string(),
        };
        let envelope = message.into_envelope().unwrap();
        let decoded = ClientMessage::from_envelope(&envelope).unwrap();
        assert!(
            matches!(decoded, ClientMessage::SelectCharacter { character_id } if character_id == "abc")
        );
    }

    #[test]
    fn character_list_round_trips_a_summary() {
        let message = ServerMessage::CharacterList {
            characters: vec![CharacterSummary {
                character_id: "c1".to_string(),
                name: "Aria".to_string(),
                zone_id: "greenwood-forest".to_string(),
            }],
        };
        let envelope = message.into_envelope().unwrap();
        let decoded = ServerMessage::from_envelope(&envelope).unwrap();
        assert!(matches!(
            decoded,
            ServerMessage::CharacterList { characters } if characters.len() == 1 && characters[0].name == "Aria"
        ));
    }

    #[test]
    fn decode_rejects_the_wrong_message_type() {
        let envelope = Envelope::new(1, b"".to_vec());
        assert!(ClientMessage::from_envelope(&envelope).is_err());
    }

    #[test]
    fn decode_rejects_an_envelope_with_no_kind_set() {
        let envelope = Envelope::new(CHARACTER_MESSAGE_TYPE, b"".to_vec());
        let err = ClientMessage::from_envelope(&envelope).unwrap_err();
        assert!(err.to_string().contains("no kind set"), "{err}");
    }
}
