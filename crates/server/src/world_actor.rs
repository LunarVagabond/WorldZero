//! Owns the phase-1 process's single `world::Zone` on its own task and
//! drives its fixed-rate tick loop — session tasks talk to it only
//! through `WorldHandle`'s command channel, never through a shared lock.
//! `world::Zone::run` (docs/PROPOSAL.md's tick-loop API, #31) is a
//! self-contained convenience for the simple "just run this zone" case;
//! `server` needs ticks interleaved with real command traffic (spawn,
//! move, despawn from connected sessions), so this reimplements the same
//! scheduling logic — fixed `dt`, log-and-resync on an overrun rather
//! than catching up — around `Zone::tick()`'s pure step instead.

use character::CharacterStore;
use common::id::EntityId;
use tokio::sync::{mpsc, oneshot};
use tokio::time::Instant;
use world::{EntityKind, MovementOutcome, Point, Zone};

use crate::plugin_startup::PluginRuntime;
use crate::session::EntityCharacters;

enum WorldCommand {
    Spawn {
        entity: EntityId,
        kind: EntityKind,
        position: Point,
    },
    Despawn {
        entity: EntityId,
    },
    RequestMove {
        entity: EntityId,
        to: Point,
    },
    PositionOf {
        entity: EntityId,
        reply: oneshot::Sender<Option<Point>>,
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
}

#[derive(Clone)]
pub struct WorldHandle {
    commands: mpsc::UnboundedSender<WorldCommand>,
}

impl WorldHandle {
    pub fn spawn(&self, entity: EntityId, kind: EntityKind, position: Point) {
        let _ = self.commands.send(WorldCommand::Spawn {
            entity,
            kind,
            position,
        });
    }

    pub fn despawn(&self, entity: EntityId) {
        let _ = self.commands.send(WorldCommand::Despawn { entity });
    }

    pub fn request_move(&self, entity: EntityId, to: Point) {
        let _ = self.commands.send(WorldCommand::RequestMove { entity, to });
    }

    /// `None` both when the entity isn't spawned and when the actor task
    /// is gone — a caller persisting a last-known position on disconnect
    /// treats both the same way (nothing to persist).
    pub async fn position_of(&self, entity: EntityId) -> Option<Point> {
        let (reply_tx, reply_rx) = oneshot::channel();
        if self
            .commands
            .send(WorldCommand::PositionOf {
                entity,
                reply: reply_tx,
            })
            .is_err()
        {
            return None;
        }
        reply_rx.await.ok().flatten()
    }

    /// Empty (not an error) if the actor task is gone.
    pub async fn entities_snapshot(&self) -> Vec<(EntityId, EntityKind, Point)> {
        let (reply_tx, reply_rx) = oneshot::channel();
        if self
            .commands
            .send(WorldCommand::EntitiesSnapshot { reply: reply_tx })
            .is_err()
        {
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
        let _ = self.commands.send(WorldCommand::PluginMessage {
            message_type,
            sender_entity_id,
            payload,
        });
    }

    /// Fire-and-forget, same contract as `dispatch_plugin_message` — the
    /// caller (`session`) has already matched `command` against the
    /// plugin's declared `chat_commands` before calling this.
    pub fn dispatch_chat_command(&self, command: String, args: String, sender_entity_id: EntityId) {
        let _ = self.commands.send(WorldCommand::ChatCommand {
            command,
            args,
            sender_entity_id,
        });
    }
}

/// Spawns the actor task and returns a handle to it. `on_tick` runs once
/// per tick with that tick's movement outcomes — broadcasting
/// `Moved`/`Rejected` to connected sessions is the caller's job
/// (`crate::session`); this only drives the simulation.
///
/// `plugin`, if configured, is moved onto this task and kept alive for
/// its whole lifetime — matching docs/specs/Plugin_API.md's "instantiated
/// for a zone-service" (#95). A `PluginMessage` command only reaches
/// `on_message` if its `message_type` is one the plugin actually declared
/// in `plugin.toml`; anything else is logged and dropped rather than
/// treated as an error, since an unroutable message type is a client or
/// config mistake, not something that should disrupt the actor.
pub fn spawn_world_actor(
    mut zone: Zone,
    tick_interval: std::time::Duration,
    mut plugin: Option<PluginRuntime>,
    character_store: std::sync::Arc<CharacterStore>,
    entity_characters: EntityCharacters,
    on_tick: impl Fn(&Zone, Vec<(EntityId, MovementOutcome)>) + Send + 'static,
) -> WorldHandle {
    let (tx, mut rx) = mpsc::unbounded_channel::<WorldCommand>();
    let dt = tick_interval.as_secs_f64();

    tokio::spawn(async move {
        let mut next_tick_at = Instant::now() + tick_interval;

        loop {
            tokio::select! {
                _ = tokio::time::sleep_until(next_tick_at) => {
                    if Instant::now() > next_tick_at + tick_interval {
                        tracing::warn!("world actor tick running behind schedule — resyncing rather than catching up");
                    }

                    let outcomes = zone.tick();
                    on_tick(&zone, outcomes);

                    if let Some(runtime) = plugin.as_mut() {
                        // The host never moves an NPC itself — it hands the
                        // plugin the NPC's position and full route data and
                        // waits for `move-entity` calls back (#57,
                        // wit/plugin.wit's `on-npc-tick` doc comment).
                        for (entity, position, route) in zone.npcs_with_routes() {
                            let entity_str = entity.to_string();
                            if let Err(e) = runtime.plugin.on_npc_tick(
                                &entity_str,
                                position.0,
                                position.1,
                                &route.waypoints,
                                route.is_loop,
                                route.speed,
                                dt,
                            ) {
                                tracing::warn!(%entity, error = %e, "plugin on_npc_tick hook failed");
                            }
                        }
                        let moves = runtime.drain_pending_moves();
                        let stat_deltas = runtime.drain_pending_stat_deltas();
                        let item_grants = runtime.drain_pending_item_grants();
                        let item_removals = runtime.drain_pending_item_removals();
                        let currency_deltas = runtime.drain_pending_currency_deltas();
                        let acquired = apply_plugin_pending_effects(
                            &mut zone, moves, stat_deltas, item_grants, item_removals,
                            currency_deltas, &character_store, &entity_characters,
                        ).await;
                        for (entity_id, item_type, new_quantity) in acquired {
                            if let Err(e) = runtime.plugin.on_item_acquire(&entity_id, &item_type, new_quantity) {
                                tracing::warn!(entity_id, item_type, error = %e, "plugin on_item_acquire hook failed");
                            }
                        }
                    }

                    next_tick_at += tick_interval;
                    let now = Instant::now();
                    if now > next_tick_at {
                        next_tick_at = now + tick_interval;
                    }
                }
                Some(command) = rx.recv() => {
                    match command {
                        WorldCommand::Spawn { entity, kind, position } => zone.spawn(entity, kind, position),
                        WorldCommand::Despawn { entity } => zone.despawn(entity),
                        WorldCommand::RequestMove { entity, to } => zone.request_move(entity, to),
                        WorldCommand::PositionOf { entity, reply } => {
                            let _ = reply.send(zone.position_of(entity));
                        }
                        WorldCommand::EntitiesSnapshot { reply } => {
                            let _ = reply.send(zone.entities());
                        }
                        WorldCommand::PluginMessage { message_type, sender_entity_id, payload } => {
                            let Some(runtime) = plugin.as_mut() else {
                                tracing::warn!(message_type, "received a plugin message but no plugin is configured");
                                continue;
                            };
                            if !runtime.message_types.contains(&message_type) {
                                tracing::warn!(message_type, "received a message_type the configured plugin didn't declare");
                                continue;
                            }
                            let sender_entity_id = sender_entity_id.to_string();
                            if let Err(e) = runtime.plugin.on_message(message_type, &sender_entity_id, &payload) {
                                tracing::warn!(message_type, error = %e, "plugin on_message hook failed");
                            }
                            for spawn_table_id in runtime.drain_pending_spawns() {
                                spawn_npc_from_table(&mut zone, &spawn_table_id);
                            }
                            let moves = runtime.drain_pending_moves();
                            let stat_deltas = runtime.drain_pending_stat_deltas();
                            let item_grants = runtime.drain_pending_item_grants();
                            let item_removals = runtime.drain_pending_item_removals();
                            let currency_deltas = runtime.drain_pending_currency_deltas();
                            let acquired = apply_plugin_pending_effects(
                                &mut zone, moves, stat_deltas, item_grants, item_removals,
                                currency_deltas, &character_store, &entity_characters,
                            ).await;
                            for (entity_id, item_type, new_quantity) in acquired {
                                if let Err(e) = runtime.plugin.on_item_acquire(&entity_id, &item_type, new_quantity) {
                                    tracing::warn!(entity_id, item_type, error = %e, "plugin on_item_acquire hook failed");
                                }
                            }
                        }
                        WorldCommand::ChatCommand { command, args, sender_entity_id } => {
                            let Some(runtime) = plugin.as_mut() else {
                                tracing::warn!(command, "received a chat command but no plugin is configured");
                                continue;
                            };
                            if !runtime.chat_commands.contains(&command) {
                                tracing::warn!(command, "received a chat command the configured plugin didn't declare");
                                continue;
                            }
                            let sender_entity_id_str = sender_entity_id.to_string();
                            if let Err(e) = runtime.plugin.on_chat_command(&command, &args, &sender_entity_id_str) {
                                tracing::warn!(command, error = %e, "plugin on_chat_command hook failed");
                            }
                            let moves = runtime.drain_pending_moves();
                            let stat_deltas = runtime.drain_pending_stat_deltas();
                            let item_grants = runtime.drain_pending_item_grants();
                            let item_removals = runtime.drain_pending_item_removals();
                            let currency_deltas = runtime.drain_pending_currency_deltas();
                            let acquired = apply_plugin_pending_effects(
                                &mut zone, moves, stat_deltas, item_grants, item_removals,
                                currency_deltas, &character_store, &entity_characters,
                            ).await;
                            for (entity_id, item_type, new_quantity) in acquired {
                                if let Err(e) = runtime.plugin.on_item_acquire(&entity_id, &item_type, new_quantity) {
                                    tracing::warn!(entity_id, item_type, error = %e, "plugin on_item_acquire hook failed");
                                }
                            }
                        }
                    }
                }
            }
        }
    });

    WorldHandle { commands: tx }
}

/// Resolves a plugin-supplied entity-id string to a real `EntityId` and,
/// via `entity_characters`, the `CharacterId` it belongs to — shared by
/// every pending-effect kind that needs character-backed storage
/// (stats, items, currency). `None` covers both "not a valid entity id"
/// and "no character for this entity" (an NPC — no NPC-backed storage
/// exists yet — or an unknown/disconnected entity); the caller logs
/// whichever it actually was.
fn resolve_character(
    entity_characters: &EntityCharacters,
    entity_id: &str,
) -> Option<common::id::CharacterId> {
    let entity_id: EntityId = entity_id.parse().ok()?;
    entity_characters.lock().unwrap().get(&entity_id).copied()
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
    pending_currency_deltas: Vec<(String, i64)>,
    character_store: &CharacterStore,
    entity_characters: &EntityCharacters,
) -> Vec<(String, String, i64)> {
    for (entity_id, x, y) in pending_moves {
        match entity_id.parse::<EntityId>() {
            Ok(entity_id) => zone.request_move(entity_id, (x, y)),
            Err(_) => {
                tracing::warn!(
                    entity_id,
                    "plugin requested a move for an invalid entity id"
                )
            }
        }
    }

    for (entity_id, stat_key, delta) in pending_stat_deltas {
        let Some(character_id) = resolve_character(entity_characters, &entity_id) else {
            tracing::warn!(
                entity_id,
                "plugin requested a stat delta for an invalid entity id, an NPC \
                 (no NPC stat storage exists yet), or an unknown entity"
            );
            continue;
        };
        if let Err(e) = character_store
            .apply_stat_delta(character_id, &stat_key, delta)
            .await
        {
            tracing::warn!(entity_id, stat_key, error = %e, "plugin's apply-stat-delta failed");
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
            Ok(new_quantity) => acquired.push((entity_id, item_type, new_quantity)),
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
        if let Err(e) = character_store
            .remove_item(character_id, &item_type, quantity)
            .await
        {
            tracing::warn!(entity_id, item_type, error = %e, "plugin's remove-item failed");
        }
    }

    for (entity_id, delta) in pending_currency_deltas {
        let Some(character_id) = resolve_character(entity_characters, &entity_id) else {
            tracing::warn!(
                entity_id,
                "plugin requested a currency delta for an invalid entity id, an NPC \
                 (currency is character-owned only), or an unknown entity"
            );
            continue;
        };
        if let Err(e) = character_store.modify_currency(character_id, delta).await {
            tracing::warn!(entity_id, error = %e, "plugin's modify-currency failed");
        }
    }

    acquired
}

/// Spawns one NPC from a zone manifest's declared spawn table, at that
/// table's first point — used both to seed the zone from a plugin's
/// `on_load` requests before this actor starts (`main`) and from later
/// `on_message`-triggered `spawn-npc` calls once the plugin is running
/// live on this task (#95).
pub fn spawn_npc_from_table(zone: &mut Zone, spawn_table_id: &str) {
    let Some(table) = zone
        .manifest
        .spawn_tables
        .iter()
        .find(|table| table.id == spawn_table_id)
    else {
        tracing::warn!(spawn_table_id, "plugin requested an unknown spawn table");
        return;
    };
    let Some(point) = table.points.first().copied() else {
        tracing::warn!(spawn_table_id, "plugin requested an empty spawn table");
        return;
    };
    let route_id = table.route_id.clone();

    let entity_id = EntityId::new();
    match &route_id {
        Some(route_id) => zone.spawn_npc_with_route(entity_id, point, route_id),
        None => zone.spawn(entity_id, EntityKind::Npc, point),
    }
    tracing::info!(%entity_id, spawn_table_id, ?route_id, "spawned NPC from plugin");
}
