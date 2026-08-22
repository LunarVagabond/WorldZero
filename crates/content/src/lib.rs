//! Map/NPC/route manifest loading, versioning, and validation.
//!
//! Design: docs/PROPOSAL.md ("World Content: Maps, NPCs, and Routes") and
//! docs/specs/Content_Manifest_Spec.md.

pub mod content_pack;
pub mod manifest;

pub use content_pack::ContentPack;
pub use manifest::ZoneManifest;
