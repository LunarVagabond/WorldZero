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

    // Live: fires once this connection has fully joined the zone (#155).
    fn on_player_join_zone(entity_id: String) {
        let _ = worldzero::plugin::host::send_message(
            &entity_id,
            "the wolves catch your scent as you arrive",
        );
    }

    // Live: fires on clean disconnect (#155).
    fn on_player_leave_zone(entity_id: String) {
        let _ = worldzero::plugin::host::send_message(&entity_id, "the wolves watch you go");
    }

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

    // No live host call site exists yet for any of the four hooks below
    // (wit/plugin.wit's doc comments explain why per hook) — implemented
    // as no-ops/minimal stand-ins here only because a WIT world's
    // exports aren't individually optional in v0, not because this
    // example plugin actually uses them yet.

    fn on_damage_calc(
        _attacker_entity_id: String,
        target_entity_id: String,
        stat_key: String,
        base_amount: i64,
    ) {
        let _ = worldzero::plugin::host::apply_stat_delta(&target_entity_id, &stat_key, -base_amount);
    }

    fn on_death(_entity_id: String) {}

    fn on_respawn(_entity_id: String) {}

    // This one *is* live: `wolf-pack-01`'s spawn table declares
    // `route_id: wolf-patrol-01` (config/zone.manifest.example.yaml), so
    // the wolf spawned in `on_load` above ticks along its patrol route —
    // just walk to the next declared waypoint each tick, looping ignored
    // for this minimal example (a real plugin would track progress and
    // advance through the list).
    fn on_npc_tick(
        entity_id: String,
        _x: f64,
        _y: f64,
        route_waypoints: Vec<(f64, f64)>,
        _route_loop: bool,
        _route_speed: f64,
        _dt: f64,
    ) {
        if let Some((wx, wy)) = route_waypoints.first() {
            let _ = worldzero::plugin::host::move_entity(&entity_id, *wx, *wy);
        }
    }

    fn on_npc_interact(npc_entity_id: String, actor_entity_id: String) {
        let _ = worldzero::plugin::host::send_message(
            &actor_entity_id,
            &format!("you interacted with npc {npc_entity_id}"),
        );
    }

    // Live: this plugin's `plugin.toml` declares `chat_commands = ["wave"]`.
    // Also grants a `wolf-fang` trinket — the shipped example's live
    // demonstration of `grant-item`/`on-item-acquire` (#57/#112).
    fn on_chat_command(command: String, args: String, sender_entity_id: String) {
        let _ = worldzero::plugin::host::send_message(
            &sender_entity_id,
            &format!("{sender_entity_id} used /{command} {args} — *the wolves howl*"),
        );
        let _ = worldzero::plugin::host::grant_item(&sender_entity_id, "wolf-fang", 1);
    }

    // Live: fires once `grant-item` (below) actually lands.
    fn on_item_acquire(entity_id: String, item_type: String, new_quantity: i64) {
        let _ = worldzero::plugin::host::send_message(
            &entity_id,
            &format!("you now have {new_quantity} {item_type}"),
        );
    }

    // No live host call site exists yet, same caveat as on_damage_calc
    // and friends above — nothing in the client protocol has a "use an
    // item" action yet.
    fn on_item_use(_entity_id: String, _item_type: String) {}
}

export!(Plugin);
