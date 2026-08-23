//! Every zone-service instance this combined `server` process runs,
//! plus enough manifest data to resolve where a transitioning player
//! should arrive in their destination zone (#45).
//!
//! One `world::Zone`/`world_actor::spawn_world_actor` task per zone,
//! each with its own `Sessions` map — a `Moved`/`EntitySpawned` broadcast
//! only ever reaches sessions actually in that zone, never every
//! connection on the process. The *set* of zones is fixed at process
//! startup (built once from `content::ContentPack` or a single
//! `zone.manifest.yaml`, see `main.rs`) — dynamically starting or
//! stopping one zone-service instance without restarting the others is
//! not built here. That's a real gap against #45's literal "started/
//! stopped independently" wording, but each zone's tick loop already
//! runs on its own independent `tokio` task with its own command
//! channel (`world_actor::spawn_world_actor` per zone, not one shared
//! loop) — one zone's task panicking or being aborted doesn't touch the
//! others' schedules, which is the operationally meaningful half of
//! that requirement. True independent process-level start/stop is
//! deferred to whenever `realm-directory` (#47) needs it for real
//! multi-process deployments.
//!
//! The plugin-host slice (#37/#38, extended by #57/#116) stays
//! single-instance for this same reason: today's `plugin.toml`/`.wasm`
//! config names exactly one plugin, attached to exactly one zone (the
//! first zone loaded — see `main.rs`). `docs/specs/Plugin_API.md`'s
//! "instantiated for a zone-service" (singular per zone) isn't fully
//! realized until a deployment can declare one plugin per zone; noted
//! as a real gap, not silently glossed over.

use std::collections::HashMap;

use content::manifest::ZoneManifest;
use world::Point;

use crate::session::Sessions;
use crate::world_actor::WorldHandle;

#[derive(Clone)]
pub struct ZoneRuntime {
    pub world: WorldHandle,
    pub sessions: Sessions,
}

pub struct ZoneRegistry {
    runtimes: HashMap<String, ZoneRuntime>,
    manifests: HashMap<String, ZoneManifest>,
}

impl ZoneRegistry {
    pub fn new(
        runtimes: HashMap<String, ZoneRuntime>,
        manifests: HashMap<String, ZoneManifest>,
    ) -> Self {
        Self {
            runtimes,
            manifests,
        }
    }

    pub fn get(&self, zone_id: &str) -> Option<&ZoneRuntime> {
        self.runtimes.get(zone_id)
    }

    pub fn contains(&self, zone_id: &str) -> bool {
        self.runtimes.contains_key(zone_id)
    }

    /// Where a player crossing from `from_zone` into `target_zone`
    /// should arrive, in `target_zone`'s own local coordinate system.
    ///
    /// Looks for a link in `target_zone`'s own manifest that points back
    /// to `from_zone` and uses that link's edge midpoint, nudged a
    /// couple meters toward the zone's bounds centroid so the arriving
    /// player lands unambiguously inside the zone rather than sitting
    /// exactly on the boundary line (which could immediately re-trigger
    /// `world::crossed_link` on the very next tick). This is a v0
    /// heuristic — docs/specs/Content_Manifest_Spec.md doesn't define
    /// portal placement beyond the edge itself — not general portal
    /// geometry (it doesn't try to preserve the player's relative
    /// position along the edge, for instance).
    ///
    /// Falls back to `target_zone`'s bounds centroid if no reciprocal
    /// link is declared (a one-way link, or the manifest author simply
    /// didn't declare one back) — always a valid in-bounds point, just
    /// not necessarily next to a matching doorway. Falls back to the
    /// origin if `target_zone` isn't a zone this registry knows about at
    /// all (defensive only — `main.rs` builds this registry from the
    /// same content pack it validates target-zone references against).
    pub fn entry_point(&self, from_zone: &str, target_zone: &str) -> Point {
        let Some(manifest) = self.manifests.get(target_zone) else {
            return (0.0, 0.0);
        };

        let reciprocal = manifest
            .links
            .iter()
            .find(|link| link.target_zone == from_zone);

        if let Some(link) = reciprocal
            && let (Some(&a), Some(&b)) = (link.edge.first(), link.edge.get(1))
        {
            let midpoint = ((a.0 + b.0) / 2.0, (a.1 + b.1) / 2.0);
            return nudge_toward(midpoint, centroid(&manifest.bounds.points), 2.0);
        }

        centroid(&manifest.bounds.points)
    }
}

fn centroid(points: &[Point]) -> Point {
    if points.is_empty() {
        return (0.0, 0.0);
    }
    let (sum_x, sum_y) = points
        .iter()
        .fold((0.0, 0.0), |(sx, sy), (x, y)| (sx + x, sy + y));
    (sum_x / points.len() as f64, sum_y / points.len() as f64)
}

fn nudge_toward(point: Point, target: Point, meters: f64) -> Point {
    let dx = target.0 - point.0;
    let dy = target.1 - point.1;
    let len = (dx * dx + dy * dy).sqrt();
    if len < 1e-6 {
        return point;
    }
    (point.0 + dx / len * meters, point.1 + dy / len * meters)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn manifest_yaml(id: &str, links: &str) -> String {
        format!(
            r#"
schema_version: 1
id: {id}
display_name: "{id}"
bounds:
  shape: polygon
  coordinate_system: {{ units: meters, origin: [0, 0] }}
  points: [[0,0], [100,0], [100,100], [0,100]]
collision:
  asset_ref: "sha256:9f2ac1b3e4d5c6a7b8c9d0e1f2a3b4c5d6e7f8a9b0c1d2e3f4a5b6c7d8e9f0a1"
  format: navmesh_v1
{links}
"#
        )
    }

    fn registry_with(manifests: Vec<(&str, &str)>) -> ZoneRegistry {
        let manifests = manifests
            .into_iter()
            .map(|(id, links)| {
                (
                    id.to_string(),
                    ZoneManifest::from_yaml(&manifest_yaml(id, links)).unwrap(),
                )
            })
            .collect();
        ZoneRegistry::new(HashMap::new(), manifests)
    }

    #[test]
    fn entry_point_uses_the_reciprocal_links_edge_midpoint() {
        let registry = registry_with(vec![
            ("zone-a", ""),
            (
                "zone-b",
                "links:\n  - target_zone: zone-a\n    edge: [[0,40],[0,60]]\n    bidirectional: true",
            ),
        ]);

        let point = registry.entry_point("zone-a", "zone-b");
        // Midpoint of the edge is (0, 50); nudged 2m toward the square's
        // centroid (50, 50), which lies along +x from the edge.
        assert!((point.0 - 2.0).abs() < 0.01, "{point:?}");
        assert!((point.1 - 50.0).abs() < 0.01, "{point:?}");
    }

    #[test]
    fn entry_point_falls_back_to_centroid_with_no_reciprocal_link() {
        let registry = registry_with(vec![("zone-a", ""), ("zone-b", "")]);

        let point = registry.entry_point("zone-a", "zone-b");
        assert_eq!(point, (50.0, 50.0));
    }

    #[test]
    fn entry_point_falls_back_to_origin_for_an_unknown_target_zone() {
        let registry = registry_with(vec![("zone-a", "")]);
        assert_eq!(registry.entry_point("zone-a", "does-not-exist"), (0.0, 0.0));
    }
}
