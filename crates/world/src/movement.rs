//! Authoritative movement validation against the zone's loaded manifest
//! geometry (docs/PROPOSAL.md, Design Principle #2: "The server owns
//! simulation and truth; it does not own art") — a client-reported
//! position update is checked here before the server ever accepts it.
//! This is what actually stops a malicious-but-legitimately-connected
//! client from cheating; DTLS (`gateway::udp`) only protects the wire,
//! not the truthfulness of what's on it (docs/specs/Networking_Spec.md).

use common::id::EntityId;
use content::manifest::ZoneManifest;

use crate::spatial::{Point, SpatialIndex};

/// A tight, fixed collision radius for "don't let two entities occupy
/// the same point" broad-phase checking — not a per-entity hitbox system
/// (no such data exists in the manifest format yet), just enough to give
/// `SpatialIndex::query_radius` a real, meaningful role here rather than
/// only being used for boundary math it isn't needed for.
const COLLISION_RADIUS_METERS: f64 = 0.5;

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum MovementRejection {
    /// The destination falls outside the zone's manifest boundary polygon.
    OutOfBounds,
    /// The requested distance exceeds what's reachable in `dt` at the
    /// configured max speed — a spoofed "teleport" update, not necessarily
    /// malice (could also be a client clock hiccup), but rejected either way.
    TooFast {
        attempted_distance: f64,
        max_allowed: f64,
    },
    /// Another entity already occupies the destination.
    Blocked { blocking_entity: EntityId },
}

/// Rejects (doesn't panic, doesn't propagate as an `Err` up through the
/// tick loop) a movement update that fails any check — a rejected move is
/// an expected, routine outcome, not a caller-visible error condition,
/// per #33's acceptance criteria.
pub fn validate_movement(
    manifest: &ZoneManifest,
    index: &dyn SpatialIndex,
    mover: EntityId,
    max_speed_meters_per_second: f64,
    dt_seconds: f64,
    from: Point,
    to: Point,
) -> Result<(), MovementRejection> {
    let max_allowed = max_speed_meters_per_second * dt_seconds;
    let attempted_distance = distance(from, to);
    if attempted_distance > max_allowed {
        return Err(MovementRejection::TooFast {
            attempted_distance,
            max_allowed,
        });
    }

    // Zone bounds are 2D-only, deliberately (#242 owns real 3D zone
    // geometry) — project `to` down to its (x, y) footprint before
    // checking it against the manifest's flat polygon. In effect: no
    // vertical limit within a zone's footprint yet.
    if !point_in_polygon((to.0, to.1), &manifest.bounds.points) {
        return Err(MovementRejection::OutOfBounds);
    }

    // Broad-phase collision via the spatial index's own range query
    // rather than a separate ad hoc entity scan (#32/#33).
    for other in index.query_radius(to, COLLISION_RADIUS_METERS) {
        if other != mover {
            return Err(MovementRejection::Blocked {
                blocking_entity: other,
            });
        }
    }

    Ok(())
}

/// `pub(crate)`, not private — `zone::Zone::tick` needs this to
/// speed-check a zone-transitioning move too (#45): a move that crosses
/// a link edge skips `validate_movement`'s bounds check entirely (it's
/// leaving this zone's bounds on purpose), but must not skip the speed
/// cap, or claiming to cross a link edge would become a free "move any
/// distance, instantly" exploit.
pub(crate) fn distance(a: Point, b: Point) -> f64 {
    let dx = a.0 - b.0;
    let dy = a.1 - b.1;
    let dz = a.2 - b.2;
    (dx * dx + dy * dy + dz * dz).sqrt()
}

/// Standard ray-casting point-in-polygon test: count crossings of a
/// horizontal ray cast from `point` to +infinity against the polygon's
/// edges; odd crossing count means inside. `content::manifest::ZoneManifest`
/// already requires `bounds.points.len() >= 3` and `bounds.shape ==
/// "polygon"` at load time, so this doesn't re-validate the shape here.
///
/// Takes `content::manifest::Point` (2D), not `crate::spatial::Point` —
/// zone bounds stay flat/2D deliberately (see `validate_movement`'s call
/// site), so this operates on the manifest's own point type directly
/// rather than the 3D simulation `Point`.
fn point_in_polygon(point: content::manifest::Point, polygon: &[content::manifest::Point]) -> bool {
    let (px, py) = point;
    let mut inside = false;

    let n = polygon.len();
    for i in 0..n {
        let (xi, yi) = polygon[i];
        let (xj, yj) = polygon[(i + n - 1) % n];

        let crosses = (yi > py) != (yj > py);
        if crosses {
            let x_intersect = xj + (py - yj) / (yi - yj) * (xi - xj);
            if px < x_intersect {
                inside = !inside;
            }
        }
    }

    inside
}

#[cfg(test)]
mod tests {
    use super::*;

    fn square_manifest() -> ZoneManifest {
        ZoneManifest::from_yaml(
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
        .unwrap()
    }

    #[test]
    fn in_bounds_reasonable_move_is_accepted() {
        let manifest = square_manifest();
        let index = crate::spatial::GridIndex::new(10.0);
        let mover = EntityId::new();

        let result = validate_movement(
            &manifest,
            &index,
            mover,
            10.0,
            0.05,
            (50.0, 50.0, 0.0),
            (50.3, 50.3, 0.0),
        );
        assert_eq!(result, Ok(()));
    }

    #[test]
    fn out_of_bounds_move_is_rejected() {
        let manifest = square_manifest();
        let index = crate::spatial::GridIndex::new(10.0);
        let mover = EntityId::new();

        // A generous speed cap here so this test isolates the boundary
        // check — the speed cap itself is covered separately below.
        let result = validate_movement(
            &manifest,
            &index,
            mover,
            10_000.0,
            0.05,
            (99.0, 50.0, 0.0),
            (150.0, 50.0, 0.0),
        );
        assert_eq!(result, Err(MovementRejection::OutOfBounds));
    }

    #[test]
    fn a_move_faster_than_the_speed_cap_is_rejected() {
        let manifest = square_manifest();
        let index = crate::spatial::GridIndex::new(10.0);
        let mover = EntityId::new();

        // 50m in one 50ms tick at a 10 m/s cap (max ~0.5m) is a spoofed teleport.
        let result = validate_movement(
            &manifest,
            &index,
            mover,
            10.0,
            0.05,
            (0.0, 0.0, 0.0),
            (50.0, 0.0, 0.0),
        );
        assert!(matches!(result, Err(MovementRejection::TooFast { .. })));
    }

    #[test]
    fn a_move_exactly_at_the_speed_cap_is_accepted() {
        // 10 m/s * 0.05s = exactly 0.5m allowed — the `>` (not `>=`)
        // boundary in `validate_movement`'s speed check, previously
        // untested at the exact edge.
        let manifest = square_manifest();
        let index = crate::spatial::GridIndex::new(10.0);
        let mover = EntityId::new();

        let result = validate_movement(
            &manifest,
            &index,
            mover,
            10.0,
            0.05,
            (10.0, 10.0, 0.0),
            (10.5, 10.0, 0.0),
        );
        assert_eq!(result, Ok(()));
    }

    #[test]
    fn moving_onto_another_entity_is_blocked() {
        let manifest = square_manifest();
        let mut index = crate::spatial::GridIndex::new(10.0);
        let blocker = EntityId::new();
        index.insert(blocker, (50.1, 50.1, 0.0));
        let mover = EntityId::new();

        let result = validate_movement(
            &manifest,
            &index,
            mover,
            10.0,
            0.05,
            (50.0, 50.0, 0.0),
            (50.1, 50.1, 0.0),
        );
        assert_eq!(
            result,
            Err(MovementRejection::Blocked {
                blocking_entity: blocker
            })
        );
    }

    #[test]
    fn a_purely_vertical_move_faster_than_the_speed_cap_is_rejected() {
        // Same (x, y) — pure altitude change — still counts against the
        // speed cap now that distance is 3D-aware (#249).
        let manifest = square_manifest();
        let index = crate::spatial::GridIndex::new(10.0);
        let mover = EntityId::new();

        let result = validate_movement(
            &manifest,
            &index,
            mover,
            10.0,
            0.05,
            (50.0, 50.0, 0.0),
            (50.0, 50.0, 50.0),
        );
        assert!(matches!(result, Err(MovementRejection::TooFast { .. })));
    }

    #[test]
    fn a_move_that_only_changes_altitude_within_bounds_is_accepted() {
        // The bounds check projects onto (x, y) — no vertical limit
        // within a zone's flat footprint yet (#242 owns real 3D zone
        // geometry, not this).
        let manifest = square_manifest();
        let index = crate::spatial::GridIndex::new(10.0);
        let mover = EntityId::new();

        let result = validate_movement(
            &manifest,
            &index,
            mover,
            10_000.0,
            0.05,
            (50.0, 50.0, 0.0),
            (50.0, 50.0, 500.0),
        );
        assert_eq!(result, Ok(()));
    }

    #[test]
    fn entities_stacked_at_different_altitudes_do_not_block_each_other() {
        // Same (x, y), far apart in z — distance being 3D-aware means
        // this is correctly outside the collision radius, unlike a 2D
        // distance check which would have wrongly reported a collision.
        let manifest = square_manifest();
        let mut index = crate::spatial::GridIndex::new(10.0);
        let other = EntityId::new();
        index.insert(other, (50.0, 50.0, 100.0));
        let mover = EntityId::new();

        let result = validate_movement(
            &manifest,
            &index,
            mover,
            10.0,
            0.05,
            (50.0, 50.0, 0.0),
            (50.0, 50.0, 0.3),
        );
        assert_eq!(result, Ok(()));
    }

    #[test]
    fn point_in_polygon_matches_intuition_for_a_simple_square() {
        let square = [(0.0, 0.0), (10.0, 0.0), (10.0, 10.0), (0.0, 10.0)];
        assert!(point_in_polygon((5.0, 5.0), &square));
        assert!(!point_in_polygon((15.0, 5.0), &square));
    }
}
