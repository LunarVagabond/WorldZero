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
            callbacks: host_callbacks,
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

    pub fn on_entity_spawn(&mut self, entity_id: &str, entity_type: &str) -> Result<()> {
        self.bindings
            .worldzero_plugin_hooks()
            .call_on_entity_spawn(&mut self.store, entity_id, entity_type)
            .map_err(|e| Error::new("plugin-host", format!("on_entity_spawn hook failed: {e:#}")))
    }

    /// Live: `server::session` calls this once a connection's character
    /// is fully spawned into a zone, after roster delivery (#155).
    pub fn on_player_join_zone(&mut self, entity_id: &str) -> Result<()> {
        self.bindings
            .worldzero_plugin_hooks()
            .call_on_player_join_zone(&mut self.store, entity_id)
            .map_err(|e| {
                Error::new(
                    "plugin-host",
                    format!("on_player_join_zone hook failed: {e:#}"),
                )
            })
    }

    /// Live: `server::session` calls this on a connection's clean
    /// disconnect (#155).
    pub fn on_player_leave_zone(&mut self, entity_id: &str) -> Result<()> {
        self.bindings
            .worldzero_plugin_hooks()
            .call_on_player_leave_zone(&mut self.store, entity_id)
            .map_err(|e| {
                Error::new(
                    "plugin-host",
                    format!("on_player_leave_zone hook failed: {e:#}"),
                )
            })
    }

    pub fn on_interact(&mut self, trigger_id: &str, actor_entity_id: &str) -> Result<()> {
        self.bindings
            .worldzero_plugin_hooks()
            .call_on_interact(&mut self.store, trigger_id, actor_entity_id)
            .map_err(|e| Error::new("plugin-host", format!("on_interact hook failed: {e:#}")))
    }

    /// Delivers a gateway-routed message whose `message_type` matched one
    /// of this plugin's declared `message_types` (#95) — the caller is
    /// responsible for that match; this always calls the hook.
    #[tracing::instrument(skip(self, payload), fields(message_type, sender_entity_id))]
    pub fn on_message(
        &mut self,
        message_type: u16,
        sender_entity_id: &str,
        payload: &[u8],
    ) -> Result<()> {
        self.bindings
            .worldzero_plugin_hooks()
            .call_on_message(&mut self.store, message_type, sender_entity_id, payload)
            .map_err(|e| Error::new("plugin-host", format!("on_message hook failed: {e:#}")))
    }

    /// No live host call site exists yet — see `wit/plugin.wit`'s
    /// `on-damage-calc` doc comment.
    pub fn on_damage_calc(
        &mut self,
        attacker_entity_id: &str,
        target_entity_id: &str,
        stat_key: &str,
        base_amount: i64,
    ) -> Result<()> {
        self.bindings
            .worldzero_plugin_hooks()
            .call_on_damage_calc(
                &mut self.store,
                attacker_entity_id,
                target_entity_id,
                stat_key,
                base_amount,
            )
            .map_err(|e| Error::new("plugin-host", format!("on_damage_calc hook failed: {e:#}")))
    }

    /// No live host call site exists yet — see `wit/plugin.wit`'s
    /// `on-death` doc comment.
    pub fn on_death(&mut self, entity_id: &str) -> Result<()> {
        self.bindings
            .worldzero_plugin_hooks()
            .call_on_death(&mut self.store, entity_id)
            .map_err(|e| Error::new("plugin-host", format!("on_death hook failed: {e:#}")))
    }

    /// No live host call site exists yet — see `wit/plugin.wit`'s
    /// `on-respawn` doc comment.
    pub fn on_respawn(&mut self, entity_id: &str) -> Result<()> {
        self.bindings
            .worldzero_plugin_hooks()
            .call_on_respawn(&mut self.store, entity_id)
            .map_err(|e| Error::new("plugin-host", format!("on_respawn hook failed: {e:#}")))
    }

    /// Called once per tick for an NPC entity whose spawn table declared
    /// a route (`world::world_actor` drives this call site — the plugin
    /// is expected to respond with `move-entity` calls, not have its NPC
    /// moved for it).
    #[allow(clippy::too_many_arguments)]
    pub fn on_npc_tick(
        &mut self,
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

    /// No live host call site exists yet — a player-targets-an-NPC
    /// client action doesn't exist in `docs/specs/Networking_Spec.md`'s
    /// message catalog yet, only the generic trigger-volume `on-interact`.
    pub fn on_npc_interact(&mut self, npc_entity_id: &str, actor_entity_id: &str) -> Result<()> {
        self.bindings
            .worldzero_plugin_hooks()
            .call_on_npc_interact(&mut self.store, npc_entity_id, actor_entity_id)
            .map_err(|e| Error::new("plugin-host", format!("on_npc_interact hook failed: {e:#}")))
    }

    /// Delivers a chat message whose leading `/command` matched one of
    /// this plugin's declared `chat_commands` (`plugin.toml`) — the
    /// caller is responsible for that match, same contract as
    /// `on_message`.
    pub fn on_chat_command(
        &mut self,
        command: &str,
        args: &str,
        sender_entity_id: &str,
    ) -> Result<()> {
        self.bindings
            .worldzero_plugin_hooks()
            .call_on_chat_command(&mut self.store, command, args, sender_entity_id)
            .map_err(|e| Error::new("plugin-host", format!("on_chat_command hook failed: {e:#}")))
    }

    /// Live: `world::world_actor` calls this itself right after applying
    /// a queued `grant-item` request, so a plugin can treat this as
    /// confirmation the grant actually went through — `new_quantity` is
    /// the item type's new total, not the delta just granted.
    pub fn on_item_acquire(
        &mut self,
        entity_id: &str,
        item_type: &str,
        new_quantity: i64,
    ) -> Result<()> {
        self.bindings
            .worldzero_plugin_hooks()
            .call_on_item_acquire(&mut self.store, entity_id, item_type, new_quantity)
            .map_err(|e| Error::new("plugin-host", format!("on_item_acquire hook failed: {e:#}")))
    }

    /// No live host call site exists yet — see `wit/plugin.wit`'s
    /// `on-item-use` doc comment.
    pub fn on_item_use(&mut self, entity_id: &str, item_type: &str) -> Result<()> {
        self.bindings
            .worldzero_plugin_hooks()
            .call_on_item_use(&mut self.store, entity_id, item_type)
            .map_err(|e| Error::new("plugin-host", format!("on_item_use hook failed: {e:#}")))
    }
}
