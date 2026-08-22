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
}

export!(Plugin);
