//! Self-registration for live service instances — see
//! [Decision #132](https://github.com/LunarVagabond/WorldZero/issues/132)
//! and implementation ticket
//! [#134](https://github.com/LunarVagabond/WorldZero/issues/134).
//!
//! No callers yet: `server` is still one combined process (see
//! `crates/server/src/zone_registry.rs`), so nothing has an independent
//! instance identity to register. This crate exists so that story is
//! ready once [#130](https://github.com/LunarVagabond/WorldZero/issues/130)
//! (process lifecycle for horizontal scaling) actually splits `server`
//! into multiple processes/machines.

pub mod registry;

pub use registry::{
    InstanceInfo, InstanceMetadata, RegistryEvent, RegistryEventKind, ServiceRegistry,
};
