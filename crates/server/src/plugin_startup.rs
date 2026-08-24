//! Plugin loading at server startup — loads the configured plugin, runs
//! its `on_load` hook, and hands back the still-alive instance (plus
//! which spawn tables it asked to spawn from) so `main` can seed the zone
//! before the world actor starts, and keep the plugin running for the
//! rest of the process's life (docs/PROPOSAL.md, "Phased Roadmap," Phase
//! 1: "minimal plugin hook (e.g. NPC spawn + one interaction)"; #95:
//! gateway-routed messages reaching the plugin via `on_message` need it
//! kept alive past startup, not dropped immediately after `on_load`).
//!
//! Deliberately minimal beyond that: this v0 wiring calls `on_load` and
//! wires `on_message` (#95) — still no `on_tick`, no `on_interact` (see
//! docs/specs/Plugin_API.md, "Beyond this v0 slice"). Also wires
//! `plugin-state-get`/`plugin-state-set` (#149,
//! `crate::plugin_state`'s module doc) — see `PluginCallbacks`'s own
//! doc comment for the cache/queue split those two need.

use std::path::Path;
use std::sync::{Arc, Mutex};

use common::Result;
use common::id::EntityId;
use plugin_host::{HostCallbacks, LoadedPlugin, PluginHost, PluginManifest, PluginStateScope};

use crate::plugin_state::{PluginStateCache, cache_key};
use crate::session::{EntityRoles, Sessions};
use crate::session_protocol::ServerMessage;

/// `(scope, key, value)` requested via `plugin-state-set` for
/// `character`/`zone` scope, queued for the caller's own drain — see
/// `PluginCallbacks`'s `pending_state_writes` field.
type PendingStateWrites = Arc<Mutex<Vec<(PluginStateScope, String, Vec<u8>)>>>;

/// `HostCallbacks` used for the plugin's whole lifetime — both the
/// one-time `on_load` call at startup and every later `on_message` call
/// once the plugin is owned by the world actor (#95). `spawn_npc` always
/// just records the request; resolving it into a real spawned entity
/// happens in the caller's own context (`main::spawn_npc_from_table`),
/// since only the caller has a `&mut Zone` to spawn into — the plugin
/// instance's callbacks can't hold one directly (see `world_actor`'s "no
/// shared lock" design). `send_message` is live from the start: it's
/// harmless (and correctly errors "no such connection") before any
/// client has connected, and works for real once `sessions` has entries.
pub struct PluginCallbacks {
    pending_spawns: Arc<Mutex<Vec<String>>>,
    /// `(entity_id, stat_key, delta)`, drained and applied through
    /// `character::CharacterStore` by the caller — this callback only
    /// records the request, same "can't reach `&mut Zone`/the DB from
    /// inside a sandboxed sync call" reasoning as `pending_spawns` (see
    /// module doc).
    pending_stat_deltas: Arc<Mutex<Vec<(String, String, i64)>>>,
    /// `(entity_id, x, y)`, drained and applied via
    /// `world::Zone::request_move` by the caller.
    pending_moves: Arc<Mutex<Vec<(String, f64, f64)>>>,
    /// `(entity_id, item_type, quantity)`, drained and applied through
    /// `character::CharacterStore::grant_item` by the caller (#57/#112).
    pending_item_grants: Arc<Mutex<Vec<(String, String, i64)>>>,
    /// `(entity_id, item_type, quantity)`, drained and applied through
    /// `character::CharacterStore::remove_item` by the caller.
    pending_item_removals: Arc<Mutex<Vec<(String, String, i64)>>>,
    /// `(entity_id, delta)`, drained and applied through
    /// `character::CharacterStore::modify_currency` by the caller.
    pending_currency_deltas: Arc<Mutex<Vec<(String, i64)>>>,
    /// Backs `caller-role` (#124) — a synchronous lookup against
    /// `session::EntityRoles`, populated at join time, never a live
    /// `auth` role-store query from inside this sandboxed sync call (see
    /// `wit/plugin.wit`'s `caller-role` doc comment for why).
    entity_roles: EntityRoles,
    sessions: Sessions,
    /// Backs `plugin-state-get`/`plugin-state-set` (#149) — a
    /// synchronous read/write against a shared in-memory cache, never a
    /// live DB read/write from inside this sandboxed call, same reason
    /// `caller_role` reads `entity_roles` instead of querying `auth`
    /// live. Hydrated at the right lifecycle point per scope
    /// (`session::handle_session` for character scope, `main` for zone
    /// scope) — see `crate::plugin_state`'s module doc.
    plugin_state_cache: PluginStateCache,
    /// `(scope, key, value)` queued for `character`/`zone` scope only —
    /// drained and persisted through `plugin_state::PluginStateStore` by
    /// the caller, same "can't reach the DB from inside a sandboxed sync
    /// call" reasoning as every other `pending_*` field above. `entity`
    /// scope writes never reach this queue — there's nothing to persist.
    pending_state_writes: PendingStateWrites,
}

impl HostCallbacks for PluginCallbacks {
    fn spawn_npc(&mut self, spawn_table_id: &str) -> std::result::Result<String, String> {
        self.pending_spawns
            .lock()
            .unwrap()
            .push(spawn_table_id.to_string());
        // No real entity id exists yet at this point (resolution happens
        // when the caller next drains `pending_spawns`) — the spawn-table
        // id is a reasonable stand-in for a v0 return value nothing
        // currently reads back.
        Ok(spawn_table_id.to_string())
    }

    fn send_message(
        &mut self,
        target_entity_id: &str,
        body: &str,
    ) -> std::result::Result<(), String> {
        let entity_id: EntityId = target_entity_id
            .parse()
            .map_err(|_| format!("{target_entity_id:?} is not a valid entity id"))?;
        let Ok(envelope) = (ServerMessage::PluginMessage {
            body: body.to_string(),
        })
        .into_envelope() else {
            return Err("failed to encode the plugin's message".to_string());
        };
        let sessions = self.sessions.lock().unwrap();
        match sessions.get(&entity_id) {
            Some(sender) => sender
                .send(envelope)
                .map_err(|_| format!("{target_entity_id} is no longer connected")),
            None => Err(format!("no connected entity {target_entity_id}")),
        }
    }

    fn apply_stat_delta(
        &mut self,
        entity_id: &str,
        stat_key: &str,
        delta: i64,
    ) -> std::result::Result<(), String> {
        self.pending_stat_deltas.lock().unwrap().push((
            entity_id.to_string(),
            stat_key.to_string(),
            delta,
        ));
        Ok(())
    }

    fn move_entity(&mut self, entity_id: &str, x: f64, y: f64) -> std::result::Result<(), String> {
        self.pending_moves
            .lock()
            .unwrap()
            .push((entity_id.to_string(), x, y));
        Ok(())
    }

    fn grant_item(
        &mut self,
        entity_id: &str,
        item_type: &str,
        quantity: i64,
    ) -> std::result::Result<(), String> {
        self.pending_item_grants.lock().unwrap().push((
            entity_id.to_string(),
            item_type.to_string(),
            quantity,
        ));
        Ok(())
    }

    fn remove_item(
        &mut self,
        entity_id: &str,
        item_type: &str,
        quantity: i64,
    ) -> std::result::Result<(), String> {
        self.pending_item_removals.lock().unwrap().push((
            entity_id.to_string(),
            item_type.to_string(),
            quantity,
        ));
        Ok(())
    }

    fn modify_currency(&mut self, entity_id: &str, delta: i64) -> std::result::Result<(), String> {
        self.pending_currency_deltas
            .lock()
            .unwrap()
            .push((entity_id.to_string(), delta));
        Ok(())
    }

    fn caller_role(&mut self, entity_id: &str) -> std::result::Result<Vec<String>, String> {
        let entity_id: EntityId = entity_id
            .parse()
            .map_err(|_| format!("{entity_id:?} is not a valid entity id"))?;
        self.entity_roles
            .lock()
            .unwrap()
            .get(&entity_id)
            .cloned()
            .ok_or_else(|| format!("{entity_id} is not a connected player entity"))
    }

    fn plugin_state_get(
        &mut self,
        scope: PluginStateScope,
        key: &str,
    ) -> std::result::Result<Option<Vec<u8>>, String> {
        Ok(self
            .plugin_state_cache
            .lock()
            .unwrap()
            .get(&cache_key(&scope, key))
            .cloned())
    }

    fn plugin_state_set(
        &mut self,
        scope: PluginStateScope,
        key: &str,
        value: Vec<u8>,
    ) -> std::result::Result<(), String> {
        self.plugin_state_cache
            .lock()
            .unwrap()
            .insert(cache_key(&scope, key), value.clone());

        if !matches!(scope, PluginStateScope::Entity(_)) {
            self.pending_state_writes
                .lock()
                .unwrap()
                .push((scope, key.to_string(), value));
        }
        Ok(())
    }
}

/// A plugin kept alive past startup: the live instance, which
/// `message_type`s it declared (empty if none), and a handle to drain
/// `spawn-npc` requests it makes later (from `on_message` — the callback
/// boxed inside the plugin's `Store` can't be reached directly, so
/// callers drain this shared queue instead; see `PluginCallbacks`' docs
/// for why `spawn_npc` only ever records rather than spawning directly).
pub struct PluginRuntime {
    pub plugin: LoadedPlugin,
    pub message_types: Vec<u16>,
    /// Command names (without the leading `/`) declared in `plugin.toml`
    /// — routed to `on-chat-command` instead of published as ordinary
    /// chat (#57).
    pub chat_commands: Vec<String>,
    pending_spawns: Arc<Mutex<Vec<String>>>,
    pending_stat_deltas: Arc<Mutex<Vec<(String, String, i64)>>>,
    pending_moves: Arc<Mutex<Vec<(String, f64, f64)>>>,
    pending_item_grants: Arc<Mutex<Vec<(String, String, i64)>>>,
    pending_item_removals: Arc<Mutex<Vec<(String, String, i64)>>>,
    pending_currency_deltas: Arc<Mutex<Vec<(String, i64)>>>,
    pending_state_writes: PendingStateWrites,
}

impl PluginRuntime {
    /// Spawn-table ids requested via `spawn-npc` since the last drain, in
    /// call order.
    pub fn drain_pending_spawns(&self) -> Vec<String> {
        std::mem::take(&mut self.pending_spawns.lock().unwrap())
    }

    /// `(entity_id, stat_key, delta)` requested via `apply-stat-delta`
    /// since the last drain, in call order.
    pub fn drain_pending_stat_deltas(&self) -> Vec<(String, String, i64)> {
        std::mem::take(&mut self.pending_stat_deltas.lock().unwrap())
    }

    /// `(entity_id, x, y)` requested via `move-entity` since the last
    /// drain, in call order.
    pub fn drain_pending_moves(&self) -> Vec<(String, f64, f64)> {
        std::mem::take(&mut self.pending_moves.lock().unwrap())
    }

    /// `(entity_id, item_type, quantity)` requested via `grant-item`
    /// since the last drain, in call order.
    pub fn drain_pending_item_grants(&self) -> Vec<(String, String, i64)> {
        std::mem::take(&mut self.pending_item_grants.lock().unwrap())
    }

    /// `(entity_id, item_type, quantity)` requested via `remove-item`
    /// since the last drain, in call order.
    pub fn drain_pending_item_removals(&self) -> Vec<(String, String, i64)> {
        std::mem::take(&mut self.pending_item_removals.lock().unwrap())
    }

    /// `(entity_id, delta)` requested via `modify-currency` since the
    /// last drain, in call order.
    pub fn drain_pending_currency_deltas(&self) -> Vec<(String, i64)> {
        std::mem::take(&mut self.pending_currency_deltas.lock().unwrap())
    }

    /// `(scope, key, value)` requested via `plugin-state-set` for
    /// `character`/`zone` scope since the last drain, in call order —
    /// `entity` scope never reaches this queue (#149, nothing to
    /// persist).
    pub fn drain_pending_state_writes(&self) -> Vec<(PluginStateScope, String, Vec<u8>)> {
        std::mem::take(&mut self.pending_state_writes.lock().unwrap())
    }
}

/// Loads the plugin at `wasm_path` (checked against `manifest_path`'s
/// declared `host_api_version` and `message_types` first) and runs its
/// `on_load` hook. Returns the still-alive plugin — the caller is
/// responsible for keeping it running; dropping it tears it down — plus
/// the spawn-table ids it requested via `spawn-npc` during `on_load`, in
/// call order (any requested during a later `on_message` call are left
/// for the caller to drain via `PluginRuntime::drain_pending_spawns`).
pub fn load_and_run_on_load(
    manifest_path: &Path,
    wasm_path: &Path,
    sessions: Sessions,
    entity_roles: EntityRoles,
    plugin_state_cache: PluginStateCache,
) -> Result<(PluginRuntime, Vec<String>)> {
    let manifest = PluginManifest::from_file(manifest_path)?;
    let message_types = manifest.plugin.message_types.clone();
    let chat_commands = manifest.plugin.chat_commands.clone();
    let host = PluginHost::new();
    let pending_spawns = Arc::new(Mutex::new(Vec::new()));
    let pending_stat_deltas = Arc::new(Mutex::new(Vec::new()));
    let pending_moves = Arc::new(Mutex::new(Vec::new()));
    let pending_item_grants = Arc::new(Mutex::new(Vec::new()));
    let pending_item_removals = Arc::new(Mutex::new(Vec::new()));
    let pending_currency_deltas = Arc::new(Mutex::new(Vec::new()));
    let pending_state_writes = Arc::new(Mutex::new(Vec::new()));
    let callbacks = PluginCallbacks {
        pending_spawns: pending_spawns.clone(),
        pending_stat_deltas: pending_stat_deltas.clone(),
        pending_moves: pending_moves.clone(),
        pending_item_grants: pending_item_grants.clone(),
        pending_item_removals: pending_item_removals.clone(),
        pending_currency_deltas: pending_currency_deltas.clone(),
        entity_roles,
        sessions,
        plugin_state_cache,
        pending_state_writes: pending_state_writes.clone(),
    };

    let mut plugin = host.load(&manifest, wasm_path, Box::new(callbacks))?;
    plugin.on_load()?;

    let on_load_spawns = std::mem::take(&mut *pending_spawns.lock().unwrap());
    let runtime = PluginRuntime {
        plugin,
        message_types,
        chat_commands,
        pending_spawns,
        pending_stat_deltas,
        pending_moves,
        pending_item_grants,
        pending_item_removals,
        pending_currency_deltas,
        pending_state_writes,
    };
    Ok((runtime, on_load_spawns))
}
