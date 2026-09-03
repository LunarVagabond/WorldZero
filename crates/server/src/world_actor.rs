//! Owns the phase-1 process's single `world::Zone` on its own task and
//! drives its fixed-rate tick loop — session tasks talk to it only
//! through `WorldHandle`'s command channel, never through a shared lock.
//! `world::Zone::run` (docs/PROPOSAL.md's tick-loop API, #31) is a
//! self-contained convenience for the simple "just run this zone" case;
//! `server` needs ticks interleaved with real command traffic (spawn,
//! move, despawn from connected sessions), so this reimplements the same
//! scheduling logic — fixed `dt`, log-and-resync on an overrun rather
//! than catching up — around `Zone::tick()`'s pure step instead.

use character::{AttributeSchema, CharacterStore, CurrencySchema};
use common::id::EntityId;
use common::metrics::Metrics;
use prometheus::IntGauge;
use tokio::sync::{mpsc, oneshot};
use tokio::time::Instant;
use world::{EntityKind, MovementOutcome, Point, Zone};

use crate::plugin_startup::PluginRuntime;
use crate::send_to;
use crate::session::{EntityCharacters, NpcStats, Sessions};
use crate::session_protocol::ServerMessage;

enum WorldCommand {
    Spawn {
        entity: EntityId,
        kind: EntityKind,
        position: Point,
    },
    Despawn {
        entity: EntityId,
    },
    /// `seq` is the client-assigned sequence number `Moved`/`Rejected`
    /// will echo back (#196) — `0` for a plugin-driven move
    /// (`apply_plugin_pending_effects`'s own `request_move` call), never
    /// a real client sequence number, which always starts at `1`.
    RequestMove {
        entity: EntityId,
        to: Point,
        seq: u32,
    },
    PositionOf {
        entity: EntityId,
        reply: oneshot::Sender<Option<Point>>,
    },
    /// The zone's current tick counter (#196) — `Moved`/`Rejected` get
    /// theirs from the tick outcomes they're built from (`Zone::tick`'s
    /// own return, stamped by the caller in `main::handle_tick_outcomes`);
    /// this is for the messages built *between* ticks instead (`Joined`),
    /// which need to ask for it explicitly.
    CurrentTick {
        reply: oneshot::Sender<u64>,
    },
    EntitiesSnapshot {
        reply: oneshot::Sender<Vec<(EntityId, EntityKind, Point)>>,
    },
    /// A gateway-received message whose `message_type` matched the
    /// configured plugin's declared `message_types` (#95) — routed here
    /// rather than handled inline in `session`, since the live plugin
    /// instance lives on this task alongside the `Zone` it was
    /// instantiated for (docs/specs/Plugin_API.md: "instantiated for a
    /// zone-service").
    PluginMessage {
        message_type: u16,
        sender_entity_id: EntityId,
        payload: Vec<u8>,
    },
    /// A chat `Send` whose leading `/command` matched the configured
    /// plugin's declared `chat_commands` (`session`'s `plugin_chat_command`
    /// already did that match) — routed here for the same reason
    /// `PluginMessage` is: the live plugin instance lives on this task (#57).
    ChatCommand {
        command: String,
        args: String,
        sender_entity_id: EntityId,
    },
    /// A connection has fully joined this zone (`session::handle_session`,
    /// after roster delivery) or is cleanly disconnecting from it (#155) —
    /// routed here for the same reason `PluginMessage`/`ChatCommand` are:
    /// the live plugin instance lives on this task. Sent for every zone a
    /// player is ever in, not just the one the configured plugin happens
    /// to be attached to — a zone actor with no plugin configured just
    /// no-ops rather than warning, since that's the common case for most
    /// zones (contrast with `PluginMessage`/`ChatCommand`, which only ever
    /// reach a zone once something already matched a plugin's own
    /// declared `message_types`/`chat_commands`).
    PlayerJoin {
        entity_id: EntityId,
    },
    /// Carries a reply so `session::handle_session` can await the hook
    /// (and any pending effects it triggers) actually running *before*
    /// it removes this entity from `entity_characters`/`entity_roles` —
    /// unlike every other fire-and-forget command here, ordering matters:
    /// a plugin's `on-player-leave-zone` handler resolving this entity's
    /// character (e.g. to `apply-stat-delta`) would silently no-op if the
    /// caller already cleared the map first (#155).
    PlayerLeave {
        entity_id: EntityId,
        reply: oneshot::Sender<()>,
    },
    /// A client's `Attack` action targeting another entity (#154) — the
    /// actor confirms `target` is actually spawned in this zone
    /// (`Zone::kind_of`) before ever calling the hook, the same
    /// server-authoritative discipline `RequestMove` already applies to
    /// a client-claimed position; an unknown/vanished target is logged
    /// and dropped, not passed through. Fires `on-damage-calc` with
    /// `base-amount` always `0` (the core never invents a damage number
    /// — see `wit/plugin.wit`'s doc comment) and `stat_key` as the client
    /// requested (an opaque, game-defined string, same "id/key is
    /// plugin-owned" discipline as `item_type` below — never a
    /// core-privileged concept like "hp").
    Attack {
        attacker: EntityId,
        target: EntityId,
        stat_key: String,
    },
    /// A client's `UseItem` action (#154) — `item_type` is an opaque
    /// string, same discipline as `grant-item`/`remove-item`; the core
    /// never validates ownership itself, the plugin decides what using it
    /// does (typically by calling `remove-item` itself in response).
    UseItem {
        entity_id: EntityId,
        item_type: String,
    },
    /// A client's `InteractNpc` action targeting a specific NPC entity
    /// (#154), distinct from the generic trigger-volume `on-interact` —
    /// the actor confirms `npc` is actually a currently-spawned NPC
    /// (`Zone::kind_of`) before ever calling the hook; a target that
    /// doesn't exist or isn't an NPC is logged and dropped.
    InteractNpc {
        npc: EntityId,
        actor: EntityId,
    },
}

#[derive(Clone)]
pub struct WorldHandle {
    commands: mpsc::UnboundedSender<WorldCommand>,
    /// `worldzero_zone_world_command_queue_depth` (#48) for this zone —
    /// `None` end to end when metrics are disabled
    /// (`ServicesConfig::metrics_enabled`), not a gauge that's tracked
    /// but never served. Incremented here, on send; decremented once per
    /// command actually dequeued, in the actor loop below.
    queue_depth: Option<IntGauge>,
}

#[cfg(test)]
impl WorldHandle {
    /// A `WorldHandle` with no actor task actually listening on the
    /// other end — every command silently goes nowhere. Only for tests
    /// (`zone_registry`'s `join_layer_of` tests, #142) that need a real
    /// `ZoneRuntime` to populate a `Sessions` map against, but never
    /// exercise `.world` itself.
    pub fn detached_for_test() -> Self {
        let (commands, _rx) = mpsc::unbounded_channel();
        Self {
            commands,
            queue_depth: None,
        }
    }
}

impl WorldHandle {
    /// Every `WorldCommand` send goes through here — the one place that
    /// increments `queue_depth`, so no dispatch method below can forget
    /// to (#48).
    fn send(&self, command: WorldCommand) -> bool {
        let sent = self.commands.send(command).is_ok();
        if sent && let Some(gauge) = &self.queue_depth {
            gauge.inc();
        }
        sent
    }

    pub fn spawn(&self, entity: EntityId, kind: EntityKind, position: Point) {
        self.send(WorldCommand::Spawn {
            entity,
            kind,
            position,
        });
    }

    pub fn despawn(&self, entity: EntityId) {
        self.send(WorldCommand::Despawn { entity });
    }

    /// `seq` is the client-assigned sequence number `Moved`/`Rejected`
    /// will echo back (#196) — see `WorldCommand::RequestMove`'s doc
    /// comment for the `0`-means-"not a real client request" convention.
    pub fn request_move(&self, entity: EntityId, to: Point, seq: u32) {
        self.send(WorldCommand::RequestMove { entity, to, seq });
    }

    /// The zone's current tick counter (#196), for a message built
    /// between ticks (`Joined`) rather than from a tick's own outcomes.
    /// `0` (indistinguishable from "no ticks have run yet") if the actor
    /// task is gone — same "empty/default on gone" contract as
    /// `entities_snapshot`.
    pub async fn current_tick(&self) -> u64 {
        let (reply_tx, reply_rx) = oneshot::channel();
        if !self.send(WorldCommand::CurrentTick { reply: reply_tx }) {
            return 0;
        }
        reply_rx.await.unwrap_or(0)
    }

    /// `None` both when the entity isn't spawned and when the actor task
    /// is gone — a caller persisting a last-known position on disconnect
    /// treats both the same way (nothing to persist).
    pub async fn position_of(&self, entity: EntityId) -> Option<Point> {
        let (reply_tx, reply_rx) = oneshot::channel();
        if !self.send(WorldCommand::PositionOf {
            entity,
            reply: reply_tx,
        }) {
            return None;
        }
        reply_rx.await.ok().flatten()
    }

    /// Empty (not an error) if the actor task is gone.
    pub async fn entities_snapshot(&self) -> Vec<(EntityId, EntityKind, Point)> {
        let (reply_tx, reply_rx) = oneshot::channel();
        if !self.send(WorldCommand::EntitiesSnapshot { reply: reply_tx }) {
            return Vec::new();
        }
        reply_rx.await.unwrap_or_default()
    }

    /// Fire-and-forget, matching the other command methods — a session
    /// task doesn't wait on a plugin's `on_message` hook, it just hands
    /// the message off (#95).
    pub fn dispatch_plugin_message(
        &self,
        message_type: u16,
        sender_entity_id: EntityId,
        payload: Vec<u8>,
    ) {
        self.send(WorldCommand::PluginMessage {
            message_type,
            sender_entity_id,
            payload,
        });
    }

    /// Fire-and-forget, same contract as `dispatch_plugin_message` — the
    /// caller (`session`) has already matched `command` against the
    /// plugin's declared `chat_commands` before calling this.
    pub fn dispatch_chat_command(&self, command: String, args: String, sender_entity_id: EntityId) {
        self.send(WorldCommand::ChatCommand {
            command,
            args,
            sender_entity_id,
        });
    }

    /// Fire-and-forget, same contract as `dispatch_plugin_message` — sent
    /// for every zone a player joins/leaves, not only the zone (if any) a
    /// configured plugin is attached to (#155).
    pub fn dispatch_player_join(&self, entity_id: EntityId) {
        self.send(WorldCommand::PlayerJoin { entity_id });
    }

    /// Awaits the actor having actually run `on-player-leave-zone` (and
    /// applied any pending effects it triggered) before returning — see
    /// `WorldCommand::PlayerLeave`'s doc comment for why this can't be
    /// fire-and-forget like the rest of this handle's methods. Resolves
    /// immediately if the actor task is gone.
    pub async fn dispatch_player_leave(&self, entity_id: EntityId) {
        let (reply_tx, reply_rx) = oneshot::channel();
        if self.send(WorldCommand::PlayerLeave {
            entity_id,
            reply: reply_tx,
        }) {
            let _ = reply_rx.await;
        }
    }

    /// Fire-and-forget, same contract as `dispatch_plugin_message` (#154).
    pub fn dispatch_attack(&self, attacker: EntityId, target: EntityId, stat_key: String) {
        self.send(WorldCommand::Attack {
            attacker,
            target,
            stat_key,
        });
    }

    /// Fire-and-forget, same contract as `dispatch_plugin_message` (#154).
    pub fn dispatch_use_item(&self, entity_id: EntityId, item_type: String) {
        self.send(WorldCommand::UseItem {
            entity_id,
            item_type,
        });
    }

    /// Fire-and-forget, same contract as `dispatch_plugin_message` (#154).
    pub fn dispatch_interact_npc(&self, npc: EntityId, actor: EntityId) {
        self.send(WorldCommand::InteractNpc { npc, actor });
    }
}

/// Spawns the actor task and returns a handle to it. `on_tick` runs once
/// per tick with that tick's movement outcomes — broadcasting
/// `Moved`/`Rejected` to connected sessions is the caller's job
/// (`crate::session`); this only drives the simulation. Not to be
/// confused with the plugin `on-tick` hook (#168, `wit/plugin.wit`) —
/// this `on_tick` parameter is `server`'s own internal callback, fired
/// unconditionally for every zone; the plugin hook of the same name is
/// dispatched further down, per opted-in plugin, alongside `on-npc-tick`.
///
/// `plugins` is shared across every zone-service `server` runs (#152) —
/// one plugin instance, process-wide, not one per zone. Every zone actor
/// locks the same `Mutex` to dispatch a hook call, passing its own
/// `zone_id` as an explicit argument (`wit/plugin.wit`'s `hooks`
/// interface doc comment) — the plugin decides for itself whether/how to
/// react to a given zone, the host never scopes a plugin to specific
/// zones. A lifecycle hook fans out to every plugin that declared it in
/// `plugin.toml`'s `hooks` list (`fire_hook` below) — the core never
/// picks a winner. `on_message`/`on_chat_command` are the one exception:
/// single-owner routing by declared `message_types`/`chat_commands`,
/// already guaranteed collision-free across every loaded plugin before
/// this task ever starts.
#[allow(clippy::too_many_arguments)]
pub fn spawn_world_actor(
    mut zone: Zone,
    tick_interval: std::time::Duration,
    plugins: std::sync::Arc<tokio::sync::Mutex<Vec<PluginRuntime>>>,
    character_store: std::sync::Arc<CharacterStore>,
    entity_characters: EntityCharacters,
    npc_stats: NpcStats,
    attribute_schema: std::sync::Arc<AttributeSchema>,
    currency_schema: std::sync::Arc<character::CurrencySchema>,
    plugin_state_store: std::sync::Arc<crate::plugin_state::PluginStateStore>,
    zone_id: String,
    metrics: Option<std::sync::Arc<Metrics>>,
    // Process-wide, not this zone's own (#211) — `apply-stat-delta`/
    // `grant-item`/`remove-item`/`modify-currency` push
    // `StatChanged`/`ItemChanged`/`CurrencyChanged` back to the exact
    // connection that owns the affected entity, same map `send-message`
    // already resolves against (`plugin_startup::PluginCallbacks`'s own
    // `sessions` field) — an entity stays reachable across a
    // `ZoneChanged` zone-hop without this needing to know it happened.
    global_sessions: Sessions,
    on_tick: impl Fn(&Zone, Vec<(EntityId, MovementOutcome)>) + Send + 'static,
) -> WorldHandle {
    let (tx, mut rx) = mpsc::unbounded_channel::<WorldCommand>();
    let dt = tick_interval.as_secs_f64();

    // Resolved once against this zone's `zone_id` label (#48), not
    // per-tick/per-command — `None` end to end when metrics are
    // disabled, matching `ServicesConfig::metrics_enabled`.
    let tick_duration = metrics.as_ref().map(|m| m.tick_duration_for_zone(&zone_id));
    let entity_count = metrics.as_ref().map(|m| m.entity_count_for_zone(&zone_id));
    let queue_depth = metrics.as_ref().map(|m| m.queue_depth_for_zone(&zone_id));
    let handle_queue_depth = queue_depth.clone();

    tokio::spawn(async move {
        let mut next_tick_at = Instant::now() + tick_interval;

        loop {
            tokio::select! {
                _ = tokio::time::sleep_until(next_tick_at) => {
                    if Instant::now() > next_tick_at + tick_interval {
                        tracing::warn!("world actor tick running behind schedule — resyncing rather than catching up");
                    }

                    let tick_started_at = Instant::now();
                    let outcomes = zone.tick();
                    if let Some(histogram) = &tick_duration {
                        histogram.observe(tick_started_at.elapsed().as_secs_f64());
                    }
                    if let Some(gauge) = &entity_count {
                        gauge.set(zone.entities().len() as i64);
                    }
                    on_tick(&zone, outcomes);

                    // The host never moves an NPC itself — it hands
                    // every opted-in plugin the NPC's position and full
                    // route data and waits for `move-entity` calls back
                    // (#57, wit/plugin.wit's `on-npc-tick` doc comment).
                    // Fan-out (#152): every plugin that declared
                    // `on-npc-tick` gets called for every route-NPC in
                    // this zone — the host doesn't attribute an NPC to
                    // whichever plugin spawned it.
                    let routes = zone.npcs_with_routes();
                    {
                        let mut plugins = plugins.lock().await;
                        for runtime in plugins.iter_mut() {
                            let wants_npc_tick = runtime.wants("on-npc-tick");
                            let wants_tick = runtime.wants("on-tick");
                            if !wants_npc_tick && !wants_tick {
                                continue;
                            }
                            if wants_npc_tick {
                                for (entity, position, route) in &routes {
                                    let entity_str = entity.to_string();
                                    if let Err(e) = runtime.plugin.on_npc_tick(
                                        &zone_id,
                                        &entity_str,
                                        position.0,
                                        position.1,
                                        &route.waypoints,
                                        route.is_loop,
                                        route.speed,
                                        dt,
                                    ) {
                                        tracing::warn!(plugin = %runtime.name, %entity, error = %e, "plugin on_npc_tick hook failed");
                                    }
                                }
                            }
                            // Zone-wide, once per plugin per tick (#168)
                            // — deliberately after this tick's
                            // `on-npc-tick` fan-out above, so a plugin
                            // declaring both sees this tick's NPC moves
                            // already queued before its own aggregate
                            // bookkeeping runs.
                            if wants_tick
                                && let Err(e) = runtime.plugin.on_tick(&zone_id, dt)
                            {
                                tracing::warn!(plugin = %runtime.name, error = %e, "plugin on_tick hook failed");
                            }
                            drain_and_apply_plugin_effects(
                                runtime, &mut zone, &character_store, &entity_characters, &npc_stats, &attribute_schema, &currency_schema, &plugin_state_store, &global_sessions,
                            ).await;
                        }
                    }

                    next_tick_at += tick_interval;
                    let now = Instant::now();
                    if now > next_tick_at {
                        next_tick_at = now + tick_interval;
                    }
                }
                Some(command) = rx.recv() => {
                    if let Some(gauge) = &queue_depth {
                        gauge.dec();
                    }
                    match command {
                        WorldCommand::Spawn { entity, kind, position } => zone.spawn(entity, kind, position),
                        WorldCommand::Despawn { entity } => {
                            zone.despawn(entity);
                            // Harmless no-op for a player entity (never
                            // has an entry here) — keeps npc_stats from
                            // growing past the zone's actual live NPC
                            // population (#197).
                            npc_stats.lock().unwrap().remove(&entity);
                        }
                        WorldCommand::RequestMove { entity, to, seq } => zone.request_move(entity, to, seq),
                        WorldCommand::PositionOf { entity, reply } => {
                            let _ = reply.send(zone.position_of(entity));
                        }
                        WorldCommand::CurrentTick { reply } => {
                            let _ = reply.send(zone.current_tick());
                        }
                        WorldCommand::EntitiesSnapshot { reply } => {
                            let _ = reply.send(zone.entities());
                        }
                        // `on-message`/`on-chat-command` are single-owner,
                        // not fan-out (#152): a `message_type`/`chat_command`
                        // is routed to whichever *one* plugin declared it
                        // (already guaranteed unique across every loaded
                        // plugin by `plugin_host::check_no_collisions` at
                        // startup) — declaring `message_types`/`chat_commands`
                        // already states interest, so neither needs to also
                        // appear in `hooks`.
                        WorldCommand::PluginMessage { message_type, sender_entity_id, payload } => {
                            let mut plugins = plugins.lock().await;
                            let Some(runtime) = plugins.iter_mut().find(|p| p.message_types.contains(&message_type)) else {
                                tracing::warn!(message_type, "received a message_type no loaded plugin declared");
                                continue;
                            };
                            let sender_entity_id = sender_entity_id.to_string();
                            if let Err(e) = runtime.plugin.on_message(&zone_id, message_type, &sender_entity_id, &payload) {
                                tracing::warn!(plugin = %runtime.name, message_type, error = %e, "plugin on_message hook failed");
                            }
                            spawn_requested_npcs(runtime, &mut zone, &zone_id);
                            drain_and_apply_plugin_effects(
                                runtime, &mut zone, &character_store, &entity_characters, &npc_stats, &attribute_schema, &currency_schema, &plugin_state_store, &global_sessions,
                            ).await;
                        }
                        WorldCommand::ChatCommand { command, args, sender_entity_id } => {
                            let mut plugins = plugins.lock().await;
                            let Some(runtime) = plugins.iter_mut().find(|p| p.chat_commands.contains(&command)) else {
                                tracing::warn!(command, "received a chat command no loaded plugin declared");
                                continue;
                            };
                            let sender_entity_id_str = sender_entity_id.to_string();
                            if let Err(e) = runtime.plugin.on_chat_command(&zone_id, &command, &args, &sender_entity_id_str) {
                                tracing::warn!(plugin = %runtime.name, command, error = %e, "plugin on_chat_command hook failed");
                            }
                            // A `spawn-npc` call made from an `on-chat-command`
                            // handler previously had its request silently
                            // dropped here — nothing drained
                            // `pending_spawns` on this path (unlike
                            // `PluginMessage` above), so the plugin's own
                            // `spawn-npc` call would queue a request nothing
                            // ever resolved into a real entity. Fixed
                            // alongside #214 since the correlation test
                            // below exercises exactly this path.
                            spawn_requested_npcs(runtime, &mut zone, &zone_id);
                            drain_and_apply_plugin_effects(
                                runtime, &mut zone, &character_store, &entity_characters, &npc_stats, &attribute_schema, &currency_schema, &plugin_state_store, &global_sessions,
                            ).await;
                        }
                        WorldCommand::PlayerJoin { entity_id } => {
                            let entity_id_str = entity_id.to_string();
                            let mut plugins = plugins.lock().await;
                            fire_hook(
                                &mut plugins, "on-player-join-zone", &mut zone, &character_store, &entity_characters, &npc_stats, &attribute_schema, &currency_schema, &plugin_state_store, &global_sessions,
                                |plugin| plugin.on_player_join_zone(&zone_id, &entity_id_str),
                            ).await;
                        }
                        WorldCommand::PlayerLeave { entity_id, reply } => {
                            let entity_id_str = entity_id.to_string();
                            let mut plugins = plugins.lock().await;
                            fire_hook(
                                &mut plugins, "on-player-leave-zone", &mut zone, &character_store, &entity_characters, &npc_stats, &attribute_schema, &currency_schema, &plugin_state_store, &global_sessions,
                                |plugin| plugin.on_player_leave_zone(&zone_id, &entity_id_str),
                            ).await;
                            let _ = reply.send(());
                        }
                        WorldCommand::Attack { attacker, target, stat_key } => {
                            if zone.kind_of(target).is_none() {
                                tracing::warn!(%attacker, %target, "attack targeted an entity that isn't spawned in this zone");
                                continue;
                            }
                            let attacker_str = attacker.to_string();
                            let target_str = target.to_string();
                            let mut plugins = plugins.lock().await;
                            fire_hook(
                                &mut plugins, "on-damage-calc", &mut zone, &character_store, &entity_characters, &npc_stats, &attribute_schema, &currency_schema, &plugin_state_store, &global_sessions,
                                |plugin| plugin.on_damage_calc(&zone_id, &attacker_str, &target_str, &stat_key, 0),
                            ).await;
                        }
                        WorldCommand::UseItem { entity_id, item_type } => {
                            let entity_id_str = entity_id.to_string();
                            let mut plugins = plugins.lock().await;
                            fire_hook(
                                &mut plugins, "on-item-use", &mut zone, &character_store, &entity_characters, &npc_stats, &attribute_schema, &currency_schema, &plugin_state_store, &global_sessions,
                                |plugin| plugin.on_item_use(&zone_id, &entity_id_str, &item_type),
                            ).await;
                        }
                        WorldCommand::InteractNpc { npc, actor } => {
                            if zone.kind_of(npc) != Some(EntityKind::Npc) {
                                tracing::warn!(%npc, %actor, "npc-interact targeted an entity that isn't a currently-spawned NPC");
                                continue;
                            }
                            let npc_str = npc.to_string();
                            let actor_str = actor.to_string();
                            let mut plugins = plugins.lock().await;
                            fire_hook(
                                &mut plugins, "on-npc-interact", &mut zone, &character_store, &entity_characters, &npc_stats, &attribute_schema, &currency_schema, &plugin_state_store, &global_sessions,
                                |plugin| plugin.on_npc_interact(&zone_id, &npc_str, &actor_str),
                            ).await;
                        }
                    }
                }
            }
        }
    });

    WorldHandle {
        commands: tx,
        queue_depth: handle_queue_depth,
    }
}

/// Resolves a plugin-supplied entity-id string to a real `EntityId` and,
/// via `entity_characters`, the `CharacterId` it belongs to — shared by
/// every pending-effect kind that's character-owned only (items,
/// currency; stats are the one exception, see `apply_npc_stat_delta`
/// below — #197). `None` covers both "not a valid entity id" and "no
/// character for this entity" (an NPC, which has no character row at
/// all, or an unknown/disconnected entity); the caller logs whichever it
/// actually was.
fn resolve_character(
    entity_characters: &EntityCharacters,
    entity_id: &str,
) -> Option<common::id::CharacterId> {
    let entity_id: EntityId = entity_id.parse().ok()?;
    entity_characters.lock().unwrap().get(&entity_id).copied()
}

/// The NPC-entity counterpart to `character::CharacterStore::apply_stat_delta`
/// (#197): same "resolve current value via the declared schema's default,
/// add `delta`, validate, write" discipline, against `npc_stats`'s
/// in-memory per-entity map instead of a `characters` row's `stats`
/// column. Synchronous (no `.await`) — there's no database round trip on
/// this path, unlike the player-character equivalent. Returns the
/// resulting value, same as the character-store version.
fn apply_npc_stat_delta(
    npc_stats: &NpcStats,
    schema: &AttributeSchema,
    entity: EntityId,
    key: &str,
    delta: i64,
) -> common::Result<i64> {
    let mut all_stats = npc_stats.lock().unwrap();
    let stats = all_stats.entry(entity).or_default();

    let stored: serde_json::Map<String, serde_json::Value> = stats
        .iter()
        .map(|(k, v)| (k.clone(), serde_json::Value::from(*v)))
        .collect();
    let current = schema.resolve_read(&stored, key)?;
    let new_value = current.checked_add(delta).ok_or_else(|| {
        common::Error::new("server", format!("stat delta overflowed for {key:?}"))
    })?;
    schema.validate_write(key, new_value)?;

    stats.insert(key.to_string(), new_value);
    Ok(new_value)
}

/// Drains every pending host-function request a plugin hook call just
/// made, applies them, and fires whatever confirmation hooks that
/// unblocks — the one call every `WorldCommand` match arm that invokes a
/// hook makes right after, so a plugin's host-function calls take effect
/// (and `on-item-acquire`/`on-death`/`on-respawn` fire back) regardless
/// of which hook made them. Draining first (`apply_plugin_pending_effects`
/// takes owned data, not a borrow) keeps no live borrow of `runtime`
/// across that function's `.await`s; the plain field accesses/synchronous
/// hook calls here are the only place `runtime` itself is touched.
#[allow(clippy::too_many_arguments)]
async fn drain_and_apply_plugin_effects(
    runtime: &mut PluginRuntime,
    zone: &mut Zone,
    character_store: &CharacterStore,
    entity_characters: &EntityCharacters,
    npc_stats: &NpcStats,
    attribute_schema: &AttributeSchema,
    currency_schema: &CurrencySchema,
    plugin_state_store: &crate::plugin_state::PluginStateStore,
    global_sessions: &Sessions,
) {
    let zone_id = zone.manifest.id.clone();
    let moves = runtime.drain_pending_moves();
    let stat_deltas = runtime.drain_pending_stat_deltas();
    let item_grants = runtime.drain_pending_item_grants();
    let item_removals = runtime.drain_pending_item_removals();
    let currency_deltas = runtime.drain_pending_currency_deltas();
    let state_writes = runtime.drain_pending_state_writes();
    let acquired = apply_plugin_pending_effects(
        zone,
        moves,
        stat_deltas,
        item_grants,
        item_removals,
        currency_deltas,
        state_writes,
        character_store,
        entity_characters,
        npc_stats,
        attribute_schema,
        currency_schema,
        plugin_state_store,
        global_sessions,
    )
    .await;
    let wants_item_acquire = runtime.wants("on-item-acquire");
    for (entity_id, item_type, new_quantity) in acquired {
        if !wants_item_acquire {
            continue;
        }
        if let Err(e) =
            runtime
                .plugin
                .on_item_acquire(&zone_id, &entity_id, &item_type, new_quantity)
        {
            tracing::warn!(entity_id, item_type, error = %e, "plugin on_item_acquire hook failed");
        }
    }
    let wants_death = runtime.wants("on-death");
    for entity_id in runtime.drain_pending_deaths() {
        if !wants_death {
            continue;
        }
        if let Err(e) = runtime.plugin.on_death(&zone_id, &entity_id) {
            tracing::warn!(entity_id, error = %e, "plugin on_death hook failed");
        }
    }
    let wants_respawn = runtime.wants("on-respawn");
    for entity_id in runtime.drain_pending_respawns() {
        if !wants_respawn {
            continue;
        }
        if let Err(e) = runtime.plugin.on_respawn(&zone_id, &entity_id) {
            tracing::warn!(entity_id, error = %e, "plugin on_respawn hook failed");
        }
    }
}

/// Fires `hook_name` on every plugin that declared it in `plugin.toml`'s
/// `hooks` list (#152) — the event-fan-out composition model: every
/// interested plugin gets called, independently, in load order (defined,
/// not meaningful); the core never picks a winner or arbitrates between
/// them. `call` invokes the actual hook (each call site's own closure,
/// since every hook's signature differs); each plugin's own pending
/// host-function effects are drained and applied right after its own
/// hook call (`drain_and_apply_plugin_effects`) before moving to the
/// next plugin, so one plugin's writes never overlap another's `.await`s.
#[allow(clippy::too_many_arguments)]
async fn fire_hook(
    plugins: &mut [PluginRuntime],
    hook_name: &str,
    zone: &mut Zone,
    character_store: &CharacterStore,
    entity_characters: &EntityCharacters,
    npc_stats: &NpcStats,
    attribute_schema: &AttributeSchema,
    currency_schema: &CurrencySchema,
    plugin_state_store: &crate::plugin_state::PluginStateStore,
    global_sessions: &Sessions,
    mut call: impl FnMut(&mut plugin_host::LoadedPlugin) -> common::Result<()>,
) {
    for runtime in plugins.iter_mut() {
        if !runtime.wants(hook_name) {
            continue;
        }
        if let Err(e) = call(&mut runtime.plugin) {
            tracing::warn!(plugin = %runtime.name, hook_name, error = %e, "plugin hook failed");
        }
        drain_and_apply_plugin_effects(
            runtime,
            zone,
            character_store,
            entity_characters,
            npc_stats,
            attribute_schema,
            currency_schema,
            plugin_state_store,
            global_sessions,
        )
        .await;
    }
}

/// Applies whatever `move-entity`/`apply-stat-delta`/`grant-item`/
/// `remove-item`/`modify-currency` requests a plugin hook call just made
/// — shared by every call site that invokes a hook, so a plugin's
/// host-function calls take effect regardless of which hook made them
/// (#57/#116). Takes the already-drained requests (not a
/// `&PluginRuntime`) deliberately: `PluginRuntime` holds the `wasmtime`
/// `Store`, which isn't `Sync`, so a `&PluginRuntime` held across the
/// `.await`s below would make the actor's whole task future non-`Send`
/// (tokio requires `Send` futures) — draining first and passing owned
/// data keeps no plugin-runtime borrow alive across any await.
///
/// Returns every item grant that actually applied, as `(entity_id,
/// item_type, new_quantity)` — the caller uses this to fire
/// `on-item-acquire` back into the plugin *after* this function returns
/// (so that synchronous call never overlaps one of this function's
/// awaits either).
#[allow(clippy::too_many_arguments)]
async fn apply_plugin_pending_effects(
    zone: &mut Zone,
    pending_moves: Vec<(String, f64, f64)>,
    pending_stat_deltas: Vec<(String, String, i64)>,
    pending_item_grants: Vec<(String, String, i64)>,
    pending_item_removals: Vec<(String, String, i64)>,
    pending_currency_deltas: Vec<(String, String, i64)>,
    pending_state_writes: Vec<(plugin_host::PluginStateScope, String, Vec<u8>)>,
    character_store: &CharacterStore,
    entity_characters: &EntityCharacters,
    npc_stats: &NpcStats,
    attribute_schema: &AttributeSchema,
    currency_schema: &CurrencySchema,
    plugin_state_store: &crate::plugin_state::PluginStateStore,
    // #211: pushes `StatChanged`/`ItemChanged`/`CurrencyChanged` to the
    // one connection that owns whichever entity/character a write below
    // actually landed for — never zone-broadcast, unlike `Moved`. Never
    // touched for the NPC branch of the stat-delta loop below (an NPC
    // has no owning connection) or for a player entity that resolved
    // (`resolve_character` returned `Some`) but whose connection has
    // since gone away (`send_to` itself is already a silent no-op for
    // an entity id with no live entry — same "best effort, not a
    // delivery guarantee" contract every other `send_to` call site in
    // this codebase already accepts).
    global_sessions: &Sessions,
) -> Vec<(String, String, i64)> {
    for (entity_id, x, y) in pending_moves {
        match entity_id.parse::<EntityId>() {
            // `0`: a plugin-driven move (`move-entity`) has no client
            // sequence number to echo (#196). `z: 0.0` — the `move-entity`
            // WIT host function is still 2D-only (#249's scope note: the
            // plugin ABI is a separately-versioned surface, not touched
            // by this ticket); a real z parameter there is tracked as a
            // follow-up.
            Ok(entity_id) => zone.request_move(entity_id, (x, y, 0.0), 0),
            Err(_) => {
                tracing::warn!(
                    entity_id,
                    "plugin requested a move for an invalid entity id"
                )
            }
        }
    }

    for (entity_id, stat_key, delta) in pending_stat_deltas {
        let Ok(parsed_entity_id) = entity_id.parse::<EntityId>() else {
            tracing::warn!(
                entity_id,
                "plugin requested a stat delta for an invalid entity id"
            );
            continue;
        };
        // Player vs. NPC decides which storage this resolves against
        // (#197) — a player's declared stats live in its `characters`
        // row, an NPC's in `npc_stats`, but both go through the same
        // `AttributeSchema` bounds/defaults discipline either way.
        match zone.kind_of(parsed_entity_id) {
            Some(EntityKind::Player) => {
                let Some(character_id) = resolve_character(entity_characters, &entity_id) else {
                    tracing::warn!(
                        entity_id,
                        "plugin requested a stat delta for a player entity with no \
                         character mapping (not fully joined yet?)"
                    );
                    continue;
                };
                match character_store
                    .apply_stat_delta(character_id, &stat_key, delta)
                    .await
                {
                    Ok(new_value) => {
                        send_to(
                            global_sessions,
                            parsed_entity_id,
                            ServerMessage::StatChanged {
                                stat_key: stat_key.clone(),
                                value: new_value,
                            },
                        );
                    }
                    Err(e) => {
                        tracing::warn!(entity_id, stat_key, error = %e, "plugin's apply-stat-delta failed");
                    }
                }
            }
            Some(EntityKind::Npc) => {
                if let Err(e) = apply_npc_stat_delta(
                    npc_stats,
                    attribute_schema,
                    parsed_entity_id,
                    &stat_key,
                    delta,
                ) {
                    tracing::warn!(entity_id, stat_key, error = %e, "plugin's apply-stat-delta failed");
                }
            }
            None => {
                tracing::warn!(
                    entity_id,
                    "plugin requested a stat delta for an entity that isn't currently \
                     spawned in this zone"
                );
            }
        }
    }

    let mut acquired = Vec::new();
    for (entity_id, item_type, quantity) in pending_item_grants {
        let Some(character_id) = resolve_character(entity_characters, &entity_id) else {
            tracing::warn!(
                entity_id,
                item_type,
                "plugin requested an item grant for an invalid entity id, an NPC \
                 (items are character-owned only), or an unknown entity"
            );
            continue;
        };
        match character_store
            .grant_item(character_id, &item_type, quantity)
            .await
        {
            Ok(new_quantity) => {
                if let Ok(parsed_entity_id) = entity_id.parse::<EntityId>() {
                    send_to(
                        global_sessions,
                        parsed_entity_id,
                        ServerMessage::ItemChanged {
                            item_type: item_type.clone(),
                            quantity: new_quantity,
                        },
                    );
                }
                acquired.push((entity_id, item_type, new_quantity))
            }
            Err(e) => {
                tracing::warn!(entity_id, item_type, error = %e, "plugin's grant-item failed")
            }
        }
    }

    for (entity_id, item_type, quantity) in pending_item_removals {
        let Some(character_id) = resolve_character(entity_characters, &entity_id) else {
            tracing::warn!(
                entity_id,
                item_type,
                "plugin requested an item removal for an invalid entity id, an NPC \
                 (items are character-owned only), or an unknown entity"
            );
            continue;
        };
        match character_store
            .remove_item(character_id, &item_type, quantity)
            .await
        {
            Ok(remaining) => {
                if let Ok(parsed_entity_id) = entity_id.parse::<EntityId>() {
                    send_to(
                        global_sessions,
                        parsed_entity_id,
                        ServerMessage::ItemChanged {
                            item_type: item_type.clone(),
                            quantity: remaining,
                        },
                    );
                }
            }
            Err(e) => {
                tracing::warn!(entity_id, item_type, error = %e, "plugin's remove-item failed");
            }
        }
    }

    for (entity_id, currency_key, delta) in pending_currency_deltas {
        let Some(character_id) = resolve_character(entity_characters, &entity_id) else {
            tracing::warn!(
                entity_id,
                currency_key,
                "plugin requested a currency delta for an invalid entity id, an NPC \
                 (currency is character-owned only), or an unknown entity"
            );
            continue;
        };
        if !currency_schema.is_declared(&currency_key) {
            tracing::warn!(
                entity_id,
                currency_key,
                "plugin requested a currency delta for an undeclared currency key \
                 (see currency.schema.yaml)"
            );
            continue;
        }
        match character_store
            .modify_currency(character_id, &currency_key, delta)
            .await
        {
            Ok(new_balance) => {
                if let Ok(parsed_entity_id) = entity_id.parse::<EntityId>() {
                    send_to(
                        global_sessions,
                        parsed_entity_id,
                        ServerMessage::CurrencyChanged {
                            currency_key: currency_key.clone(),
                            balance: new_balance,
                        },
                    );
                }
            }
            Err(e) => {
                tracing::warn!(entity_id, currency_key, error = %e, "plugin's modify-currency failed");
            }
        }
    }

    for (scope, key, value) in pending_state_writes {
        let result = match &scope {
            plugin_host::PluginStateScope::Character(entity_id) => {
                match resolve_character(entity_characters, entity_id) {
                    Some(character_id) => {
                        plugin_state_store
                            .set_character_state(character_id, &key, &value)
                            .await
                    }
                    None => {
                        tracing::warn!(
                            entity_id,
                            key,
                            "plugin requested a character-state write for an invalid \
                             entity id, an NPC, or an unknown entity"
                        );
                        continue;
                    }
                }
            }
            plugin_host::PluginStateScope::Zone(zone_id) => {
                plugin_state_store
                    .set_zone_state(zone_id, &key, &value)
                    .await
            }
            plugin_host::PluginStateScope::Entity(_) => {
                // Never queued (see `PluginCallbacks::plugin_state_set`)
                // — nothing to persist for transient entity scope.
                continue;
            }
        };
        if let Err(e) = result {
            tracing::warn!(key, error = %e, "plugin's plugin-state-set failed to persist");
        }
    }

    acquired
}

/// Spawns one NPC from a zone manifest's declared spawn table, at that
/// table's first point — used both to seed the zone from a plugin's
/// `on_load` requests before this actor starts (`main`) and from later
/// `on_message`/`on_chat_command`-triggered `spawn-npc` calls once the
/// plugin is running live on this task (#95). Returns the real
/// `(entity_id, entity_type)` on success — `None` on an unknown or empty
/// spawn table, already logged here, nothing further for the caller to
/// do. The caller (`spawn_requested_npcs`) uses the returned pair to fire
/// `on-entity-spawn` back to the requesting plugin (#214) — this
/// function itself has no plugin handle to call that with.
pub fn spawn_npc_from_table(zone: &mut Zone, spawn_table_id: &str) -> Option<(EntityId, String)> {
    let Some(table) = zone
        .manifest
        .spawn_tables
        .iter()
        .find(|table| table.id == spawn_table_id)
    else {
        tracing::warn!(spawn_table_id, "plugin requested an unknown spawn table");
        return None;
    };
    let Some(point) = table.points.first().copied() else {
        tracing::warn!(spawn_table_id, "plugin requested an empty spawn table");
        return None;
    };
    // `table.points` is manifest-declared, 2D — ground level (#249's
    // scope note: real per-spawn-table height is #242's job).
    let point: world::Point = (point.0, point.1, 0.0);
    let route_id = table.route_id.clone();
    let entity_type = table.entity_type.clone();

    let entity_id = EntityId::new();
    match &route_id {
        Some(route_id) => zone.spawn_npc_with_route(entity_id, point, route_id),
        None => zone.spawn(entity_id, EntityKind::Npc, point),
    }
    tracing::info!(%entity_id, spawn_table_id, ?route_id, "spawned NPC from plugin");
    Some((entity_id, entity_type))
}

/// Drains `runtime`'s pending `spawn-npc` requests, spawns each real NPC
/// into `zone` via `spawn_npc_from_table`, and fires `on-entity-spawn`
/// back to `runtime` for each one that actually spawned, carrying the
/// `spawn-table-id` that caused it — the correlation a plugin needs to
/// match a specific `spawn-npc` call it made to the resulting real entity
/// id, since `spawn-npc` itself can't synchronously return one (#214).
/// Shared by every call site that drains a plugin's pending spawns (the
/// `on-zone-loaded` startup seeding in `main`, and every later
/// `on-message`/`on-chat-command` dispatch below) so this firing behavior
/// is identical no matter which hook queued the request. Only fires back
/// to `runtime` itself — never a fan-out to every loaded plugin — since
/// only the plugin that made the request has any correlation token to
/// consume.
pub fn spawn_requested_npcs(runtime: &mut PluginRuntime, zone: &mut Zone, zone_id: &str) {
    let wants_entity_spawn = runtime.wants("on-entity-spawn");
    for spawn_table_id in runtime.drain_pending_spawns() {
        let Some((entity_id, entity_type)) = spawn_npc_from_table(zone, &spawn_table_id) else {
            continue;
        };
        if !wants_entity_spawn {
            continue;
        }
        let entity_id_str = entity_id.to_string();
        if let Err(e) =
            runtime
                .plugin
                .on_entity_spawn(zone_id, &entity_id_str, &entity_type, &spawn_table_id)
        {
            tracing::warn!(plugin = %runtime.name, %entity_id, spawn_table_id, error = %e, "plugin on_entity_spawn hook failed");
        }
    }
}
