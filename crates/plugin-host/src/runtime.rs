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
use crate::manifest::PluginManifest;

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

    pub fn on_interact(&mut self, trigger_id: &str, actor_entity_id: &str) -> Result<()> {
        self.bindings
            .worldzero_plugin_hooks()
            .call_on_interact(&mut self.store, trigger_id, actor_entity_id)
            .map_err(|e| Error::new("plugin-host", format!("on_interact hook failed: {e:#}")))
    }
}
