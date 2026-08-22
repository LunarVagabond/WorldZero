//! Owns the phase-1 process's single `world::Zone` on its own task and
//! drives its fixed-rate tick loop — session tasks talk to it only
//! through `WorldHandle`'s command channel, never through a shared lock.
//! `world::Zone::run` (docs/PROPOSAL.md's tick-loop API, #31) is a
//! self-contained convenience for the simple "just run this zone" case;
//! `server` needs ticks interleaved with real command traffic (spawn,
//! move, despawn from connected sessions), so this reimplements the same
//! scheduling logic — fixed `dt`, log-and-resync on an overrun rather
//! than catching up — around `Zone::tick()`'s pure step instead.

use common::id::EntityId;
use tokio::sync::{mpsc, oneshot};
use tokio::time::Instant;
use world::{EntityKind, MovementOutcome, Point, Zone};

use crate::plugin_startup::PluginRuntime;

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
    on_tick: impl Fn(&Zone, Vec<(EntityId, MovementOutcome)>) + Send + 'static,
) -> WorldHandle {
    let (tx, mut rx) = mpsc::unbounded_channel::<WorldCommand>();

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
                        }
                    }
                }
            }
        }
    });

    WorldHandle { commands: tx }
}

/// Spawns one NPC from a zone manifest's declared spawn table, at that
/// table's first point — used both to seed the zone from a plugin's
/// `on_load` requests before this actor starts (`main`) and from later
/// `on_message`-triggered `spawn-npc` calls once the plugin is running
/// live on this task (#95).
pub fn spawn_npc_from_table(zone: &mut Zone, spawn_table_id: &str) {
    let Some(point) = zone
        .manifest
        .spawn_tables
        .iter()
        .find(|table| table.id == spawn_table_id)
        .and_then(|table| table.points.first().copied())
    else {
        tracing::warn!(
            spawn_table_id,
            "plugin requested an unknown or empty spawn table"
        );
        return;
    };

    let entity_id = EntityId::new();
    zone.spawn(entity_id, EntityKind::Npc, point);
    tracing::info!(%entity_id, spawn_table_id, "spawned NPC from plugin");
}
