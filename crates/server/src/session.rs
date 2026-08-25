//! Per-connection handling for the phase-1 combined `server`: the auth
//! handshake first (docs/specs/Auth_Spec.md, "Gateway handshake"), then
//! load-or-create a character and drive its movement in the zone
//! (docs/PROPOSAL.md, "Phased Roadmap," Phase 1).

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use character::CharacterStore;
use chat::gateway_protocol::CHAT_MESSAGE_TYPE;
use common::id::{AccountId, CharacterId, EntityId, GuildId, RealmId};
use common::{Error, Result};
use futures_util::{SinkExt, StreamExt};
use gateway::Envelope;
use tokio::sync::mpsc;
use tokio_util::codec::Framed;
use world::EntityKind;

use crate::character_protocol;
use crate::chat_session::{self, ChatDeps};
use crate::realm_protocol;
use crate::session_protocol::{
    ClientMessage, GuildMemberEntry, RosterEntry, ServerMessage, WORLD_MESSAGE_TYPE,
};
use crate::zone_registry::{ZoneRegistry, ZoneRuntime};
use crate::{despawn_from_layer, send_to, spawn_into_layer};

pub type ServerStream =
    Framed<tokio_rustls::server::TlsStream<tokio::net::TcpStream>, gateway::EnvelopeCodec>;
type ServerSink = futures_util::stream::SplitSink<ServerStream, Envelope>;

/// Every connected entity's outgoing channel — how the world actor's
/// tick-outcome broadcast and a newly-joined session's roster reach
/// every other connected client. Locked only for quick, synchronous
/// insert/remove/iterate — never held across an `.await`.
pub type Sessions = Arc<Mutex<HashMap<EntityId, mpsc::UnboundedSender<Envelope>>>>;

/// Which `character` row a connected player entity belongs to — the
/// resolution `plugin_host`'s `apply-stat-delta` needs (a plugin only
/// knows the opaque entity id; the actual stat write is per-character),
/// populated alongside `Sessions` at spawn and removed at disconnect.
/// Never has an NPC entry — an NPC entity has no backing character row;
/// see [`NpcStats`] for the parallel storage `apply-stat-delta` resolves
/// against when the target entity isn't a player (#197).
pub type EntityCharacters = Arc<Mutex<HashMap<EntityId, CharacterId>>>;

/// Declared-schema-validated stats for NPC entities (#197) — the
/// non-player counterpart to `character`'s `stats` column, keyed by
/// entity id rather than character id since an NPC has no character row
/// at all. Deliberately in-memory only, process-wide (mirrors
/// `EntityCharacters`'s scope), not persisted to Postgres: an NPC's
/// entity id is generated fresh at spawn time (`spawn_npc_from_table`),
/// never stable across a zone-service restart the way a character id is,
/// so there is nothing meaningful to durably key stored stats against —
/// a restarted server respawns its NPCs from the same manifest-declared
/// spawn tables at their schema-declared defaults either way. Read
/// through the same `character::AttributeSchema` bounds/defaults real
/// character stats get, not an unvalidated ad-hoc blob — see
/// `crate::world_actor::apply_npc_stat_delta`. An entry is removed when
/// its entity despawns (`WorldCommand::Despawn`) so this map never grows
/// past the zone's actual live NPC population.
pub type NpcStats = Arc<Mutex<HashMap<EntityId, HashMap<String, i64>>>>;

/// `EntityCharacters`, reversed (#142) — which currently-connected entity
/// id a given character is playing as right now, if any. `entity_id`
/// alone can't answer "is this character's party member still online" at
/// a later *login* (a fresh connection always gets a brand-new entity
/// id, so any entity id recorded before a disconnect is already stale by
/// the time a reconnect needs to consult it) — `CharacterId` is the
/// stable key that survives the gap, and `character::PartyStore`'s
/// membership is keyed by `CharacterId` for the same reason (#178).
/// Populated/cleared at the exact same two points as `EntityCharacters`.
pub type CharacterEntities = Arc<Mutex<HashMap<CharacterId, EntityId>>>;

/// A party invite awaiting a response, keyed by the *invitee's* entity
/// id (value: `(inviter's entity id, requested party_type)`) — #178's
/// invite/accept/decline flow. Deliberately entity-id-scoped and
/// process-wide, not durable: an invite only ever makes sense against a
/// live connection (there's no "accept an invite from someone who's
/// since logged off" case worth supporting), so there's nothing to
/// persist and nothing to reconcile across a reconnect the way
/// `character::PartyStore`'s actual membership needs to. A second invite
/// to the same invitee simply overwrites the first — last invite wins,
/// no queueing — the simplest v0 shape; a `PartyInviteResponse` always
/// answers whichever invite is currently pending for the responder.
pub type PendingPartyInvites = Arc<Mutex<HashMap<EntityId, (EntityId, String)>>>;

/// Which account a connected player entity belongs to — the resolution
/// guild handling needs (#179): `guild::GuildStore` is keyed by
/// `AccountId`, not `CharacterId` like `character::PartyStore`, so
/// resolving a message's `target_entity_id` into the account it acts
/// against needs this rather than `EntityCharacters`. Populated/cleared
/// at the same two points as `EntityCharacters`/`EntityRoles`. Never has
/// an NPC entry.
pub type EntityAccounts = Arc<Mutex<HashMap<EntityId, AccountId>>>;

/// `EntityAccounts`, reversed (#179) — which currently-connected entity
/// id a given account is playing as right now, if any, so a guild
/// roster refresh can find which (possibly offline) members are
/// actually reachable to push a `GuildUpdate` to. Populated/cleared
/// alongside `EntityAccounts`.
pub type AccountEntities = Arc<Mutex<HashMap<AccountId, EntityId>>>;

/// A guild invite awaiting a response, keyed by the *invitee's* entity
/// id (value: the inviter's entity id) — #179's invite/accept/decline
/// flow, same "entity-id-scoped, process-wide, not durable" shape as
/// `PendingPartyInvites` (see its own doc comment for why).
pub type PendingGuildInvites = Arc<Mutex<HashMap<EntityId, EntityId>>>;

/// Which roles (docs/specs/Auth_Spec.md, "Account roles", #114/#124) the
/// account behind a connected player entity holds — populated once at
/// join time (below) and consulted synchronously by `plugin_startup`'s
/// `caller-role` host function, never queried live from `auth`'s role
/// store from inside a sandboxed plugin call (see `wit/plugin.wit`'s
/// `caller-role` doc comment for why: `plugin_host::HostCallbacks` is
/// called synchronously from inside `wasmtime`, while the role store is
/// async-only). Global scope for v0, so a plugin sees the same roles for
/// the life of the connection — a role granted/revoked mid-session isn't
/// reflected until reconnect, an accepted staleness window for v0. Never
/// has an NPC entry, same as `EntityCharacters`.
pub type EntityRoles = Arc<Mutex<HashMap<EntityId, Vec<String>>>>;

pub struct SessionDeps {
    pub auth_provider: Arc<auth::UsernamePasswordProvider>,
    pub character_store: Arc<CharacterStore>,
    /// The one realm this process serves (#136) — resolved at startup
    /// from `WZ_REALM_ID` against `realm-directory::RealmStore`, never a
    /// hardcoded placeholder. A process serving more than one realm at
    /// once is #130's job; every login this process handles targets this
    /// realm.
    pub realm_id: RealmId,
    /// `realm_id`'s display name, resolved once at startup alongside it
    /// — #192's `RealmList` reports this, same staleness acceptance as
    /// `realm_open_or_bound` below.
    pub realm_name: String,
    /// `realm_id`'s own open/bound policy, resolved once at startup
    /// alongside it — cheap to cache since it can't change without an
    /// operator editing it via `realm-directory`'s CLI mid-process, an
    /// accepted v0 staleness window (same shape `entity_roles` already
    /// uses). Used to decide whether a connection needs #21's lease
    /// renewal loop below at all (a bound realm never takes a lease).
    pub realm_open_or_bound: realm_directory::OpenOrBound,
    /// The single login-time enforcement point (#51) — resolves which
    /// character an account logs in with per `realm_id`'s policy, then
    /// authorizes the login (bound-realm mismatch rejection, or
    /// open-realm lease acquisition) before the connection is allowed to
    /// join the world.
    pub login_policy: Arc<realm_directory::LoginPolicy>,
    /// #21's session lease, released on clean disconnect (below) — a
    /// harmless no-op for a bound-realm character, since `login_policy`
    /// never acquires a lease for one in the first place.
    pub character_lease: Arc<character::CharacterSessionLease>,
    pub lease_ttl: std::time::Duration,
    /// #137's live-connection counter, backing #192's `RealmList`
    /// (`live_connection_count`) — registered at join, refreshed
    /// alongside the lease renewal loop below, removed at disconnect.
    /// Unlike `character_lease`, this applies to every connection
    /// regardless of `realm_open_or_bound` (docs/specs/Data_Model_Spec.md:
    /// "a live-connection count needs to work for bound realms too").
    pub realm_presence: Arc<realm_directory::RealmPresence>,
    /// #193's character-creation cap, per account per realm
    /// (`WZ_CHARACTER_MAX_PER_ACCOUNT`) — enforced here, not inside
    /// `character::CharacterStore::create` itself (see `main`'s doc
    /// comment on why this stays a `server`-side policy value).
    pub max_characters_per_account: u32,
    /// Every loaded plugin (#152: one instance, process-wide), shared
    /// with every zone actor — the character-creation loop below also
    /// dispatches into this directly (#194's `on-character-create`),
    /// since that hook fires before any zone/entity context exists to
    /// route it through `world_actor::fire_hook` the way every other
    /// hook does.
    pub plugins: Arc<tokio::sync::Mutex<Vec<crate::plugin_startup::PluginRuntime>>>,
    /// Every zone-service instance this process runs (#45) — a
    /// connection looks up its current zone's `WorldHandle`/`Sessions`
    /// here at join time, and again on every `ZoneChanged` handoff.
    pub zones: Arc<ZoneRegistry>,
    /// Which zone a brand-new character starts in, and the fallback for
    /// an existing character whose persisted `zone_id` no longer names a
    /// zone this content pack declares (a pack that's since dropped a
    /// zone) — never silently drops the connection over a stale zone_id.
    pub default_zone_id: String,
    pub entity_characters: EntityCharacters,
    /// `EntityCharacters` reversed, for reconnect-to-party lookups
    /// (#142/#178) — see its own doc comment.
    pub character_entities: CharacterEntities,
    /// The real party/group system (#178) — invite/accept/leave, and the
    /// membership `#142`'s placement primitive (`ZoneRegistry::join_layer_of`)
    /// actually consults, both for the live `JoinGroupLayer` trigger and
    /// reconnect placement.
    pub party_store: Arc<character::PartyStore>,
    /// See [`PendingPartyInvites`]'s own doc comment (#178).
    pub pending_party_invites: PendingPartyInvites,
    /// The real guild system (#179) — roster, dev-declared rank
    /// hierarchy, permissions. Unlike `party_store`, `GuildStore` knows
    /// nothing about `chat`; syncing a guild's optional chat channel
    /// membership happens here in `session`, guarded by `chat` being
    /// `Some` (see the `GuildCreate`/`GuildInviteResponse`/etc. handlers
    /// below).
    pub guild_store: Arc<guild::GuildStore>,
    /// See [`PendingGuildInvites`]'s own doc comment (#179).
    pub pending_guild_invites: PendingGuildInvites,
    /// See [`EntityAccounts`]'s own doc comment (#179).
    pub entity_accounts: EntityAccounts,
    /// See [`AccountEntities`]'s own doc comment (#179).
    pub account_entities: AccountEntities,
    /// Backs `EntityRoles` population at join time (#124) — `auth` (like
    /// `character`) is always wired in this combined process, so this is
    /// never optional the way `chat`/`metrics` are.
    pub role_store: Arc<dyn auth::AccountRoleStore>,
    pub entity_roles: EntityRoles,
    /// `message_type`s the configured plugin declared in `plugin.toml`
    /// (empty if no plugin is configured) — checked here rather than
    /// only in the world actor so an envelope with an unroutable
    /// `message_type` still gets a clear per-connection error reply
    /// instead of silently vanishing into the actor's command queue
    /// (#95).
    pub plugin_message_types: Vec<u16>,
    /// Chat command names (without the leading `/`) the configured
    /// plugin declared (empty if none) — checked here, before a `Send`
    /// ever reaches `chat_session`, so a matched command is routed to
    /// the plugin instead of published as an ordinary chat message (#57).
    pub plugin_chat_commands: Vec<String>,
    /// `Some` when `ServicesConfig::chat_enabled` — `None` end to end
    /// means chat is disabled and never touched, not just no-op'd (#104).
    pub chat: Option<ChatDeps>,
    /// `Some` when `ServicesConfig::metrics_enabled` — `None` end to end
    /// means metrics are disabled and `worldzero_connection_count` is
    /// never touched, not just excluded from what gets scraped (#48).
    pub metrics: Option<Arc<common::metrics::Metrics>>,
    /// Backs character-scope `plugin-state-get`/`plugin-state-set`
    /// (#149) — hydrated into `plugin_state_cache` at join time, same
    /// "populate a cache at join, never a live DB read from inside a
    /// sandboxed call" shape `entity_roles` already uses.
    pub plugin_state_store: Arc<crate::plugin_state::PluginStateStore>,
    pub plugin_state_cache: crate::plugin_state::PluginStateCache,
    /// Every connected entity's outgoing channel, process-wide, regardless
    /// of which zone it's currently in (#152) — backs the plugin
    /// `send-message` host function, since a plugin instance is now
    /// shared across every zone and needs to reach a target entity no
    /// matter where they are. Distinct from each zone's own `Sessions`
    /// (`zone.sessions`, used for that zone's broadcast/roster) — an
    /// entry here is added once at initial join and removed once at
    /// final disconnect; it's untouched by `ZoneChanged` zone-hops, since
    /// the same connection/`outgoing_tx` carries straight through those.
    pub global_sessions: Sessions,
}

pub async fn handle_session(framed: ServerStream, deps: Arc<SessionDeps>) -> Result<()> {
    let (mut sink, mut stream) = framed.split();
    let (outgoing_tx, mut outgoing_rx) = mpsc::unbounded_channel::<Envelope>();

    let Some(frame) = stream.next().await else {
        return Ok(());
    };
    let envelope = frame.map_err(|e| Error::wrap("server", "connection error", e))?;
    let auth_message = match auth::gateway_protocol::ClientMessage::from_envelope(&envelope) {
        Ok(m) => m,
        Err(e) => {
            send_auth_error(&mut sink, e.to_string()).await?;
            return Ok(());
        }
    };

    let (account_id, username, session_token) =
        match authenticate(auth_message, &deps.auth_provider).await {
            Ok(result) => result,
            Err(e) => {
                send_auth_error(&mut sink, e.to_string()).await?;
                return Ok(());
            }
        };

    send_auth(
        &mut sink,
        &auth::gateway_protocol::ServerMessage::Authenticated {
            account_id,
            username: username.clone(),
            session_token,
        },
    )
    .await?;

    if let Some(chat) = &deps.chat {
        chat.usernames
            .write()
            .unwrap()
            .insert(account_id, username.clone());
    }
    let mut joined_channels = chat_session::JoinedChannels::new();

    // Realm discovery/selection (#192) — a client can request the realm
    // list any number of times, but must settle on `SelectRealm{ realm_id }`
    // naming the one realm this process serves (#136) before this
    // connection is allowed any further. "Skippable" for a single-realm
    // deployment means no realm-picker UI is required client-side — the
    // client can send `SelectRealm` immediately with a realm id it
    // already knows, without ever calling `ListRealms` first — not that
    // this step itself can be omitted from the wire.
    loop {
        let Some(frame) = stream.next().await else {
            return Ok(());
        };
        let envelope = frame.map_err(|e| Error::wrap("server", "connection error", e))?;
        let realm_message = match realm_protocol::ClientMessage::from_envelope(&envelope) {
            Ok(m) => m,
            Err(e) => {
                send_realm_error(&mut sink, e.to_string()).await?;
                return Ok(());
            }
        };
        match realm_message {
            realm_protocol::ClientMessage::ListRealms => {
                let realm = build_realm_summary(&deps).await?;
                send_realm(
                    &mut sink,
                    &realm_protocol::ServerMessage::RealmList {
                        realms: vec![realm],
                    },
                )
                .await?;
            }
            realm_protocol::ClientMessage::SelectRealm { realm_id } => {
                match realm_protocol::validate_selection(&realm_id, deps.realm_id) {
                    Ok(selected) => {
                        send_realm(
                            &mut sink,
                            &realm_protocol::ServerMessage::RealmSelected {
                                realm_id: selected.to_string(),
                            },
                        )
                        .await?;
                        break;
                    }
                    Err(e) => {
                        send_realm_error(&mut sink, e.to_string()).await?;
                        return Ok(());
                    }
                }
            }
        }
    }

    // #21's lease is keyed by `zone_service_id`, which in the real
    // multi-process model (#130) names a whole zone-service instance —
    // but within *this* combined process, the thing that needs to be
    // distinguished from any other still-connected claim on the same
    // character is the connection itself, not the process (every
    // connection this process handles shares one process identity, so a
    // process-wide id here would make two concurrent connections for the
    // same character look like the same lease holder renewing, not a
    // conflict — `acquire`'s `zone_service_id = EXCLUDED.zone_service_id`
    // clause is exactly the case that's supposed to allow a renewal, not
    // a second login). `entity_id`, generated fresh per connection right
    // here (used for exactly that purpose everywhere else in this
    // function already), is stable for this connection's whole lifetime
    // — including across a later `ZoneChanged` hop — and unique across
    // any other concurrent one.
    let entity_id = EntityId::new();
    let lease_holder_id = entity_id.to_string();

    // Character list/create/select (#193) — unlike realm selection above,
    // a rejected `SelectCharacter` (e.g. `authorize_login` finding this
    // particular character already leased elsewhere) doesn't close the
    // connection: the account may own other characters that aren't
    // contended, so the loop keeps going rather than forcing a full
    // reconnect just to try a different one.
    let character = loop {
        let Some(frame) = stream.next().await else {
            return Ok(());
        };
        let envelope = frame.map_err(|e| Error::wrap("server", "connection error", e))?;
        let character_message = match character_protocol::ClientMessage::from_envelope(&envelope) {
            Ok(m) => m,
            Err(e) => {
                send_character_error(&mut sink, e.to_string()).await?;
                return Ok(());
            }
        };
        match character_message {
            character_protocol::ClientMessage::ListCharacters => {
                let characters = deps
                    .login_policy
                    .list_characters(&deps.character_store, account_id, deps.realm_id)
                    .await?
                    .into_iter()
                    .map(|c| character_protocol::CharacterSummary {
                        character_id: c.id.to_string(),
                        name: c.name,
                        zone_id: c.zone_id,
                    })
                    .collect();
                send_character(
                    &mut sink,
                    &character_protocol::ServerMessage::CharacterList { characters },
                )
                .await?;
            }
            character_protocol::ClientMessage::CreateCharacter { name } => {
                if name.trim().is_empty() {
                    send_character_error(&mut sink, "character name must not be empty".to_string())
                        .await?;
                    continue;
                }
                let existing = deps
                    .character_store
                    .count_for_account(account_id, deps.realm_id)
                    .await?;
                if existing >= i64::from(deps.max_characters_per_account) {
                    send_character_error(
                        &mut sink,
                        format!(
                            "character limit reached: {existing} already exist on this realm, \
                             limit is {} (WZ_CHARACTER_MAX_PER_ACCOUNT)",
                            deps.max_characters_per_account
                        ),
                    )
                    .await?;
                    continue;
                }
                match deps
                    .character_store
                    .create(account_id, &name, deps.realm_id, &deps.default_zone_id)
                    .await
                {
                    Ok(id) => {
                        // #194's extension point — fires (and its
                        // starting-stat writes are applied) before the
                        // client's own acknowledgement, so a client that
                        // immediately selects and joins never observes
                        // the character pre-hook.
                        fire_on_character_create(
                            &deps.plugins,
                            &deps.character_store,
                            &deps.character_entities,
                            &deps.global_sessions,
                            id,
                            &deps.default_zone_id,
                        )
                        .await;
                        send_character(
                            &mut sink,
                            &character_protocol::ServerMessage::CharacterCreated {
                                character_id: id.to_string(),
                            },
                        )
                        .await?;
                    }
                    Err(e) => send_character_error(&mut sink, e.to_string()).await?,
                }
            }
            character_protocol::ClientMessage::SelectCharacter { character_id } => {
                let Ok(parsed_id) = character_id.parse::<CharacterId>() else {
                    send_character_error(
                        &mut sink,
                        format!("{character_id:?} is not a valid character id"),
                    )
                    .await?;
                    continue;
                };
                let Some(character) = deps
                    .character_store
                    .get_for_account(parsed_id, account_id)
                    .await?
                else {
                    send_character_error(
                        &mut sink,
                        format!("{character_id:?} is not one of your characters"),
                    )
                    .await?;
                    continue;
                };
                if let Err(e) = deps
                    .login_policy
                    .authorize_login(
                        character.id,
                        character.realm_id,
                        deps.realm_id,
                        &lease_holder_id,
                    )
                    .await
                {
                    send_character_error(&mut sink, e.to_string()).await?;
                    continue;
                }
                send_character(
                    &mut sink,
                    &character_protocol::ServerMessage::CharacterSelected {
                        character_id: character.id.to_string(),
                    },
                )
                .await?;
                break character;
            }
        }
    };
    let character_id = character.id;
    let position = (character.position.0, character.position.1);

    // #137's live-connection registration — applies regardless of
    // open/bound (unlike `character_lease` above), see `realm_presence`'s
    // own doc comment on `SessionDeps`.
    deps.realm_presence
        .connect(deps.realm_id, entity_id.as_uuid())
        .await?;

    // A character's persisted `zone_id` might name a zone this content
    // pack no longer declares (the pack changed since they last logged
    // in) — fall back to the default rather than failing the connection
    // over it.
    let mut current_zone_id = if deps.zones.contains(&character.zone_id) {
        character.zone_id.clone()
    } else {
        tracing::warn!(
            character_zone_id = character.zone_id,
            default_zone_id = deps.default_zone_id,
            "character's persisted zone no longer exists in this content pack, using the default"
        );
        deps.default_zone_id.clone()
    };
    // Reconnecting to a still-live party (#142/#178) is the one case
    // initial connection resolution needs to consult group state at all
    // — looked up *before* population-balanced assignment, which only
    // runs if this comes back empty (not in a party, or no party member
    // is actually online in this zone right now). Tries every current
    // party member, not just one — an N-member party might have several
    // online, only some of them in this zone. Selecting a *different*
    // character at login than the one that was partied naturally has no
    // party membership under this new character_id and falls straight
    // through to an ordinary login.
    let party_members = deps
        .party_store
        .members_of(character_id)
        .await
        .unwrap_or_default();
    let groupmate_layer = party_members.into_iter().find_map(|member_character_id| {
        let member_entity_id = deps
            .character_entities
            .lock()
            .unwrap()
            .get(&member_character_id)
            .copied()?;
        deps.zones.join_layer_of(&current_zone_id, member_entity_id)
    });

    // Population-balanced layer assignment (#50) happens once, here, at
    // initial join — see `zone_registry`'s doc comment for why a later
    // zone-link transition or mid-connection `ZoneChanged` (below) always
    // lands on layer 0 instead rather than going through this too.
    let mut zone = match groupmate_layer {
        Some(zone) => zone,
        None => deps
            .zones
            .assign_layer(&current_zone_id)
            .expect("default_zone_id must always resolve to a real zone in the registry"),
    };

    zone.world.spawn(entity_id, EntityKind::Player, position);
    deps.entity_characters
        .lock()
        .unwrap()
        .insert(entity_id, character_id);
    deps.character_entities
        .lock()
        .unwrap()
        .insert(character_id, entity_id);
    deps.entity_accounts
        .lock()
        .unwrap()
        .insert(entity_id, account_id);
    deps.account_entities
        .lock()
        .unwrap()
        .insert(account_id, entity_id);
    let roles = deps.role_store.roles_for(account_id).await?;
    deps.entity_roles.lock().unwrap().insert(entity_id, roles);

    // Character-scope plugin state (#149), hydrated once here — before
    // this entity can possibly receive a `plugin-state-get` call — same
    // shape as `entity_roles` just above.
    let plugin_state = deps
        .plugin_state_store
        .character_state(character_id)
        .await?;
    if !plugin_state.is_empty() {
        let mut cache = deps.plugin_state_cache.lock().unwrap();
        for (key, value) in plugin_state {
            cache.insert(
                crate::plugin_state::cache_key(
                    &plugin_host::PluginStateScope::Character(entity_id.to_string()),
                    &key,
                ),
                value,
            );
        }
    }

    if let Some(metrics) = &deps.metrics {
        metrics.connection_count.inc();
    }

    // Everything already in the zone, delivered as one `Joined` message
    // rather than `Spawned` plus a separate `EntitySpawned` per entity —
    // a pre-spawned NPC (or another already-connected player) otherwise
    // has no way to become visible to this connection, and a single
    // message keeps the join a single write on a freshly-established
    // connection instead of several in a row.
    let roster: Vec<RosterEntry> = zone
        .world
        .entities_snapshot()
        .await
        .into_iter()
        .filter(|(other_id, ..)| *other_id != entity_id)
        .map(|(other_id, kind, other_position)| RosterEntry {
            entity_id: other_id.to_string(),
            entity_type: entity_type_label(kind),
            x: other_position.0,
            y: other_position.1,
        })
        .collect();
    let join_tick = zone.world.current_tick().await;

    zone.sessions
        .lock()
        .unwrap()
        .insert(entity_id, outgoing_tx.clone());
    deps.global_sessions
        .lock()
        .unwrap()
        .insert(entity_id, outgoing_tx.clone());

    queue(
        &outgoing_tx,
        &ServerMessage::Joined {
            entity_id: entity_id.to_string(),
            x: position.0,
            y: position.1,
            roster,
            tick: join_tick,
        },
    );

    broadcast_except(
        &zone.sessions,
        entity_id,
        ServerMessage::EntitySpawned {
            entity_id: entity_id.to_string(),
            entity_type: entity_type_label(EntityKind::Player),
            x: position.0,
            y: position.1,
        },
    );

    // After roster delivery, so a plugin's own `send-message` call made
    // from inside `on-player-join-zone` reaches a client that's actually
    // ready to receive it (#155).
    zone.world.dispatch_player_join(entity_id);

    // #21's lease renewal (open realms only — a bound one never held a
    // lease in the first place, and calling `renew` for one would just
    // log a spurious "doesn't hold a lease" warning every tick for no
    // reason) and #137's live-connection heartbeat (every realm, open or
    // bound — see `realm_presence`'s doc comment on `SessionDeps`), both
    // on the same interval since they're the same "how stale can this
    // connection's liveness signal get" question. A third of the TTL so
    // a couple of missed ticks in a row don't let either lapse out from
    // under a still-connected character.
    let renew_open_lease = deps.realm_open_or_bound == realm_directory::OpenOrBound::Open;
    let mut heartbeat_interval =
        tokio::time::interval((deps.lease_ttl / 3).max(std::time::Duration::from_secs(1)));
    heartbeat_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    heartbeat_interval.tick().await; // first tick fires immediately; both were just acquired

    loop {
        tokio::select! {
            _ = heartbeat_interval.tick() => {
                if renew_open_lease && let Err(e) = deps.character_lease
                    .renew(character_id, &lease_holder_id, deps.lease_ttl)
                    .await
                {
                    tracing::warn!(error = %e, %character_id, "failed to renew character session lease");
                }
                if let Err(e) = deps.realm_presence.connect(deps.realm_id, entity_id.as_uuid()).await {
                    tracing::warn!(error = %e, %character_id, "failed to renew realm presence heartbeat");
                }
            }
            maybe_frame = stream.next() => {
                let Some(frame) = maybe_frame else { break };
                let Ok(envelope) = frame else { break };
                if envelope.message_type == WORLD_MESSAGE_TYPE {
                    match ClientMessage::from_envelope(&envelope) {
                        Ok(ClientMessage::Move { x, y, seq }) => {
                            zone.world.request_move(entity_id, (x, y), seq);
                        }
                        Ok(ClientMessage::Ping { client_sent_at }) => {
                            send_world(&mut sink, &ServerMessage::Pong {
                                client_sent_at,
                                server_time: unix_millis_now(),
                            }).await?;
                        }
                        Ok(ClientMessage::Attack { target_entity_id, stat_key }) => {
                            match target_entity_id.parse::<EntityId>() {
                                Ok(target) => zone.world.dispatch_attack(entity_id, target, stat_key),
                                Err(_) => {
                                    send_world(&mut sink, &ServerMessage::Error {
                                        message: format!("{target_entity_id:?} is not a valid entity id"),
                                    }).await?;
                                }
                            }
                        }
                        Ok(ClientMessage::UseItem { item_type }) => {
                            zone.world.dispatch_use_item(entity_id, item_type);
                        }
                        Ok(ClientMessage::InteractNpc { npc_entity_id }) => {
                            match npc_entity_id.parse::<EntityId>() {
                                Ok(npc) => zone.world.dispatch_interact_npc(npc, entity_id),
                                Err(_) => {
                                    send_world(&mut sink, &ServerMessage::Error {
                                        message: format!("{npc_entity_id:?} is not a valid entity id"),
                                    }).await?;
                                }
                            }
                        }
                        Ok(ClientMessage::JoinGroupLayer { other_entity_id }) => {
                            let Ok(other_entity_id) = other_entity_id.parse::<EntityId>() else {
                                send_world(&mut sink, &ServerMessage::Error {
                                    message: format!("{other_entity_id:?} is not a valid entity id"),
                                }).await?;
                                continue;
                            };
                            // #142's placement primitive is real
                            // group-aware as of #178: `other_entity_id`
                            // must actually be a fellow party member, not
                            // just any currently-spawned entity — the
                            // membership check #142 deliberately deferred
                            // ("that's the group system's job") now that
                            // the group system exists.
                            let other_character_id = deps
                                .entity_characters
                                .lock()
                                .unwrap()
                                .get(&other_entity_id)
                                .copied();
                            let is_party_member = match other_character_id {
                                Some(id) => deps
                                    .party_store
                                    .members_of(character_id)
                                    .await
                                    .map(|members| members.contains(&id))
                                    .unwrap_or(false),
                                None => false,
                            };
                            if !is_party_member {
                                send_world(&mut sink, &ServerMessage::Error {
                                    message: "you are not in a party with that player".to_string(),
                                }).await?;
                                continue;
                            }
                            perform_group_layer_move(
                                &mut zone,
                                &current_zone_id,
                                entity_id,
                                other_entity_id,
                                &deps.zones,
                                &mut sink,
                                &outgoing_tx,
                            ).await?;
                        }
                        Ok(ClientMessage::PartyInvite { target_entity_id, party_type }) => {
                            let Ok(target_entity_id) = target_entity_id.parse::<EntityId>() else {
                                send_world(&mut sink, &ServerMessage::Error {
                                    message: format!("{target_entity_id:?} is not a valid entity id"),
                                }).await?;
                                continue;
                            };
                            if target_entity_id == entity_id {
                                send_world(&mut sink, &ServerMessage::Error {
                                    message: "you can't invite yourself".to_string(),
                                }).await?;
                                continue;
                            }
                            let target_is_player = deps
                                .entity_characters
                                .lock()
                                .unwrap()
                                .contains_key(&target_entity_id);
                            if !target_is_player {
                                send_world(&mut sink, &ServerMessage::Error {
                                    message: "that entity isn't a player you can invite".to_string(),
                                }).await?;
                                continue;
                            }
                            deps.pending_party_invites
                                .lock()
                                .unwrap()
                                .insert(target_entity_id, (entity_id, party_type));
                            send_to(&deps.global_sessions, target_entity_id, ServerMessage::PartyInviteReceived {
                                from_entity_id: entity_id.to_string(),
                            });
                        }
                        Ok(ClientMessage::PartyInviteResponse { accept }) => {
                            let Some((inviter_entity_id, requested_party_type)) = deps
                                .pending_party_invites
                                .lock()
                                .unwrap()
                                .remove(&entity_id)
                            else {
                                send_world(&mut sink, &ServerMessage::Error {
                                    message: "you have no pending party invite".to_string(),
                                }).await?;
                                continue;
                            };
                            if !accept {
                                send_to(&deps.global_sessions, inviter_entity_id, ServerMessage::PartyInviteDeclined {
                                    by_entity_id: entity_id.to_string(),
                                });
                                continue;
                            }
                            let inviter_character_id = deps
                                .entity_characters
                                .lock()
                                .unwrap()
                                .get(&inviter_entity_id)
                                .copied();
                            let Some(inviter_character_id) = inviter_character_id else {
                                send_world(&mut sink, &ServerMessage::Error {
                                    message: "the player who invited you is no longer connected".to_string(),
                                }).await?;
                                continue;
                            };
                            match deps.party_store.accept_invite(inviter_character_id, character_id, &requested_party_type).await {
                                Ok(_) => {
                                    let mut interested = deps
                                        .party_store
                                        .members_of(character_id)
                                        .await
                                        .unwrap_or_default();
                                    interested.push(character_id);
                                    refresh_party_rosters(&deps, &interested).await;
                                    // Lands the accepter alongside the
                                    // inviter live if they're already in
                                    // the same zone (#142) — a no-op
                                    // otherwise (different zone, or the
                                    // inviter's own connection just
                                    // dropped), same as `JoinGroupLayer`.
                                    perform_group_layer_move(
                                        &mut zone,
                                        &current_zone_id,
                                        entity_id,
                                        inviter_entity_id,
                                        &deps.zones,
                                        &mut sink,
                                        &outgoing_tx,
                                    ).await?;
                                }
                                Err(e) => {
                                    send_world(&mut sink, &ServerMessage::Error { message: e.to_string() }).await?;
                                }
                            }
                        }
                        Ok(ClientMessage::PartyLeave {}) => {
                            let mut interested = deps
                                .party_store
                                .members_of(character_id)
                                .await
                                .unwrap_or_default();
                            interested.push(character_id);
                            match deps.party_store.leave(character_id).await {
                                Ok(()) => refresh_party_rosters(&deps, &interested).await,
                                Err(e) => {
                                    send_world(&mut sink, &ServerMessage::Error { message: e.to_string() }).await?;
                                }
                            }
                        }
                        Ok(ClientMessage::GuildCreate { name }) => {
                            let chat_channel_id = match &deps.chat {
                                Some(chat) => chat.store.create_guild(account_id, &name).await.ok(),
                                None => None,
                            };
                            match deps.guild_store.create(account_id, &name, deps.realm_id, chat_channel_id).await {
                                Ok(guild_id) => refresh_guild_rosters(&deps, Some(guild_id), &[account_id]).await,
                                Err(e) => {
                                    send_world(&mut sink, &ServerMessage::Error { message: e.to_string() }).await?;
                                }
                            }
                        }
                        Ok(ClientMessage::GuildInvite { target_entity_id }) => {
                            let Ok(target_entity_id) = target_entity_id.parse::<EntityId>() else {
                                send_world(&mut sink, &ServerMessage::Error {
                                    message: format!("{target_entity_id:?} is not a valid entity id"),
                                }).await?;
                                continue;
                            };
                            if target_entity_id == entity_id {
                                send_world(&mut sink, &ServerMessage::Error {
                                    message: "you can't invite yourself".to_string(),
                                }).await?;
                                continue;
                            }
                            let target_is_player = deps
                                .entity_accounts
                                .lock()
                                .unwrap()
                                .contains_key(&target_entity_id);
                            if !target_is_player {
                                send_world(&mut sink, &ServerMessage::Error {
                                    message: "that entity isn't a player you can invite".to_string(),
                                }).await?;
                                continue;
                            }
                            deps.pending_guild_invites
                                .lock()
                                .unwrap()
                                .insert(target_entity_id, entity_id);
                            send_to(&deps.global_sessions, target_entity_id, ServerMessage::GuildInviteReceived {
                                from_entity_id: entity_id.to_string(),
                            });
                        }
                        Ok(ClientMessage::GuildInviteResponse { accept }) => {
                            let Some(inviter_entity_id) = deps
                                .pending_guild_invites
                                .lock()
                                .unwrap()
                                .remove(&entity_id)
                            else {
                                send_world(&mut sink, &ServerMessage::Error {
                                    message: "you have no pending guild invite".to_string(),
                                }).await?;
                                continue;
                            };
                            if !accept {
                                send_to(&deps.global_sessions, inviter_entity_id, ServerMessage::GuildInviteDeclined {
                                    by_entity_id: entity_id.to_string(),
                                });
                                continue;
                            }
                            let inviter_account_id = deps
                                .entity_accounts
                                .lock()
                                .unwrap()
                                .get(&inviter_entity_id)
                                .copied();
                            let Some(inviter_account_id) = inviter_account_id else {
                                send_world(&mut sink, &ServerMessage::Error {
                                    message: "the player who invited you is no longer connected".to_string(),
                                }).await?;
                                continue;
                            };
                            match deps.guild_store.accept_invite(inviter_account_id, account_id).await {
                                Ok(guild_id) => {
                                    if let Some(chat) = &deps.chat
                                        && let Ok(Some(info)) = deps.guild_store.info(guild_id).await
                                        && let Some(channel_id) = info.chat_channel_id
                                        && let Err(e) = chat.store.join(channel_id, account_id).await
                                    {
                                        tracing::warn!(error = %e, %account_id, "failed to sync guild chat channel membership");
                                    }
                                    let members = deps.guild_store.members_of(guild_id).await.unwrap_or_default();
                                    let interested: Vec<AccountId> = members.into_iter().map(|(a, _)| a).collect();
                                    refresh_guild_rosters(&deps, Some(guild_id), &interested).await;
                                }
                                Err(e) => {
                                    send_world(&mut sink, &ServerMessage::Error { message: e.to_string() }).await?;
                                }
                            }
                        }
                        Ok(ClientMessage::GuildLeave {}) => {
                            handle_guild_leave_or_disband(&deps, &mut sink, account_id, false).await?;
                        }
                        Ok(ClientMessage::GuildDisband {}) => {
                            handle_guild_leave_or_disband(&deps, &mut sink, account_id, true).await?;
                        }
                        Ok(ClientMessage::GuildKick { target_entity_id }) => {
                            let Some(target_account_id) = resolve_target_account(&deps, &mut sink, &target_entity_id).await? else { continue };
                            let guild_id_before = deps.guild_store.guild_of(account_id).await.ok().flatten();
                            match deps.guild_store.kick(account_id, target_account_id).await {
                                Ok(()) => {
                                    // The kicked target always gets the
                                    // "no guild" empty update, never the
                                    // real (now-stale-for-them) roster;
                                    // remaining members get the current
                                    // one separately — a kick never
                                    // dissolves the guild (the founder
                                    // can't be kicked), so `guild_id_before`
                                    // is still valid to query.
                                    sync_chat_leave(&deps, guild_id_before, target_account_id).await;
                                    refresh_guild_rosters(&deps, None, &[target_account_id]).await;
                                    if let Some(guild_id) = guild_id_before {
                                        let remaining: Vec<AccountId> = deps
                                            .guild_store
                                            .members_of(guild_id)
                                            .await
                                            .unwrap_or_default()
                                            .into_iter()
                                            .map(|(a, _)| a)
                                            .collect();
                                        refresh_guild_rosters(&deps, Some(guild_id), &remaining).await;
                                    }
                                }
                                Err(e) => {
                                    send_world(&mut sink, &ServerMessage::Error { message: e.to_string() }).await?;
                                }
                            }
                        }
                        Ok(ClientMessage::GuildPromote { target_entity_id, rank_key }) => {
                            let Some(target_account_id) = resolve_target_account(&deps, &mut sink, &target_entity_id).await? else { continue };
                            match deps.guild_store.promote(account_id, target_account_id, &rank_key).await {
                                Ok(()) => refresh_after_guild_change(&deps, account_id).await,
                                Err(e) => {
                                    send_world(&mut sink, &ServerMessage::Error { message: e.to_string() }).await?;
                                }
                            }
                        }
                        Ok(ClientMessage::GuildDemote { target_entity_id, rank_key }) => {
                            let Some(target_account_id) = resolve_target_account(&deps, &mut sink, &target_entity_id).await? else { continue };
                            match deps.guild_store.demote(account_id, target_account_id, &rank_key).await {
                                Ok(()) => refresh_after_guild_change(&deps, account_id).await,
                                Err(e) => {
                                    send_world(&mut sink, &ServerMessage::Error { message: e.to_string() }).await?;
                                }
                            }
                        }
                        Ok(ClientMessage::GuildSetMotd { motd }) => {
                            let value = if motd.is_empty() { None } else { Some(motd.as_str()) };
                            match deps.guild_store.set_motd(account_id, value).await {
                                Ok(()) => refresh_after_guild_change(&deps, account_id).await,
                                Err(e) => {
                                    send_world(&mut sink, &ServerMessage::Error { message: e.to_string() }).await?;
                                }
                            }
                        }
                        Ok(ClientMessage::GuildSetTag { tag }) => {
                            let value = if tag.is_empty() { None } else { Some(tag.as_str()) };
                            match deps.guild_store.set_tag(account_id, value).await {
                                Ok(()) => refresh_after_guild_change(&deps, account_id).await,
                                Err(e) => {
                                    send_world(&mut sink, &ServerMessage::Error { message: e.to_string() }).await?;
                                }
                            }
                        }
                        Err(e) => {
                            send_world(&mut sink, &ServerMessage::Error { message: e.to_string() }).await?;
                        }
                    }
                } else if envelope.message_type == CHAT_MESSAGE_TYPE {
                    match &deps.chat {
                        None => {
                            send_world(&mut sink, &ServerMessage::Error {
                                message: "chat is disabled on this server".to_string(),
                            }).await?;
                        }
                        Some(chat) => {
                            let parsed = chat::gateway_protocol::ClientMessage::from_envelope(&envelope);
                            let command_send = match &parsed {
                                Ok(chat::gateway_protocol::ClientMessage::Send { body, .. }) => {
                                    plugin_chat_command(&deps.plugin_chat_commands, body)
                                }
                                _ => None,
                            };
                            if let Some((command, args)) = command_send {
                                // A matched command is consumed here — never
                                // also forwarded to
                                // `chat_session::handle_message`/published as
                                // an ordinary chat message (#57).
                                zone.world.dispatch_chat_command(command, args, entity_id);
                            } else {
                                match parsed {
                                    Ok(message) => {
                                        if let Some(reply) = chat_session::handle_message(
                                            message,
                                            account_id,
                                            chat,
                                            &outgoing_tx,
                                            &mut joined_channels,
                                        ).await {
                                            send_chat(&mut sink, &reply).await?;
                                        }
                                    }
                                    Err(e) => {
                                        send_chat(&mut sink, &chat::gateway_protocol::ServerMessage::Error {
                                            message: e.to_string(),
                                        }).await?;
                                    }
                                }
                            }
                        }
                    }
                } else if deps.plugin_message_types.contains(&envelope.message_type) {
                    // Goes to whichever zone this connection is in right
                    // now — harmless (just an actor-side "no plugin
                    // configured" warning) if that's not the one zone the
                    // configured plugin is attached to (#45's
                    // single-plugin-single-zone scope, see this module's
                    // `SessionDeps`/`zone_registry` doc comments).
                    zone.world.dispatch_plugin_message(
                        envelope.message_type,
                        entity_id,
                        envelope.payload.to_vec(),
                    );
                } else {
                    let message_type = envelope.message_type;
                    send_world(
                        &mut sink,
                        &ServerMessage::Error {
                            message: format!("unrecognized message_type {message_type}"),
                        },
                    )
                    .await?;
                }
            }
            Some(envelope) = outgoing_rx.recv() => {
                // A `ZoneChanged` envelope both goes out to the client
                // (below, same as any other envelope) and tells this
                // task to switch which zone's `WorldHandle`/`Sessions` it
                // talks to from now on — the connection itself never
                // drops for this (#45).
                if envelope.message_type == WORLD_MESSAGE_TYPE
                    && let Ok(ServerMessage::ZoneChanged { zone_id, .. }) = ServerMessage::from_envelope(&envelope)
                {
                    match deps.zones.get(&zone_id) {
                        Some(new_zone) => {
                            current_zone_id = zone_id;
                            zone = new_zone;
                        }
                        None => {
                            tracing::error!(zone_id, "zone transition target vanished from the registry");
                        }
                    }
                }
                let send_result = sink.send(envelope).await;
                if send_result.is_err() {
                    break;
                }
            }
        }
    }

    chat_session::abort_all(joined_channels);
    if let Some(metrics) = &deps.metrics {
        metrics.connection_count.dec();
    }

    // Dispatched before `entity_characters`/`entity_roles` are cleared
    // below, so a plugin's `on-player-leave-zone` handler can still
    // resolve this entity's character if it makes its own host-function
    // calls in response (#155).
    zone.world.dispatch_player_leave(entity_id).await;

    let final_position = zone.world.position_of(entity_id).await;
    zone.world.despawn(entity_id);
    zone.sessions.lock().unwrap().remove(&entity_id);
    deps.global_sessions.lock().unwrap().remove(&entity_id);
    deps.entity_characters.lock().unwrap().remove(&entity_id);
    deps.character_entities
        .lock()
        .unwrap()
        .remove(&character_id);
    deps.entity_accounts.lock().unwrap().remove(&entity_id);
    deps.account_entities.lock().unwrap().remove(&account_id);
    deps.entity_roles.lock().unwrap().remove(&entity_id);
    // #21's clean-disconnect release — a harmless no-op for a bound-realm
    // character (never took a lease to begin with), so this runs
    // unconditionally rather than branching on `realm_open_or_bound`.
    if let Err(e) = deps.character_lease.release(character_id).await {
        tracing::warn!(error = %e, %character_id, "failed to release character session lease on disconnect");
    }
    // #137's clean-disconnect deregistration — runs unconditionally, same
    // as the lease release above (applies to every realm, not just open
    // ones).
    if let Err(e) = deps
        .realm_presence
        .disconnect(deps.realm_id, entity_id.as_uuid())
        .await
    {
        tracing::warn!(error = %e, %character_id, "failed to remove realm presence on disconnect");
    }
    // Character-scope (and any leftover entity-scope) cache entries for
    // this connection — keeps the shared process-wide cache from growing
    // unbounded across reconnects (#149). Zone-scope entries are never
    // touched here; they live for the zone's/process's lifetime.
    let character_prefix = format!("character:{entity_id}:");
    let entity_prefix = format!("entity:{entity_id}:");
    deps.plugin_state_cache
        .lock()
        .unwrap()
        .retain(|k, _| !k.starts_with(&character_prefix) && !k.starts_with(&entity_prefix));
    broadcast(
        &zone.sessions,
        ServerMessage::EntityDespawned {
            entity_id: entity_id.to_string(),
        },
    );

    if let Some((x, y)) = final_position {
        deps.character_store
            .update_position_and_zone(character_id, (x, y, 0.0), &current_zone_id)
            .await?;
    }

    Ok(())
}

/// Matches a chat `Send`'s `body` against `declared_commands` (a
/// plugin's `plugin.toml` `chat_commands`, without leading slashes) —
/// `body` must start with `/`, and everything up to the first space (or
/// the rest of the string if there's no space) is the command name,
/// case-sensitive. Returns the matched command name and the remaining
/// args (trimmed of the one separating space, empty string if none).
fn plugin_chat_command(declared_commands: &[String], body: &str) -> Option<(String, String)> {
    let rest = body.strip_prefix('/')?;
    let (command, args) = match rest.split_once(' ') {
        Some((command, args)) => (command, args),
        None => (rest, ""),
    };
    declared_commands
        .iter()
        .any(|declared| declared == command)
        .then(|| (command.to_string(), args.to_string()))
}

/// Root span for #49's demonstrated cross-service trace path: `gateway`
/// (this connection's own task) → `auth` (`register`/`verify_credentials`,
/// `issue_session`) → Redis (`auth::SessionManager::issue`'s write). A
/// single client action — the connection's very first envelope — nests
/// three crates' worth of spans under one trace, exported as one
/// reconstructable request if `WZ_OTEL_ENDPOINT` is set
/// (`common::logging::init`), otherwise just ordinary nested log context.
#[tracing::instrument(skip_all)]
async fn authenticate(
    message: auth::gateway_protocol::ClientMessage,
    provider: &auth::UsernamePasswordProvider,
) -> Result<(AccountId, String, String)> {
    use auth::AuthProvider;
    use auth::gateway_protocol::ClientMessage as AuthMessage;

    // `Resume` (#195) is the one branch that returns early with its own
    // token instead of falling through to `issue_session` below — it
    // reuses the token the client already presented, renewed in place by
    // `SessionManager::resolve` (see that method's own doc comment for
    // why: same token, sliding expiration, a deliberate bearer-token
    // choice). `Register`/`Login` both still issue a brand new session.
    let (account_id, username) = match message {
        AuthMessage::Register { username, password } => {
            let account_id = provider.register(&username, &password).await?;
            (account_id, username)
        }
        AuthMessage::Login { username, password } => {
            let credentials = auth::Credentials::new(
                serde_json::json!({ "username": username, "password": password }),
            );
            let account_id = provider.verify_credentials(&credentials).await?;
            (account_id, username)
        }
        AuthMessage::Resume { session_token } => {
            let (account_id, username) = provider.resume(&session_token).await?;
            return Ok((account_id, username, session_token));
        }
    };

    let session = provider.issue_session(account_id).await?;
    Ok((account_id, username, session.token))
}

pub(crate) fn entity_type_label(kind: EntityKind) -> String {
    match kind {
        EntityKind::Player => String::new(),
        EntityKind::Npc => "npc".to_string(),
    }
}

/// The server's own wall-clock, as Unix millis — `Pong.server_time`
/// (#196). Never used for simulation logic (that's `tick`'s job); purely
/// so a client can estimate clock skew alongside round-trip time.
fn unix_millis_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("system clock is before the Unix epoch")
        .as_millis() as i64
}

/// Actually performs a live layer move onto `other_entity_id`'s layer,
/// if one is actually needed — the mechanics half of the `#142`
/// placement primitive, shared by `JoinGroupLayer` (which validates real
/// party membership before calling this) and `PartyInviteResponse`'s
/// accept path (membership was just established, no separate check
/// needed). A no-op — no message sent — if `other_entity_id` isn't
/// spawned anywhere in this zone right now, or is already on the
/// caller's own layer.
async fn perform_group_layer_move(
    zone: &mut ZoneRuntime,
    zone_id: &str,
    entity_id: EntityId,
    other_entity_id: EntityId,
    zones: &ZoneRegistry,
    sink: &mut ServerSink,
    outgoing_tx: &mpsc::UnboundedSender<Envelope>,
) -> Result<()> {
    match zones.join_layer_of(zone_id, other_entity_id) {
        Some(target) if !Arc::ptr_eq(&target.sessions, &zone.sessions) => {
            let Some(position) = zone.world.position_of(entity_id).await else {
                // Already despawned/disconnecting — nothing to move.
                return Ok(());
            };
            despawn_from_layer(zone, entity_id);
            let message = spawn_into_layer(
                &target,
                zone_id.to_string(),
                entity_id,
                position,
                outgoing_tx.clone(),
            )
            .await;
            send_world(sink, &message).await?;
            *zone = target;
        }
        _ => {}
    }
    Ok(())
}

/// Sends every currently-online character in `interested` a fresh
/// `PartyUpdate` reflecting *their own* current party roster (#178) —
/// called after any membership change (accept, leave/dissolve), since a
/// change to one member's party can change what every other member
/// should see. A character no longer in a party (the one who just left,
/// or everyone if the party just dissolved) correctly gets an empty
/// roster back from `PartyStore::members_of` — no special-casing needed.
async fn refresh_party_rosters(deps: &SessionDeps, interested: &[CharacterId]) {
    for &character_id in interested {
        let Some(entity_id) = deps
            .character_entities
            .lock()
            .unwrap()
            .get(&character_id)
            .copied()
        else {
            continue;
        };
        let Ok(members) = deps.party_store.members_of(character_id).await else {
            continue;
        };
        let member_entity_ids: Vec<String> = members
            .iter()
            .filter_map(|member_character_id| {
                deps.character_entities
                    .lock()
                    .unwrap()
                    .get(member_character_id)
                    .copied()
            })
            .map(|id| id.to_string())
            .collect();
        send_to(
            &deps.global_sessions,
            entity_id,
            ServerMessage::PartyUpdate {
                members: member_entity_ids,
            },
        );
    }
}

/// Parses `target_entity_id` and resolves it to a currently-connected
/// account (#179) — the shared precondition every guild message that
/// names another player needs. Sends a clear `Error` to `sink` and
/// returns `Ok(None)` for an unparseable id or a target that isn't
/// (currently) a connected player; the caller's match arm should
/// `continue` in that case rather than proceeding.
async fn resolve_target_account(
    deps: &SessionDeps,
    sink: &mut ServerSink,
    target_entity_id: &str,
) -> Result<Option<AccountId>> {
    let Ok(target_entity_id) = target_entity_id.parse::<EntityId>() else {
        send_world(
            sink,
            &ServerMessage::Error {
                message: format!("{target_entity_id:?} is not a valid entity id"),
            },
        )
        .await?;
        return Ok(None);
    };
    let target_account_id = deps
        .entity_accounts
        .lock()
        .unwrap()
        .get(&target_entity_id)
        .copied();
    if target_account_id.is_none() {
        send_world(
            sink,
            &ServerMessage::Error {
                message: "that entity isn't a currently-connected player".to_string(),
            },
        )
        .await?;
    }
    Ok(target_account_id)
}

/// Shared by the `GuildLeave`/`GuildDisband` handlers — both need the
/// same "capture the guild and its members before mutating, so the
/// departing/disbanding account(s) still get a final empty `GuildUpdate`"
/// shape (#179).
async fn handle_guild_leave_or_disband(
    deps: &SessionDeps,
    sink: &mut ServerSink,
    account_id: AccountId,
    disband: bool,
) -> Result<()> {
    let guild_id_before = deps.guild_store.guild_of(account_id).await.ok().flatten();
    let members_before: Vec<AccountId> = match guild_id_before {
        Some(guild_id) => deps
            .guild_store
            .members_of(guild_id)
            .await
            .unwrap_or_default()
            .into_iter()
            .map(|(a, _)| a)
            .collect(),
        None => Vec::new(),
    };

    let result = if disband {
        deps.guild_store.disband(account_id).await
    } else {
        deps.guild_store.leave(account_id).await
    };

    match result {
        Ok(()) if disband => {
            // The whole guild is gone — every former member (including
            // the disbander) gets the "no guild" empty update and a
            // chat-leave sync, plus an explicit `GuildDisbanded`. Unlike
            // a plain leave, there's no "remaining roster" to send
            // anyone here.
            for &member in &members_before {
                sync_chat_leave(deps, guild_id_before, member).await;
            }
            refresh_guild_rosters(deps, None, &members_before).await;
            for &member in &members_before {
                if let Some(member_entity_id) =
                    deps.account_entities.lock().unwrap().get(&member).copied()
                {
                    send_to(
                        &deps.global_sessions,
                        member_entity_id,
                        ServerMessage::GuildDisbanded {},
                    );
                }
            }
            Ok(())
        }
        Ok(()) => {
            // A plain leave: `account_id` always gets the "no guild"
            // empty update. The guild may still exist with other
            // members (ordinary leave) or may have just dissolved (a
            // lone founder leaving) — `info` after the mutation tells
            // us which; only in the former case does anyone else need a
            // refreshed roster.
            sync_chat_leave(deps, guild_id_before, account_id).await;
            refresh_guild_rosters(deps, None, &[account_id]).await;
            if let Some(guild_id) = guild_id_before
                && deps
                    .guild_store
                    .info(guild_id)
                    .await
                    .ok()
                    .flatten()
                    .is_some()
            {
                let remaining: Vec<AccountId> = deps
                    .guild_store
                    .members_of(guild_id)
                    .await
                    .unwrap_or_default()
                    .into_iter()
                    .map(|(a, _)| a)
                    .collect();
                refresh_guild_rosters(deps, Some(guild_id), &remaining).await;
            }
            Ok(())
        }
        Err(e) => {
            send_world(
                sink,
                &ServerMessage::Error {
                    message: e.to_string(),
                },
            )
            .await
        }
    }
}

/// Removes `account_id` from the chat channel synced to `guild_id`, if
/// chat is enabled and that guild actually had a channel — a no-op
/// otherwise (#179). Failures are logged, not propagated: a guild
/// mutation that already committed should never be reported as failed
/// to the client just because the optional chat sync afterward had a
/// problem.
async fn sync_chat_leave(deps: &SessionDeps, guild_id: Option<GuildId>, account_id: AccountId) {
    let (Some(chat), Some(guild_id)) = (&deps.chat, guild_id) else {
        return;
    };
    let Ok(Some(info)) = deps.guild_store.info(guild_id).await else {
        return;
    };
    let Some(channel_id) = info.chat_channel_id else {
        return;
    };
    if let Err(e) = chat.store.leave(channel_id, account_id).await {
        tracing::warn!(error = %e, %account_id, "failed to sync guild chat channel membership");
    }
}

/// Looks up `account_id`'s current guild (if any) and pushes a fresh
/// `GuildUpdate` to every member of it — the common "something about my
/// guild changed, tell everyone in it" step after promote/demote/
/// metadata edits (#179), which never change who's a member so there's
/// no chat sync to do.
async fn refresh_after_guild_change(deps: &SessionDeps, account_id: AccountId) {
    let Ok(Some(guild_id)) = deps.guild_store.guild_of(account_id).await else {
        return;
    };
    let interested: Vec<AccountId> = deps
        .guild_store
        .members_of(guild_id)
        .await
        .unwrap_or_default()
        .into_iter()
        .map(|(a, _)| a)
        .collect();
    refresh_guild_rosters(deps, Some(guild_id), &interested).await;
}

/// Pushes a `GuildUpdate` to every currently-connected account in
/// `interested` (#179) — `guild_id` names the roster to report; `None`
/// (or a guild that no longer resolves, e.g. just dissolved) sends the
/// "no guild" empty update instead. An account not currently connected
/// is silently skipped — it picks up current state at its next login,
/// same "no push to the offline" convention `refresh_party_rosters`
/// already follows implicitly (a party member with no live entity id is
/// filtered out of the roster, never separately notified).
async fn refresh_guild_rosters(
    deps: &SessionDeps,
    guild_id: Option<GuildId>,
    interested: &[AccountId],
) {
    let snapshot = match guild_id {
        Some(id) => match deps.guild_store.info(id).await {
            Ok(Some(info)) => {
                let members = deps.guild_store.members_of(id).await.unwrap_or_default();
                Some((info, members))
            }
            _ => None,
        },
        None => None,
    };

    for &account_id in interested {
        let Some(entity_id) = deps
            .account_entities
            .lock()
            .unwrap()
            .get(&account_id)
            .copied()
        else {
            continue;
        };
        let message = match &snapshot {
            Some((info, members)) => ServerMessage::GuildUpdate {
                guild_id: info.id.to_string(),
                name: info.name.clone(),
                motd: info.motd.clone().unwrap_or_default(),
                tag: info.tag.clone().unwrap_or_default(),
                members: members
                    .iter()
                    .map(|(member_account_id, rank_key)| GuildMemberEntry {
                        entity_id: deps
                            .account_entities
                            .lock()
                            .unwrap()
                            .get(member_account_id)
                            .map(|id| id.to_string())
                            .unwrap_or_default(),
                        rank_key: rank_key.clone(),
                    })
                    .collect(),
            },
            None => ServerMessage::GuildUpdate {
                guild_id: String::new(),
                name: String::new(),
                motd: String::new(),
                tag: String::new(),
                members: Vec::new(),
            },
        };
        send_to(&deps.global_sessions, entity_id, message);
    }
}

fn queue(outgoing_tx: &mpsc::UnboundedSender<Envelope>, message: &ServerMessage) {
    if let Ok(envelope) = message.into_envelope() {
        let _ = outgoing_tx.send(envelope);
    }
}

fn broadcast(sessions: &Sessions, message: ServerMessage) {
    let Ok(envelope) = message.into_envelope() else {
        return;
    };
    for sender in sessions.lock().unwrap().values() {
        let _ = sender.send(envelope.clone());
    }
}

fn broadcast_except(sessions: &Sessions, exclude: EntityId, message: ServerMessage) {
    let Ok(envelope) = message.into_envelope() else {
        return;
    };
    for (id, sender) in sessions.lock().unwrap().iter() {
        if *id != exclude {
            let _ = sender.send(envelope.clone());
        }
    }
}

async fn send_world(sink: &mut ServerSink, message: &ServerMessage) -> Result<()> {
    let envelope = message.into_envelope()?;
    sink.send(envelope)
        .await
        .map_err(|e| Error::wrap("server", "failed to send to client", e))
}

async fn send_auth(
    sink: &mut ServerSink,
    message: &auth::gateway_protocol::ServerMessage,
) -> Result<()> {
    let envelope = message.into_envelope()?;
    sink.send(envelope)
        .await
        .map_err(|e| Error::wrap("server", "failed to send to client", e))
}

async fn send_chat(
    sink: &mut ServerSink,
    message: &chat::gateway_protocol::ServerMessage,
) -> Result<()> {
    let envelope = message.into_envelope()?;
    sink.send(envelope)
        .await
        .map_err(|e| Error::wrap("server", "failed to send to client", e))
}

async fn send_auth_error(sink: &mut ServerSink, message: String) -> Result<()> {
    send_auth(
        sink,
        &auth::gateway_protocol::ServerMessage::Error { message },
    )
    .await
}

async fn send_realm(sink: &mut ServerSink, message: &realm_protocol::ServerMessage) -> Result<()> {
    let envelope = message.into_envelope()?;
    sink.send(envelope)
        .await
        .map_err(|e| Error::wrap("server", "failed to send to client", e))
}

async fn send_realm_error(sink: &mut ServerSink, message: String) -> Result<()> {
    send_realm(sink, &realm_protocol::ServerMessage::Error { message }).await
}

async fn send_character(
    sink: &mut ServerSink,
    message: &character_protocol::ServerMessage,
) -> Result<()> {
    let envelope = message.into_envelope()?;
    sink.send(envelope)
        .await
        .map_err(|e| Error::wrap("server", "failed to send to client", e))
}

async fn send_character_error(sink: &mut ServerSink, message: String) -> Result<()> {
    send_character(sink, &character_protocol::ServerMessage::Error { message }).await
}

/// Builds #192's `RealmList` entry for the one realm this process
/// serves — `character_count`/`live_connection_count` are read fresh
/// from `realm_presence::population` on every call (not cached), so a
/// client polling `ListRealms` sees current numbers.
async fn build_realm_summary(deps: &SessionDeps) -> Result<realm_protocol::RealmSummary> {
    let population = deps
        .realm_presence
        .population(&deps.character_store, deps.realm_id)
        .await?;
    let open_or_bound = match deps.realm_open_or_bound {
        realm_directory::OpenOrBound::Open => "open",
        realm_directory::OpenOrBound::Bound => "bound",
    };
    Ok(realm_protocol::RealmSummary {
        realm_id: deps.realm_id.to_string(),
        name: deps.realm_name.clone(),
        open_or_bound: open_or_bound.to_string(),
        character_count: population.character_count,
        live_connection_count: population.live_connections,
    })
}

/// Fires `on-character-create` (#194) on every plugin that declared it,
/// then drains and applies each plugin's own
/// `apply-stat-delta-for-character` requests — the character-creation
/// counterpart to `world_actor::fire_hook`/`drain_and_apply_plugin_effects`,
/// but not built on either of them: there's no `Zone`/`EntityCharacters`
/// context here (the character has no entity yet), so this dispatches
/// directly against `character_store` instead. A failed hook call or a
/// rejected stat write is logged and otherwise ignored — same
/// never-fatal discipline every other hook call site already uses.
///
/// A successful stat write also pushes `StatChanged` (#211), but only if
/// `character_entities` already has a live entity for this character —
/// this hook fires right after the character row is created, before any
/// client has selected/joined it (see `wit/plugin.wit`'s doc comment on
/// `on-character-create`), so the ordinary case is no live connection to
/// push to at all; skipped silently, same "no owning connection, nothing
/// to push" discipline the NPC branch of `apply-stat-delta` already
/// follows.
async fn fire_on_character_create(
    plugins: &Arc<tokio::sync::Mutex<Vec<crate::plugin_startup::PluginRuntime>>>,
    character_store: &CharacterStore,
    character_entities: &CharacterEntities,
    global_sessions: &Sessions,
    character_id: CharacterId,
    zone_id: &str,
) {
    let character_id_str = character_id.to_string();
    let mut plugins = plugins.lock().await;
    for runtime in plugins.iter_mut() {
        if !runtime.wants("on-character-create") {
            continue;
        }
        if let Err(e) = runtime
            .plugin
            .on_character_create(&character_id_str, zone_id)
        {
            tracing::warn!(plugin = %runtime.name, %character_id, error = %e, "plugin on_character_create hook failed");
        }
        for (target, stat_key, delta) in runtime.drain_pending_character_stat_deltas() {
            let Ok(target_id) = target.parse::<CharacterId>() else {
                tracing::warn!(plugin = %runtime.name, character_id = %target, "plugin apply-stat-delta-for-character called with an invalid character id");
                continue;
            };
            match character_store
                .apply_stat_delta(target_id, &stat_key, delta)
                .await
            {
                Ok(new_value) => {
                    let live_entity = character_entities.lock().unwrap().get(&target_id).copied();
                    if let Some(entity_id) = live_entity {
                        send_to(
                            global_sessions,
                            entity_id,
                            ServerMessage::StatChanged {
                                stat_key: stat_key.clone(),
                                value: new_value,
                            },
                        );
                    }
                }
                Err(e) => {
                    tracing::warn!(plugin = %runtime.name, character_id = %target_id, stat_key, error = %e, "plugin apply-stat-delta-for-character failed");
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn declared() -> Vec<String> {
        vec!["roll".to_string(), "whisper".to_string()]
    }

    #[test]
    fn a_declared_command_with_args_is_matched() {
        assert_eq!(
            plugin_chat_command(&declared(), "/roll 2d6"),
            Some(("roll".to_string(), "2d6".to_string()))
        );
    }

    #[test]
    fn a_declared_command_with_no_args_is_matched_with_empty_args() {
        assert_eq!(
            plugin_chat_command(&declared(), "/roll"),
            Some(("roll".to_string(), "".to_string()))
        );
    }

    #[test]
    fn an_undeclared_command_is_not_matched() {
        assert_eq!(plugin_chat_command(&declared(), "/unknown foo"), None);
    }

    #[test]
    fn ordinary_chat_without_a_leading_slash_is_not_matched() {
        assert_eq!(plugin_chat_command(&declared(), "hello everyone"), None);
    }
}
