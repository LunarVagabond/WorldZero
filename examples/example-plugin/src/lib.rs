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
    fn on_load() {}

    fn on_unload() {}

    // Not declared in this plugin's own `hooks` list (it never needed a
    // starting-stat/archetype system), but the WIT world still requires
    // every plugin to export it (exports aren't individually optional in
    // the Component Model) — an empty body is the correct "not
    // interested" implementation, same as on_entity_spawn below.
    fn on_character_create(_character_id: String, _zone_id: String) {}

    // One plugin instance now serves every zone (#152) — spawning
    // "wolf-pack-01" lives here, not `on_load`, since `on_load` no
    // longer has any zone context. This example only ever runs against
    // one zone, so it spawns unconditionally; a plugin covering several
    // zones would check `zone_id` here to decide what (if anything) to
    // seed for each one.
    fn on_zone_loaded(_zone_id: String) {
        let _ = worldzero::plugin::host::spawn_npc("wolf-pack-01");
    }

    fn on_entity_spawn(_zone_id: String, _entity_id: String, _entity_type: String) {}

    // Live: fires once this connection has fully joined the zone (#155).
    fn on_player_join_zone(_zone_id: String, entity_id: String) {
        let _ = worldzero::plugin::host::send_message(
            &entity_id,
            "the wolves catch your scent as you arrive",
        );
    }

    // Live: fires on clean disconnect (#155).
    fn on_player_leave_zone(_zone_id: String, entity_id: String) {
        let _ = worldzero::plugin::host::send_message(&entity_id, "the wolves watch you go");
    }

    fn on_interact(_zone_id: String, trigger_id: String, actor_entity_id: String) {
        let _ = worldzero::plugin::host::send_message(
            &actor_entity_id,
            &format!("you interacted with {trigger_id}"),
        );
    }

    // Echoes back any message_type 1000 envelope (declared in this
    // plugin's own `plugin.toml`) — the "gateway message routed to a
    // plugin" case, live end to end (#95), unlike `on_interact` above
    // (see this crate's module doc).
    fn on_message(_zone_id: String, message_type: u16, sender_entity_id: String, payload: Vec<u8>) {
        let body = String::from_utf8_lossy(&payload);
        let _ = worldzero::plugin::host::send_message(
            &sender_entity_id,
            &format!("plugin got message_type {message_type}: {body}"),
        );
    }

    // Live: fires when a client sends an Attack action (#154) —
    // base_amount is always 0 (the core never invents a damage number),
    // so this fixed 5-point wolf bite is entirely this plugin's own
    // formula, same as any real game's damage calc would be.
    fn on_damage_calc(
        _zone_id: String,
        attacker_entity_id: String,
        target_entity_id: String,
        stat_key: String,
        _base_amount: i64,
    ) {
        let _ = worldzero::plugin::host::apply_stat_delta(&target_entity_id, &stat_key, -5);
        let _ = worldzero::plugin::host::send_message(
            &attacker_entity_id,
            &format!("a wolf bites {target_entity_id} for 5 {stat_key}"),
        );
    }

    // Live: fires once this plugin's own report-death call (wherever it
    // decides a declared stat has hit a meaningful threshold) is applied
    // (#154) — this minimal example never calls report-death itself, so
    // in practice this only fires if some other plugin does once #152's
    // multi-plugin support is actually exercised by more than one plugin
    // at a time.
    fn on_death(_zone_id: String, entity_id: String) {
        let _ = worldzero::plugin::host::send_message(&entity_id, "the wolves finish you off");
    }

    fn on_respawn(_zone_id: String, entity_id: String) {
        let _ = worldzero::plugin::host::send_message(
            &entity_id,
            "you wake up, wolves nowhere in sight",
        );
    }

    // This one *is* live: `wolf-pack-01`'s spawn table declares
    // `route_id: wolf-patrol-01` (config/zone.manifest.example.yaml), so
    // the wolf spawned in `on_zone_loaded` above ticks along its patrol
    // route — just walk to the next declared waypoint each tick, looping
    // ignored for this minimal example (a real plugin would track
    // progress and advance through the list).
    fn on_npc_tick(
        _zone_id: String,
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

    fn on_npc_interact(_zone_id: String, npc_entity_id: String, actor_entity_id: String) {
        let _ = worldzero::plugin::host::send_message(
            &actor_entity_id,
            &format!("you interacted with npc {npc_entity_id}"),
        );
    }

    // Live: this plugin's `plugin.toml` declares `chat_commands = ["wave"]`.
    // Also grants a `wolf-fang` trinket — the shipped example's live
    // demonstration of `grant-item`/`on-item-acquire` (#57/#112).
    fn on_chat_command(
        _zone_id: String,
        command: String,
        args: String,
        sender_entity_id: String,
    ) {
        let _ = worldzero::plugin::host::send_message(
            &sender_entity_id,
            &format!("{sender_entity_id} used /{command} {args} — *the wolves howl*"),
        );
        let _ = worldzero::plugin::host::grant_item(&sender_entity_id, "wolf-fang", 1);
    }

    // Live: fires once `grant-item` (below) actually lands.
    fn on_item_acquire(_zone_id: String, entity_id: String, item_type: String, new_quantity: i64) {
        let _ = worldzero::plugin::host::send_message(
            &entity_id,
            &format!("you now have {new_quantity} {item_type}"),
        );
    }

    // Live: fires when a client sends a UseItem action (#154) — the core
    // never validates ownership itself, so this fixed response fires
    // even for an item this connection doesn't actually own; a real
    // plugin wanting to prevent that checks its own bookkeeping first.
    fn on_item_use(_zone_id: String, entity_id: String, item_type: String) {
        let _ = worldzero::plugin::host::send_message(
            &entity_id,
            &format!("you use the {item_type} — the wolves eye it warily"),
        );
    }

    // Live: fires once a CraftItem request actually consumes its inputs
    // and grants its output (#216). No entity id is given here (a craft
    // is character-scoped, not entity-scoped — same reasoning
    // on_character_create's own character_id-only signature follows),
    // so there's no connection to send-message a reply to; a real game
    // would typically call apply-stat-delta-for-character here (e.g. a
    // profession XP bonus) the way `test-plugin`'s own on_craft_complete
    // fixture does.
    fn on_craft_complete(_character_id: String, _recipe_key: String) {}
}

export!(Plugin);
