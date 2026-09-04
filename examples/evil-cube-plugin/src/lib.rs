//! evil-cube-plugin — a small custom WASM plugin built for
//! world-zero-test-grounds's manual "Evil Cube" combat test (see that
//! sibling repo's `PROMPT.md` §7.2). Spawns one stationary NPC in
//! whatever zone this plugin is loaded into, tracks its HP in
//! zone-scoped plugin state (since `apply-stat-delta` still has no
//! "read the current value back" host function — `plugin.wit`'s own
//! doc comment on `apply-stat-delta` notes this), and pushes the
//! cube's HP to the attacker as an ad-hoc `PluginMessage` text
//! convention, since `StatChanged` (#211) is never sent for an
//! NPC-targeted stat write (no owning connection to push to).
//!
//! Also backs the test grounds' admin panel: a handful of
//! `caller-role`-gated chat commands (`docs/specs/Auth_Spec.md`'s
//! "Account roles" — the real, backend-enforced mechanism; there is no
//! core wire concept of "admin" beyond this) plus an
//! `on-player-join-zone` announcement of the caller's own roles (an
//! ad-hoc `PluginMessage` convention, same shape as the `cube:`
//! convention, since nothing in core tells a *client* its own roles
//! either — `caller-role` is plugin-facing only).
//!
//! Build with `rustup target add wasm32-wasip2` then
//! `cargo build --manifest-path examples/evil-cube-plugin/Cargo.toml
//! --target wasm32-wasip2 --release`.

wit_bindgen::generate!({
    path: "../../crates/plugin-host/wit",
    world: "plugin",
    generate_all,
});

use crate::exports::worldzero::plugin::hooks::Guest;
use crate::worldzero::plugin::host::{self, PluginStateScope};

const SPAWN_TABLE_ID: &str = "evil-cube-01";
const HOME_ZONE_ID: &str = "greenwood-forest";
const HP_STATE_KEY: &str = "evil-cube-hp";
const CUBE_ENTITY_ID_KEY: &str = "evil-cube-entity-id";
const MAX_HP: i64 = 50;
const HIT_DAMAGE: i64 = 10;
const ADMIN_ROLE: &str = "admin";

fn read_tracked_hp(zone_id: &str) -> i64 {
    match host::plugin_state_get(&PluginStateScope::Zone(zone_id.to_string()), HP_STATE_KEY) {
        Ok(Some(bytes)) if bytes.len() == 8 => {
            i64::from_le_bytes(bytes.try_into().unwrap_or([0; 8]))
        }
        _ => MAX_HP,
    }
}

fn write_tracked_hp(zone_id: &str, hp: i64) {
    let _ = host::plugin_state_set(
        &PluginStateScope::Zone(zone_id.to_string()),
        HP_STATE_KEY,
        &hp.to_le_bytes(),
    );
}

// The cube's real entity_id, learned from `on-entity-spawn` (`spawn-npc`
// itself can't return it synchronously — `plugin.wit`'s own doc comment)
// and stashed in zone-scoped state so the admin commands below can act
// on it directly, instead of on-damage-calc's own "any call in this zone
// is the cube" shortcut (§7.2 step 6) — that shortcut only works because
// a client's Attack always names a real target it already knows from its
// roster; an admin chat command has no target parameter at all.
fn read_cube_entity_id(zone_id: &str) -> Option<String> {
    match host::plugin_state_get(&PluginStateScope::Zone(zone_id.to_string()), CUBE_ENTITY_ID_KEY) {
        Ok(Some(bytes)) => String::from_utf8(bytes).ok(),
        _ => None,
    }
}

fn write_cube_entity_id(zone_id: &str, entity_id: &str) {
    let _ = host::plugin_state_set(
        &PluginStateScope::Zone(zone_id.to_string()),
        CUBE_ENTITY_ID_KEY,
        entity_id.as_bytes(),
    );
}

fn cube_convention_hp(entity_id: &str, current: i64, max: i64) -> String {
    format!("cube:{entity_id}:hp:{current}/{max}")
}

fn cube_convention_dead(entity_id: &str) -> String {
    format!("cube:{entity_id}:dead")
}

fn is_admin(entity_id: &str) -> bool {
    host::caller_role(entity_id)
        .map(|roles| roles.iter().any(|r| r == ADMIN_ROLE))
        .unwrap_or(false)
}

fn deny_admin(sender_entity_id: &str, command: &str) {
    let _ = host::send_message(
        sender_entity_id,
        &format!("admin:denied:{command} (your account has no \"admin\" role)"),
    );
}

struct Plugin;

impl Guest for Plugin {
    fn on_load() {}

    fn on_unload() {}

    // One plugin instance serves every zone loaded via content-pack.yaml
    // (#152, and this test grounds' content-pack now declares both
    // greenwood-forest and stonebridge-village per PROMPT.md §5.2) — the
    // evil-cube-01 spawn table only exists in greenwood-forest.yaml, so
    // this must check zone_id before spawning, or on_zone_loaded firing
    // for stonebridge-village too would log a spurious "unknown spawn
    // table" warning there.
    fn on_zone_loaded(zone_id: String) {
        if zone_id != HOME_ZONE_ID {
            return;
        }
        let _ = host::spawn_npc(SPAWN_TABLE_ID);
        write_tracked_hp(&zone_id, MAX_HP);
    }

    fn on_character_create(_character_id: String, _zone_id: String) {}

    // Live (#214) — fires once the spawn requested in on_zone_loaded
    // above actually lands, carrying the real entity_id spawn-npc
    // couldn't return synchronously. Stashed so the admin commands below
    // (killcube/respawncube) have something to act on.
    fn on_entity_spawn(
        zone_id: String,
        entity_id: String,
        _entity_type: String,
        spawn_table_id: String,
    ) {
        if spawn_table_id == SPAWN_TABLE_ID {
            write_cube_entity_id(&zone_id, &entity_id);
        }
    }

    // Announces the newly-joined connection's own account roles back to
    // it, ad-hoc `PluginMessage` convention (`roles:role1,role2`, empty
    // after the colon if none) — core has no wire message for "tell a
    // client its own roles" (`caller-role` is plugin-facing only, see
    // this file's module doc), so the test grounds' admin-panel UI
    // learns whether to show itself this way, the same shape as the
    // `cube:` HP convention. Every admin command below still
    // independently re-checks caller-role itself — this announcement is
    // purely a UI convenience, never trusted as the actual authorization
    // check.
    fn on_player_join_zone(_zone_id: String, entity_id: String) {
        let roles = host::caller_role(&entity_id).unwrap_or_default();
        let _ = host::send_message(&entity_id, &format!("roles:{}", roles.join(",")));
    }

    fn on_player_leave_zone(_zone_id: String, _entity_id: String) {}

    fn on_interact(_zone_id: String, _trigger_id: String, _actor_entity_id: String) {}

    fn on_message(_zone_id: String, _message_type: u16, _sender_entity_id: String, _payload: Vec<u8>) {
    }

    // Fires when a client sends an Attack action (#154). `spawn-npc`
    // can't synchronously return the cube's real entity id (see
    // `plugin.wit`'s own doc comment on it), and this plugin only ever
    // spawns one NPC into one zone, so — per PROMPT.md §7.2 step 6 —
    // any on-damage-calc call in this zone is treated as "the cube",
    // rather than trying to correlate target_entity_id against a
    // remembered id.
    fn on_damage_calc(
        zone_id: String,
        attacker_entity_id: String,
        target_entity_id: String,
        stat_key: String,
        _base_amount: i64,
    ) {
        let _ = host::apply_stat_delta(&target_entity_id, &stat_key, -HIT_DAMAGE);

        let remaining = (read_tracked_hp(&zone_id) - HIT_DAMAGE).max(0);
        write_tracked_hp(&zone_id, remaining);

        let _ = host::send_message(
            &attacker_entity_id,
            &cube_convention_hp(&target_entity_id, remaining, MAX_HP),
        );

        if remaining == 0 {
            let _ = host::report_death(&target_entity_id);
            let _ = host::send_message(&attacker_entity_id, &cube_convention_dead(&target_entity_id));
        }
    }

    // The plugin's own confirmation that the report-death call above
    // was applied (#154) — re-send the dead convention in case the
    // attacker's client only reliably tracks state from this hook
    // rather than the immediate reply in on_damage_calc above.
    fn on_death(_zone_id: String, entity_id: String) {
        let _ = host::send_message(&entity_id, &cube_convention_dead(&entity_id));
    }

    fn on_respawn(zone_id: String, entity_id: String) {
        write_tracked_hp(&zone_id, MAX_HP);
        let _ = host::send_message(
            &entity_id,
            &format!("cube:{entity_id}:respawned:hp:{MAX_HP}"),
        );
    }

    fn on_tick(_zone_id: String, _dt: f64) {}

    fn on_npc_tick(
        _zone_id: String,
        _entity_id: String,
        _x: f64,
        _y: f64,
        _z: f64,
        _route_waypoints: Vec<(f64, f64)>,
        _route_loop: bool,
        _route_speed: f64,
        _dt: f64,
    ) {
    }

    fn on_npc_interact(_zone_id: String, _npc_entity_id: String, _actor_entity_id: String) {}

    // The test grounds' admin panel — every branch re-checks
    // `caller-role` itself (never trusts that the client only shows
    // these buttons to admins) since that's the one real,
    // backend-enforced authorization mechanism World Zero has
    // (`docs/specs/Auth_Spec.md`'s "Account roles"). `zone_id` here is
    // whichever zone the sender is actually standing in when they send
    // the command — since this test grounds only ever has one cube (in
    // greenwood-forest), commands issued from elsewhere just won't find
    // one to act on.
    fn on_chat_command(zone_id: String, command: String, args: String, sender_entity_id: String) {
        match command.as_str() {
            "killcube" => {
                if !is_admin(&sender_entity_id) {
                    deny_admin(&sender_entity_id, &command);
                    return;
                }
                let Some(cube_id) = read_cube_entity_id(&zone_id) else {
                    let _ = host::send_message(&sender_entity_id, "admin:killcube:no cube known in this zone");
                    return;
                };
                let remaining = read_tracked_hp(&zone_id);
                if remaining > 0 {
                    let _ = host::apply_stat_delta(&cube_id, "hp", -remaining);
                }
                write_tracked_hp(&zone_id, 0);
                let _ = host::report_death(&cube_id);
                let _ = host::send_message(&sender_entity_id, &format!("admin:killcube:done ({cube_id})"));
            }
            "respawncube" => {
                if !is_admin(&sender_entity_id) {
                    deny_admin(&sender_entity_id, &command);
                    return;
                }
                let Some(cube_id) = read_cube_entity_id(&zone_id) else {
                    let _ = host::send_message(&sender_entity_id, "admin:respawncube:no cube known in this zone");
                    return;
                };
                let missing = MAX_HP - read_tracked_hp(&zone_id);
                if missing > 0 {
                    let _ = host::apply_stat_delta(&cube_id, "hp", missing);
                }
                // report-respawn fires on_respawn back to this plugin,
                // which resets the tracked HP and sends the
                // `cube:...:respawned:hp:...` convention — reused rather
                // than duplicated here.
                let _ = host::report_respawn(&cube_id);
                let _ = host::send_message(&sender_entity_id, &format!("admin:respawncube:done ({cube_id})"));
            }
            "grant" => {
                if !is_admin(&sender_entity_id) {
                    deny_admin(&sender_entity_id, &command);
                    return;
                }
                let mut parts = args.split_whitespace();
                let (Some(item_type), Some(qty_str)) = (parts.next(), parts.next()) else {
                    let _ = host::send_message(&sender_entity_id, "admin:grant:usage /grant <item_type> <quantity>");
                    return;
                };
                let Ok(quantity) = qty_str.parse::<i64>() else {
                    let _ = host::send_message(&sender_entity_id, "admin:grant:quantity must be a whole number");
                    return;
                };
                match host::grant_item(&sender_entity_id, item_type, quantity) {
                    Ok(()) => {
                        let _ = host::send_message(&sender_entity_id, &format!("admin:grant:granted {quantity} {item_type}"));
                    }
                    Err(e) => {
                        let _ = host::send_message(&sender_entity_id, &format!("admin:grant:failed ({e})"));
                    }
                }
            }
            "grantcurrency" => {
                if !is_admin(&sender_entity_id) {
                    deny_admin(&sender_entity_id, &command);
                    return;
                }
                let mut parts = args.split_whitespace();
                let (Some(currency_key), Some(amount_str)) = (parts.next(), parts.next()) else {
                    let _ = host::send_message(&sender_entity_id, "admin:grantcurrency:usage /grantcurrency <currency_key> <amount>");
                    return;
                };
                let Ok(amount) = amount_str.parse::<i64>() else {
                    let _ = host::send_message(&sender_entity_id, "admin:grantcurrency:amount must be a whole number");
                    return;
                };
                match host::modify_currency(&sender_entity_id, currency_key, amount) {
                    Ok(()) => {
                        let _ = host::send_message(&sender_entity_id, &format!("admin:grantcurrency:granted {amount} {currency_key}"));
                    }
                    Err(e) => {
                        let _ = host::send_message(&sender_entity_id, &format!("admin:grantcurrency:failed ({e})"));
                    }
                }
            }
            _ => {}
        }
    }

    fn on_item_acquire(_zone_id: String, _entity_id: String, _item_type: String, _new_quantity: i64) {}

    fn on_item_use(_zone_id: String, _entity_id: String, _item_type: String) {}

    fn on_craft_complete(_character_id: String, _recipe_key: String) {}
}

export!(Plugin);
