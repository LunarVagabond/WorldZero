//! Plugin loading at server startup — loads the configured plugin, runs
//! its `on_load` hook, and hands back the still-alive instance (plus
//! which spawn tables it asked to spawn from) so `main` can seed the zone
//! before the world actor starts, and keep the plugin running for the
//! rest of the process's life (docs/PROPOSAL.md, "Phased Roadmap," Phase
//! 1: "minimal plugin hook (e.g. NPC spawn + one interaction)"; #95:
//! gateway-routed messages reaching the plugin via `on_message` need it
//! kept alive past startup, not dropped immediately after `on_load`).
//!
//! Also wires `plugin-state-get`/`plugin-state-set` (#149,
//! `crate::plugin_state`'s module doc) — see `PluginCallbacks`'s own
//! doc comment for the cache/queue split those two need — and
//! `report-death`/`report-respawn` (#154), the plugin-owned trigger for
//! `on-death`/`on-respawn` — and `block-zone-channel` (#186), the
//! zone-chat-auto-join block/restriction primitive (`server::chat_session`'s
//! own doc comments have the auto-join side). Still no `on_tick` (see
//! docs/specs/Plugin_API.md, "Beyond this v0 slice").

use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex};

use common::Result;
use common::id::EntityId;
use plugin_host::manifest::KNOWN_HOOKS;
use plugin_host::{HostCallbacks, LoadedPlugin, PluginHost, PluginManifest, PluginStateScope};

use crate::plugin_state::{PluginStateCache, cache_key};
use crate::session::{BlockedZoneChannels, EntityRoles, Sessions};
use crate::session_protocol::ServerMessage;

/// `(scope, key, value)` requested via `plugin-state-set` for
/// `character`/`zone` scope, queued for the caller's own drain — see
/// `PluginCallbacks`'s `pending_state_writes` field.
type PendingStateWrites = Arc<Mutex<Vec<(PluginStateScope, String, Vec<u8>)>>>;

/// `(entity_id, x, y, z)` requested via `move-entity` (#249/#254 — `z`
/// is authoritative), queued for the caller's own drain — see
/// `PluginCallbacks`'s `pending_moves` field.
type PendingMoves = Arc<Mutex<Vec<(String, f64, f64, f64)>>>;

/// `item_type -> content::ItemCatalogEntry` (#287) — the in-memory mirror
/// of `content::ItemCatalogStore`'s current rows, shared process-wide
/// (`main` hydrates it from the DB once, before any plugin's `on_load`
/// runs) and kept current by every `register-item-type` call since —
/// backs `get-item`'s synchronous read, same "no live DB read/write from
/// inside a sandboxed call" discipline `PluginStateCache`/`EntityRoles`
/// already follow.
pub type ItemCatalogCache = Arc<Mutex<HashMap<String, content::ItemCatalogEntry>>>;

/// `(item_type, display_name, tags, metadata)` requested via
/// `register-item-type` since the last drain — queued for the caller's
/// own drain into `content::ItemCatalogStore::register` (the actual
/// Postgres write; `PluginCallbacks::register_item_type` already applied
/// it to `item_catalog_cache` immediately, for same-process
/// `get-item` visibility, before this queue is ever drained).
type PendingItemRegistrations = Arc<Mutex<Vec<content::ItemCatalogEntry>>>;

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
    /// `(character_id, stat_key, delta)` from `apply-stat-delta-for-character`
    /// (#194) — same "record now, drain-and-apply later" reasoning as
    /// `pending_stat_deltas`, but keyed by character id directly since
    /// this is only ever called from `on-character-create`, before any
    /// entity exists.
    pending_character_stat_deltas: Arc<Mutex<Vec<(String, String, i64)>>>,
    /// `(entity_id, x, y, z)`, drained and applied via
    /// `world::Zone::request_move` by the caller.
    pending_moves: PendingMoves,
    /// `(entity_id, item_type, quantity)`, drained and applied through
    /// `character::CharacterStore::grant_item` by the caller (#57/#112).
    pending_item_grants: Arc<Mutex<Vec<(String, String, i64)>>>,
    /// `(entity_id, item_type, quantity)`, drained and applied through
    /// `character::CharacterStore::remove_item` by the caller.
    pending_item_removals: Arc<Mutex<Vec<(String, String, i64)>>>,
    /// `(entity_id, currency_key, delta)`, drained and applied through
    /// `character::CharacterStore::modify_currency` by the caller.
    pending_currency_deltas: Arc<Mutex<Vec<(String, String, i64)>>>,
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
    /// Entity ids reported dead via `report-death`, drained and applied
    /// (fires `on-death` back) by the caller — same "can't reach `&mut
    /// Zone`/the DB from inside a sandboxed sync call" reasoning as
    /// every other `pending_*` field above (#154).
    pending_deaths: Arc<Mutex<Vec<String>>>,
    /// Same shape as `pending_deaths`, for `report-respawn`/`on-respawn`.
    pending_respawns: Arc<Mutex<Vec<String>>>,
    /// Backs `block-zone-channel` (#186) — a direct, synchronous write
    /// into the shared cache `server::session` also reads from when
    /// deciding whether to auto-join a zone channel, same "no queue,
    /// applied immediately" shape `plugin_state_cache`'s `entity` scope
    /// already uses (there's nothing to durably persist here either).
    blocked_zone_channels: BlockedZoneChannels,
    /// Backs `register-item-type`/`get-item` (#287) — see
    /// [`ItemCatalogCache`]'s own doc comment.
    item_catalog_cache: ItemCatalogCache,
    /// `register-item-type` requests since the last drain — see
    /// [`PendingItemRegistrations`].
    pending_item_registrations: PendingItemRegistrations,
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

    fn apply_stat_delta_for_character(
        &mut self,
        character_id: &str,
        stat_key: &str,
        delta: i64,
    ) -> std::result::Result<(), String> {
        self.pending_character_stat_deltas.lock().unwrap().push((
            character_id.to_string(),
            stat_key.to_string(),
            delta,
        ));
        Ok(())
    }

    fn move_entity(
        &mut self,
        entity_id: &str,
        x: f64,
        y: f64,
        z: f64,
    ) -> std::result::Result<(), String> {
        self.pending_moves
            .lock()
            .unwrap()
            .push((entity_id.to_string(), x, y, z));
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

    fn modify_currency(
        &mut self,
        entity_id: &str,
        currency_key: &str,
        delta: i64,
    ) -> std::result::Result<(), String> {
        self.pending_currency_deltas.lock().unwrap().push((
            entity_id.to_string(),
            currency_key.to_string(),
            delta,
        ));
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

    fn report_death(&mut self, entity_id: &str) -> std::result::Result<(), String> {
        self.pending_deaths
            .lock()
            .unwrap()
            .push(entity_id.to_string());
        Ok(())
    }

    fn report_respawn(&mut self, entity_id: &str) -> std::result::Result<(), String> {
        self.pending_respawns
            .lock()
            .unwrap()
            .push(entity_id.to_string());
        Ok(())
    }

    fn block_zone_channel(
        &mut self,
        entity_id: &str,
        category: &str,
    ) -> std::result::Result<(), String> {
        let entity_id: EntityId = entity_id
            .parse()
            .map_err(|_| format!("{entity_id:?} is not a valid entity id"))?;
        self.blocked_zone_channels
            .lock()
            .unwrap()
            .entry(entity_id)
            .or_default()
            .insert(category.to_string());
        Ok(())
    }

    fn register_item_type(
        &mut self,
        item_type: &str,
        display_name: &str,
        tags: Vec<String>,
        metadata: &str,
    ) -> std::result::Result<(), String> {
        let metadata: serde_json::Value = serde_json::from_str(metadata)
            .map_err(|e| format!("register-item-type metadata is not valid JSON: {e}"))?;
        let entry = content::ItemCatalogEntry {
            item_type: item_type.to_string(),
            display_name: display_name.to_string(),
            tags,
            metadata,
        };
        let problems = content::items::validate_entry(&entry);
        if !problems.is_empty() {
            return Err(format!(
                "register-item-type: item {item_type:?} is invalid: {}",
                problems.join("; ")
            ));
        }

        // Applied to the in-memory cache immediately — a `get-item` call
        // later in the same `on_load` (this plugin's, or another loaded
        // plugin's) sees it right away — same "cache first, durable
        // write queued" shape `plugin_state_set` already uses above.
        self.item_catalog_cache
            .lock()
            .unwrap()
            .insert(entry.item_type.clone(), entry.clone());
        self.pending_item_registrations.lock().unwrap().push(entry);
        Ok(())
    }

    fn get_item(
        &mut self,
        item_type: &str,
    ) -> std::result::Result<Option<plugin_host::ItemCatalogEntry>, String> {
        Ok(self
            .item_catalog_cache
            .lock()
            .unwrap()
            .get(item_type)
            .map(|entry| plugin_host::ItemCatalogEntry {
                item_type: entry.item_type.clone(),
                display_name: entry.display_name.clone(),
                tags: entry.tags.clone(),
                metadata: entry.metadata.to_string(),
            }))
    }
}

/// A plugin kept alive past startup: the live instance, which
/// `message_type`s it declared (empty if none), and a handle to drain
/// `spawn-npc` requests it makes later (from `on_message` — the callback
/// boxed inside the plugin's `Store` can't be reached directly, so
/// callers drain this shared queue instead; see `PluginCallbacks`' docs
/// for why `spawn_npc` only ever records rather than spawning directly).
pub struct PluginRuntime {
    /// Free-form, from `plugin.toml`'s `plugin.name` — used only in log
    /// messages (`world_actor`'s per-plugin warnings on a failed hook
    /// call) now that more than one plugin can be loaded at once (#152).
    pub name: String,
    pub plugin: LoadedPlugin,
    pub message_types: Vec<u16>,
    /// Command names (without the leading `/`) declared in `plugin.toml`
    /// — routed to `on-chat-command` instead of published as ordinary
    /// chat (#57).
    pub chat_commands: Vec<String>,
    /// Which hooks (`plugin_host::manifest::KNOWN_HOOKS`) this plugin
    /// declared — `world_actor`'s dispatch only calls a hook if it's
    /// listed here (#152); `on-message`/`on-chat-command` are the
    /// exception, routed on `message_types`/`chat_commands` membership
    /// alone (declaring either already states interest, see
    /// `plugin_host::manifest::PluginDeclaration::hooks`'s doc comment).
    pub hooks: Vec<String>,
    /// Which capabilities (`plugin_host::manifest::KNOWN_CAPABILITIES`)
    /// this plugin declared — every other capability-gated boundary in
    /// this codebase enforces at the host-*function*-call level
    /// (`plugin_host::runtime::CapabilityGatedCallbacks`), but
    /// `on-craft-complete` (#216) is gated at the hook-*firing* level
    /// instead, since it has no corresponding host function call to gate:
    /// `fire_on_craft_complete` (`session.rs`) only calls the hook at all
    /// if this includes `economy`.
    pub capabilities: Vec<String>,
    pending_spawns: Arc<Mutex<Vec<String>>>,
    pending_stat_deltas: Arc<Mutex<Vec<(String, String, i64)>>>,
    pending_character_stat_deltas: Arc<Mutex<Vec<(String, String, i64)>>>,
    pending_moves: PendingMoves,
    pending_item_grants: Arc<Mutex<Vec<(String, String, i64)>>>,
    pending_item_removals: Arc<Mutex<Vec<(String, String, i64)>>>,
    pending_currency_deltas: Arc<Mutex<Vec<(String, String, i64)>>>,
    pending_state_writes: PendingStateWrites,
    pending_deaths: Arc<Mutex<Vec<String>>>,
    pending_respawns: Arc<Mutex<Vec<String>>>,
    pending_item_registrations: PendingItemRegistrations,
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

    /// `(character_id, stat_key, delta)` requested via
    /// `apply-stat-delta-for-character` since the last drain, in call
    /// order (#194).
    pub fn drain_pending_character_stat_deltas(&self) -> Vec<(String, String, i64)> {
        std::mem::take(&mut self.pending_character_stat_deltas.lock().unwrap())
    }

    /// `(entity_id, x, y, z)` requested via `move-entity` since the last
    /// drain, in call order.
    pub fn drain_pending_moves(&self) -> Vec<(String, f64, f64, f64)> {
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

    /// `(entity_id, currency_key, delta)` requested via `modify-currency`
    /// since the last drain, in call order.
    pub fn drain_pending_currency_deltas(&self) -> Vec<(String, String, i64)> {
        std::mem::take(&mut self.pending_currency_deltas.lock().unwrap())
    }

    /// `(scope, key, value)` requested via `plugin-state-set` for
    /// `character`/`zone` scope since the last drain, in call order —
    /// `entity` scope never reaches this queue (#149, nothing to
    /// persist).
    pub fn drain_pending_state_writes(&self) -> Vec<(PluginStateScope, String, Vec<u8>)> {
        std::mem::take(&mut self.pending_state_writes.lock().unwrap())
    }

    /// Entity ids reported via `report-death` since the last drain, in
    /// call order (#154).
    pub fn drain_pending_deaths(&self) -> Vec<String> {
        std::mem::take(&mut self.pending_deaths.lock().unwrap())
    }

    /// Same shape as `drain_pending_deaths`, for `report-respawn`.
    pub fn drain_pending_respawns(&self) -> Vec<String> {
        std::mem::take(&mut self.pending_respawns.lock().unwrap())
    }

    /// Entries requested via `register-item-type` since the last drain,
    /// in call order (#287) — `load_plugin` already drains and persists
    /// whatever was requested during `on_load` itself before this
    /// `PluginRuntime` is ever handed back (see that function's own doc
    /// comment for why); `world_actor::drain_and_apply_plugin_effects`
    /// drains and persists this queue for every *later* call (from
    /// `on_tick`, `on_interact`, a chat command, or any other hook), the
    /// same "drain after every hook call, regardless of which hook made
    /// the request" discipline every other `pending_*` queue on this
    /// struct already gets.
    pub fn drain_pending_item_registrations(&self) -> Vec<content::ItemCatalogEntry> {
        std::mem::take(&mut self.pending_item_registrations.lock().unwrap())
    }

    /// Whether this plugin declared `hook` in `plugin.toml`'s `hooks`
    /// list — the gate `world_actor`'s dispatch checks before calling
    /// any hook except `on-message`/`on-chat-command` (#152).
    pub fn wants(&self, hook: &str) -> bool {
        self.hooks.iter().any(|h| h == hook)
    }

    /// Whether this plugin declared `capability` in `plugin.toml`'s
    /// `capabilities` list — see this struct's `capabilities` field doc
    /// comment for why `on-craft-complete` needs this checked at the
    /// hook-firing site rather than relying on
    /// `CapabilityGatedCallbacks` alone.
    pub fn has_capability(&self, capability: &str) -> bool {
        self.capabilities.iter().any(|c| c == capability)
    }
}

/// Loads one plugin instance from an already-parsed, already-validated
/// `manifest` (`main` discovers and validates every manifest in the
/// plugins directory up front — individually via `check_compatible` and
/// collectively via `check_no_collisions` — before any of them are ever
/// instantiated, #152) plus its compiled `wasm_path`, and runs its
/// `on_load` hook if (and only if) the manifest declared `"on-load"` in
/// its `hooks` list — same opt-in gate every other hook goes through,
/// applied here since `on_load` is otherwise a special unconditional
/// case. Returns the still-alive plugin — the caller is responsible for
/// keeping it running; dropping it tears it down — plus the spawn-table
/// ids it requested via `spawn-npc` during `on_load`, in call order (any
/// requested during a later hook call are left for the caller to drain
/// via `PluginRuntime::drain_pending_spawns`), plus the item catalog
/// entries it requested via `register-item-type` during `on_load` (#287)
/// — unlike the spawn-table ids, the caller is expected to actually
/// persist these (`content::ItemCatalogStore::register`) right away, not
/// just log them; see this function's own body comment for why.
///
/// Called once per plugin **per zone-service** it's attached to (#152) —
/// every zone gets its own live instance (its own `wasmtime::Store`),
/// never a single instance shared across zones, matching
/// docs/specs/Plugin_API.md's "instantiated for a zone-service" wording.
/// `host` is shared across every plugin attached to the same zone-service
/// (`main` constructs one `PluginHost` per zone and passes it to every
/// `load_plugin` call for that zone) — `PluginHost`'s own doc comment:
/// "compiling/loading is the expensive part, the engine itself is cheap
/// to share." A zone with multiple plugins attached previously created a
/// separate `wasmtime::Engine` per plugin; #152 fixed that.
#[allow(clippy::too_many_arguments)]
pub fn load_plugin(
    manifest: &PluginManifest,
    wasm_path: &Path,
    host: &PluginHost,
    sessions: Sessions,
    entity_roles: EntityRoles,
    plugin_state_cache: PluginStateCache,
    blocked_zone_channels: BlockedZoneChannels,
    item_catalog_cache: ItemCatalogCache,
) -> Result<(PluginRuntime, Vec<String>, Vec<content::ItemCatalogEntry>)> {
    let name = manifest.plugin.name.clone();
    let message_types = manifest.plugin.message_types.clone();
    let chat_commands = manifest.plugin.chat_commands.clone();
    let hooks = manifest.plugin.hooks.clone();
    let capabilities = manifest.plugin.capabilities.clone();
    let pending_spawns = Arc::new(Mutex::new(Vec::new()));
    let pending_stat_deltas = Arc::new(Mutex::new(Vec::new()));
    let pending_character_stat_deltas = Arc::new(Mutex::new(Vec::new()));
    let pending_moves = Arc::new(Mutex::new(Vec::new()));
    let pending_item_grants = Arc::new(Mutex::new(Vec::new()));
    let pending_item_removals = Arc::new(Mutex::new(Vec::new()));
    let pending_currency_deltas = Arc::new(Mutex::new(Vec::new()));
    let pending_state_writes = Arc::new(Mutex::new(Vec::new()));
    let pending_deaths = Arc::new(Mutex::new(Vec::new()));
    let pending_respawns = Arc::new(Mutex::new(Vec::new()));
    let pending_item_registrations = Arc::new(Mutex::new(Vec::new()));
    let callbacks = PluginCallbacks {
        pending_spawns: pending_spawns.clone(),
        pending_stat_deltas: pending_stat_deltas.clone(),
        pending_character_stat_deltas: pending_character_stat_deltas.clone(),
        pending_moves: pending_moves.clone(),
        pending_item_grants: pending_item_grants.clone(),
        pending_item_removals: pending_item_removals.clone(),
        pending_currency_deltas: pending_currency_deltas.clone(),
        entity_roles,
        sessions,
        plugin_state_cache,
        pending_state_writes: pending_state_writes.clone(),
        pending_deaths: pending_deaths.clone(),
        pending_respawns: pending_respawns.clone(),
        blocked_zone_channels,
        item_catalog_cache,
        pending_item_registrations: pending_item_registrations.clone(),
    };

    // A plugin's compiled component always exports every hook function in
    // the WIT `Guest` interface regardless of whether the author wrote real
    // logic into it — `hooks` (this plugin's declared opt-in list) is the
    // only thing that gates whether the host ever calls it (`wants` above).
    // Implementing a hook without declaring it here compiles and loads
    // fine; the hook then just silently never fires. Surface the gap at
    // load time rather than leaving it to be found by reading source.
    let undeclared_hooks: Vec<&str> = KNOWN_HOOKS
        .iter()
        .copied()
        .filter(|known| !hooks.iter().any(|h| h == known))
        .collect();
    if !undeclared_hooks.is_empty() {
        tracing::warn!(
            plugin = %name,
            ?undeclared_hooks,
            "hooks not declared in plugin.toml's `hooks` list — even if implemented, these will never be called"
        );
    }

    let mut plugin = host.load(manifest, wasm_path, Box::new(callbacks))?;
    if hooks.iter().any(|h| h == "on-load") {
        plugin.on_load()?;
    }

    let on_load_spawns = std::mem::take(&mut *pending_spawns.lock().unwrap());
    // Drained here, synchronously with `on_load` itself, rather than left
    // for the caller to pick up later like `on_load_spawns` above — #287's
    // whole point is that a plugin-registered item type is visible to
    // `crafting.schema.yaml`/`equipment.schema.yaml`'s own load-time
    // validation, which `main` runs right after every plugin has loaded;
    // that only works if the registration has already reached
    // `content::ItemCatalogStore` (not just this process's in-memory
    // cache, which `register_item_type` already updated) by the time
    // this function returns.
    let on_load_item_registrations =
        std::mem::take(&mut *pending_item_registrations.lock().unwrap());
    let runtime = PluginRuntime {
        name,
        plugin,
        message_types,
        chat_commands,
        hooks,
        capabilities,
        pending_spawns,
        pending_stat_deltas,
        pending_character_stat_deltas,
        pending_moves,
        pending_item_grants,
        pending_item_removals,
        pending_currency_deltas,
        pending_state_writes,
        pending_deaths,
        pending_respawns,
        pending_item_registrations,
    };
    Ok((runtime, on_load_spawns, on_load_item_registrations))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// #287's actual acceptance criterion, exercised without `wasmtime`/
    /// Postgres: a plugin's `register-item-type` call (through the real
    /// `PluginCallbacks::register_item_type` — the same code
    /// `plugin_host::PluginState`'s `HostInterface` impl calls from
    /// inside the sandboxed guest call) lands in `item_catalog_cache`
    /// immediately, and `character::CraftingSchema` — given a
    /// `KnownItemTypes` built from that same cache, exactly the shape
    /// `server::main` builds after every plugin's `on_load` runs — then
    /// accepts a recipe referencing the newly-registered item_type. This
    /// is the real production data flow (`PluginCallbacks` ->
    /// `item_catalog_cache` -> `character::KnownItemTypes` ->
    /// `CraftingSchema::from_yaml`), just without the wasm boundary
    /// itself (covered separately, real end to end, by
    /// `plugin-host`'s `plugin_sandbox.rs`::
    /// `a_plugin_registers_an_item_type_and_reads_it_back_via_get_item`).
    #[test]
    fn a_plugin_registered_item_type_makes_crafting_schema_accept_a_recipe_referencing_it() {
        let item_catalog_cache: ItemCatalogCache = Arc::new(Mutex::new(HashMap::new()));
        let mut callbacks = PluginCallbacks {
            pending_spawns: Arc::new(Mutex::new(Vec::new())),
            pending_stat_deltas: Arc::new(Mutex::new(Vec::new())),
            pending_character_stat_deltas: Arc::new(Mutex::new(Vec::new())),
            pending_moves: Arc::new(Mutex::new(Vec::new())),
            pending_item_grants: Arc::new(Mutex::new(Vec::new())),
            pending_item_removals: Arc::new(Mutex::new(Vec::new())),
            pending_currency_deltas: Arc::new(Mutex::new(Vec::new())),
            entity_roles: Arc::new(Mutex::new(HashMap::new())),
            sessions: Arc::new(Mutex::new(HashMap::new())),
            plugin_state_cache: Arc::new(Mutex::new(HashMap::new())),
            pending_state_writes: Arc::new(Mutex::new(Vec::new())),
            pending_deaths: Arc::new(Mutex::new(Vec::new())),
            pending_respawns: Arc::new(Mutex::new(Vec::new())),
            blocked_zone_channels: Arc::new(Mutex::new(HashMap::new())),
            item_catalog_cache: item_catalog_cache.clone(),
            pending_item_registrations: Arc::new(Mutex::new(Vec::new())),
        };

        // Before registering: a crafting.schema.yaml recipe naming this
        // item_type is rejected, same as any other unknown item_type.
        // #289's requires/grants aren't used here, so an empty declared
        // attribute schema is enough.
        let attribute_schema =
            character::AttributeSchema::from_yaml("schema_version: 1\nstats: []\n").unwrap();
        let known_item_types_before: character::KnownItemTypes = HashMap::new();
        let recipe_yaml = r#"
schema_version: 1
recipes:
  - key: plugin-forged-blade
    category: plugin-blacksmithing
    inputs:
      - item_type: iron-ore
        amount: 1
    output:
      item_type: plugin-forged-blade
      amount: 1
"#;
        assert!(
            character::CraftingSchema::from_yaml(
                recipe_yaml,
                &attribute_schema,
                &known_item_types_before
            )
            .is_err()
        );

        // The plugin registers both item_types this recipe needs, each
        // tagged appropriately (`craftable_output` on the output only,
        // matching `CraftingSchema::from_yaml`'s own check).
        callbacks
            .register_item_type("iron-ore", "Iron Ore", vec![], "{}")
            .expect("register-item-type should succeed");
        callbacks
            .register_item_type(
                "plugin-forged-blade",
                "Plugin-Forged Blade",
                vec!["craftable_output".to_string()],
                "{}",
            )
            .expect("register-item-type should succeed");

        // `get-item` answers from the same cache `register-item-type`
        // just wrote into.
        let looked_up = callbacks
            .get_item("plugin-forged-blade")
            .expect("get-item should succeed")
            .expect("plugin-forged-blade should now be registered");
        assert_eq!(looked_up.display_name, "Plugin-Forged Blade");

        // The exact `KnownItemTypes` shape `server::main` builds from
        // `item_catalog_cache` after every plugin's `on_load` runs.
        let known_item_types_after: character::KnownItemTypes = item_catalog_cache
            .lock()
            .unwrap()
            .values()
            .map(|entry| {
                (
                    entry.item_type.clone(),
                    entry.tags.iter().cloned().collect(),
                )
            })
            .collect();

        // Now the same recipe loads successfully — the plugin-registered
        // item_type is indistinguishable from a dev-authored one to
        // `CraftingSchema`.
        let schema = character::CraftingSchema::from_yaml(
            recipe_yaml,
            &attribute_schema,
            &known_item_types_after,
        )
        .unwrap();
        assert_eq!(
            schema
                .resolve("plugin-forged-blade")
                .unwrap()
                .output
                .item_type,
            "plugin-forged-blade"
        );
    }
}
