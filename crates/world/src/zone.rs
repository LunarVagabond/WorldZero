//! The single-zone simulation tick loop (docs/PROPOSAL.md, "Phased
//! Roadmap," Phase 1: "one zone-service instance running one map") —
//! everything else in Phase 1 (movement validation, NPC behavior, spawn
//! tables) runs inside this loop's heartbeat.
//!
//! Tick rate: a fixed 20 Hz (`WorldConfig::default`, `crate::config`) —
//! chosen and documented there, not left implicit, per #31's acceptance
//! criteria.

use std::collections::HashMap;

use common::id::EntityId;
use content::manifest::ZoneManifest;
use tokio::time::Instant;

use content::manifest::Route;

use crate::config::WorldConfig;
use crate::links::crossed_link;
use crate::movement::{MovementRejection, validate_movement};
use crate::spatial::{GridIndex, Point, SpatialIndex};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntityKind {
    Player,
    Npc,
}

/// One simulated zone: its loaded manifest, the entities currently in
/// it, and the spatial index tracking their positions. Owns exactly one
/// `SpatialIndex` — macro partitioning across zones is just "one `Zone`
/// per zone-service instance," not something this type itself models
/// (docs/PROPOSAL.md, "Spatial Index: A → Z Roadmap").
pub struct Zone {
    pub manifest: ZoneManifest,
    config: WorldConfig,
    index: Box<dyn SpatialIndex>,
    entities: HashMap<EntityId, EntityKind>,
    /// Which declared route (`content::manifest::Route::id`) an NPC
    /// entity was spawned against, if its spawn table declared one
    /// (#57's `on-npc-tick` — the host hands the plugin this route's
    /// data every tick, it never drives NPC movement on its own).
    /// Player entities are never present here.
    npc_routes: HashMap<EntityId, String>,
    pending_moves: Vec<(EntityId, Point)>,
}

/// What actually happened to a queued movement request — surfaced to the
/// caller (e.g. to tell a rejected client why) rather than silently dropped.
#[derive(Debug, Clone, PartialEq)]
pub enum MovementOutcome {
    Applied,
    Rejected(MovementRejection),
    /// The move crossed a manifest-declared `content::manifest::Link`
    /// edge (#45) — the mover is leaving this zone for `target_zone`,
    /// never a normal in-zone move. `Zone::tick` has already despawned
    /// the entity from this zone by the time this outcome is returned
    /// (same as if the caller had called `despawn` itself) — the caller
    /// (`server::world_actor`/`server::session`) is responsible for
    /// spawning the entity into `target_zone`'s own `Zone`/`WorldHandle`;
    /// `world` has no notion of "the other zone" to do that itself (one
    /// `Zone` per zone-service instance, per this crate's own doc comment).
    ZoneTransition {
        target_zone: String,
    },
}

impl Zone {
    pub fn new(manifest: ZoneManifest, config: WorldConfig) -> Self {
        Self {
            index: Box::new(GridIndex::new(config.grid_cell_size_meters)),
            manifest,
            config,
            entities: HashMap::new(),
            npc_routes: HashMap::new(),
            pending_moves: Vec::new(),
        }
    }

    pub fn spawn(&mut self, entity: EntityId, kind: EntityKind, position: Point) {
        self.entities.insert(entity, kind);
        self.index.insert(entity, position);
    }

    /// Same as [`Self::spawn`], plus recording which declared route this
    /// NPC should tick against — used for a spawn-table entry with a
    /// `route_id` (`content::manifest::SpawnTable`). `route_id` must
    /// name a route in this zone's manifest; a caller passing an unknown
    /// id gets no route recorded (not a panic — `spawn_npc_from_table`
    /// callers already validate spawn-table data against the loaded
    /// manifest before this point).
    pub fn spawn_npc_with_route(&mut self, entity: EntityId, position: Point, route_id: &str) {
        self.spawn(entity, EntityKind::Npc, position);
        if self.manifest.routes.iter().any(|r| r.id == route_id) {
            self.npc_routes.insert(entity, route_id.to_string());
        }
    }

    pub fn despawn(&mut self, entity: EntityId) {
        self.entities.remove(&entity);
        self.npc_routes.remove(&entity);
        self.index.remove(entity);
    }

    /// Every currently-spawned NPC that has a route assigned, alongside
    /// that route's full declared data — what `on-npc-tick` (#57) is
    /// called with once per tick. Returns owned `Route` clones (routes
    /// are small, tick-rate-cloned data, not a hot allocation path) so a
    /// caller can hold the result across a later `&mut self` call (e.g.
    /// `request_move`) without fighting the borrow checker.
    pub fn npcs_with_routes(&self) -> Vec<(EntityId, Point, Route)> {
        self.npc_routes
            .iter()
            .filter_map(|(&entity, route_id)| {
                let position = self.index.position_of(entity)?;
                let route = self.manifest.routes.iter().find(|r| &r.id == route_id)?;
                Some((entity, position, route.clone()))
            })
            .collect()
    }

    pub fn position_of(&self, entity: EntityId) -> Option<Point> {
        self.index.position_of(entity)
    }

    /// Every currently-spawned entity, its kind, and its position — the
    /// "here's who's already here" roster a newly-joining client needs
    /// (a pre-spawned NPC otherwise has no way to become visible to a
    /// client that joins after it spawned).
    pub fn entities(&self) -> Vec<(EntityId, EntityKind, Point)> {
        self.entities
            .iter()
            .filter_map(|(&id, &kind)| {
                self.index
                    .position_of(id)
                    .map(|position| (id, kind, position))
            })
            .collect()
    }

    /// Queues a movement request for the next `tick` — movement is never
    /// applied immediately on receipt, only as part of the fixed-rate
    /// simulation step, so every accepted move is validated against a
    /// consistent, known `dt`.
    pub fn request_move(&mut self, entity: EntityId, to: Point) {
        self.pending_moves.push((entity, to));
    }

    /// Advances the simulation by exactly one tick at the configured
    /// fixed rate. Pure and synchronous — the async wall-clock scheduling
    /// lives in [`Self::run`], kept separate so tick logic itself is
    /// trivially unit-testable without a real clock.
    pub fn tick(&mut self) -> Vec<(EntityId, MovementOutcome)> {
        let dt = self.config.tick_interval().as_secs_f64();
        let moves = std::mem::take(&mut self.pending_moves);

        let mut outcomes = Vec::with_capacity(moves.len());
        for (entity, to) in moves {
            let Some(from) = self.index.position_of(entity) else {
                // Not currently spawned in this zone (e.g. despawned
                // between the request and this tick) — nothing to move.
                continue;
            };

            // Only player entities transition zones (#45) — an NPC has
            // no connected session to hand off, and NPC movement is
            // entirely plugin-driven within the zone it was spawned in;
            // an NPC whose route happens to cross a link edge is left to
            // ordinary in-bounds movement validation below, same as any
            // other move.
            if self.entities.get(&entity) == Some(&EntityKind::Player)
                && let Some(link) = crossed_link(&self.manifest.links, from, to)
            {
                // A transition skips `validate_movement`'s bounds check
                // on purpose (`to` is meant to fall outside this zone's
                // polygon — that's what makes it a transition), but the
                // speed cap still applies: without this, "the move
                // crosses a link edge" would be a free pass to claim any
                // distance at all, instantly.
                let max_allowed = self.config.max_speed_meters_per_second * dt;
                let attempted_distance = crate::movement::distance(from, to);
                if attempted_distance > max_allowed {
                    outcomes.push((
                        entity,
                        MovementOutcome::Rejected(MovementRejection::TooFast {
                            attempted_distance,
                            max_allowed,
                        }),
                    ));
                    continue;
                }

                let target_zone = link.target_zone.clone();
                self.despawn(entity);
                outcomes.push((entity, MovementOutcome::ZoneTransition { target_zone }));
                continue;
            }

            match validate_movement(
                &self.manifest,
                self.index.as_ref(),
                entity,
                self.config.max_speed_meters_per_second,
                dt,
                from,
                to,
            ) {
                Ok(()) => {
                    self.index.update(entity, to);
                    outcomes.push((entity, MovementOutcome::Applied));
                }
                Err(rejection) => {
                    outcomes.push((entity, MovementOutcome::Rejected(rejection)));
                }
            }
        }

        outcomes
    }

    /// Runs the fixed-rate tick loop forever, calling `on_tick` once per
    /// tick with this zone and the fixed `dt` — the call site #31
    /// requires for the plugin host's `on_tick(zone, dt)` hook, kept as a
    /// plain callback rather than a direct dependency on `plugin-host` so
    /// `world` doesn't need to know that crate exists; `server` is what
    /// wires a real plugin-host callback in here.
    ///
    /// A tick that runs long doesn't try to catch up with back-to-back
    /// ticks (that would flood, not fix, an already-overloaded server) —
    /// it logs a `WARN` (docs/specs/Observability_Spec.md's severity
    /// policy: oncall-worthy conditions are `ERROR`, this can wait until
    /// morning) and resyncs the schedule from "now," at the cost of a
    /// slower *perceived* tick rate under load. `dt` handed to `on_tick`
    /// and movement validation is always the fixed configured interval,
    /// never the actual elapsed wall-clock time — the simulation clock
    /// itself never drifts, only the loop's real-world cadence does.
    pub async fn run(
        &mut self,
        mut on_tick: impl FnMut(&mut Zone, f64, Vec<(EntityId, MovementOutcome)>),
    ) -> ! {
        let interval = self.config.tick_interval();
        let dt = interval.as_secs_f64();
        let mut next_tick_at = Instant::now() + interval;

        loop {
            tokio::time::sleep_until(next_tick_at).await;

            if Instant::now() > next_tick_at + interval {
                tracing::warn!(
                    tick_rate_hz = self.config.tick_rate_hz,
                    "zone tick running behind schedule — resyncing rather than catching up"
                );
            }

            let outcomes = self.tick();
            on_tick(self, dt, outcomes);

            next_tick_at += interval;
            let now = Instant::now();
            if now > next_tick_at {
                next_tick_at = now + interval;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn zone_with_square_bounds() -> Zone {
        let manifest = ZoneManifest::from_yaml(
            r#"
schema_version: 1
id: test-zone
display_name: "Test Zone"

bounds:
  shape: polygon
  coordinate_system: { units: meters, origin: [0, 0] }
  points: [[0,0], [100,0], [100,100], [0,100]]

collision:
  asset_ref: "sha256:9f2ac1b3e4d5c6a7b8c9d0e1f2a3b4c5d6e7f8a9b0c1d2e3f4a5b6c7d8e9f0a1"
  format: navmesh_v1
"#,
        )
        .unwrap();
        Zone::new(manifest, WorldConfig::default())
    }

    fn zone_with_a_route() -> Zone {
        let manifest = ZoneManifest::from_yaml(
            r#"
schema_version: 1
id: test-zone
display_name: "Test Zone"

bounds:
  shape: polygon
  coordinate_system: { units: meters, origin: [0, 0] }
  points: [[0,0], [100,0], [100,100], [0,100]]

collision:
  asset_ref: "sha256:9f2ac1b3e4d5c6a7b8c9d0e1f2a3b4c5d6e7f8a9b0c1d2e3f4a5b6c7d8e9f0a1"
  format: navmesh_v1

routes:
  - id: patrol-01
    waypoints: [[10,10], [20,20], [30,10]]
    loop: true
    speed: 2.0
"#,
        )
        .unwrap();
        Zone::new(manifest, WorldConfig::default())
    }

    #[test]
    fn a_spawned_npc_with_a_known_route_is_returned_by_npcs_with_routes() {
        let mut zone = zone_with_a_route();
        let entity = EntityId::new();
        zone.spawn_npc_with_route(entity, (10.0, 10.0), "patrol-01");

        let routes = zone.npcs_with_routes();
        assert_eq!(routes.len(), 1);
        assert_eq!(routes[0].0, entity);
        assert_eq!(routes[0].1, (10.0, 10.0));
        assert_eq!(routes[0].2.id, "patrol-01");
        assert_eq!(routes[0].2.waypoints.len(), 3);
    }

    #[test]
    fn spawn_npc_with_an_unknown_route_id_records_no_route() {
        let mut zone = zone_with_a_route();
        let entity = EntityId::new();
        zone.spawn_npc_with_route(entity, (10.0, 10.0), "does-not-exist");

        assert!(zone.npcs_with_routes().is_empty());
    }

    #[test]
    fn despawning_an_npc_removes_it_from_npcs_with_routes() {
        let mut zone = zone_with_a_route();
        let entity = EntityId::new();
        zone.spawn_npc_with_route(entity, (10.0, 10.0), "patrol-01");
        zone.despawn(entity);

        assert!(zone.npcs_with_routes().is_empty());
    }

    #[test]
    fn a_reasonable_queued_move_is_applied_on_tick() {
        let mut zone = zone_with_square_bounds();
        let entity = EntityId::new();
        zone.spawn(entity, EntityKind::Player, (50.0, 50.0));

        zone.request_move(entity, (50.1, 50.1));
        let outcomes = zone.tick();

        assert_eq!(outcomes, vec![(entity, MovementOutcome::Applied)]);
        assert_eq!(zone.position_of(entity), Some((50.1, 50.1)));
    }

    #[test]
    fn an_out_of_bounds_move_is_rejected_and_position_is_unchanged() {
        // A generous speed cap so this test isolates the boundary check —
        // the speed cap itself is covered by `movement`'s own tests.
        let manifest = zone_with_square_bounds().manifest;
        let mut zone = Zone::new(
            manifest,
            WorldConfig {
                max_speed_meters_per_second: 10_000.0,
                ..WorldConfig::default()
            },
        );
        let entity = EntityId::new();
        zone.spawn(entity, EntityKind::Player, (99.0, 50.0));

        zone.request_move(entity, (500.0, 50.0));
        let outcomes = zone.tick();

        assert!(matches!(
            outcomes[0],
            (e, MovementOutcome::Rejected(MovementRejection::OutOfBounds)) if e == entity
        ));
        assert_eq!(zone.position_of(entity), Some((99.0, 50.0)));
    }

    #[test]
    fn a_move_for_a_despawned_entity_is_silently_skipped() {
        let mut zone = zone_with_square_bounds();
        let entity = EntityId::new();
        zone.spawn(entity, EntityKind::Player, (50.0, 50.0));
        zone.request_move(entity, (50.1, 50.1));
        zone.despawn(entity);

        let outcomes = zone.tick();
        assert!(outcomes.is_empty());
    }

    #[test]
    fn tick_clears_the_pending_queue_even_with_no_entities() {
        let mut zone = zone_with_square_bounds();
        zone.request_move(EntityId::new(), (1.0, 1.0));
        assert!(zone.tick().is_empty());
        // A second tick with nothing newly queued does nothing further.
        assert!(zone.tick().is_empty());
    }

    #[test]
    fn entities_lists_every_spawned_entity_with_its_current_position() {
        let mut zone = zone_with_square_bounds();
        let player = EntityId::new();
        let npc = EntityId::new();
        zone.spawn(player, EntityKind::Player, (1.0, 1.0));
        zone.spawn(npc, EntityKind::Npc, (2.0, 2.0));

        let mut entities = zone.entities();
        entities.sort_by_key(|(id, ..)| *id);
        let mut expected = vec![
            (player, EntityKind::Player, (1.0, 1.0)),
            (npc, EntityKind::Npc, (2.0, 2.0)),
        ];
        expected.sort_by_key(|(id, ..)| *id);
        assert_eq!(entities, expected);
    }

    #[test]
    fn a_player_move_crossing_a_link_edge_transitions_and_despawns_locally() {
        let manifest = ZoneManifest::from_yaml(
            r#"
schema_version: 1
id: test-zone
display_name: "Test Zone"

bounds:
  shape: polygon
  coordinate_system: { units: meters, origin: [0, 0] }
  points: [[0,0], [100,0], [100,100], [0,100]]

collision:
  asset_ref: "sha256:9f2ac1b3e4d5c6a7b8c9d0e1f2a3b4c5d6e7f8a9b0c1d2e3f4a5b6c7d8e9f0a1"
  format: navmesh_v1

links:
  - target_zone: next-zone
    edge: [[50,0], [50,100]]
    bidirectional: true
"#,
        )
        .unwrap();
        let mut zone = Zone::new(
            manifest,
            WorldConfig {
                max_speed_meters_per_second: 10_000.0,
                ..WorldConfig::default()
            },
        );
        let entity = EntityId::new();
        zone.spawn(entity, EntityKind::Player, (49.0, 50.0));

        zone.request_move(entity, (51.0, 50.0));
        let outcomes = zone.tick();

        assert_eq!(
            outcomes,
            vec![(
                entity,
                MovementOutcome::ZoneTransition {
                    target_zone: "next-zone".to_string()
                }
            )]
        );
        assert_eq!(zone.position_of(entity), None);
        assert!(zone.entities().is_empty());
    }

    #[test]
    fn a_speed_hack_cannot_hide_behind_a_zone_transition() {
        let manifest = ZoneManifest::from_yaml(
            r#"
schema_version: 1
id: test-zone
display_name: "Test Zone"

bounds:
  shape: polygon
  coordinate_system: { units: meters, origin: [0, 0] }
  points: [[0,0], [1000,0], [1000,1000], [0,1000]]

collision:
  asset_ref: "sha256:9f2ac1b3e4d5c6a7b8c9d0e1f2a3b4c5d6e7f8a9b0c1d2e3f4a5b6c7d8e9f0a1"
  format: navmesh_v1

links:
  - target_zone: next-zone
    edge: [[500,0], [500,1000]]
    bidirectional: true
"#,
        )
        .unwrap();
        // The default speed cap (10 m/s at a 50ms tick allows ~0.5m) —
        // this "move" crosses the link edge, but claims to cover 450m in
        // one tick.
        let mut zone = Zone::new(manifest, WorldConfig::default());
        let entity = EntityId::new();
        zone.spawn(entity, EntityKind::Player, (49.0, 50.0));

        zone.request_move(entity, (500.1, 50.0));
        let outcomes = zone.tick();

        assert!(
            matches!(
                outcomes.as_slice(),
                [(e, MovementOutcome::Rejected(MovementRejection::TooFast { .. }))] if *e == entity
            ),
            "{outcomes:?}"
        );
        // Rejected, not transitioned — still spawned in this zone, at
        // its original position.
        assert_eq!(zone.position_of(entity), Some((49.0, 50.0)));
    }

    #[test]
    fn an_npc_move_crossing_a_link_edge_is_validated_normally_not_transitioned() {
        let manifest = ZoneManifest::from_yaml(
            r#"
schema_version: 1
id: test-zone
display_name: "Test Zone"

bounds:
  shape: polygon
  coordinate_system: { units: meters, origin: [0, 0] }
  points: [[0,0], [100,0], [100,100], [0,100]]

collision:
  asset_ref: "sha256:9f2ac1b3e4d5c6a7b8c9d0e1f2a3b4c5d6e7f8a9b0c1d2e3f4a5b6c7d8e9f0a1"
  format: navmesh_v1

links:
  - target_zone: next-zone
    edge: [[50,0], [50,100]]
    bidirectional: true
"#,
        )
        .unwrap();
        let mut zone = Zone::new(
            manifest,
            WorldConfig {
                max_speed_meters_per_second: 10_000.0,
                ..WorldConfig::default()
            },
        );
        let entity = EntityId::new();
        zone.spawn(entity, EntityKind::Npc, (49.0, 50.0));

        zone.request_move(entity, (51.0, 50.0));
        let outcomes = zone.tick();

        assert_eq!(outcomes, vec![(entity, MovementOutcome::Applied)]);
        assert_eq!(zone.position_of(entity), Some((51.0, 50.0)));
    }

    #[test]
    fn entities_excludes_a_despawned_entity() {
        let mut zone = zone_with_square_bounds();
        let entity = EntityId::new();
        zone.spawn(entity, EntityKind::Player, (1.0, 1.0));
        zone.despawn(entity);

        assert!(zone.entities().is_empty());
    }
}
