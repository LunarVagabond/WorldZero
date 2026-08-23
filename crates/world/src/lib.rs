//! Per-zone simulation: tick loop, spatial index, and authoritative movement/collision.
//!
//! Design: docs/PROPOSAL.md ("Spatial Index: A -> Z Roadmap") and docs/architecture/System_Architecture.md.

pub mod config;
pub mod links;
pub mod movement;
pub mod spatial;
pub mod zone;

pub use config::WorldConfig;
pub use links::crossed_link;
pub use movement::{MovementRejection, validate_movement};
pub use spatial::{GridIndex, Point, SpatialIndex};
pub use zone::{EntityKind, MovementOutcome, Zone};
