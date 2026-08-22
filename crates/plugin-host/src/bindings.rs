//! Generated Rust bindings for `wit/plugin.wit`'s `plugin` world — the
//! typed host/guest boundary (docs/PROPOSAL.md, "Interface Technology":
//! WASM Component Model + WIT, not a hand-rolled ABI).

wasmtime::component::bindgen!({
    path: "wit",
    world: "plugin",
});
