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
        }

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
