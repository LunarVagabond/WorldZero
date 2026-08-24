//! Every zone-service instance this combined `server` process runs,
//! plus enough manifest data to resolve where a transitioning player
//! should arrive in their destination zone (#45), plus dynamic layer
//! assignment within a zone (#50).
//!
//! One `world::Zone`/`world_actor::spawn_world_actor` task per *layer* of
//! a zone (a zone always has at least one — "layer 0" — spawned at
//! process startup, same as before #50), each with its own `Sessions`
//! map — a `Moved`/`EntitySpawned` broadcast only ever reaches sessions
//! actually in that layer, never every connection on the process, and
//! never another layer of the same zone either. The *set* of zones is
//! still fixed at process startup (built once from `content::ContentPack`
//! or a single `zone.manifest.yaml`, see `main.rs`) — only the *layer
//! count within* an already-running zone is dynamic. Dynamically
//! starting/stopping a whole zone-service instance without restarting
//! the others is still not built here; each layer's tick loop (like each
//! zone's before it) already runs on its own independent `tokio` task
//! with its own command channel, so one layer's task panicking or being
//! aborted doesn't touch any other layer's or zone's schedule — the
//! operationally meaningful half of #45's "started/stopped
//! independently" wording. True independent process-level start/stop is
//! deferred to whenever `realm-directory` (#47) needs it for real
//! multi-process deployments (#130).
//!
//! **Layer assignment only happens at initial join** ([`ZoneRegistry::assign_layer`],
//! called once in `session::handle_session`) — the case that actually
//! needs population spreading, since most connections start there. A
//! zone-link transition ([`main::complete_zone_transition`]) and a
//! mid-connection zone switch always land on [`ZoneRegistry::get`]'s
//! layer 0 instead, a deliberate simplification: `session::handle_session`
//! re-resolves its `ZoneRuntime` purely from the `zone_id` carried on the
//! wire `ZoneChanged` message, with no layer identifier in that message
//! (on purpose — a wire field would be exactly the kind of
//! player-visible layering artifact #50's acceptance criteria rules out),
//! so a transition has no way to land on the same layer
//! [`ZoneRegistry::assign_layer`] would have picked without adding one.
//! Real population balancing across zone-link transitions too is left
//! for later, not silently glossed over.
//!
//! **Enabled by default, per-deployment configurable:** `WZ_LAYER_ENABLED`
//! (default `true`, see `main.rs`) turns layering off entirely for a
//! deployment that doesn't want it — every zone then stays at exactly
//! one layer forever, no matter its population.
//!
//! **Trigger for spinning up a new layer**, when enabled: a zone's
//! existing layers are all at or above `layer_population_threshold`
//! connected sessions (`WZ_LAYER_POPULATION_THRESHOLD`, default `200`,
//! see `main.rs`) — checked at every [`ZoneRegistry::assign_layer`]
//! call, not on a timer. Deployments differ wildly here (a small
//! community server might want 10 per layer, a big one 1000+), which is
//! exactly why this is a runtime env var and not a hardcoded constant.
//!
//! The plugin-host slice (#37/#38, extended by #57/#116) stays
//! single-instance for this same reason as before: today's
//! `plugin.toml`/`.wasm` config names exactly one plugin, attached to
//! exactly the first zone's layer 0 (see `main.rs`) — never to a
//! dynamically-spawned layer. `docs/specs/Plugin_API.md`'s "instantiated
//! for a zone-service" (singular per zone) isn't fully realized until a
//! deployment can declare one plugin per zone; noted as a real gap, not
//! silently glossed over.

use std::collections::HashMap;
use std::sync::RwLock;

use content::manifest::ZoneManifest;
use world::Point;

use crate::session::Sessions;
use crate::world_actor::WorldHandle;

#[derive(Clone)]
pub struct ZoneRuntime {
    pub world: WorldHandle,
    pub sessions: Sessions,
}

impl ZoneRuntime {
    fn population(&self) -> usize {
        self.sessions.lock().unwrap().len()
    }
}

/// Builds a brand-new layer's [`ZoneRuntime`] for `zone_id`/its manifest
/// — called by [`ZoneRegistry::assign_layer`] on demand, never at
/// startup (every zone's layer 0 is spawned directly in `main.rs`
/// instead, since that one needs the possibility of an attached plugin —
/// see this module's doc comment). Boxed rather than generic since
/// `ZoneRegistry` is constructed once with a concrete closure and never
/// needs to be generic over its type.
pub type LayerSpawner = Box<dyn Fn(&str, &ZoneManifest) -> ZoneRuntime + Send + Sync>;

pub struct ZoneRegistry {
    /// Every zone's layers, in spawn order — index 0 is always the layer
    /// spawned at process startup. A `RwLock`, not a plain `HashMap`,
    /// specifically so [`Self::assign_layer`] can push a newly-spawned
    /// layer onto an already-running zone without rebuilding the whole
    /// registry (#50) — every other zone-set-shaping operation still
    /// only happens at startup, per this module's doc comment.
    runtimes: RwLock<HashMap<String, Vec<ZoneRuntime>>>,
    manifests: HashMap<String, ZoneManifest>,
    /// `WZ_LAYER_ENABLED` (default `true`, see `main.rs`) — a deployment
    /// that doesn't want layering at all (small player counts, or a game
    /// that relies on every player in a zone being able to see every
    /// other) sets this `false` rather than fighting the threshold with
    /// an arbitrarily huge number. When `false`, [`Self::assign_layer`]
    /// always returns layer 0 and `layer_population_threshold`/
    /// `layer_spawner` are never consulted — a zone never grows past one
    /// layer, full stop.
    layering_enabled: bool,
    layer_population_threshold: usize,
    layer_spawner: LayerSpawner,
}

impl ZoneRegistry {
    pub fn new(
        runtimes: HashMap<String, ZoneRuntime>,
        manifests: HashMap<String, ZoneManifest>,
        layering_enabled: bool,
        layer_population_threshold: usize,
        layer_spawner: LayerSpawner,
    ) -> Self {
        let runtimes = runtimes
            .into_iter()
            .map(|(zone_id, runtime)| (zone_id, vec![runtime]))
            .collect();
        Self {
            runtimes: RwLock::new(runtimes),
            manifests,
            layering_enabled,
            layer_population_threshold,
            layer_spawner,
        }
    }

    /// `zone_id`'s layer 0 — every caller that isn't
    /// [`Self::assign_layer`] itself (zone-link transitions, a
    /// mid-connection zone switch) uses this, per this module's doc
    /// comment on why those don't participate in layer assignment yet.
    pub fn get(&self, zone_id: &str) -> Option<ZoneRuntime> {
        self.runtimes.read().unwrap().get(zone_id)?.first().cloned()
    }

    pub fn contains(&self, zone_id: &str) -> bool {
        self.manifests.contains_key(zone_id)
    }

    /// Assigns a newly-joining connection to a layer of `zone_id`: the
    /// least-populated existing layer, if it's under
    /// `layer_population_threshold`; otherwise spins up a brand-new layer
    /// via [`LayerSpawner`] and assigns to that instead. `None` only if
    /// `zone_id` isn't a zone this registry knows about at all. Always
    /// layer 0 if `layering_enabled` is `false` — see the field's doc
    /// comment.
    ///
    /// Holds the write lock for the whole call (check *and* possible
    /// spawn) rather than checking then re-locking to spawn — two
    /// concurrent joins racing the same "all layers full" moment would
    /// otherwise both decide to spin up a new layer instead of one of
    /// them landing on the other's brand-new one.
    pub fn assign_layer(&self, zone_id: &str) -> Option<ZoneRuntime> {
        if !self.layering_enabled {
            return self.get(zone_id);
        }

        let mut runtimes = self.runtimes.write().unwrap();
        let layers = runtimes.get_mut(zone_id)?;

        let populations: Vec<usize> = layers.iter().map(ZoneRuntime::population).collect();
        if let Some(index) =
            least_populated_under_threshold(&populations, self.layer_population_threshold)
        {
            return Some(layers[index].clone());
        }

        let manifest = self.manifests.get(zone_id)?;
        let new_runtime = (self.layer_spawner)(zone_id, manifest);
        layers.push(new_runtime.clone());
        tracing::info!(
            zone_id,
            layer_count = layers.len(),
            population_threshold = self.layer_population_threshold,
            "spinning up a new zone layer: every existing layer is at or above the population threshold"
        );
        Some(new_runtime)
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

/// The index of the lowest-population layer among `populations`, if it
/// has room under `threshold` — `None` if every layer is already at or
/// above it (the caller's cue to spin up a new one instead). Kept as a
/// pure function over plain counts, not `&[ZoneRuntime]`, specifically so
/// this — the actual "documented trigger" contract #50 asks for — is
/// unit-testable without spinning up real zone-service actors.
fn least_populated_under_threshold(populations: &[usize], threshold: usize) -> Option<usize> {
    populations
        .iter()
        .enumerate()
        .min_by_key(|&(_, &population)| population)
        .filter(|&(_, &population)| population < threshold)
        .map(|(index, _)| index)
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
        ZoneRegistry::new(
            HashMap::new(),
            manifests,
            true,
            usize::MAX,
            Box::new(|_, _| panic!("layer_spawner should not be called in these tests")),
        )
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

    #[test]
    fn assign_layer_returns_none_for_an_unknown_zone() {
        let registry = registry_with(vec![("zone-a", "")]);
        assert!(registry.assign_layer("does-not-exist").is_none());
    }

    #[test]
    fn least_populated_under_threshold_picks_the_emptiest_layer() {
        assert_eq!(least_populated_under_threshold(&[5, 1, 3], 10), Some(1));
    }

    #[test]
    fn least_populated_under_threshold_returns_none_when_every_layer_is_full() {
        assert_eq!(least_populated_under_threshold(&[10, 12, 20], 10), None);
    }

    #[test]
    fn least_populated_under_threshold_is_inclusive_of_the_threshold_itself() {
        // A layer sitting exactly at the threshold still counts as full —
        // "under threshold" is a strict `<`, not `<=`, so a new layer
        // spins up rather than letting one layer creep past the intended
        // cap by however many joins race the same tick.
        assert_eq!(least_populated_under_threshold(&[10], 10), None);
        assert_eq!(least_populated_under_threshold(&[9], 10), Some(0));
    }

    #[test]
    fn least_populated_under_threshold_handles_no_layers() {
        assert_eq!(least_populated_under_threshold(&[], 10), None);
    }
}
