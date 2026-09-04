//! Wire protocol for realm discovery/selection (#136's follow-on, #192)
//! — a connecting client's view of "here are the realm(s) this server
//! serves, pick one," slotted between `auth::gateway_protocol`'s
//! handshake and `server::session_protocol`'s world-join.
//! `message_type` 2 — see docs/specs/Networking_Spec.md's catalog note.
//! Protobuf payloads (`proto/realm.proto`) — see
//! `auth::gateway_protocol`'s doc comment for why the ergonomic
//! `ClientMessage`/`ServerMessage` enums below wrap the generated
//! `proto` module rather than being it.
//!
//! Today a `server` process only ever resolves and serves exactly one
//! realm (`WZ_REALM_ID`, #136) — a process serving more than one at once
//! is #130's job, not this protocol's. `RealmList` always reports a
//! single-entry list for that reason; the wire shape (a `repeated`
//! field) is already ready for #130 to fill in without a protocol
//! change. `SelectRealm` still has to name that same one realm — this
//! is the single point that feeds #136's `LoginPolicy` resolution/
//! enforcement for the rest of the connection, not a rubber stamp.

use common::id::RealmId;
use common::{Error, Result};
use gateway::Envelope;

mod proto {
    include!(concat!(env!("OUT_DIR"), "/worldzero.realm.rs"));
}

pub const REALM_MESSAGE_TYPE: u16 = 2;

#[derive(Debug, Clone)]
pub enum ClientMessage {
    /// Requests the realm list — a client can skip sending this
    /// entirely and go straight to `SelectRealm` if it already knows
    /// the realm id (e.g. hardcoded for a single-realm game), same
    /// "no UI required" sense the milestone ticket's "skippable" means.
    ListRealms,
    /// Picks which realm the rest of this connection's session applies
    /// to — must name the one realm this `server` process actually
    /// serves; anything else is rejected.
    SelectRealm { realm_id: String },
}

#[derive(Debug, Clone, PartialEq)]
pub struct RealmSummary {
    pub realm_id: String,
    pub name: String,
    pub open_or_bound: String,
    /// A strict census of characters whose `characters.realm_id` row is
    /// *this specific realm* (`character::CharacterStore::count_for_realm`)
    /// — a population number, not "characters this account can select
    /// here". For an `open`-policy realm those are different things: the
    /// character *list* a client actually gets back (`ListCharacters` →
    /// `realm_directory::login_policy::list_characters` →
    /// `CharacterStore::list_by_account_in_open_realms`) spans every open
    /// realm's characters for that account, not just this one, by design
    /// (open realms share one character pool). See
    /// `selectable_character_count` below for the number that actually
    /// matches what `ListCharacters` would return.
    pub character_count: i64,
    pub live_connection_count: u64,
    /// The number of characters `ListCharacters` would actually return
    /// if this connection selected this realm right now (#261) — what a
    /// realm-select picker should show, since it's the number that
    /// matches Character Select's own list. Equals `character_count`
    /// for a `bound` realm; can exceed it for an `open` realm, whose
    /// list spans every sibling open realm's characters for this
    /// account (`realm_directory::LoginPolicy::list_characters`, the
    /// same call `ListCharacters` itself makes).
    pub selectable_character_count: i64,
}

#[derive(Debug, Clone)]
pub enum ServerMessage {
    RealmList { realms: Vec<RealmSummary> },
    RealmSelected { realm_id: String },
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

impl From<&RealmSummary> for proto::RealmSummary {
    fn from(summary: &RealmSummary) -> Self {
        proto::RealmSummary {
            realm_id: summary.realm_id.clone(),
            name: summary.name.clone(),
            open_or_bound: summary.open_or_bound.clone(),
            character_count: summary.character_count,
            live_connection_count: summary.live_connection_count,
            selectable_character_count: summary.selectable_character_count,
        }
    }
}

impl From<proto::RealmSummary> for RealmSummary {
    fn from(summary: proto::RealmSummary) -> Self {
        RealmSummary {
            realm_id: summary.realm_id,
            name: summary.name,
            open_or_bound: summary.open_or_bound,
            character_count: summary.character_count,
            selectable_character_count: summary.selectable_character_count,
            live_connection_count: summary.live_connection_count,
        }
    }
}

impl From<&ClientMessage> for proto::ClientMessage {
    fn from(message: &ClientMessage) -> Self {
        use proto::client_message::Kind;
        let kind = match message {
            ClientMessage::ListRealms => Kind::ListRealms(proto::ListRealms {}),
            ClientMessage::SelectRealm { realm_id } => Kind::SelectRealm(proto::SelectRealm {
                realm_id: realm_id.clone(),
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
            Some(Kind::ListRealms(proto::ListRealms {})) => Ok(ClientMessage::ListRealms),
            Some(Kind::SelectRealm(proto::SelectRealm { realm_id })) => {
                Ok(ClientMessage::SelectRealm { realm_id })
            }
            None => Err(Error::new(
                "server",
                "gateway realm message has no kind set",
            )),
        }
    }
}

impl From<&ServerMessage> for proto::ServerMessage {
    fn from(message: &ServerMessage) -> Self {
        use proto::server_message::Kind;
        let kind = match message {
            ServerMessage::RealmList { realms } => Kind::RealmList(proto::RealmList {
                realms: realms.iter().map(proto::RealmSummary::from).collect(),
            }),
            ServerMessage::RealmSelected { realm_id } => {
                Kind::RealmSelected(proto::RealmSelected {
                    realm_id: realm_id.clone(),
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
            Some(Kind::RealmList(proto::RealmList { realms })) => Ok(ServerMessage::RealmList {
                realms: realms.into_iter().map(RealmSummary::from).collect(),
            }),
            Some(Kind::RealmSelected(proto::RealmSelected { realm_id })) => {
                Ok(ServerMessage::RealmSelected { realm_id })
            }
            Some(Kind::Error(proto::Error { message })) => Ok(ServerMessage::Error { message }),
            None => Err(Error::new(
                "server",
                "gateway realm message has no kind set",
            )),
        }
    }
}

/// Parses and validates a `SelectRealm.realm_id` against the one realm
/// `server` actually serves — the single point `session::handle_session`
/// calls to decide accept/reject, kept here so the validation logic
/// lives next to the protocol it validates.
pub fn validate_selection(realm_id: &str, serving: RealmId) -> Result<RealmId> {
    let selected: RealmId = realm_id
        .parse()
        .map_err(|_| Error::new("server", format!("{realm_id:?} is not a valid realm id")))?;
    if selected != serving {
        return Err(Error::new(
            "server",
            format!(
                "this server only serves realm {serving} — multi-realm selection isn't supported yet"
            ),
        ));
    }
    Ok(selected)
}

fn encode(message: &impl prost::Message) -> Result<Envelope> {
    Ok(Envelope::new(REALM_MESSAGE_TYPE, message.encode_to_vec()))
}

fn decode<T: prost::Message + Default>(envelope: &Envelope) -> Result<T> {
    if envelope.message_type != REALM_MESSAGE_TYPE {
        return Err(Error::new(
            "server",
            format!(
                "expected message_type {REALM_MESSAGE_TYPE} (realm), got {}",
                envelope.message_type
            ),
        ));
    }
    T::decode(envelope.payload.clone())
        .map_err(|e| Error::wrap("server", "failed to decode gateway realm message", e))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn list_realms_round_trips_through_an_envelope() {
        let message = ClientMessage::ListRealms;
        let envelope = message.into_envelope().unwrap();
        assert_eq!(envelope.message_type, REALM_MESSAGE_TYPE);
        assert!(matches!(
            ClientMessage::from_envelope(&envelope).unwrap(),
            ClientMessage::ListRealms
        ));
    }

    #[test]
    fn select_realm_round_trips_through_an_envelope() {
        let message = ClientMessage::SelectRealm {
            realm_id: "abc".to_string(),
        };
        let envelope = message.into_envelope().unwrap();
        let decoded = ClientMessage::from_envelope(&envelope).unwrap();
        assert!(matches!(decoded, ClientMessage::SelectRealm { realm_id } if realm_id == "abc"));
    }

    #[test]
    fn realm_list_round_trips_a_summary() {
        let message = ServerMessage::RealmList {
            realms: vec![RealmSummary {
                realm_id: "r1".to_string(),
                name: "Test Realm".to_string(),
                open_or_bound: "open".to_string(),
                character_count: 3,
                selectable_character_count: 2,
                live_connection_count: 1,
            }],
        };
        let envelope = message.into_envelope().unwrap();
        let decoded = ServerMessage::from_envelope(&envelope).unwrap();
        assert!(matches!(
            decoded,
            ServerMessage::RealmList { realms } if realms.len() == 1 && realms[0].name == "Test Realm"
        ));
    }

    #[test]
    fn decode_rejects_the_wrong_message_type() {
        let envelope = Envelope::new(1, b"".to_vec());
        assert!(ClientMessage::from_envelope(&envelope).is_err());
    }

    #[test]
    fn decode_rejects_an_envelope_with_no_kind_set() {
        let envelope = Envelope::new(REALM_MESSAGE_TYPE, b"".to_vec());
        let err = ClientMessage::from_envelope(&envelope).unwrap_err();
        assert!(err.to_string().contains("no kind set"), "{err}");
    }

    #[test]
    fn validate_selection_accepts_the_serving_realm() {
        let realm_id = RealmId::new();
        assert_eq!(
            validate_selection(&realm_id.to_string(), realm_id).unwrap(),
            realm_id
        );
    }

    #[test]
    fn validate_selection_rejects_a_different_realm() {
        let serving = RealmId::new();
        let other = RealmId::new();
        let err = validate_selection(&other.to_string(), serving).unwrap_err();
        assert!(err.to_string().contains("only serves realm"), "{err}");
    }

    #[test]
    fn validate_selection_rejects_a_malformed_realm_id() {
        let err = validate_selection("not-a-uuid", RealmId::new()).unwrap_err();
        assert!(err.to_string().contains("not a valid realm id"), "{err}");
    }
}
