//! `wasmtime`-based sandbox: loads a compiled plugin `.wasm` component,
//! instantiates it with zero ambient capability beyond the `host`
//! interface's two v0 functions, and drives its hooks
//! (docs/PROPOSAL.md, "Plugin System"; #37/#38's acceptance criteria).

use std::path::Path;

use common::{Error, Result};
use wasmtime::component::{Component, Linker, ResourceTable};
use wasmtime::{Engine, Store};
use wasmtime_wasi::{WasiCtx, WasiCtxBuilder, WasiCtxView, WasiView};

use crate::bindings::Plugin as PluginBindings;
use crate::bindings::worldzero::plugin::host::Host as HostInterface;
use crate::bindings::worldzero::plugin::host::PluginStateScope as WitPluginStateScope;
use crate::manifest::PluginManifest;

/// Mirrors `wit/plugin.wit`'s `plugin-state-scope` variant — kept as our
/// own type rather than exposing the `wasmtime`-generated one directly,
/// same reason every other `HostCallbacks` method takes plain `&str`/etc.
/// instead of generated binding types: this trait's implementors
/// (`server`) shouldn't need to depend on `plugin-host`'s private
/// `bindings` module.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PluginStateScope {
    /// A character, identified by the *entity* id currently representing
    /// it in the zone (not a `CharacterId` directly — the plugin only
    /// ever knows entity ids, same as every other `HostCallbacks` method).
    Character(String),
    /// An entity — transient, in-memory only, no persistence.
    Entity(String),
    /// A zone, identified by its content-manifest zone id.
    Zone(String),
}

impl From<WitPluginStateScope> for PluginStateScope {
    fn from(scope: WitPluginStateScope) -> Self {
        match scope {
            WitPluginStateScope::Character(id) => PluginStateScope::Character(id),
            WitPluginStateScope::Entity(id) => PluginStateScope::Entity(id),
            WitPluginStateScope::Zone(id) => PluginStateScope::Zone(id),
        }
    }
}

/// What a loaded plugin is allowed to actually *do* — the host side of
/// the `host` WIT interface's v0 functions. A trait, not a hand-called
/// free function, so `server` can supply a real `world`/`chat`-backed
/// implementation while tests supply a fake one — the same
/// "policy, not hardcoding" shape as `auth::AuthProvider`.
pub trait HostCallbacks: Send + 'static {
    fn spawn_npc(&mut self, spawn_table_id: &str) -> std::result::Result<String, String>;
    fn send_message(
        &mut self,
        target_entity_id: &str,
        body: &str,
    ) -> std::result::Result<(), String>;

    /// Adjusts one declared stat by `delta` (`wit/plugin.wit`'s
    /// `apply-stat-delta`) — the actual write still goes through
    /// whatever validates it against the game's declared attribute
    /// schema (docs/specs/Data_Model_Spec.md); this trait method is only
    /// the sandboxed call boundary, same division of responsibility as
    /// `spawn_npc`/`send_message`.
    fn apply_stat_delta(
        &mut self,
        entity_id: &str,
        stat_key: &str,
        delta: i64,
    ) -> std::result::Result<(), String>;

    /// Queues a move for `entity_id` (`wit/plugin.wit`'s `move-entity`)
    /// — applied and validated on the zone's next tick through the same
    /// path a player's own movement goes through, never a direct
    /// position write.
    fn move_entity(&mut self, entity_id: &str, x: f64, y: f64) -> std::result::Result<(), String>;

    /// Grants an item stack (`wit/plugin.wit`'s `grant-item`) — queued,
    /// applied through `character::CharacterStore::grant_item` (#112).
    fn grant_item(
        &mut self,
        entity_id: &str,
        item_type: &str,
        quantity: i64,
    ) -> std::result::Result<(), String>;

    /// Removes from an item stack (`wit/plugin.wit`'s `remove-item`) —
    /// queued, applied through `character::CharacterStore::remove_item`.
    fn remove_item(
        &mut self,
        entity_id: &str,
        item_type: &str,
        quantity: i64,
    ) -> std::result::Result<(), String>;

    /// Adjusts a currency balance (`wit/plugin.wit`'s `modify-currency`)
    /// — queued, applied through `character::CharacterStore::modify_currency`.
    fn modify_currency(&mut self, entity_id: &str, delta: i64) -> std::result::Result<(), String>;

    /// Returns the roles held by the account behind `entity_id`
    /// (`wit/plugin.wit`'s `caller-role`) — unlike every other method
    /// here, the implementation is expected to answer from an in-memory
    /// cache populated at session join, not a live DB read; see the WIT
    /// doc comment for why.
    fn caller_role(&mut self, entity_id: &str) -> std::result::Result<Vec<String>, String>;

    /// Reads plugin state (`wit/plugin.wit`'s `plugin-state-get`) — same
    /// cache-not-live-DB-read constraint as `caller_role`, see that WIT
    /// doc comment and `plugin-state-get`'s own.
    fn plugin_state_get(
        &mut self,
        scope: PluginStateScope,
        key: &str,
    ) -> std::result::Result<Option<Vec<u8>>, String>;

    /// Writes plugin state (`wit/plugin.wit`'s `plugin-state-set`) —
    /// updates the in-memory cache immediately; for `Character`/`Zone`
    /// scope, also queues a durable write for the implementor's own
    /// drain mechanism (same shape as `apply_stat_delta`/`grant_item`).
    fn plugin_state_set(
        &mut self,
        scope: PluginStateScope,
        key: &str,
        value: Vec<u8>,
    ) -> std::result::Result<(), String>;

    /// Reports a death (`wit/plugin.wit`'s `report-death`) — queued,
    /// applied on the implementor's own drain mechanism, same shape as
    /// `apply_stat_delta` (#154).
    fn report_death(&mut self, entity_id: &str) -> std::result::Result<(), String>;

    /// Reports a respawn (`wit/plugin.wit`'s `report-respawn`) — same
    /// shape as `report_death`.
    fn report_respawn(&mut self, entity_id: &str) -> std::result::Result<(), String>;
}

/// Which capability (`manifest::KNOWN_CAPABILITIES`) a host function
/// requires, `None` for the two that are ungated regardless of what a
/// plugin declared (#153): `caller-role` (read-only, answers from a
/// cache already scoped to the calling connection) and
/// `plugin-state-get`/`-set` (self-scoped storage — a plugin reading or
/// writing its own state can't affect another entity/plugin). Every
/// other host function reaches across entity boundaries — moving/
/// damaging/granting-to/messaging another entity, spawning a new one —
/// and is gated, `send-message` included: it can target *any* connected
/// entity by id, not just the one a hook call was actually about.
fn required_capability(function: &str) -> Option<&'static str> {
    use crate::manifest::{
        CAPABILITY_COMBAT, CAPABILITY_ECONOMY, CAPABILITY_MESSAGING, CAPABILITY_MOVEMENT,
        CAPABILITY_SPAWNING,
    };
    match function {
        "spawn-npc" => Some(CAPABILITY_SPAWNING),
        "send-message" => Some(CAPABILITY_MESSAGING),
        "move-entity" => Some(CAPABILITY_MOVEMENT),
        "apply-stat-delta" | "report-death" | "report-respawn" => Some(CAPABILITY_COMBAT),
        "grant-item" | "remove-item" | "modify-currency" => Some(CAPABILITY_ECONOMY),
        _ => None,
    }
}

/// Wraps a real `HostCallbacks` implementor and refuses any call to a
/// gated host function the plugin didn't declare the covering capability
/// for (#153) — every `PluginHost::load`ed plugin goes through this, so
/// enforcement lives once in `plugin-host` itself rather than being
/// re-implemented by every `HostCallbacks` implementor (`server`'s
/// `PluginCallbacks` and friends stay unaware of capability gating
/// entirely). A rejected call surfaces as an ordinary `Err` string back
/// to the plugin, the same shape every other host-function failure
/// already takes — never a trap/panic.
struct CapabilityGatedCallbacks {
    inner: Box<dyn HostCallbacks>,
    granted: std::collections::HashSet<String>,
}

impl CapabilityGatedCallbacks {
    fn new(inner: Box<dyn HostCallbacks>, capabilities: &[String]) -> Self {
        Self {
            inner,
            granted: capabilities.iter().cloned().collect(),
        }
    }

    fn check(&self, function: &str) -> std::result::Result<(), String> {
        match required_capability(function) {
            Some(capability) if !self.granted.contains(capability) => Err(format!(
                "plugin lacks the {capability:?} capability required to call {function} \
                 — declare it in plugin.toml's capabilities list"
            )),
            _ => Ok(()),
        }
    }
}

impl HostCallbacks for CapabilityGatedCallbacks {
    fn spawn_npc(&mut self, spawn_table_id: &str) -> std::result::Result<String, String> {
        self.check("spawn-npc")?;
        self.inner.spawn_npc(spawn_table_id)
    }

    fn send_message(
        &mut self,
        target_entity_id: &str,
        body: &str,
    ) -> std::result::Result<(), String> {
        self.check("send-message")?;
        self.inner.send_message(target_entity_id, body)
    }

    fn apply_stat_delta(
        &mut self,
        entity_id: &str,
        stat_key: &str,
        delta: i64,
    ) -> std::result::Result<(), String> {
        self.check("apply-stat-delta")?;
        self.inner.apply_stat_delta(entity_id, stat_key, delta)
    }

    fn move_entity(&mut self, entity_id: &str, x: f64, y: f64) -> std::result::Result<(), String> {
        self.check("move-entity")?;
        self.inner.move_entity(entity_id, x, y)
    }

    fn grant_item(
        &mut self,
        entity_id: &str,
        item_type: &str,
        quantity: i64,
    ) -> std::result::Result<(), String> {
        self.check("grant-item")?;
        self.inner.grant_item(entity_id, item_type, quantity)
    }

    fn remove_item(
        &mut self,
        entity_id: &str,
        item_type: &str,
        quantity: i64,
    ) -> std::result::Result<(), String> {
        self.check("remove-item")?;
        self.inner.remove_item(entity_id, item_type, quantity)
    }

    fn modify_currency(&mut self, entity_id: &str, delta: i64) -> std::result::Result<(), String> {
        self.check("modify-currency")?;
        self.inner.modify_currency(entity_id, delta)
    }

    fn caller_role(&mut self, entity_id: &str) -> std::result::Result<Vec<String>, String> {
        self.inner.caller_role(entity_id)
    }

    fn plugin_state_get(
        &mut self,
        scope: PluginStateScope,
        key: &str,
    ) -> std::result::Result<Option<Vec<u8>>, String> {
        self.inner.plugin_state_get(scope, key)
    }

    fn plugin_state_set(
        &mut self,
        scope: PluginStateScope,
        key: &str,
        value: Vec<u8>,
    ) -> std::result::Result<(), String> {
        self.inner.plugin_state_set(scope, key, value)
    }

    fn report_death(&mut self, entity_id: &str) -> std::result::Result<(), String> {
        self.check("report-death")?;
        self.inner.report_death(entity_id)
    }

    fn report_respawn(&mut self, entity_id: &str) -> std::result::Result<(), String> {
        self.check("report-respawn")?;
        self.inner.report_respawn(entity_id)
    }
}

struct PluginState {
    wasi_ctx: WasiCtx,
    table: ResourceTable,
    callbacks: Box<dyn HostCallbacks>,
}

impl WasiView for PluginState {
    fn ctx(&mut self) -> WasiCtxView<'_> {
        WasiCtxView {
            ctx: &mut self.wasi_ctx,
            table: &mut self.table,
        }
    }
}

impl HostInterface for PluginState {
    fn spawn_npc(&mut self, spawn_table_id: String) -> std::result::Result<String, String> {
        self.callbacks.spawn_npc(&spawn_table_id)
    }

    fn send_message(
        &mut self,
        target_entity_id: String,
        body: String,
    ) -> std::result::Result<(), String> {
        self.callbacks.send_message(&target_entity_id, &body)
    }

    fn apply_stat_delta(
        &mut self,
        entity_id: String,
        stat_key: String,
        delta: i64,
    ) -> std::result::Result<(), String> {
        self.callbacks
            .apply_stat_delta(&entity_id, &stat_key, delta)
    }

    fn move_entity(
        &mut self,
        entity_id: String,
        x: f64,
        y: f64,
    ) -> std::result::Result<(), String> {
        self.callbacks.move_entity(&entity_id, x, y)
    }

    fn grant_item(
        &mut self,
        entity_id: String,
        item_type: String,
        quantity: i64,
    ) -> std::result::Result<(), String> {
        self.callbacks.grant_item(&entity_id, &item_type, quantity)
    }

    fn remove_item(
        &mut self,
        entity_id: String,
        item_type: String,
        quantity: i64,
    ) -> std::result::Result<(), String> {
        self.callbacks.remove_item(&entity_id, &item_type, quantity)
    }

    fn modify_currency(
        &mut self,
        entity_id: String,
        delta: i64,
    ) -> std::result::Result<(), String> {
        self.callbacks.modify_currency(&entity_id, delta)
    }

    fn caller_role(&mut self, entity_id: String) -> std::result::Result<Vec<String>, String> {
        self.callbacks.caller_role(&entity_id)
    }

    fn plugin_state_get(
        &mut self,
        scope: WitPluginStateScope,
        key: String,
    ) -> std::result::Result<Option<Vec<u8>>, String> {
        self.callbacks.plugin_state_get(scope.into(), &key)
    }

    fn plugin_state_set(
        &mut self,
        scope: WitPluginStateScope,
        key: String,
        value: Vec<u8>,
    ) -> std::result::Result<(), String> {
        self.callbacks.plugin_state_set(scope.into(), &key, value)
    }

    fn report_death(&mut self, entity_id: String) -> std::result::Result<(), String> {
        self.callbacks.report_death(&entity_id)
    }

    fn report_respawn(&mut self, entity_id: String) -> std::result::Result<(), String> {
        self.callbacks.report_respawn(&entity_id)
    }
}

/// One `wasmtime::Engine` shared across every loaded plugin in a
/// zone-service — compiling/loading is the expensive part, the engine
/// itself is cheap to share (it's `Send + Sync`, designed for this).
pub struct PluginHost {
    engine: Engine,
}

impl Default for PluginHost {
    fn default() -> Self {
        Self::new()
    }
}

impl PluginHost {
    pub fn new() -> Self {
        Self {
            engine: Engine::default(),
        }
    }

    /// Loads and instantiates a plugin from its manifest + compiled
    /// `.wasm` component. Refuses a manifest declaring an incompatible
    /// `host_api_version` before ever touching the `.wasm` file — never
    /// a silent instantiate-and-hope.
    ///
    /// The instantiated plugin has zero ambient capability: no
    /// filesystem preopens, no network, no inherited stdio/env/args —
    /// `WasiCtxBuilder::new().build()` grants nothing beyond what the
    /// baseline WASI Preview 2 CLI imports need to let the guest's Rust
    /// std runtime start at all (docs comment on `wit/plugin.wit`'s
    /// world). Anything a plugin can actually *do* comes only through
    /// `host_callbacks`.
    pub fn load(
        &self,
        manifest: &PluginManifest,
        wasm_path: &Path,
        host_callbacks: Box<dyn HostCallbacks>,
    ) -> Result<LoadedPlugin> {
        manifest.check_compatible()?;

        let component = Component::from_file(&self.engine, wasm_path).map_err(|e| {
            Error::new(
                "plugin-host",
                format!(
                    "failed to load plugin component at {}: {e:#}",
                    wasm_path.display()
                ),
            )
        })?;

        let mut linker = Linker::<PluginState>::new(&self.engine);
        wasmtime_wasi::p2::add_to_linker_sync(&mut linker)
            .map_err(|e| Error::new("plugin-host", format!("failed to link WASI: {e:#}")))?;
        crate::bindings::worldzero::plugin::host::add_to_linker::<_, HasPluginState>(
            &mut linker,
            |state| state,
        )
        .map_err(|e| {
            Error::new(
                "plugin-host",
                format!("failed to link the host interface: {e:#}"),
            )
        })?;

        let state = PluginState {
            wasi_ctx: WasiCtxBuilder::new().build(),
            table: ResourceTable::new(),
            // Every loaded plugin's calls go through the capability gate
            // (#153) — enforced here, once, rather than trusting each
            // `HostCallbacks` implementor to re-check it.
            callbacks: Box::new(CapabilityGatedCallbacks::new(
                host_callbacks,
                &manifest.plugin.capabilities,
            )),
        };
        let mut store = Store::new(&self.engine, state);

        let bindings =
            PluginBindings::instantiate(&mut store, &component, &linker).map_err(|e| {
                Error::new(
                    "plugin-host",
                    format!(
                        "failed to instantiate plugin {:?}: {e:#}",
                        manifest.plugin.name
                    ),
                )
            })?;

        Ok(LoadedPlugin { store, bindings })
    }
}

struct HasPluginState;

impl wasmtime::component::HasData for HasPluginState {
    type Data<'a> = &'a mut PluginState;
}

/// A live, sandboxed plugin instance. A panic/trap inside the guest
/// surfaces as an `Err` from whichever hook call triggered it — it does
/// not, and cannot, crash the host process (#37's acceptance criteria);
/// `wasmtime` traps are ordinary Rust errors at this boundary, not
/// process-level signals.
pub struct LoadedPlugin {
    store: Store<PluginState>,
    bindings: PluginBindings,
}

impl LoadedPlugin {
    #[tracing::instrument(skip_all)]
    /// Called once, at process startup, for genuinely global setup only
    /// (#152) — one plugin instance now serves every zone-service, so
    /// there's no zone context here; see `on_zone_loaded` for per-zone
    /// initialization.
    pub fn on_load(&mut self) -> Result<()> {
        self.bindings
            .worldzero_plugin_hooks()
            .call_on_load(&mut self.store)
            .map_err(|e| Error::new("plugin-host", format!("on_load hook failed: {e:#}")))
    }

    pub fn on_unload(&mut self) -> Result<()> {
        self.bindings
            .worldzero_plugin_hooks()
            .call_on_unload(&mut self.store)
            .map_err(|e| Error::new("plugin-host", format!("on_unload hook failed: {e:#}")))
    }

    /// Live: `server::main` calls this once per zone-service, as that
    /// zone starts up (#152) — the per-zone counterpart to `on_load`,
    /// e.g. for `spawn-npc` calls against that zone's own spawn tables.
    pub fn on_zone_loaded(&mut self, zone_id: &str) -> Result<()> {
        self.bindings
            .worldzero_plugin_hooks()
            .call_on_zone_loaded(&mut self.store, zone_id)
            .map_err(|e| Error::new("plugin-host", format!("on_zone_loaded hook failed: {e:#}")))
    }

    pub fn on_entity_spawn(
        &mut self,
        zone_id: &str,
        entity_id: &str,
        entity_type: &str,
    ) -> Result<()> {
        self.bindings
            .worldzero_plugin_hooks()
            .call_on_entity_spawn(&mut self.store, zone_id, entity_id, entity_type)
            .map_err(|e| Error::new("plugin-host", format!("on_entity_spawn hook failed: {e:#}")))
    }

    /// Live: `server::session` calls this once a connection's character
    /// is fully spawned into `zone_id`, after roster delivery (#155).
    pub fn on_player_join_zone(&mut self, zone_id: &str, entity_id: &str) -> Result<()> {
        self.bindings
            .worldzero_plugin_hooks()
            .call_on_player_join_zone(&mut self.store, zone_id, entity_id)
            .map_err(|e| {
                Error::new(
                    "plugin-host",
                    format!("on_player_join_zone hook failed: {e:#}"),
                )
            })
    }

    /// Live: `server::session` calls this on a connection's clean
    /// disconnect from `zone_id` (#155).
    pub fn on_player_leave_zone(&mut self, zone_id: &str, entity_id: &str) -> Result<()> {
        self.bindings
            .worldzero_plugin_hooks()
            .call_on_player_leave_zone(&mut self.store, zone_id, entity_id)
            .map_err(|e| {
                Error::new(
                    "plugin-host",
                    format!("on_player_leave_zone hook failed: {e:#}"),
                )
            })
    }

    pub fn on_interact(
        &mut self,
        zone_id: &str,
        trigger_id: &str,
        actor_entity_id: &str,
    ) -> Result<()> {
        self.bindings
            .worldzero_plugin_hooks()
            .call_on_interact(&mut self.store, zone_id, trigger_id, actor_entity_id)
            .map_err(|e| Error::new("plugin-host", format!("on_interact hook failed: {e:#}")))
    }

    /// Delivers a gateway-routed message whose `message_type` matched one
    /// of this plugin's declared `message_types` (#95) — the caller is
    /// responsible for that match; this always calls the hook.
    #[tracing::instrument(skip(self, payload), fields(zone_id, message_type, sender_entity_id))]
    pub fn on_message(
        &mut self,
        zone_id: &str,
        message_type: u16,
        sender_entity_id: &str,
        payload: &[u8],
    ) -> Result<()> {
        self.bindings
            .worldzero_plugin_hooks()
            .call_on_message(
                &mut self.store,
                zone_id,
                message_type,
                sender_entity_id,
                payload,
            )
            .map_err(|e| Error::new("plugin-host", format!("on_message hook failed: {e:#}")))
    }

    /// Live: `server::world_actor` calls this when a client's `Attack`
    /// action names a valid target entity (#154) — `base_amount` is
    /// always `0` (the core never invents a damage number, see
    /// `wit/plugin.wit`'s doc comment); the plugin owns the whole
    /// mitigation formula and must call `apply_stat_delta` itself.
    #[allow(clippy::too_many_arguments)]
    pub fn on_damage_calc(
        &mut self,
        zone_id: &str,
        attacker_entity_id: &str,
        target_entity_id: &str,
        stat_key: &str,
        base_amount: i64,
    ) -> Result<()> {
        self.bindings
            .worldzero_plugin_hooks()
            .call_on_damage_calc(
                &mut self.store,
                zone_id,
                attacker_entity_id,
                target_entity_id,
                stat_key,
                base_amount,
            )
            .map_err(|e| Error::new("plugin-host", format!("on_damage_calc hook failed: {e:#}")))
    }

    /// Live: `server::world_actor` calls this after applying a queued
    /// `report-death` request (#154) — the plugin decided this entity
    /// died and reported it; this is the host's confirmation callback,
    /// not a request for the plugin to decide anything.
    pub fn on_death(&mut self, zone_id: &str, entity_id: &str) -> Result<()> {
        self.bindings
            .worldzero_plugin_hooks()
            .call_on_death(&mut self.store, zone_id, entity_id)
            .map_err(|e| Error::new("plugin-host", format!("on_death hook failed: {e:#}")))
    }

    /// Live, same shape as `on_death` — fired after a queued
    /// `report-respawn` request is applied (#154).
    pub fn on_respawn(&mut self, zone_id: &str, entity_id: &str) -> Result<()> {
        self.bindings
            .worldzero_plugin_hooks()
            .call_on_respawn(&mut self.store, zone_id, entity_id)
            .map_err(|e| Error::new("plugin-host", format!("on_respawn hook failed: {e:#}")))
    }

    /// Called once per tick, per zone, for an NPC entity whose spawn
    /// table declared a route (`world::world_actor` drives this call
    /// site — the plugin is expected to respond with `move-entity`
    /// calls, not have its NPC moved for it).
    #[allow(clippy::too_many_arguments)]
    pub fn on_npc_tick(
        &mut self,
        zone_id: &str,
        entity_id: &str,
        x: f64,
        y: f64,
        route_waypoints: &[(f64, f64)],
        route_loop: bool,
        route_speed: f64,
        dt: f64,
    ) -> Result<()> {
        self.bindings
            .worldzero_plugin_hooks()
            .call_on_npc_tick(
                &mut self.store,
                zone_id,
                entity_id,
                x,
                y,
                route_waypoints,
                route_loop,
                route_speed,
                dt,
            )
            .map_err(|e| Error::new("plugin-host", format!("on_npc_tick hook failed: {e:#}")))
    }

    /// Live: `server::world_actor` calls this when a client's
    /// `InteractNpc` action names a currently-spawned NPC entity (#154) —
    /// distinct from the generic trigger-volume `on_interact` above.
    pub fn on_npc_interact(
        &mut self,
        zone_id: &str,
        npc_entity_id: &str,
        actor_entity_id: &str,
    ) -> Result<()> {
        self.bindings
            .worldzero_plugin_hooks()
            .call_on_npc_interact(&mut self.store, zone_id, npc_entity_id, actor_entity_id)
            .map_err(|e| Error::new("plugin-host", format!("on_npc_interact hook failed: {e:#}")))
    }

    /// Delivers a chat message whose leading `/command` matched one of
    /// this plugin's declared `chat_commands` (`plugin.toml`) — the
    /// caller is responsible for that match, same contract as
    /// `on_message`.
    pub fn on_chat_command(
        &mut self,
        zone_id: &str,
        command: &str,
        args: &str,
        sender_entity_id: &str,
    ) -> Result<()> {
        self.bindings
            .worldzero_plugin_hooks()
            .call_on_chat_command(&mut self.store, zone_id, command, args, sender_entity_id)
            .map_err(|e| Error::new("plugin-host", format!("on_chat_command hook failed: {e:#}")))
    }

    /// Live: `world::world_actor` calls this itself right after applying
    /// a queued `grant-item` request, so a plugin can treat this as
    /// confirmation the grant actually went through — `new_quantity` is
    /// the item type's new total, not the delta just granted.
    pub fn on_item_acquire(
        &mut self,
        zone_id: &str,
        entity_id: &str,
        item_type: &str,
        new_quantity: i64,
    ) -> Result<()> {
        self.bindings
            .worldzero_plugin_hooks()
            .call_on_item_acquire(&mut self.store, zone_id, entity_id, item_type, new_quantity)
            .map_err(|e| Error::new("plugin-host", format!("on_item_acquire hook failed: {e:#}")))
    }

    /// Live: `server::world_actor` calls this when a client sends a
    /// `UseItem` action (#154) — the core never validates ownership
    /// itself; the plugin decides what using `item_type` does and is
    /// expected to call `remove-item` if that's the right response.
    pub fn on_item_use(&mut self, zone_id: &str, entity_id: &str, item_type: &str) -> Result<()> {
        self.bindings
            .worldzero_plugin_hooks()
            .call_on_item_use(&mut self.store, zone_id, entity_id, item_type)
            .map_err(|e| Error::new("plugin-host", format!("on_item_use hook failed: {e:#}")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A `HostCallbacks` that always succeeds — these tests only care
    /// whether `CapabilityGatedCallbacks` lets a call *through*, not what
    /// the underlying implementor does with it (`plugin_sandbox.rs`
    /// covers the real-wasm end-to-end case, #153's acceptance criteria).
    struct AlwaysOk;

    impl HostCallbacks for AlwaysOk {
        fn spawn_npc(&mut self, _: &str) -> std::result::Result<String, String> {
            Ok(String::new())
        }
        fn send_message(&mut self, _: &str, _: &str) -> std::result::Result<(), String> {
            Ok(())
        }
        fn apply_stat_delta(
            &mut self,
            _: &str,
            _: &str,
            _: i64,
        ) -> std::result::Result<(), String> {
            Ok(())
        }
        fn move_entity(&mut self, _: &str, _: f64, _: f64) -> std::result::Result<(), String> {
            Ok(())
        }
        fn grant_item(&mut self, _: &str, _: &str, _: i64) -> std::result::Result<(), String> {
            Ok(())
        }
        fn remove_item(&mut self, _: &str, _: &str, _: i64) -> std::result::Result<(), String> {
            Ok(())
        }
        fn modify_currency(&mut self, _: &str, _: i64) -> std::result::Result<(), String> {
            Ok(())
        }
        fn caller_role(&mut self, _: &str) -> std::result::Result<Vec<String>, String> {
            Ok(Vec::new())
        }
        fn plugin_state_get(
            &mut self,
            _: PluginStateScope,
            _: &str,
        ) -> std::result::Result<Option<Vec<u8>>, String> {
            Ok(None)
        }
        fn plugin_state_set(
            &mut self,
            _: PluginStateScope,
            _: &str,
            _: Vec<u8>,
        ) -> std::result::Result<(), String> {
            Ok(())
        }
        fn report_death(&mut self, _: &str) -> std::result::Result<(), String> {
            Ok(())
        }
        fn report_respawn(&mut self, _: &str) -> std::result::Result<(), String> {
            Ok(())
        }
    }

    fn gated(capabilities: &[&str]) -> CapabilityGatedCallbacks {
        CapabilityGatedCallbacks::new(
            Box::new(AlwaysOk),
            &capabilities
                .iter()
                .map(|s| s.to_string())
                .collect::<Vec<_>>(),
        )
    }

    #[test]
    fn ungated_functions_always_succeed_with_no_capabilities_declared() {
        let mut callbacks = gated(&[]);
        assert!(callbacks.caller_role("e1").is_ok());
        assert!(
            callbacks
                .plugin_state_get(PluginStateScope::Entity("e1".to_string()), "k")
                .is_ok()
        );
        assert!(
            callbacks
                .plugin_state_set(PluginStateScope::Entity("e1".to_string()), "k", vec![])
                .is_ok()
        );
    }

    #[test]
    fn each_gated_function_is_rejected_without_its_capability_and_allowed_with_it() {
        let mut none = gated(&[]);
        assert!(none.spawn_npc("table").is_err());
        assert!(none.send_message("e1", "hi").is_err());
        assert!(none.move_entity("e1", 0.0, 0.0).is_err());
        assert!(none.apply_stat_delta("e1", "hp", -1).is_err());
        assert!(none.report_death("e1").is_err());
        assert!(none.report_respawn("e1").is_err());
        assert!(none.grant_item("e1", "torch", 1).is_err());
        assert!(none.remove_item("e1", "torch", 1).is_err());
        assert!(none.modify_currency("e1", 1).is_err());

        let mut spawning = gated(&["spawning"]);
        assert!(spawning.spawn_npc("table").is_ok());
        assert!(spawning.move_entity("e1", 0.0, 0.0).is_err());

        let mut messaging = gated(&["messaging"]);
        assert!(messaging.send_message("e1", "hi").is_ok());
        assert!(messaging.spawn_npc("table").is_err());

        let mut movement = gated(&["movement"]);
        assert!(movement.move_entity("e1", 0.0, 0.0).is_ok());
        assert!(movement.apply_stat_delta("e1", "hp", -1).is_err());

        let mut combat = gated(&["combat"]);
        assert!(combat.apply_stat_delta("e1", "hp", -1).is_ok());
        assert!(combat.report_death("e1").is_ok());
        assert!(combat.report_respawn("e1").is_ok());
        assert!(combat.grant_item("e1", "torch", 1).is_err());

        let mut economy = gated(&["economy"]);
        assert!(economy.grant_item("e1", "torch", 1).is_ok());
        assert!(economy.remove_item("e1", "torch", 1).is_ok());
        assert!(economy.modify_currency("e1", 1).is_ok());
        assert!(economy.spawn_npc("table").is_err());
    }

    #[test]
    fn a_rejection_names_the_missing_capability() {
        let mut none = gated(&[]);
        let err = none.grant_item("e1", "torch", 1).unwrap_err();
        assert!(err.contains("economy"), "{err}");
        assert!(err.contains("grant-item"), "{err}");
    }
}
