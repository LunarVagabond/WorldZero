//! `zone.manifest.yaml` loading and validation
//! (docs/specs/Content_Manifest_Spec.md, "zone.manifest.yaml: field by field").

use std::collections::HashSet;
use std::path::Path;

use common::{Error, Result};
use serde::Deserialize;

/// The only manifest schema version this build understands. Bumped only
/// on a breaking change to the format itself.
pub const SUPPORTED_SCHEMA_VERSION: u32 = 1;

pub type Point = (f64, f64);

#[derive(Debug, Clone, Deserialize)]
pub struct Bounds {
    pub shape: String,
    pub coordinate_system: CoordinateSystem,
    pub points: Vec<Point>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct CoordinateSystem {
    pub units: String,
    pub origin: Point,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Collision {
    pub asset_ref: String,
    pub format: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Link {
    pub target_zone: String,
    pub edge: Vec<Point>,
    pub bidirectional: bool,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SpawnTable {
    pub id: String,
    pub entity_type: String,
    pub points: Vec<Point>,
    pub max_population: u32,
    pub respawn_seconds: u32,
    pub route_id: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Route {
    pub id: String,
    pub waypoints: Vec<Point>,
    #[serde(rename = "loop")]
    pub is_loop: bool,
    pub speed: f64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct TriggerShape {
    #[serde(rename = "type")]
    pub shape_type: String,
    pub center: Point,
    pub radius: f64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Trigger {
    pub id: String,
    pub shape: TriggerShape,
    pub event: String,
    pub one_shot: bool,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ZoneManifest {
    pub schema_version: u32,
    pub id: String,
    pub display_name: String,
    pub bounds: Bounds,
    pub collision: Collision,
    #[serde(default)]
    pub links: Vec<Link>,
    #[serde(default)]
    pub spawn_tables: Vec<SpawnTable>,
    #[serde(default)]
    pub routes: Vec<Route>,
    #[serde(default)]
    pub triggers: Vec<Trigger>,
}

/// A link edge point must lie within this many meters of the declared
/// `bounds` boundary polygon to count as "on the boundary" — real
/// manifests are hand-authored/generated from map data, so an exact
/// floating-point match isn't realistic, but a link edge floating tens
/// of meters from the actual zone shape is almost certainly an authoring
/// mistake (wrong coordinates copy-pasted, wrong zone's edge, ...).
const LINK_EDGE_BOUNDARY_TOLERANCE_METERS: f64 = 1.0;

/// A polygon whose absolute shoelace-formula area is below this is
/// treated as degenerate (collinear points, or points that otherwise
/// cancel out to ~zero enclosed area) rather than a real shape.
const DEGENERATE_POLYGON_AREA_EPSILON: f64 = 1e-6;

/// Shoelace-formula signed area — used only to detect a degenerate
/// (near-zero-area) polygon below, not for its sign.
fn polygon_signed_area(points: &[Point]) -> f64 {
    let n = points.len();
    let mut area = 0.0;
    for i in 0..n {
        let (x1, y1) = points[i];
        let (x2, y2) = points[(i + 1) % n];
        area += x1 * y2 - x2 * y1;
    }
    area / 2.0
}

/// Ray-casting point-in-polygon test — a private duplicate of
/// `world::movement`'s identical check. `world` depends on `content`
/// (not the other way around), so this can't be shared without
/// restructuring; small and stable enough that duplicating it here was
/// judged cheaper than that restructuring.
fn point_in_polygon(point: Point, polygon: &[Point]) -> bool {
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

fn distance_point_to_segment(point: Point, a: Point, b: Point) -> f64 {
    let (px, py) = point;
    let (ax, ay) = a;
    let (bx, by) = b;
    let (dx, dy) = (bx - ax, by - ay);
    let len_sq = dx * dx + dy * dy;
    let t = if len_sq > 0.0 {
        (((px - ax) * dx + (py - ay) * dy) / len_sq).clamp(0.0, 1.0)
    } else {
        0.0
    };
    let (cx, cy) = (ax + t * dx, ay + t * dy);
    ((px - cx).powi(2) + (py - cy).powi(2)).sqrt()
}

fn distance_to_polygon_boundary(point: Point, polygon: &[Point]) -> f64 {
    let n = polygon.len();
    (0..n)
        .map(|i| distance_point_to_segment(point, polygon[i], polygon[(i + 1) % n]))
        .fold(f64::INFINITY, f64::min)
}

impl ZoneManifest {
    pub fn from_yaml(input: &str) -> Result<Self> {
        let manifest: Self = serde_yaml::from_str(input)
            .map_err(|e| Error::wrap("content", "failed to parse zone manifest", e))?;

        let problems = manifest.validate();
        if !problems.is_empty() {
            return Err(Error::new("content", problems.join("; ")));
        }

        Ok(manifest)
    }

    pub fn from_file(path: &Path) -> Result<Self> {
        let contents = std::fs::read_to_string(path)
            .map_err(|e| Error::wrap("content", format!("failed to read {}", path.display()), e))?;
        Self::from_yaml(&contents)
    }

    /// Every problem found, prefixed with a dotted field path — empty if
    /// the manifest is valid. Collects everything rather than stopping at
    /// the first issue, so `validate` (the CLI) can report it all at once.
    pub fn validate(&self) -> Vec<String> {
        let mut problems = Vec::new();

        if self.schema_version != SUPPORTED_SCHEMA_VERSION {
            // Nothing else is worth checking against an unrecognized format.
            return vec![format!(
                "schema_version: unsupported version {} (this build understands {SUPPORTED_SCHEMA_VERSION})",
                self.schema_version
            )];
        }

        if self.id.trim().is_empty() {
            problems.push("id: must not be empty".to_string());
        }
        if self.display_name.trim().is_empty() {
            problems.push("display_name: must not be empty".to_string());
        }

        // "polygon" is a 2D-only representation. #89 (2D vs 3D movement/
        // space support decision) is already closed — 3D-first, but not
        // 3D-only. Bounds staying 2D here isn't blocked on that decision;
        // it's a deliberate v0 scope cut (#249's own scope note) —
        // world::spatial::Point is 3D and movement/collision are
        // 3D-aware, but real 3D zone geometry (floors, interiors) is
        // #242's job, not this field's, yet.
        if self.bounds.shape != "polygon" {
            problems.push(format!(
                "bounds.shape: unsupported shape {:?} (only \"polygon\" is supported)",
                self.bounds.shape
            ));
        }
        if self.bounds.coordinate_system.units != "meters" {
            problems.push(format!(
                "bounds.coordinate_system.units: unsupported units {:?} (only \"meters\" is supported)",
                self.bounds.coordinate_system.units
            ));
        }
        if self.bounds.points.len() < 3 {
            problems.push(format!(
                "bounds.points: needs at least 3 points, has {}",
                self.bounds.points.len()
            ));
        } else if polygon_signed_area(&self.bounds.points).abs() < DEGENERATE_POLYGON_AREA_EPSILON {
            problems.push(
                "bounds.points: describes a degenerate polygon (~zero enclosed area — likely collinear points)"
                    .to_string(),
            );
        }
        // Every in-bounds/on-boundary check below only makes sense
        // against a real, non-degenerate polygon — skip them rather than
        // pile on confusing follow-on errors when `bounds` itself is
        // already broken.
        let bounds_are_usable = self.bounds.points.len() >= 3
            && polygon_signed_area(&self.bounds.points).abs() >= DEGENERATE_POLYGON_AREA_EPSILON;

        if !is_valid_asset_ref(&self.collision.asset_ref) {
            problems.push(format!(
                "collision.asset_ref: {:?} is not a valid sha256:<64 hex chars> reference",
                self.collision.asset_ref
            ));
        }
        if self.collision.format != "navmesh_v1" {
            problems.push(format!(
                "collision.format: unsupported format {:?} (only \"navmesh_v1\" is supported)",
                self.collision.format
            ));
        }

        for (i, link) in self.links.iter().enumerate() {
            if link.edge.len() != 2 {
                problems.push(format!(
                    "links[{i}].edge: needs exactly 2 points, has {}",
                    link.edge.len()
                ));
            } else if bounds_are_usable {
                for (j, point) in link.edge.iter().enumerate() {
                    let distance = distance_to_polygon_boundary(*point, &self.bounds.points);
                    if distance > LINK_EDGE_BOUNDARY_TOLERANCE_METERS {
                        problems.push(format!(
                            "links[{i}].edge[{j}]: {point:?} is {distance:.1}m from the declared bounds boundary (must lie on/near it)"
                        ));
                    }
                }
            }
        }

        let route_ids: HashSet<&str> = self.routes.iter().map(|r| r.id.as_str()).collect();
        if route_ids.len() != self.routes.len() {
            problems.push("routes: ids must be unique within a manifest".to_string());
        }
        for (i, route) in self.routes.iter().enumerate() {
            if route.waypoints.len() < 2 {
                problems.push(format!(
                    "routes[{i}].waypoints: needs at least 2 points, has {}",
                    route.waypoints.len()
                ));
            }
            if route.speed <= 0.0 {
                problems.push(format!(
                    "routes[{i}].speed: must be > 0, got {}",
                    route.speed
                ));
            }
        }

        let spawn_table_ids: HashSet<&str> =
            self.spawn_tables.iter().map(|s| s.id.as_str()).collect();
        if spawn_table_ids.len() != self.spawn_tables.len() {
            problems.push("spawn_tables: ids must be unique within a manifest".to_string());
        }
        for (i, spawn_table) in self.spawn_tables.iter().enumerate() {
            if spawn_table.points.is_empty() {
                problems.push(format!("spawn_tables[{i}].points: needs at least 1 point"));
            } else if bounds_are_usable {
                for (j, point) in spawn_table.points.iter().enumerate() {
                    if !point_in_polygon(*point, &self.bounds.points) {
                        problems.push(format!(
                            "spawn_tables[{i}].points[{j}]: {point:?} lies outside bounds"
                        ));
                    }
                }
            }
            if let Some(route_id) = &spawn_table.route_id
                && !route_ids.contains(route_id.as_str())
            {
                problems.push(format!("spawn_tables[{i}].route_id: {route_id:?} does not match any routes[].id in this manifest"));
            }
        }

        let trigger_ids: HashSet<&str> = self.triggers.iter().map(|t| t.id.as_str()).collect();
        if trigger_ids.len() != self.triggers.len() {
            problems.push("triggers: ids must be unique within a manifest".to_string());
        }
        for (i, trigger) in self.triggers.iter().enumerate() {
            if trigger.shape.shape_type != "circle" {
                problems.push(format!(
                    "triggers[{i}].shape.type: unsupported shape {:?} (only \"circle\" is supported)",
                    trigger.shape.shape_type
                ));
            }
            if trigger.shape.radius <= 0.0 {
                problems.push(format!(
                    "triggers[{i}].shape.radius: must be > 0, got {}",
                    trigger.shape.radius
                ));
            }
            if bounds_are_usable && !point_in_polygon(trigger.shape.center, &self.bounds.points) {
                problems.push(format!(
                    "triggers[{i}].shape.center: {:?} lies outside bounds",
                    trigger.shape.center
                ));
            }
        }

        problems
    }
}

pub fn is_valid_asset_ref(value: &str) -> bool {
    match value.strip_prefix("sha256:") {
        Some(hex) => {
            hex.len() == 64
                && hex
                    .chars()
                    .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
        }
        None => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn example_manifest() -> &'static str {
        r#"
schema_version: 1
id: greenwood-forest
display_name: "Greenwood Forest"

bounds:
  shape: polygon
  coordinate_system: { units: meters, origin: [0, 0] }
  points: [[0,0], [500,0], [500,500], [0,500]]

collision:
  asset_ref: "sha256:9f2ac1b3e4d5c6a7b8c9d0e1f2a3b4c5d6e7f8a9b0c1d2e3f4a5b6c7d8e9f0a1"
  format: navmesh_v1

links:
  - target_zone: stonebridge-village
    edge: [[500,200], [500,260]]
    bidirectional: true

spawn_tables:
  - id: wolf-pack-01
    entity_type: npc.wolf
    points: [[120,80], [140,95]]
    max_population: 6
    respawn_seconds: 45
    route_id: wolf-patrol-01

routes:
  - id: wolf-patrol-01
    waypoints: [[110,70],[150,70],[150,110],[110,110]]
    loop: true
    speed: 1.4

triggers:
  - id: forest-entrance
    shape: { type: circle, center: [10,10], radius: 5 }
    event: on_trigger_enter
    one_shot: false
"#
    }

    #[test]
    fn parses_the_proposals_example_manifest() {
        let manifest = ZoneManifest::from_yaml(example_manifest()).unwrap();
        assert_eq!(manifest.id, "greenwood-forest");
        assert_eq!(
            manifest.spawn_tables[0].route_id.as_deref(),
            Some("wolf-patrol-01")
        );
        assert!(manifest.routes[0].is_loop);
    }

    #[test]
    fn unsupported_schema_version_fails_immediately_without_other_checks() {
        let yaml = example_manifest().replacen("schema_version: 1", "schema_version: 99", 1);
        let err = ZoneManifest::from_yaml(&yaml).unwrap_err();
        assert!(err.to_string().contains("unsupported version 99"), "{err}");
    }

    #[test]
    fn malformed_manifest_names_the_field() {
        // bounds.points has only 2 points — an invalid polygon.
        let yaml = example_manifest().replace(
            "points: [[0,0], [500,0], [500,500], [0,500]]",
            "points: [[0,0], [500,0]]",
        );
        let err = ZoneManifest::from_yaml(&yaml).unwrap_err();
        assert!(err.to_string().contains("bounds.points"), "{err}");
    }

    #[test]
    fn bad_asset_ref_is_rejected() {
        let yaml = example_manifest().replace(
            "asset_ref: \"sha256:9f2ac1b3e4d5c6a7b8c9d0e1f2a3b4c5d6e7f8a9b0c1d2e3f4a5b6c7d8e9f0a1\"",
            "asset_ref: \"sha256:not-hex\"",
        );
        let err = ZoneManifest::from_yaml(&yaml).unwrap_err();
        assert!(err.to_string().contains("collision.asset_ref"), "{err}");
    }

    #[test]
    fn spawn_table_route_id_must_exist() {
        let yaml =
            example_manifest().replace("route_id: wolf-patrol-01", "route_id: does-not-exist");
        let err = ZoneManifest::from_yaml(&yaml).unwrap_err();
        assert!(
            err.to_string().contains("spawn_tables[0].route_id"),
            "{err}"
        );
    }

    #[test]
    fn duplicate_route_ids_are_rejected() {
        let yaml = example_manifest().replace(
            "routes:\n  - id: wolf-patrol-01",
            "routes:\n  - id: wolf-patrol-01\n    waypoints: [[0,0],[1,1]]\n    loop: false\n    speed: 1.0\n  - id: wolf-patrol-01",
        );
        let err = ZoneManifest::from_yaml(&yaml).unwrap_err();
        assert!(
            err.to_string().contains("routes: ids must be unique"),
            "{err}"
        );
    }

    #[test]
    fn collects_multiple_problems_at_once() {
        let yaml = example_manifest()
            .replace(
                "points: [[0,0], [500,0], [500,500], [0,500]]",
                "points: [[0,0], [500,0]]",
            )
            .replace("speed: 1.4", "speed: -1.0");
        let err = ZoneManifest::from_yaml(&yaml).unwrap_err();
        let message = err.to_string();
        assert!(message.contains("bounds.points"), "{message}");
        assert!(message.contains("routes[0].speed"), "{message}");
    }

    #[test]
    fn a_degenerate_collinear_bounds_polygon_is_rejected() {
        let yaml = example_manifest().replace(
            "points: [[0,0], [500,0], [500,500], [0,500]]",
            "points: [[0,0], [250,0], [500,0]]",
        );
        let err = ZoneManifest::from_yaml(&yaml).unwrap_err();
        assert!(err.to_string().contains("degenerate polygon"), "{err}");
    }

    #[test]
    fn a_spawn_point_outside_bounds_is_rejected() {
        let yaml = example_manifest().replace(
            "points: [[120,80], [140,95]]",
            "points: [[120,80], [9999,9999]]",
        );
        let err = ZoneManifest::from_yaml(&yaml).unwrap_err();
        assert!(
            err.to_string().contains("spawn_tables[0].points[1]"),
            "{err}"
        );
    }

    #[test]
    fn a_trigger_center_outside_bounds_is_rejected() {
        let yaml = example_manifest().replace("center: [10,10]", "center: [9999,9999]");
        let err = ZoneManifest::from_yaml(&yaml).unwrap_err();
        assert!(
            err.to_string().contains("triggers[0].shape.center"),
            "{err}"
        );
    }

    #[test]
    fn a_link_edge_far_from_the_boundary_is_rejected() {
        let yaml = example_manifest().replace(
            "edge: [[500,200], [500,260]]",
            "edge: [[9999,200], [9999,260]]",
        );
        let err = ZoneManifest::from_yaml(&yaml).unwrap_err();
        assert!(err.to_string().contains("links[0].edge[0]"), "{err}");
    }

    #[test]
    fn a_link_edge_exactly_on_the_boundary_is_accepted() {
        // The proposal's example manifest already places its link edge
        // exactly on the bounds polygon's right edge (x=500) — this is
        // the same assertion as `parses_the_proposals_example_manifest`,
        // just naming explicitly that the boundary-distance check (added
        // alongside the far-edge rejection above) doesn't false-positive
        // on a legitimately-placed edge.
        assert!(ZoneManifest::from_yaml(example_manifest()).is_ok());
    }

    #[test]
    fn valid_asset_ref_shapes() {
        assert!(is_valid_asset_ref(
            "sha256:9f2ac1b3e4d5c6a7b8c9d0e1f2a3b4c5d6e7f8a9b0c1d2e3f4a5b6c7d8e9f0a1"
        ));
        assert!(!is_valid_asset_ref("sha256:tooShort"));
        assert!(!is_valid_asset_ref("md5:9f2ac1b3e4d5c6a7b8c9d0e1f2a3b4c5"));
        assert!(!is_valid_asset_ref(
            "sha256:9F2AC1B3E4D5C6A7B8C9D0E1F2A3B4C5D6E7F8A9B0C1D2E3F4A5B6C7D8E9F0A1"
        ));
    }
}
