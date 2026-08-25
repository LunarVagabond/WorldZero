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
    }

    fn on_unload() {}

    // One plugin instance now serves every zone (#152) — spawning
    // "wolf-pack-01" moved here from on_load, since on_load no longer has
    // any zone context. This fixture doesn't care which zone; a real
    // plugin would check zone_id if it only wanted this for specific zones.
    fn on_zone_loaded(_zone_id: String) {
        #[cfg(not(any(feature = "panic_on_load", feature = "escape_attempt")))]
        {
            let _ = worldzero::plugin::host::spawn_npc("wolf-pack-01");
        }
    }

    // Exercises apply-stat-delta-for-character end to end (#194) — sets a
    // starting stat on a character with no entity/session yet, using the
    // character-id-scoped host function `apply-stat-delta` can't reach
    // for (there's no entity id at this point).
    fn on_character_create(character_id: String, _zone_id: String) {
        let _ = worldzero::plugin::host::apply_stat_delta_for_character(
            &character_id,
            "reputation.ironclad_guild",
            25,
        );
    }

    fn on_entity_spawn(_zone_id: String, _entity_id: String, _entity_type: String) {}

    fn on_player_join_zone(_zone_id: String, entity_id: String) {
        let _ = worldzero::plugin::host::send_message(&entity_id, &format!("welcome, {entity_id}"));
    }

    // Exercises a leave hook still being able to reach the departing
    // entity's own character-backed storage (#155) — a plugin might want
    // to record a farewell bonus, close out a timed buff, etc. Also
    // records the departing entity id under zone-scope plugin state
    // (#149) — the only way a black-box, network-only end-to-end test can
    // observe this hook fired for real, since the departing connection
    // itself is gone by the time it would receive any reply.
    fn on_player_leave_zone(zone_id: String, entity_id: String) {
        let _ =
            worldzero::plugin::host::apply_stat_delta(&entity_id, "reputation.ironclad_guild", 1);
        let _ = worldzero::plugin::host::plugin_state_set(
            &worldzero::plugin::host::PluginStateScope::Zone(zone_id),
            "last-left-entity",
            entity_id.as_bytes(),
        );
    }

    fn on_interact(_zone_id: String, trigger_id: String, actor_entity_id: String) {
        let _ = worldzero::plugin::host::send_message(
            &actor_entity_id,
            &format!("you interacted with {trigger_id}"),
        );
    }

    fn on_message(zone_id: String, message_type: u16, sender_entity_id: String, payload: Vec<u8>) {
        let body = String::from_utf8_lossy(&payload);
        // Reads back what `on_player_leave_zone` recorded (#155) — the
        // only way a black-box, network-only test can observe that hook
        // fired, since the departing connection is already gone by the
        // time it would receive a reply of its own.
        if body == "last-left" {
            let value = worldzero::plugin::host::plugin_state_get(
                &worldzero::plugin::host::PluginStateScope::Zone(zone_id),
                "last-left-entity",
            )
            .ok()
            .flatten()
            .map(|bytes| String::from_utf8_lossy(&bytes).to_string())
            .unwrap_or_else(|| "<nobody has left yet>".to_string());
            let _ = worldzero::plugin::host::send_message(
                &sender_entity_id,
                &format!("last-left: {value}"),
            );
            return;
        }
        // Reads back what `on_death` recorded (#197) — same reasoning as
        // `last-left` above, but for an NPC target: it has no connection
        // of its own for `on_death`'s direct `send_message` to reach.
        if body == "last-death" {
            let value = worldzero::plugin::host::plugin_state_get(
                &worldzero::plugin::host::PluginStateScope::Zone(zone_id),
                "last-death-entity",
            )
            .ok()
            .flatten()
            .map(|bytes| String::from_utf8_lossy(&bytes).to_string())
            .unwrap_or_else(|| "<nobody has died yet>".to_string());
            let _ = worldzero::plugin::host::send_message(
                &sender_entity_id,
                &format!("last-death: {value}"),
            );
            return;
        }
        // Exercises report-death/report-respawn (#154) end to end: the
        // plugin decides "died"/"respawned" for its own reasons (here,
        // just because the client asked) and reports it; the resulting
        // on-death/on-respawn call back is what actually confirms to the
        // client it happened.
        if body == "die" {
            let _ = worldzero::plugin::host::report_death(&sender_entity_id);
            return;
        }
        if body == "respawn" {
            let _ = worldzero::plugin::host::report_respawn(&sender_entity_id);
            return;
        }
        let _ = worldzero::plugin::host::send_message(
            &sender_entity_id,
            &format!("on-message {message_type}: {body}"),
        );
    }

    fn on_damage_calc(
        _zone_id: String,
        attacker_entity_id: String,
        target_entity_id: String,
        stat_key: String,
        base_amount: i64,
    ) {
        // The core never invents `base_amount` (#154, always 0) — a real
        // plugin would compute its own damage here (weapon data, roll,
        // whatever); this fixture just applies a fixed 3-point hit so the
        // effect is observable, and confirms to the attacker it landed.
        // `apply-stat-delta` works against the target's real, declared,
        // schema-validated stat regardless of whether it's a player or
        // an NPC entity (#197) — the core resolves which storage that is
        // itself, this call site doesn't need to know or care.
        let _ = worldzero::plugin::host::apply_stat_delta(&target_entity_id, &stat_key, -3);
        let _ = worldzero::plugin::host::send_message(
            &attacker_entity_id,
            &format!("hit {target_entity_id} for 3 {stat_key} (base_amount was {base_amount})"),
        );

        // `apply-stat-delta` is fire-and-forget (queued, applied on the
        // next tick's drain — see its own doc comment), so this hook
        // never gets a synchronous read of the real value it just wrote.
        // Deciding "dead" instead uses a small combat-scoped counter of
        // its own (`plugin-state`'s `entity` scope: in-memory only,
        // read-your-own-write within the same session, exactly suited to
        // this) — the plugin's own choice of when a target dies, same
        // "core has no notion of HP or a death condition" discipline as
        // any other death decision (#154). Composes with #197's NPC stat
        // storage the same way it already does for a player target: the
        // core doesn't care which kind of entity `target_entity_id` is.
        let scope = worldzero::plugin::host::PluginStateScope::Entity(target_entity_id.clone());
        let remaining_before_this_hit = worldzero::plugin::host::plugin_state_get(
            &scope,
            "combat-hits-remaining",
        )
        .ok()
        .flatten()
        .and_then(|bytes| std::str::from_utf8(&bytes).ok().and_then(|s| s.parse::<i64>().ok()))
        .unwrap_or(3);
        let remaining_after_this_hit = remaining_before_this_hit - 1;
        if remaining_after_this_hit <= 0 {
            let _ = worldzero::plugin::host::report_death(&target_entity_id);
        } else {
            let _ = worldzero::plugin::host::plugin_state_set(
                &scope,
                "combat-hits-remaining",
                remaining_after_this_hit.to_string().as_bytes(),
            );
        }
    }

    fn on_death(zone_id: String, entity_id: String) {
        let _ = worldzero::plugin::host::send_message(&entity_id, "you died");
        // `send_message` above only ever reaches `entity_id` itself — no
        // help to a test proving an *NPC* died, since an NPC has no
        // connection of its own to receive it on (#197). Recorded under
        // zone-scope state instead, same "black-box test reads it back
        // via on-message" pattern `on_player_leave_zone` already uses for
        // the same underlying problem (#155's "last-left-entity").
        let _ = worldzero::plugin::host::plugin_state_set(
            &worldzero::plugin::host::PluginStateScope::Zone(zone_id),
            "last-death-entity",
            entity_id.as_bytes(),
        );
    }

    fn on_respawn(_zone_id: String, entity_id: String) {
        let _ = worldzero::plugin::host::send_message(&entity_id, "you respawned");
    }

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

    fn on_chat_command(zone_id: String, command: String, args: String, sender_entity_id: String) {
        if command == "give" {
            // Reports a capability rejection (#153) back to the client —
            // the only way a black-box/host-side test can observe a
            // gated host-function call was actually refused through the
            // real sandboxed call boundary, since the hook call itself
            // still returns Ok regardless.
            if let Err(e) = worldzero::plugin::host::grant_item(&sender_entity_id, &args, 1) {
                let _ = worldzero::plugin::host::send_message(
                    &sender_entity_id,
                    &format!("grant-item failed: {e}"),
                );
            }
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
                &worldzero::plugin::host::PluginStateScope::Zone(zone_id),
                "note",
                args.as_bytes(),
            );
            return;
        }
        if command == "recall" {
            let value = worldzero::plugin::host::plugin_state_get(
                &worldzero::plugin::host::PluginStateScope::Zone(zone_id),
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

    fn on_item_acquire(_zone_id: String, entity_id: String, item_type: String, new_quantity: i64) {
        let _ = worldzero::plugin::host::send_message(
            &entity_id,
            &format!("acquired {item_type}, now have {new_quantity}"),
        );
    }

    // "Using" an item removes it and pays out a bit of currency — a
    // minimal but real exercise of remove-item/modify-currency, not
    // meant to model an actual game economy.
    fn on_item_use(_zone_id: String, entity_id: String, item_type: String) {
        let _ = worldzero::plugin::host::remove_item(&entity_id, &item_type, 1);
        let _ = worldzero::plugin::host::modify_currency(&entity_id, 5);
        let _ = worldzero::plugin::host::send_message(&entity_id, &format!("used {item_type}"));
    }

    // Exercises on-craft-complete end to end (#216) — no entity id is
    // given (a craft is character-scoped, same reasoning as
    // on_character_create), so this applies a small reputation bonus via
    // apply-stat-delta-for-character, the one host function reachable
    // without an entity id (grant-item/remove-item/modify-currency are
    // all entity-id-scoped and unreachable from here). Directly
    // observable by a black-box test via the StatChanged push it
    // triggers and a direct DB read, same convention
    // on_character_create's own +25 write already established.
    //
    // Deliberately doesn't also demonstrate `plugin-state-set`'s
    // `character` scope here: that scope's durable-persistence drain
    // (`server::world_actor`'s pending_state_writes handling) assumes
    // the given id is always an *entity* id and resolves it through
    // `entity_characters` — it doesn't yet accept a raw `character-id`
    // the way `apply-stat-delta-for-character` does, so a call from
    // this hook would only ever update the in-memory cache, never
    // persist. Left as a known gap rather than worked around here.
    fn on_craft_complete(character_id: String, recipe_key: String) {
        let _ = recipe_key;
        let _ = worldzero::plugin::host::apply_stat_delta_for_character(
            &character_id,
            "reputation.ironclad_guild",
            5,
        );
    }
}

export!(Plugin);
