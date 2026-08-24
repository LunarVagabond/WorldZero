//! Guest-side test fixture compiled to a `wasm32-wasip2` component
//! against `../../../wit/plugin.wit` — used by `plugin-host`'s
//! integration tests (#37/#38's acceptance criteria), not shipped
//! anywhere. Three behaviors selected at build time via Cargo features
//! so one crate covers the "well-behaved plugin," "panics," and
//! "attempts a sandbox escape" scenarios without three near-identical crates.

wit_bindgen::generate!({
    path: "../../../wit",
    world: "plugin",
    generate_all,
});

use crate::exports::worldzero::plugin::hooks::Guest;

struct Plugin;

impl Guest for Plugin {
    fn on_load() {
        #[cfg(feature = "panic_on_load")]
        panic!("deliberate test panic — proves a trap doesn't crash the host process");

        #[cfg(feature = "escape_attempt")]
        {
            // No filesystem is preopened for this plugin (`PluginHost::load`
            // grants nothing beyond the `host` interface) — this must fail.
            // Panicking on an unexpected *success* turns "the sandbox held"
            // into something the host-side test can observe as this hook
            // call returning `Ok(())` rather than needing to inspect guest
            // internals it has no access to anyway.
            if std::fs::read_to_string("/etc/passwd").is_ok() {
                panic!("sandbox escape: read a file with no preopened filesystem access");
            }
        }

        #[cfg(not(any(feature = "panic_on_load", feature = "escape_attempt")))]
        {
            let _ = worldzero::plugin::host::spawn_npc("wolf-pack-01");
        }
    }

    fn on_unload() {}

    fn on_entity_spawn(_entity_id: String, _entity_type: String) {}

    fn on_interact(trigger_id: String, actor_entity_id: String) {
        let _ = worldzero::plugin::host::send_message(
            &actor_entity_id,
            &format!("you interacted with {trigger_id}"),
        );
    }

    fn on_message(message_type: u16, sender_entity_id: String, payload: Vec<u8>) {
        let body = String::from_utf8_lossy(&payload);
        let _ = worldzero::plugin::host::send_message(
            &sender_entity_id,
            &format!("on-message {message_type}: {body}"),
        );
    }

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

    fn on_chat_command(command: String, args: String, sender_entity_id: String) {
        if command == "give" {
            let _ = worldzero::plugin::host::grant_item(&sender_entity_id, &args, 1);
            return;
        }
        if command == "whoami" {
            let roles = worldzero::plugin::host::caller_role(&sender_entity_id)
                .unwrap_or_default()
                .join(",");
            let _ = worldzero::plugin::host::send_message(
                &sender_entity_id,
                &format!("roles: {roles}"),
            );
            return;
        }
        // Exercises plugin-state-get/set (#149) end to end: "remember X"
        // writes X under zone scope, "recall" reads it back — a real
        // round trip through the actual sandboxed call boundary, not
        // just a host-side fake.
        if command == "remember" {
            let _ = worldzero::plugin::host::plugin_state_set(
                &worldzero::plugin::host::PluginStateScope::Zone("test-zone".to_string()),
                "note",
                args.as_bytes(),
            );
            return;
        }
        if command == "recall" {
            let value = worldzero::plugin::host::plugin_state_get(
                &worldzero::plugin::host::PluginStateScope::Zone("test-zone".to_string()),
                "note",
            )
            .ok()
            .flatten()
            .map(|bytes| String::from_utf8_lossy(&bytes).to_string())
            .unwrap_or_else(|| "<nothing remembered>".to_string());
            let _ = worldzero::plugin::host::send_message(
                &sender_entity_id,
                &format!("recalled: {value}"),
            );
            return;
        }
        let _ = worldzero::plugin::host::send_message(
            &sender_entity_id,
            &format!("ran command {command} with args {args}"),
        );
    }

    fn on_item_acquire(entity_id: String, item_type: String, new_quantity: i64) {
        let _ = worldzero::plugin::host::send_message(
            &entity_id,
            &format!("acquired {item_type}, now have {new_quantity}"),
        );
    }

    // "Using" an item removes it and pays out a bit of currency — a
    // minimal but real exercise of remove-item/modify-currency, not
    // meant to model an actual game economy.
    fn on_item_use(entity_id: String, item_type: String) {
        let _ = worldzero::plugin::host::remove_item(&entity_id, &item_type, 1);
        let _ = worldzero::plugin::host::modify_currency(&entity_id, 5);
    }
}

export!(Plugin);
