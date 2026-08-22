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
//! docs/specs/Plugin_API.md, "Beyond this v0 slice").

use std::path::Path;
use std::sync::{Arc, Mutex};

use common::Result;
use common::id::EntityId;
use plugin_host::{HostCallbacks, LoadedPlugin, PluginHost, PluginManifest};

use crate::session::Sessions;
use crate::session_protocol::ServerMessage;

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
    sessions: Sessions,
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
    pending_spawns: Arc<Mutex<Vec<String>>>,
}

impl PluginRuntime {
    /// Spawn-table ids requested via `spawn-npc` since the last drain, in
    /// call order.
    pub fn drain_pending_spawns(&self) -> Vec<String> {
        std::mem::take(&mut self.pending_spawns.lock().unwrap())
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
) -> Result<(PluginRuntime, Vec<String>)> {
    let manifest = PluginManifest::from_file(manifest_path)?;
    let message_types = manifest.plugin.message_types.clone();
    let host = PluginHost::new();
    let pending_spawns = Arc::new(Mutex::new(Vec::new()));
    let callbacks = PluginCallbacks {
        pending_spawns: pending_spawns.clone(),
        sessions,
    };

    let mut plugin = host.load(&manifest, wasm_path, Box::new(callbacks))?;
    plugin.on_load()?;

    let on_load_spawns = std::mem::take(&mut *pending_spawns.lock().unwrap());
    let runtime = PluginRuntime {
        plugin,
        message_types,
        pending_spawns,
    };
    Ok((runtime, on_load_spawns))
}
