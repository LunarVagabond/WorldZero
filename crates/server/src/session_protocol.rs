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
    /// Requests moving this connection's own entity onto whichever layer
    /// of the *current* zone `other_entity_id` is already spawned into
    /// (#142) — the live layer-reassignment mechanism the real party
    /// system (#178) uses once two players form a party. `other_entity_id`
    /// must actually be a fellow party member (#178) — rejected otherwise.
    /// A no-op if `other_entity_id` isn't spawned anywhere in this zone,
    /// or is already on this connection's own layer.
    JoinGroupLayer { other_entity_id: String },
    /// Invites `target_entity_id` (any currently-connected player, any
    /// zone) to a party (#178) — creates one first if this connection
    /// isn't already in one, or grows its existing party. `party_type`
    /// names a `party.schema.yaml`-declared type; empty resolves to the
    /// schema's first declared entry, and only matters when this invite
    /// founds a *new* party (joining an existing one always uses
    /// whatever type it was actually founded under).
    PartyInvite {
        target_entity_id: String,
        party_type: String,
    },
    /// Answers this connection's own most recently received party
    /// invite, if any (#178) — an `Error` if there is none pending.
    PartyInviteResponse { accept: bool },
    /// Leaves this connection's own current party (#178) — a real
    /// `character::PartyStore` write, not a chat-channel leave. `Error`
    /// if this connection isn't currently in a party.
    PartyLeave {},
    /// Creates a new guild owned by this connection's own account,
    /// named `name` (#179) — `Error` if the account is already in a
    /// guild. The founder is placed at the guild schema's founder rank
    /// (`guild::GuildSchema::founder_rank`).
    GuildCreate { name: String },
    /// Invites `target_entity_id`'s account to this connection's own
    /// guild (#179) — `Error` if this connection has no guild, its rank
    /// lacks the `invite` permission, or the target is already in a
    /// guild.
    GuildInvite { target_entity_id: String },
    /// Answers this connection's own most recently received guild
    /// invite, if any (#179) — an `Error` if there is none pending.
    GuildInviteResponse { accept: bool },
    /// Leaves this connection's own current guild (#179). A member at
    /// the guild's founder rank can't leave while other members remain
    /// — they must promote a successor or disband first; a lone founder
    /// leaving dissolves the guild entirely.
    GuildLeave {},
    /// Disbands this connection's own current guild (#179) — only a
    /// founder-rank member may do this.
    GuildDisband {},
    /// Removes `target_entity_id`'s account from this connection's own
    /// guild (#179) — requires the `kick` permission; a founder-rank
    /// target can never be kicked.
    GuildKick { target_entity_id: String },
    /// Moves `target_entity_id`'s account to `rank_key` within this
    /// connection's own guild (#179) — requires the `promote`
    /// permission. Moving anyone into or out of the founder rank is
    /// restricted to an actor who already holds the founder rank
    /// themselves, regardless of the `promote` permission
    /// (`guild::GuildStore`'s own core invariant).
    GuildPromote {
        target_entity_id: String,
        rank_key: String,
    },
    /// Same shape as `GuildPromote`, gated by the `demote` permission
    /// instead (#179) — the store applies the identical rank-move logic
    /// either way; which message a client sends is purely a UI-intent
    /// distinction.
    GuildDemote {
        target_entity_id: String,
        rank_key: String,
    },
    /// Sets this connection's own guild's message-of-the-day (#179) —
    /// requires the `edit_motd` permission. Empty string clears it.
    GuildSetMotd { motd: String },
    /// Sets this connection's own guild's short tag/abbreviation (#179)
    /// — requires the `edit_tag` permission. Empty string clears it.
    GuildSetTag { tag: String },
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
    /// A party invite has been sent to this connection's own entity
    /// (#178) — `from_entity_id` names the inviter. Answer with
    /// `PartyInviteResponse`.
    PartyInviteReceived {
        from_entity_id: String,
    },
    /// The party invite this connection sent was declined (#178).
    PartyInviteDeclined {
        by_entity_id: String,
    },
    /// This connection's current party roster, sent after any membership
    /// change (accept, leave, disband — #178) — every *other* character
    /// currently in the party, as their live entity ids. Empty means "no
    /// party" (just left, or the party just dissolved).
    PartyUpdate {
        members: Vec<String>,
    },
    /// A guild invite has been sent to this connection's own entity
    /// (#179) — `from_entity_id` names the inviter. Answer with
    /// `GuildInviteResponse`.
    GuildInviteReceived {
        from_entity_id: String,
    },
    /// The guild invite this connection sent was declined (#179).
    GuildInviteDeclined {
        by_entity_id: String,
    },
    /// This connection's current guild, sent after any membership or
    /// metadata change (create, invite accept, leave, kick, disband,
    /// rename, motd/tag edit — #179). `guild_id` is empty and `members`
    /// is empty to mean "no guild" (just left, kicked, or the guild just
    /// dissolved). Each member's `entity_id` is empty if that account
    /// isn't currently connected — unlike a party roster, a guild
    /// roster includes offline members.
    GuildUpdate {
        guild_id: String,
        name: String,
        motd: String,
        tag: String,
        members: Vec<GuildMemberEntry>,
    },
    /// This connection's guild was disbanded (#179).
    GuildDisbanded {},
    /// Pushed to this connection whenever one of its own character's
    /// declared stats actually changes via `apply-stat-delta`/
    /// `apply-stat-delta-for-character` (#211/#210) — automatic, no
    /// plugin-side `send-message` required. `value` is the resulting
    /// stat value after the delta, not the delta itself. Never sent for
    /// an NPC-targeted `apply-stat-delta` (#197) — an NPC has no owning
    /// connection to push to; and never sent for
    /// `apply-stat-delta-for-character` if the character has no live
    /// connection at the moment it's called (it fires from
    /// `on-character-create`, before any entity/session necessarily
    /// exists — see `wit/plugin.wit`'s doc comment).
    StatChanged {
        stat_key: String,
        value: i64,
    },
    /// Pushed to this connection whenever one of its own character's
    /// item stacks actually changes via `grant-item`/`remove-item`
    /// (#211/#210) — `quantity` is the stack's resulting total (`0` if a
    /// `remove-item` emptied the stack), not the delta granted/removed.
    ItemChanged {
        item_type: String,
        quantity: i64,
    },
    /// Pushed to this connection whenever its own character's currency
    /// balance actually changes via `modify-currency` (#211/#210) —
    /// `balance` is the resulting balance, not the delta. `currency_key`
    /// (#218) names which of the dev-declared currencies
    /// (`currency.schema.yaml`) changed.
    CurrencyChanged {
        currency_key: String,
        balance: i64,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub struct GuildMemberEntry {
    pub entity_id: String,
    pub rank_key: String,
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

impl From<&GuildMemberEntry> for proto::GuildMember {
    fn from(entry: &GuildMemberEntry) -> Self {
        proto::GuildMember {
            entity_id: entry.entity_id.clone(),
            rank_key: entry.rank_key.clone(),
        }
    }
}

impl From<proto::GuildMember> for GuildMemberEntry {
    fn from(entry: proto::GuildMember) -> Self {
        GuildMemberEntry {
            entity_id: entry.entity_id,
            rank_key: entry.rank_key,
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
            ClientMessage::JoinGroupLayer { other_entity_id } => {
                Kind::JoinGroupLayer(proto::JoinGroupLayer {
                    other_entity_id: other_entity_id.clone(),
                })
            }
            ClientMessage::PartyInvite {
                target_entity_id,
                party_type,
            } => Kind::PartyInvite(proto::PartyInvite {
                target_entity_id: target_entity_id.clone(),
                party_type: party_type.clone(),
            }),
            ClientMessage::PartyInviteResponse { accept } => {
                Kind::PartyInviteResponse(proto::PartyInviteResponse { accept: *accept })
            }
            ClientMessage::PartyLeave {} => Kind::PartyLeave(proto::PartyLeave {}),
            ClientMessage::GuildCreate { name } => {
                Kind::GuildCreate(proto::GuildCreate { name: name.clone() })
            }
            ClientMessage::GuildInvite { target_entity_id } => {
                Kind::GuildInvite(proto::GuildInvite {
                    target_entity_id: target_entity_id.clone(),
                })
            }
            ClientMessage::GuildInviteResponse { accept } => {
                Kind::GuildInviteResponse(proto::GuildInviteResponse { accept: *accept })
            }
            ClientMessage::GuildLeave {} => Kind::GuildLeave(proto::GuildLeave {}),
            ClientMessage::GuildDisband {} => Kind::GuildDisband(proto::GuildDisband {}),
            ClientMessage::GuildKick { target_entity_id } => Kind::GuildKick(proto::GuildKick {
                target_entity_id: target_entity_id.clone(),
            }),
            ClientMessage::GuildPromote {
                target_entity_id,
                rank_key,
            } => Kind::GuildPromote(proto::GuildPromote {
                target_entity_id: target_entity_id.clone(),
                rank_key: rank_key.clone(),
            }),
            ClientMessage::GuildDemote {
                target_entity_id,
                rank_key,
            } => Kind::GuildDemote(proto::GuildDemote {
                target_entity_id: target_entity_id.clone(),
                rank_key: rank_key.clone(),
            }),
            ClientMessage::GuildSetMotd { motd } => {
                Kind::GuildSetMotd(proto::GuildSetMotd { motd: motd.clone() })
            }
            ClientMessage::GuildSetTag { tag } => {
                Kind::GuildSetTag(proto::GuildSetTag { tag: tag.clone() })
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
            Some(Kind::JoinGroupLayer(proto::JoinGroupLayer { other_entity_id })) => {
                Ok(ClientMessage::JoinGroupLayer { other_entity_id })
            }
            Some(Kind::PartyInvite(proto::PartyInvite {
                target_entity_id,
                party_type,
            })) => Ok(ClientMessage::PartyInvite {
                target_entity_id,
                party_type,
            }),
            Some(Kind::PartyInviteResponse(proto::PartyInviteResponse { accept })) => {
                Ok(ClientMessage::PartyInviteResponse { accept })
            }
            Some(Kind::PartyLeave(proto::PartyLeave {})) => Ok(ClientMessage::PartyLeave {}),
            Some(Kind::GuildCreate(proto::GuildCreate { name })) => {
                Ok(ClientMessage::GuildCreate { name })
            }
            Some(Kind::GuildInvite(proto::GuildInvite { target_entity_id })) => {
                Ok(ClientMessage::GuildInvite { target_entity_id })
            }
            Some(Kind::GuildInviteResponse(proto::GuildInviteResponse { accept })) => {
                Ok(ClientMessage::GuildInviteResponse { accept })
            }
            Some(Kind::GuildLeave(proto::GuildLeave {})) => Ok(ClientMessage::GuildLeave {}),
            Some(Kind::GuildDisband(proto::GuildDisband {})) => Ok(ClientMessage::GuildDisband {}),
            Some(Kind::GuildKick(proto::GuildKick { target_entity_id })) => {
                Ok(ClientMessage::GuildKick { target_entity_id })
            }
            Some(Kind::GuildPromote(proto::GuildPromote {
                target_entity_id,
                rank_key,
            })) => Ok(ClientMessage::GuildPromote {
                target_entity_id,
                rank_key,
            }),
            Some(Kind::GuildDemote(proto::GuildDemote {
                target_entity_id,
                rank_key,
            })) => Ok(ClientMessage::GuildDemote {
                target_entity_id,
                rank_key,
            }),
            Some(Kind::GuildSetMotd(proto::GuildSetMotd { motd })) => {
                Ok(ClientMessage::GuildSetMotd { motd })
            }
            Some(Kind::GuildSetTag(proto::GuildSetTag { tag })) => {
                Ok(ClientMessage::GuildSetTag { tag })
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
            ServerMessage::PartyInviteReceived { from_entity_id } => {
                Kind::PartyInviteReceived(proto::PartyInviteReceived {
                    from_entity_id: from_entity_id.clone(),
                })
            }
            ServerMessage::PartyInviteDeclined { by_entity_id } => {
                Kind::PartyInviteDeclined(proto::PartyInviteDeclined {
                    by_entity_id: by_entity_id.clone(),
                })
            }
            ServerMessage::PartyUpdate { members } => Kind::PartyUpdate(proto::PartyUpdate {
                members: members.clone(),
            }),
            ServerMessage::GuildInviteReceived { from_entity_id } => {
                Kind::GuildInviteReceived(proto::GuildInviteReceived {
                    from_entity_id: from_entity_id.clone(),
                })
            }
            ServerMessage::GuildInviteDeclined { by_entity_id } => {
                Kind::GuildInviteDeclined(proto::GuildInviteDeclined {
                    by_entity_id: by_entity_id.clone(),
                })
            }
            ServerMessage::GuildUpdate {
                guild_id,
                name,
                motd,
                tag,
                members,
            } => Kind::GuildUpdate(proto::GuildUpdate {
                guild_id: guild_id.clone(),
                name: name.clone(),
                motd: motd.clone(),
                tag: tag.clone(),
                members: members.iter().map(proto::GuildMember::from).collect(),
            }),
            ServerMessage::GuildDisbanded {} => Kind::GuildDisbanded(proto::GuildDisbanded {}),
            ServerMessage::StatChanged { stat_key, value } => {
                Kind::StatChanged(proto::StatChanged {
                    stat_key: stat_key.clone(),
                    value: *value,
                })
            }
            ServerMessage::ItemChanged {
                item_type,
                quantity,
            } => Kind::ItemChanged(proto::ItemChanged {
                item_type: item_type.clone(),
                quantity: *quantity,
            }),
            ServerMessage::CurrencyChanged {
                currency_key,
                balance,
            } => Kind::CurrencyChanged(proto::CurrencyChanged {
                currency_key: currency_key.clone(),
                balance: *balance,
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
            Some(Kind::PartyInviteReceived(proto::PartyInviteReceived { from_entity_id })) => {
                Ok(ServerMessage::PartyInviteReceived { from_entity_id })
            }
            Some(Kind::PartyInviteDeclined(proto::PartyInviteDeclined { by_entity_id })) => {
                Ok(ServerMessage::PartyInviteDeclined { by_entity_id })
            }
            Some(Kind::PartyUpdate(proto::PartyUpdate { members })) => {
                Ok(ServerMessage::PartyUpdate { members })
            }
            Some(Kind::GuildInviteReceived(proto::GuildInviteReceived { from_entity_id })) => {
                Ok(ServerMessage::GuildInviteReceived { from_entity_id })
            }
            Some(Kind::GuildInviteDeclined(proto::GuildInviteDeclined { by_entity_id })) => {
                Ok(ServerMessage::GuildInviteDeclined { by_entity_id })
            }
            Some(Kind::GuildUpdate(proto::GuildUpdate {
                guild_id,
                name,
                motd,
                tag,
                members,
            })) => Ok(ServerMessage::GuildUpdate {
                guild_id,
                name,
                motd,
                tag,
                members: members.into_iter().map(GuildMemberEntry::from).collect(),
            }),
            Some(Kind::GuildDisbanded(proto::GuildDisbanded {})) => {
                Ok(ServerMessage::GuildDisbanded {})
            }
            Some(Kind::StatChanged(proto::StatChanged { stat_key, value })) => {
                Ok(ServerMessage::StatChanged { stat_key, value })
            }
            Some(Kind::ItemChanged(proto::ItemChanged {
                item_type,
                quantity,
            })) => Ok(ServerMessage::ItemChanged {
                item_type,
                quantity,
            }),
            Some(Kind::CurrencyChanged(proto::CurrencyChanged {
                currency_key,
                balance,
            })) => Ok(ServerMessage::CurrencyChanged {
                currency_key,
                balance,
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
    fn join_group_layer_round_trips_through_an_envelope() {
        let message = ClientMessage::JoinGroupLayer {
            other_entity_id: "some-entity-id".to_string(),
        };
        let envelope = message.into_envelope().unwrap();
        let decoded = ClientMessage::from_envelope(&envelope).unwrap();
        assert!(matches!(
            decoded,
            ClientMessage::JoinGroupLayer { other_entity_id } if other_entity_id == "some-entity-id"
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
