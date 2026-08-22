//! One-time, synchronous plugin loading at server startup — runs a
//! configured plugin's `on_load` hook and reports which spawn tables it
//! asked to spawn from (docs/PROPOSAL.md, "Phased Roadmap," Phase 1:
//! "minimal plugin hook (e.g. NPC spawn + one interaction)").
//!
//! Deliberately minimal: this v0 wiring calls `on_load` and nothing
//! else — no `on_tick`, no `on_interact` (see docs/specs/Plugin_API.md,
//! "Beyond this v0 slice"). Resolving a requested spawn-table id into a
//! real entity (looking up the table's points, calling
//! `world::Zone::spawn`) is the caller's job — this module never touches
//! a `Zone` so it can stay fully synchronous, called before any async
//! task (the world actor, session handling) is running.

use std::path::Path;
use std::sync::{Arc, Mutex};

use common::Result;
use plugin_host::{HostCallbacks, PluginHost, PluginManifest};

struct StartupCallbacks {
    pending_spawns: Arc<Mutex<Vec<String>>>,
}

impl HostCallbacks for StartupCallbacks {
    fn spawn_npc(&mut self, spawn_table_id: &str) -> std::result::Result<String, String> {
        self.pending_spawns
            .lock()
            .unwrap()
            .push(spawn_table_id.to_string());
        // No real entity id exists yet at this point (resolution happens
        // after `on_load` returns) — the spawn-table id is a reasonable
        // stand-in for a v0 return value nothing currently reads back.
        Ok(spawn_table_id.to_string())
    }

    fn send_message(
        &mut self,
        _target_entity_id: &str,
        _body: &str,
    ) -> std::result::Result<(), String> {
        Err(
            "send_message is not available during plugin startup — no clients are connected yet"
                .to_string(),
        )
    }
}

/// Loads the plugin at `wasm_path` (checked against `manifest_path`'s
/// declared `host_api_version` first), runs its `on_load` hook, and
/// returns the spawn-table ids it requested via the `spawn-npc` host
/// function, in call order.
pub fn run_on_load(manifest_path: &Path, wasm_path: &Path) -> Result<Vec<String>> {
    let manifest = PluginManifest::from_file(manifest_path)?;
    let host = PluginHost::new();
    let pending_spawns = Arc::new(Mutex::new(Vec::new()));
    let callbacks = StartupCallbacks {
        pending_spawns: pending_spawns.clone(),
    };

    let mut plugin = host.load(&manifest, wasm_path, Box::new(callbacks))?;
    plugin.on_load()?;
    drop(plugin);

    Ok(Arc::try_unwrap(pending_spawns)
        .map(|cell| cell.into_inner().unwrap())
        .unwrap_or_default())
}
