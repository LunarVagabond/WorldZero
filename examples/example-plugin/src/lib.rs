//! The shipped example plugin — "one example plugin" per
//! docs/PROPOSAL.md's Developer Experience Bar (#43's scaffold), doing
//! exactly the v0 slice's minimal case (docs/specs/Plugin_API.md): spawn
//! one NPC, respond to one interaction. Deliberately paired with
//! `config/zone.manifest.example.yaml`'s existing `wolf-pack-01` spawn
//! table and `forest-entrance` trigger — the shipped example zone and
//! this shipped example plugin work together out of the box, no extra
//! content authoring needed to see something happen.
//!
//! Build with `rustup target add wasm32-wasip2` then
//! `cargo build --manifest-path examples/example-plugin/Cargo.toml
//! --target wasm32-wasip2 --release` (or just `make quickstart`, which
//! does this for you). Not a workspace member on purpose — it targets
//! `wasm32-wasip2`, a different target than the rest of the workspace
//! ever needs, so keeping it out avoids pulling that target requirement
//! into an ordinary `cargo build --workspace`.

wit_bindgen::generate!({
    path: "../../crates/plugin-host/wit",
    world: "plugin",
    generate_all,
});

use crate::exports::worldzero::plugin::hooks::Guest;

struct Plugin;

impl Guest for Plugin {
    fn on_load() {
        let _ = worldzero::plugin::host::spawn_npc("wolf-pack-01");
    }

    fn on_unload() {}

    fn on_entity_spawn(_entity_id: String, _entity_type: String) {}

    fn on_interact(trigger_id: String, actor_entity_id: String) {
        let _ = worldzero::plugin::host::send_message(
            &actor_entity_id,
            &format!("you interacted with {trigger_id}"),
        );
    }

    // Echoes back any message_type 1000 envelope (declared in this
    // plugin's own `plugin.toml`) — the "gateway message routed to a
    // plugin" case, live end to end (#95), unlike `on_interact` above
    // (see this crate's module doc).
    fn on_message(message_type: u16, sender_entity_id: String, payload: Vec<u8>) {
        let body = String::from_utf8_lossy(&payload);
        let _ = worldzero::plugin::host::send_message(
            &sender_entity_id,
            &format!("plugin got message_type {message_type}: {body}"),
        );
    }
}

export!(Plugin);
