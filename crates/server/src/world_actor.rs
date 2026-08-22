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
}

/// Spawns the actor task and returns a handle to it. `on_tick` runs once
/// per tick with that tick's movement outcomes — broadcasting
/// `Moved`/`Rejected` to connected sessions is the caller's job
/// (`crate::session`); this only drives the simulation.
pub fn spawn_world_actor(
    mut zone: Zone,
    tick_interval: std::time::Duration,
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
                    }
                }
            }
        }
    });

    WorldHandle { commands: tx }
}
