//! WASM runtime, sandboxing, and the host API surface exposed to plugins.
//!
//! Design: docs/PROPOSAL.md ("Plugin System") and docs/specs/Plugin_API.md.

mod bindings;
pub mod manifest;
pub mod runtime;

pub use manifest::{HOST_API_VERSION, PluginManifest, check_no_collisions};
pub use runtime::{HostCallbacks, LoadedPlugin, PluginHost, PluginStateScope};
