//! `navmesh_v1`: the real format behind `zone.manifest.yaml`'s
//! `collision.format` field (#280, split out of #242 alongside #279's
//! asset store — see that issue's closing comment for the recorded design
//! rationale). Documented in `docs/specs/Content_Manifest_Spec.md`'s
//! "navmesh_v1" section.
//!
//! A JSON polygon list — convex walkable polygons, each a list of real
//! `(x, y, z)` vertices. Authored externally (a content pipeline converts
//! Tiled/glTF/whatever a dev's tooling emits into this simple format at
//! content-build time — the server never owns art tooling, per the
//! proposal's Design Principle #2) and loaded through `crate::AssetStore`
//! (#279).
//!
//! **No separate "structure"/"interior" primitive.** An interior room, a
//! second floor, a doorway are all just more navmesh geometry: a second
//! floor is walkable polygons at a higher `z` that happen to overlap the
//! ground floor's `(x, y)`. "Which floor am I on" resolves by checking
//! which polygon actually contains the point, `z` included, not by a new
//! manifest concept — see [`NavMesh::containing_polygon`].

use std::path::Path;

use common::{Error, Result};
use serde::Deserialize;

use crate::assets::AssetStore;

/// A 3D vertex/point — `(x, y, z)` in the same meters/zone-local
/// coordinate space as `content::manifest::Bounds`.
pub type Point3 = (f64, f64, f64);

/// The only `navmesh_v1` payload version this build understands, named in
/// the asset file's own `format` field (independent of, but analogous to,
/// `manifest::SUPPORTED_SCHEMA_VERSION` for `zone.manifest.yaml` itself)
/// — an asset file claiming a different format fails to load rather than
/// being guessed at.
pub const SUPPORTED_FORMAT: &str = "navmesh_v1";

/// A destination within this many meters of a polygon's own walkable
/// plane still counts as "on" it — authored navmesh data is a simplified
/// approximation of real ground (a "flat" floor is rarely exactly planar
/// down to the millimeter, and a gentle ramp is still one polygon), and a
/// real player's feet aren't a mathematical point either. Loose enough to
/// absorb authoring/float noise, tight enough that a second floor several
/// meters up is never mistaken for the ground floor beneath it.
const HEIGHT_TOLERANCE_METERS: f64 = 0.5;

/// A magnitude below this on a polygon's Newell-method normal's `z`
/// component means the polygon is (near-)vertical — a wall, not a
/// walkable floor/ramp. Such a polygon never contains any point (there's
/// no meaningful "the floor is at this height" to compare a query's `z`
/// against).
const VERTICAL_NORMAL_EPSILON: f64 = 1e-9;

#[derive(Debug, Clone, Deserialize)]
struct RawNavMesh {
    format: String,
    polygons: Vec<RawPolygon>,
}

#[derive(Debug, Clone, Deserialize)]
struct RawPolygon {
    vertices: Vec<Point3>,
}

/// One convex walkable polygon.
#[derive(Debug, Clone)]
pub struct Polygon {
    pub vertices: Vec<Point3>,
}

impl Polygon {
    /// Newell's method: robust against small non-planarity (authored data
    /// is never perfectly flat) and works for any polygon winding.
    /// Returns an unnormalized normal — only used here to test its `z`
    /// component's sign/magnitude and to solve the plane equation for
    /// `z`, so normalizing would only cost precision for no benefit.
    fn normal(&self) -> (f64, f64, f64) {
        let mut n = (0.0, 0.0, 0.0);
        let count = self.vertices.len();
        for i in 0..count {
            let (x0, y0, z0) = self.vertices[i];
            let (x1, y1, z1) = self.vertices[(i + 1) % count];
            n.0 += (y0 - y1) * (z0 + z1);
            n.1 += (z0 - z1) * (x0 + x1);
            n.2 += (x0 - x1) * (y0 + y1);
        }
        n
    }

    /// Real 3D containment: `point`'s `(x, y)` must fall inside this
    /// polygon's flat projection, *and* `point.z` must land within
    /// [`HEIGHT_TOLERANCE_METERS`] of the polygon's own walkable plane at
    /// that `(x, y)` (solved from the polygon's plane equation, not just
    /// the vertices' raw min/max `z` — this is what lets one polygon
    /// represent a sloped ramp, not only a perfectly flat floor).
    pub fn contains(&self, point: Point3) -> bool {
        let (px, py, pz) = point;
        if !point_in_polygon_2d((px, py), &self.vertices) {
            return false;
        }

        let (nx, ny, nz) = self.normal();
        if nz.abs() < VERTICAL_NORMAL_EPSILON {
            // A vertical polygon (a wall) has no walkable "height at this
            // (x, y)" to compare against.
            return false;
        }

        let (x0, y0, z0) = self.vertices[0];
        let expected_z = z0 - (nx * (px - x0) + ny * (py - y0)) / nz;
        (pz - expected_z).abs() <= HEIGHT_TOLERANCE_METERS
    }
}

/// The loaded, checkable form of a `navmesh_v1` asset — the actual
/// walkability authority `world::movement::validate_movement` consumes,
/// replacing the old flat-bounds-only check.
#[derive(Debug, Clone)]
pub struct NavMesh {
    pub polygons: Vec<Polygon>,
}

impl NavMesh {
    /// Parses and validates a `navmesh_v1` JSON payload: the top-level
    /// `format` field must be `"navmesh_v1"`, there must be at least one
    /// polygon, and every polygon must have at least 3 vertices (the
    /// same "needs a real shape" bar `manifest::Bounds` enforces).
    pub fn from_json(input: &str) -> Result<Self> {
        let raw: RawNavMesh = serde_json::from_str(input)
            .map_err(|e| Error::wrap("content", "failed to parse navmesh_v1 asset", e))?;

        if raw.format != SUPPORTED_FORMAT {
            return Err(Error::new(
                "content",
                format!(
                    "navmesh format: unsupported format {:?} (only {SUPPORTED_FORMAT:?} is supported)",
                    raw.format
                ),
            ));
        }
        if raw.polygons.is_empty() {
            return Err(Error::new(
                "content",
                "navmesh polygons: needs at least 1 polygon, has 0",
            ));
        }
        for (i, polygon) in raw.polygons.iter().enumerate() {
            if polygon.vertices.len() < 3 {
                return Err(Error::new(
                    "content",
                    format!(
                        "navmesh polygons[{i}].vertices: needs at least 3 points, has {}",
                        polygon.vertices.len()
                    ),
                ));
            }
        }

        Ok(Self {
            polygons: raw
                .polygons
                .into_iter()
                .map(|p| Polygon {
                    vertices: p.vertices,
                })
                .collect(),
        })
    }

    pub fn from_file(path: &Path) -> Result<Self> {
        let contents = std::fs::read_to_string(path)
            .map_err(|e| Error::wrap("content", format!("failed to read {}", path.display()), e))?;
        Self::from_json(&contents)
    }

    /// Resolves `asset_ref` through `store` (verifying its digest, per
    /// `AssetStore::resolve`) and parses the result as `navmesh_v1` —
    /// the actual `collision.asset_ref`/`collision.format` consumer this
    /// format has been missing since the manifest field was first added.
    pub fn from_asset_store(store: &AssetStore, asset_ref: &str) -> Result<Self> {
        let bytes = store.resolve(asset_ref)?;
        let text = String::from_utf8(bytes).map_err(|e| {
            Error::wrap(
                "content",
                format!("navmesh asset {asset_ref} is not valid UTF-8"),
                e,
            )
        })?;
        Self::from_json(&text)
    }

    /// The first polygon that actually contains `point` (3D, `z`
    /// included) — `None` if it lands outside every polygon. Exposed
    /// (not just a bare `bool`) so multi-floor resolution ("which floor
    /// am I on at this `(x, y)`") is directly testable: two polygons can
    /// share a projected footprint and still resolve to exactly one of
    /// them once `z` is checked.
    pub fn containing_polygon(&self, point: Point3) -> Option<&Polygon> {
        self.polygons.iter().find(|p| p.contains(point))
    }

    pub fn contains(&self, point: Point3) -> bool {
        self.containing_polygon(point).is_some()
    }
}

/// Standard ray-casting point-in-polygon test on a 2D projection —
/// deliberately not shared with `manifest`'s or `world::movement`'s own
/// copies (see `manifest::point_in_polygon`'s doc comment on why those
/// two already can't share one): this one operates on `Point3` vertices
/// projected down to `(x, y)`, not `manifest::Point`.
fn point_in_polygon_2d(point: (f64, f64), vertices: &[Point3]) -> bool {
    let (px, py) = point;
    let mut inside = false;

    let n = vertices.len();
    for i in 0..n {
        let (xi, yi, _) = vertices[i];
        let (xj, yj, _) = vertices[(i + n - 1) % n];

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

    fn flat_square(z: f64) -> Polygon {
        Polygon {
            vertices: vec![
                (0.0, 0.0, z),
                (10.0, 0.0, z),
                (10.0, 10.0, z),
                (0.0, 10.0, z),
            ],
        }
    }

    fn flat_navmesh_json(z: f64) -> String {
        format!(
            r#"{{"format":"navmesh_v1","polygons":[{{"vertices":[[0,0,{z}],[10,0,{z}],[10,10,{z}],[0,10,{z}]]}}]}}"#
        )
    }

    #[test]
    fn parses_a_valid_navmesh_v1_payload() {
        let mesh = NavMesh::from_json(&flat_navmesh_json(0.0)).unwrap();
        assert_eq!(mesh.polygons.len(), 1);
        assert_eq!(mesh.polygons[0].vertices.len(), 4);
    }

    #[test]
    fn rejects_an_unsupported_format() {
        let json = r#"{"format":"navmesh_v2","polygons":[]}"#;
        let err = NavMesh::from_json(json).unwrap_err();
        assert!(err.to_string().contains("unsupported format"), "{err}");
    }

    #[test]
    fn rejects_zero_polygons() {
        let json = r#"{"format":"navmesh_v1","polygons":[]}"#;
        let err = NavMesh::from_json(json).unwrap_err();
        assert!(err.to_string().contains("at least 1 polygon"), "{err}");
    }

    #[test]
    fn rejects_a_degenerate_polygon() {
        let json = r#"{"format":"navmesh_v1","polygons":[{"vertices":[[0,0,0],[1,1,0]]}]}"#;
        let err = NavMesh::from_json(json).unwrap_err();
        assert!(err.to_string().contains("polygons[0].vertices"), "{err}");
    }

    #[test]
    fn a_flat_polygon_contains_a_point_inside_it_at_the_right_height() {
        let polygon = flat_square(0.0);
        assert!(polygon.contains((5.0, 5.0, 0.0)));
        // Within height tolerance of the flat plane.
        assert!(polygon.contains((5.0, 5.0, 0.2)));
    }

    #[test]
    fn a_flat_polygon_rejects_a_point_outside_its_2d_projection() {
        let polygon = flat_square(0.0);
        assert!(!polygon.contains((50.0, 50.0, 0.0)));
    }

    #[test]
    fn a_flat_polygon_rejects_a_point_far_above_its_plane() {
        let polygon = flat_square(0.0);
        assert!(!polygon.contains((5.0, 5.0, 5.0)));
    }

    #[test]
    fn a_sloped_polygon_ramp_accepts_a_point_that_follows_the_slope() {
        // A ramp rising from z=0 at x=0 to z=10 at x=10.
        let ramp = Polygon {
            vertices: vec![
                (0.0, 0.0, 0.0),
                (10.0, 0.0, 10.0),
                (10.0, 10.0, 10.0),
                (0.0, 10.0, 0.0),
            ],
        };
        // Halfway across, the ramp's surface should be at roughly z=5.
        assert!(ramp.contains((5.0, 5.0, 5.0)));
        // Standing at ground level at the ramp's high end is not on its surface.
        assert!(!ramp.contains((10.0, 5.0, 0.0)));
    }

    #[test]
    fn multi_floor_overlapping_footprints_resolve_to_the_right_polygon() {
        // Ground floor and a second floor directly above it, sharing the
        // same (x, y) footprint — exactly the "second floor is more
        // navmesh geometry" scenario this format exists for.
        let ground = flat_square(0.0);
        let second_floor = flat_square(4.0);
        let mesh = NavMesh {
            polygons: vec![ground, second_floor],
        };

        let at_ground = mesh.containing_polygon((5.0, 5.0, 0.0)).unwrap();
        assert_eq!(at_ground.vertices[0].2, 0.0);

        let at_second_floor = mesh.containing_polygon((5.0, 5.0, 4.0)).unwrap();
        assert_eq!(at_second_floor.vertices[0].2, 4.0);

        // Between the two floors, at neither height, resolves to neither.
        assert!(mesh.containing_polygon((5.0, 5.0, 2.0)).is_none());
    }

    #[test]
    fn a_point_outside_every_polygon_is_not_contained() {
        let mesh = NavMesh::from_json(&flat_navmesh_json(0.0)).unwrap();
        assert!(!mesh.contains((500.0, 500.0, 0.0)));
    }

    #[test]
    fn a_vertical_polygon_never_contains_anything() {
        // A wall: constant x, varying y/z — Newell's normal here has a
        // ~zero z component.
        let wall = Polygon {
            vertices: vec![
                (0.0, 0.0, 0.0),
                (0.0, 10.0, 0.0),
                (0.0, 10.0, 10.0),
                (0.0, 0.0, 10.0),
            ],
        };
        assert!(!wall.contains((0.0, 5.0, 5.0)));
    }

    #[test]
    fn loads_a_navmesh_through_the_asset_store() {
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let unique = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "wz-navmesh-asset-store-test-{}-{unique}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();

        let json = flat_navmesh_json(0.0);
        let hash = {
            use sha2::{Digest, Sha256};
            Sha256::digest(json.as_bytes())
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect::<String>()
        };
        std::fs::write(dir.join(&hash), &json).unwrap();

        let store = AssetStore::new(dir.clone());
        let mesh = NavMesh::from_asset_store(&store, &format!("sha256:{hash}")).unwrap();
        assert_eq!(mesh.polygons.len(), 1);

        let _ = std::fs::remove_dir_all(&dir);
    }
}
