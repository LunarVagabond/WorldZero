//! A second, independently-authored guest-side test fixture — used
//! alongside `test-plugin` to prove real multi-plugin support (#152):
//! two distinct compiled `.wasm` components loaded into the same
//! `server` process at once, each with its own hooks, capabilities, and
//! declared `message_types`/`chat_commands`, never colliding with
//! `test-plugin`'s own. Not shipped anywhere, same as `test-plugin`.

wit_bindgen::generate!({
    path: "../../../wit",
    world: "plugin",
    generate_all,
});

use crate::exports::worldzero::plugin::hooks::Guest;

struct Plugin;

impl Guest for Plugin {
    fn on_load() {}

    fn on_unload() {}

    fn on_zone_loaded(_zone_id: String) {}

    fn on_entity_spawn(_zone_id: String, _entity_id: String, _entity_type: String) {}

    // Distinct wording from test-plugin's own on-player-join-zone
    // greeting — a real end-to-end test can tell the two apart by
    // message content alone, proving both actually fired independently
    // for the same event (fan-out, #152).
    fn on_player_join_zone(_zone_id: String, entity_id: String) {
        let _ = worldzero::plugin::host::send_message(
            &entity_id,
            &format!("second-plugin also welcomes {entity_id}"),
        );
    }

    fn on_player_leave_zone(_zone_id: String, _entity_id: String) {}

    fn on_interact(_zone_id: String, _trigger_id: String, _actor_entity_id: String) {}

    // Deliberately a different message_type than test-plugin's 1000 —
    // proves two plugins with distinct declared message_types coexist
    // without any collision handling ever kicking in.
    fn on_message(
        _zone_id: String,
        message_type: u16,
        sender_entity_id: String,
        payload: Vec<u8>,
    ) {
        let body = String::from_utf8_lossy(&payload);
        let _ = worldzero::plugin::host::send_message(
            &sender_entity_id,
            &format!("second-plugin on-message {message_type}: {body}"),
        );
    }

    fn on_damage_calc(
        _zone_id: String,
        _attacker_entity_id: String,
        _target_entity_id: String,
        _stat_key: String,
        _base_amount: i64,
    ) {
    }

    fn on_death(_zone_id: String, _entity_id: String) {}

    fn on_respawn(_zone_id: String, _entity_id: String) {}

    fn on_npc_tick(
        _zone_id: String,
        _entity_id: String,
        _x: f64,
        _y: f64,
        _route_waypoints: Vec<(f64, f64)>,
        _route_loop: bool,
        _route_speed: f64,
        _dt: f64,
    ) {
    }

    fn on_npc_interact(_zone_id: String, _npc_entity_id: String, _actor_entity_id: String) {}

    // Deliberately a different command name than test-plugin declares —
    // same "no collision" proof as on-message above, for chat_commands.
    fn on_chat_command(
        _zone_id: String,
        command: String,
        args: String,
        sender_entity_id: String,
    ) {
        let _ = worldzero::plugin::host::send_message(
            &sender_entity_id,
            &format!("second-plugin ran command {command} with args {args}"),
        );
    }

    fn on_item_acquire(
        _zone_id: String,
        _entity_id: String,
        _item_type: String,
        _new_quantity: i64,
    ) {
    }

    fn on_item_use(_zone_id: String, _entity_id: String, _item_type: String) {}
}

export!(Plugin);
