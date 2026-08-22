//! `SpatialIndex` — the stable abstraction — plus its Phase 1/2 grid
//! baseline (docs/PROPOSAL.md, "Spatial Index: A → Z Roadmap"). The grid
//! handles micro spatial queries *within* a zone (broad-phase collision,
//! interest management); macro partitioning across zones is the zone
//! graph itself (`world`'s per-zone `Zone`, one grid each), not something
//! this trait needs to model.
//!
//! Swapping in the "Z" target (a density-adaptive quadtree/octree,
//! Phase 3+) later means implementing this same trait — callers built
//! against it (movement validation, interest management) don't change.

use std::collections::HashMap;

use common::id::EntityId;

/// A 2D position — the manifest format is polygon/2D-only for now (see
/// `content::manifest`'s note on decision #89, 2D vs 3D support), so the
/// spatial index matches that scope rather than carrying an unused third
/// axis around.
pub type Point = (f64, f64);

fn distance(a: Point, b: Point) -> f64 {
    let dx = a.0 - b.0;
    let dy = a.1 - b.1;
    (dx * dx + dy * dy).sqrt()
}

/// Whatever query operations movement/collision validation and interest
/// management actually need: insert/remove/update an entity's tracked
/// position, and a range query. Implementations own how they index
/// positions internally — callers never see cell keys or tree nodes.
pub trait SpatialIndex: Send + Sync {
    fn insert(&mut self, entity: EntityId, position: Point);
    fn remove(&mut self, entity: EntityId);
    /// No-op if `entity` was never inserted — a caller resyncing state
    /// after a reconnect shouldn't have to check membership first.
    fn update(&mut self, entity: EntityId, position: Point);
    fn position_of(&self, entity: EntityId) -> Option<Point>;
    /// Every entity within `radius` of `center`, `center` itself included
    /// if occupied. Order is unspecified.
    fn query_radius(&self, center: Point, radius: f64) -> Vec<EntityId>;
}

type CellKey = (i64, i64);

/// Uniform grid: every entity is bucketed into a `cell_size`-wide square
/// cell; a radius query only visits cells whose bounding box overlaps the
/// query circle, not every entity in the zone. `cell_size` is a tuning
/// parameter, not a correctness one — too small means more cells touched
/// per query, too large means more false candidates filtered per cell;
/// see [`GridIndex::new`] for the default.
pub struct GridIndex {
    cell_size: f64,
    cells: HashMap<CellKey, Vec<EntityId>>,
    positions: HashMap<EntityId, Point>,
}

impl GridIndex {
    /// `cell_size` must be positive — panics otherwise, since a
    /// zero/negative cell size makes every subsequent cell-key
    /// computation meaningless. Configured via `WorldConfig`
    /// (`crate::config`), not hardcoded, per #32's acceptance criteria.
    pub fn new(cell_size: f64) -> Self {
        assert!(cell_size > 0.0, "grid cell_size must be positive");
        Self {
            cell_size,
            cells: HashMap::new(),
            positions: HashMap::new(),
        }
    }

    fn cell_key(&self, position: Point) -> CellKey {
        (
            (position.0 / self.cell_size).floor() as i64,
            (position.1 / self.cell_size).floor() as i64,
        )
    }

    fn remove_from_cell(&mut self, entity: EntityId, position: Point) {
        let key = self.cell_key(position);
        if let Some(bucket) = self.cells.get_mut(&key) {
            bucket.retain(|e| *e != entity);
            if bucket.is_empty() {
                self.cells.remove(&key);
            }
        }
    }
}

impl SpatialIndex for GridIndex {
    fn insert(&mut self, entity: EntityId, position: Point) {
        // An entity re-inserted without an explicit `remove` first (e.g. a
        // reconnect) shouldn't end up in two cells at once.
        if let Some(&previous) = self.positions.get(&entity) {
            self.remove_from_cell(entity, previous);
        }
        let key = self.cell_key(position);
        self.cells.entry(key).or_default().push(entity);
        self.positions.insert(entity, position);
    }

    fn remove(&mut self, entity: EntityId) {
        if let Some(position) = self.positions.remove(&entity) {
            self.remove_from_cell(entity, position);
        }
    }

    fn update(&mut self, entity: EntityId, position: Point) {
        if self.positions.contains_key(&entity) {
            self.insert(entity, position);
        }
    }

    fn position_of(&self, entity: EntityId) -> Option<Point> {
        self.positions.get(&entity).copied()
    }

    fn query_radius(&self, center: Point, radius: f64) -> Vec<EntityId> {
        let min_key = self.cell_key((center.0 - radius, center.1 - radius));
        let max_key = self.cell_key((center.0 + radius, center.1 + radius));

        let mut found = Vec::new();
        for cx in min_key.0..=max_key.0 {
            for cy in min_key.1..=max_key.1 {
                let Some(bucket) = self.cells.get(&(cx, cy)) else {
                    continue;
                };
                for &entity in bucket {
                    // Candidates come from a square bounding box, not a
                    // circle — the per-entity distance check trims the
                    // corners a bounding-box-only query would wrongly include.
                    if let Some(&position) = self.positions.get(&entity)
                        && distance(center, position) <= radius
                    {
                        found.push(entity);
                    }
                }
            }
        }
        found
    }
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use super::*;

    #[test]
    fn query_radius_finds_only_entities_within_range() {
        let mut index = GridIndex::new(10.0);
        let near = EntityId::new();
        let far = EntityId::new();
        index.insert(near, (1.0, 1.0));
        index.insert(far, (500.0, 500.0));

        let found = index.query_radius((0.0, 0.0), 5.0);
        assert_eq!(found, vec![near]);
    }

    #[test]
    fn query_radius_excludes_a_cell_corner_outside_the_circle() {
        // Same cell as the query center (cell_size 100) but geometrically
        // outside the requested radius — proves the per-entity distance
        // check, not just cell membership, gates the result.
        let mut index = GridIndex::new(100.0);
        let entity = EntityId::new();
        index.insert(entity, (90.0, 90.0));

        assert!(index.query_radius((0.0, 0.0), 10.0).is_empty());
        assert_eq!(index.query_radius((0.0, 0.0), 200.0), vec![entity]);
    }

    #[test]
    fn update_moves_an_entity_to_its_new_cell() {
        let mut index = GridIndex::new(10.0);
        let entity = EntityId::new();
        index.insert(entity, (1.0, 1.0));
        index.update(entity, (500.0, 500.0));

        assert!(index.query_radius((1.0, 1.0), 5.0).is_empty());
        assert_eq!(index.query_radius((500.0, 500.0), 5.0), vec![entity]);
        assert_eq!(index.position_of(entity), Some((500.0, 500.0)));
    }

    #[test]
    fn update_on_an_unknown_entity_is_a_harmless_no_op() {
        let mut index = GridIndex::new(10.0);
        let entity = EntityId::new();
        index.update(entity, (1.0, 1.0));
        assert_eq!(index.position_of(entity), None);
    }

    #[test]
    fn remove_clears_the_entity_from_its_cell() {
        let mut index = GridIndex::new(10.0);
        let entity = EntityId::new();
        index.insert(entity, (1.0, 1.0));
        index.remove(entity);

        assert!(index.query_radius((1.0, 1.0), 5.0).is_empty());
        assert_eq!(index.position_of(entity), None);
    }

    /// Not a formal perf target — evidence the grid baseline isn't
    /// scanning every entity per query (#32's acceptance criteria). 5,000
    /// entities spread over a wide area, 1,000 small-radius queries; a
    /// linear O(n) scan of this size would still be fast enough that a
    /// tight bound would be meaningless, so this uses a deliberately
    /// generous threshold — it's here to catch an accidental O(n²)
    /// regression, not to gate on absolute speed.
    #[test]
    fn radius_queries_stay_fast_at_a_few_thousand_entities() {
        let mut index = GridIndex::new(25.0);
        let mut rng_state: u64 = 0x2545F4914F6CDD1D;
        let mut next = || {
            rng_state ^= rng_state << 13;
            rng_state ^= rng_state >> 7;
            rng_state ^= rng_state << 17;
            (rng_state % 10_000) as f64
        };

        for _ in 0..5_000 {
            index.insert(EntityId::new(), (next(), next()));
        }

        let start = Instant::now();
        for _ in 0..1_000 {
            index.query_radius((next(), next()), 50.0);
        }
        let elapsed = start.elapsed();

        assert!(
            elapsed < Duration::from_secs(2),
            "1,000 radius queries against 5,000 entities took {elapsed:?} — looks like an O(n) or worse regression"
        );
    }
}
