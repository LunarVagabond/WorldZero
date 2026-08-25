//! Combined-process runnable binary — the Phase 1 target (docs/PROPOSAL.md,
//! "Phased Roadmap"): wires `auth`, `character`, `world`, `gateway`,
//! `content`, `chat`, and (as of #136) `realm-directory` into one process
//! a self-hoster can run end to end. `transfer` comes online in a later
//! phase — `plugin-host` gets a minimal, optional slice here already
//! (spawning one NPC per zone-service via a configured plugin's
//! `on-zone-loaded` hook, plus live `on-message` routing, #95), matching
//! Phase 1's "minimal plugin hook." `chat` is the first optional-service
//! crate wired in, gated by `WZ_SERVICE_CHAT_ENABLED` per the #91/#92
//! runtime-toggle decision (#104).
//!
//! `cargo run -p server`. Needs, at minimum:
//! - `WZ_POSTGRES_*` / `WZ_REDIS_*` (`.env`)
//! - `WZ_REALM_ID` — the realm this process serves (#136); create one
//!   first with `make realm ARGS="create <name> open|bound"`
//!   (docs/specs/Realm_Character_Policy_Spec.md, "Managing realms
//!   today") and pass the id it prints. A process serving more than one
//!   realm at once is #130's job, not this one's.
//! - `<config_dir>/content-pack.yaml` (see
//!   `config/content-pack.example.yaml`), for **multiple** zone-service
//!   instances (#45) — a player crosses between them by walking through
//!   a manifest-declared `links[]` edge, no client reconnect involved.
//!   If this file isn't present, falls back to a **single** zone loaded
//!   from `<config_dir>/zone.manifest.yaml` (see
//!   `config/zone.manifest.example.yaml`) — the original Phase 1
//!   single-zone behavior, unchanged.
//! - `<config_dir>/stats.schema.yaml` (see
//!   `config/stats.schema.example.yaml`) — the declared
//!   character attribute schema
//! - `<config_dir>/party.schema.yaml` (see
//!   `config/party.schema.example.yaml`) — the declared party-type/size
//!   schema (#178)
//! - `<config_dir>/guild.schema.yaml` (see
//!   `config/guild.schema.example.yaml`) — the declared guild rank
//!   schema (#179)
//! - `<config_dir>/character.archetypes.yaml` (see
//!   `config/character.archetypes.example.yaml`) — the declared
//!   character-archetype schema (#213/#212)
//! - `<config_dir>/crafting.schema.yaml` (see
//!   `config/crafting.schema.example.yaml`) — the declared recipe schema
//!   (#216/#215)
//!
//! Optional: `WZ_SERVER_ADDR` (default `127.0.0.1:7900`),
//! `WZ_PLUGINS_DIR` (default `<config_dir>/plugins`) — a directory of
//! `<name>/{plugin.toml,*.wasm}` subdirectories, auto-discovered at
//! startup (#152; replaces the old single `WZ_PLUGIN_MANIFEST_PATH`/
//! `WZ_PLUGIN_WASM_PATH` pair). More than one plugin can load at once;
//! each loads exactly once, process-wide — not once per zone-service —
//! and every zone-specific hook takes an explicit `zone-id` argument
//! instead (see docs/specs/Plugin_API.md's "Multi-plugin support").
//! `WZ_SERVICE_CHAT_ENABLED` (default
//! `true`), `WZ_SERVICE_METRICS_ENABLED` (default `true`) +
//! `WZ_METRICS_ADDR` (default `127.0.0.1:9090` — a separate `/metrics`
//! HTTP listener for Prometheus scraping, #48; see
//! docs/specs/Observability_Spec.md), `WZ_LAYER_ENABLED` (default
//! `true` — #50's dynamic layering; `false` pins every zone to exactly
//! one layer forever) + `WZ_LAYER_POPULATION_THRESHOLD` (default `200`
//! — connected sessions per zone layer before a new one spins up, only
//! consulted while layering is enabled; see `zone_registry`'s doc
//! comment), `WZ_REALM_LEASE_TTL_SECS` (default `60` — #21's open-realm
//! session lease TTL; only consulted when `WZ_REALM_ID` names an `open`
//! realm), `WZ_CHARACTER_MAX_PER_ACCOUNT` (default `5` — #193's
//! character-creation cap, per account per realm).
//!
//! A connected client speaks `auth::gateway_protocol` first (login or
//! register), then `server::realm_protocol` (realm discovery/selection,
//! #192), then `server::character_protocol` (character list/create/
//! select, #193), then `server::session_protocol` (move, see other
//! entities move, zone transitions) and, when chat is enabled,
//! `chat::gateway_protocol` (join/leave/send) over the same connection —
//! same gateway-first-authenticate pattern as `chat`'s standalone
//! gateway demo (docs/specs/Auth_Spec.md, "Gateway handshake").

mod character_protocol;
mod chat_session;
mod plugin_startup;
mod plugin_state;
mod realm_protocol;
mod session;
mod session_protocol;
mod world_actor;
mod zone_registry;

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock, RwLock};

use character::inventory::InventoryConfig;
use character::{AttributeSchema, CharacterStore};
use common::config::{PostgresConfig, RedisConfig, ServicesConfig};
use common::id::{EntityId, RealmId};
use common::pool::{PoolOptions, postgres_pool, redis_pool};
use content::content_pack::ContentPack;
use content::manifest::ZoneManifest;
use futures_util::StreamExt;
use session::{
    AccountEntities, CharacterEntities, EntityAccounts, EntityCharacters, EntityRoles, NpcStats,
    PendingGuildInvites, PendingPartyInvites, SessionDeps, Sessions,
};
use session_protocol::{RosterEntry, ServerMessage};
use tokio::sync::mpsc;
use world::{EntityKind, MovementOutcome, Point, Zone};
use zone_registry::{ZoneRegistry, ZoneRuntime};

const DEFAULT_ADDR: &str = "127.0.0.1:7900";
const DEFAULT_METRICS_ADDR: &str = "127.0.0.1:9090";
/// #50's layer-spin-up trigger, in connected sessions per layer — see
/// `zone_registry`'s doc comment for what actually happens at this
/// point. Generous on purpose: real per-deployment tuning is expected
/// via `WZ_LAYER_POPULATION_THRESHOLD`, this is just a default that
/// doesn't layer-split a small/testing deployment for no reason.
const DEFAULT_LAYER_POPULATION_THRESHOLD: usize = 200;

/// Default session-lease TTL (#21/#51) for this process's open-realm
/// logins, in seconds — only ever consulted when the realm this process
/// serves (below) is `open`; a `bound` realm never takes a lease at all.
/// Generous relative to the renewal interval `session::handle_session`
/// runs at (`lease_ttl / 3`) so a couple of missed renewals in a row
/// don't spuriously expire a still-connected character's lease.
const DEFAULT_LEASE_TTL_SECS: u64 = 60;

/// Default cap on how many characters one account may create per realm
/// (#193) — generous, same "solid default, never a wall" spirit as
/// `character::InventoryConfig`'s own default; a self-hoster with an
/// unusual policy overrides via `WZ_CHARACTER_MAX_PER_ACCOUNT`.
const DEFAULT_MAX_CHARACTERS_PER_ACCOUNT: u32 = 5;

/// Resolves `WZ_REALM_ID` against `realm-directory::RealmStore` — the
/// single realm this `server` process serves (#136; a multi-realm
/// *process* is #130's job, out of scope here). Panics with a clear
/// message rather than falling back to a placeholder: unlike Phase 1's
/// `placeholder_realm_id()` (a fixed nil UUID nothing ever validated),
/// every character created from here on is stamped with a real
/// `realms.id` foreign-key candidate (#170), so silently inventing one
/// would just move the failure to whenever #170's constraint lands.
/// Create one first with `make realm ARGS="create <name> open|bound"`
/// (docs/specs/Realm_Character_Policy_Spec.md, "Managing realms today").
async fn resolve_realm(realms: &realm_directory::RealmStore) -> realm_directory::Realm {
    let raw = std::env::var("WZ_REALM_ID").unwrap_or_else(|_| {
        panic!(
            "WZ_REALM_ID must be set to an existing realm id — \
             create one with `make realm ARGS=\"create <name> open|bound\"` \
             and pass the id it prints"
        )
    });
    let realm_id: RealmId = raw
        .parse()
        .unwrap_or_else(|e| panic!("WZ_REALM_ID {raw:?} is not a valid realm id: {e}"));
    realms
        .get(realm_id)
        .await
        .unwrap_or_else(|e| panic!("failed to look up realm {realm_id}: {e}"))
        .unwrap_or_else(|| {
            panic!(
                "WZ_REALM_ID {realm_id} does not name an existing realm — \
                 create one with `make realm ARGS=\"create <name> open|bound\"` first"
            )
        })
}

#[tokio::main]
async fn main() {
    common::logging::init();

    let addr = std::env::var("WZ_SERVER_ADDR").unwrap_or_else(|_| DEFAULT_ADDR.to_string());
    let config_dir = common::config::config_dir();

    let pg_config = PostgresConfig::from_env().expect("WZ_POSTGRES_* env vars set");
    let pool = postgres_pool(&pg_config, PoolOptions::default())
        .await
        .expect("failed to connect to Postgres");
    let redis_config = RedisConfig::from_env().expect("WZ_REDIS_* env vars set");
    let redis =
        redis_pool(&redis_config, PoolOptions::default()).expect("failed to build Redis pool");

    let world_config = world::WorldConfig::from_env().expect("invalid WZ_WORLD_* config");
    let services = ServicesConfig::from_env().expect("invalid WZ_SERVICE_* config");

    // `None` end to end (not just an unused `Metrics`) when disabled —
    // no `/metrics` HTTP listener, and every instrumentation call site
    // below (`world_actor`, `session`) skips its `Some(...)` branch
    // entirely rather than updating a gauge/histogram nobody scrapes
    // (#48, per the #91/#92 runtime-toggle decision, same discipline
    // `chat_deps` already applies).
    let metrics = if services.metrics_enabled {
        tracing::info!("metrics enabled");
        let metrics = Arc::new(common::metrics::Metrics::new());
        let metrics_addr: std::net::SocketAddr = std::env::var("WZ_METRICS_ADDR")
            .unwrap_or_else(|_| DEFAULT_METRICS_ADDR.to_string())
            .parse()
            .expect("WZ_METRICS_ADDR must be a valid socket address");
        let metrics_for_listener = metrics.clone();
        tokio::spawn(async move {
            if let Err(e) = common::metrics::serve(metrics_addr, metrics_for_listener).await {
                tracing::error!(error = %e, "metrics listener stopped");
            }
        });
        Some(metrics)
    } else {
        tracing::info!("metrics disabled (WZ_SERVICE_METRICS_ENABLED=false)");
        None
    };

    let zone_manifests = load_zone_manifests(&config_dir);
    let default_zone_id = zone_manifests[0].id.clone();
    tracing::info!(
        zone_count = zone_manifests.len(),
        default_zone_id,
        "loaded zone manifest(s)"
    );

    let schema_path = config_dir.join("stats.schema.yaml");
    let schema = AttributeSchema::from_file(&schema_path).unwrap_or_else(|e| {
        panic!(
            "failed to load the declared attribute schema at {} (see config/stats.schema.example.yaml): {e}",
            schema_path.display()
        )
    });
    let party_schema_path = config_dir.join("party.schema.yaml");
    let party_schema = character::PartySchema::from_file(&party_schema_path).unwrap_or_else(|e| {
        panic!(
            "failed to load the declared party-type schema at {} (see config/party.schema.example.yaml): {e}",
            party_schema_path.display()
        )
    });
    let guild_schema_path = config_dir.join("guild.schema.yaml");
    let guild_schema = guild::GuildSchema::from_file(&guild_schema_path).unwrap_or_else(|e| {
        panic!(
            "failed to load the declared guild rank schema at {} (see config/guild.schema.example.yaml): {e}",
            guild_schema_path.display()
        )
    });
    // #213/#212 — validated at load against the same declared attribute
    // schema `schema` names below (a clone, since `schema` itself is
    // moved into `CharacterStore::new` shortly after).
    let archetype_schema_path = config_dir.join("character.archetypes.yaml");
    let archetype_schema =
        character::ArchetypeSchema::from_file(&archetype_schema_path, &schema).unwrap_or_else(|e| {
            panic!(
                "failed to load the declared character archetype schema at {} (see config/character.archetypes.example.yaml): {e}",
                archetype_schema_path.display()
            )
        });
    let crafting_schema_path = config_dir.join("crafting.schema.yaml");
    let crafting_schema =
        character::CraftingSchema::from_file(&crafting_schema_path).unwrap_or_else(|e| {
            panic!(
                "failed to load the declared recipe schema at {} (see config/crafting.schema.example.yaml): {e}",
                crafting_schema_path.display()
            )
        });
    let inventory_config =
        InventoryConfig::from_env().expect("invalid WZ_INVENTORY_MAX_ITEM_TYPES");
    // #193's character-creation cap — a `server`-side policy value (like
    // `WZ_LAYER_POPULATION_THRESHOLD`), not pushed down into
    // `character::CharacterStore` itself, since `CharacterStore::create`
    // stays a raw insert with no policy opinion of its own (matching
    // `RealmStore`/`character`'s existing "storage vs. policy" split).
    let max_characters_per_account: u32 = std::env::var("WZ_CHARACTER_MAX_PER_ACCOUNT")
        .ok()
        .map(|v| {
            v.parse()
                .expect("WZ_CHARACTER_MAX_PER_ACCOUNT must be a positive integer")
        })
        .unwrap_or(DEFAULT_MAX_CHARACTERS_PER_ACCOUNT);

    let account_store: Arc<dyn auth::AccountStore> =
        Arc::new(auth::PostgresAccountStore::new(pool.clone()));
    let sessions_manager = auth::SessionManager::new(redis.clone());
    let auth_provider = Arc::new(auth::UsernamePasswordProvider::new(
        account_store,
        sessions_manager,
    ));
    let role_store: Arc<dyn auth::AccountRoleStore> =
        Arc::new(auth::PostgresAccountRoleStore::new(pool.clone()));
    // NPC-targetable stats (#197) read the same declared schema real
    // character stats do, so this is a clone of the exact instance
    // `character_store` gets — not a second file load that could drift.
    let npc_attribute_schema = Arc::new(schema.clone());
    let character_store = Arc::new(CharacterStore::new(pool.clone(), schema, inventory_config));
    let party_store = Arc::new(character::PartyStore::new(pool.clone(), party_schema));
    let guild_store = Arc::new(guild::GuildStore::new(pool.clone(), guild_schema));
    let archetype_schema = Arc::new(archetype_schema);
    let crafting_schema = Arc::new(crafting_schema);

    let realm_store = realm_directory::RealmStore::new(pool.clone());
    let realm = resolve_realm(&realm_store).await;
    let realm_id = realm.id;
    tracing::info!(%realm_id, realm_name = realm.name, realm_policy = ?realm.open_or_bound, "resolved realm");

    let lease_ttl_secs: u64 = std::env::var("WZ_REALM_LEASE_TTL_SECS")
        .ok()
        .map(|v| {
            v.parse()
                .expect("WZ_REALM_LEASE_TTL_SECS must be a positive integer")
        })
        .unwrap_or(DEFAULT_LEASE_TTL_SECS);
    let lease_ttl = std::time::Duration::from_secs(lease_ttl_secs);
    let login_policy = Arc::new(realm_directory::LoginPolicy::new(
        realm_directory::RealmStore::new(pool.clone()),
        character::CharacterSessionLease::new(pool.clone()),
        lease_ttl,
    ));
    let character_lease = Arc::new(character::CharacterSessionLease::new(pool.clone()));
    // Reuses `lease_ttl` rather than a separate knob — both are "how
    // stale can a per-connection heartbeat get before something else
    // could reclaim it," same sizing question `RealmPresence::new`'s own
    // doc comment asks, so a second env var here would just be one more
    // thing to keep in sync with no real difference in what it tunes.
    let realm_presence = Arc::new(realm_directory::RealmPresence::new(
        redis.clone(),
        lease_ttl,
    ));

    // `None` end to end (not just an unused `ChatDeps`) when disabled —
    // no `ChannelStore`/`ChatBus` construction, no per-connection chat
    // dispatch, nothing (#104, per the #91/#92 runtime-toggle decision).
    let chat_deps = if services.chat_enabled {
        tracing::info!("chat service enabled");
        Some(chat_session::ChatDeps {
            pool: pool.clone(),
            store: Arc::new(chat::ChannelStore::new(pool.clone())),
            bus: Arc::new(chat::ChatBus::new(redis.clone(), redis_config.clone())),
            usernames: Arc::new(RwLock::new(HashMap::new())),
        })
    } else {
        tracing::info!("chat service disabled (WZ_SERVICE_CHAT_ENABLED=false)");
        None
    };

    let entity_characters: EntityCharacters = Arc::new(Mutex::new(HashMap::new()));
    let character_entities: CharacterEntities = Arc::new(Mutex::new(HashMap::new()));
    let entity_roles: EntityRoles = Arc::new(Mutex::new(HashMap::new()));
    let npc_stats: NpcStats = Arc::new(Mutex::new(HashMap::new()));
    let pending_party_invites: PendingPartyInvites = Arc::new(Mutex::new(HashMap::new()));
    let pending_guild_invites: PendingGuildInvites = Arc::new(Mutex::new(HashMap::new()));
    let entity_accounts: EntityAccounts = Arc::new(Mutex::new(HashMap::new()));
    let account_entities: AccountEntities = Arc::new(Mutex::new(HashMap::new()));

    // Every connected entity's outgoing channel, process-wide, regardless
    // of which zone it's currently in (#152) — backs the plugin
    // `send-message` host function, since a plugin instance is shared
    // across every zone now and needs to reach a target entity no matter
    // where they are. Distinct from each zone's own `Sessions` (used for
    // that zone's broadcast/roster) — see `session::SessionDeps`'s own
    // doc comment on this field.
    let global_sessions: Sessions = Arc::new(Mutex::new(HashMap::new()));

    // Shared process-wide (#149) — every connection's character-scope
    // state and every zone's zone-scope state lands in this one cache.
    // Not namespaced per plugin even with #152's multi-plugin support: a
    // known, separate gap (docs/specs/Plugin_API.md's "Beyond this v0
    // slice") — every plugin shares the same zone-scope bucket for a
    // given zone. See `plugin_state`'s module doc.
    let plugin_state_cache: plugin_state::PluginStateCache = Arc::new(Mutex::new(HashMap::new()));
    let plugin_state_store = Arc::new(plugin_state::PluginStateStore::new(pool.clone()));

    // Discovered once, up front — every manifest is validated
    // individually and, as a whole set, for message_type/chat_command
    // collisions before any plugin is ever instantiated (#152). Empty is
    // the ordinary "no plugins configured" case, not an error.
    let plugins_dir = std::env::var("WZ_PLUGINS_DIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| config_dir.join("plugins"));
    let discovered_plugins = discover_plugins(&plugins_dir);
    tracing::info!(plugin_count = discovered_plugins.len(), plugins_dir = %plugins_dir.display(), "discovered plugin(s)");

    // The union across every discovered plugin's declared
    // message_types/chat_commands — already guaranteed collision-free by
    // `discover_plugins` — used only as `session`'s early routing filter;
    // the zone actor itself decides which *specific* loaded plugin (if
    // any) a given value actually belongs to.
    let plugin_message_types: Vec<u16> = discovered_plugins
        .iter()
        .flat_map(|(manifest, _)| manifest.plugin.message_types.clone())
        .collect();
    let plugin_chat_commands: Vec<String> = discovered_plugins
        .iter()
        .flat_map(|(manifest, _)| manifest.plugin.chat_commands.clone())
        .collect();

    // One plugin instance, process-wide (#152) — loaded exactly once,
    // here, before any zone exists (matching `on-load`'s "genuinely
    // global setup only" contract, `wit/plugin.wit`'s doc comment), then
    // shared across every zone-service via `Arc<tokio::sync::Mutex<_>>`.
    // One `wasmtime::Engine` for the whole process too —
    // `plugin_host::PluginHost`'s own doc comment: "compiling/loading is
    // the expensive part, the engine itself is cheap to share."
    let plugin_host = plugin_host::PluginHost::new();
    let mut loaded_plugins = Vec::new();
    for (manifest, wasm_path) in &discovered_plugins {
        let (runtime, on_load_spawns) = plugin_startup::load_plugin(
            manifest,
            wasm_path,
            &plugin_host,
            global_sessions.clone(),
            entity_roles.clone(),
            plugin_state_cache.clone(),
        )
        .unwrap_or_else(|e| {
            panic!(
                "failed to load plugin {:?} ({}): {e}",
                manifest.plugin.name,
                wasm_path.display(),
            )
        });
        // `on_load` has no zone context anymore (#152) — a well-behaved
        // plugin doesn't call `spawn-npc` from it (that belongs in
        // `on_zone_loaded`, per-zone, below). Warn rather than silently
        // drop if one does anyway, so a plugin author notices.
        for spawn_table_id in on_load_spawns {
            tracing::warn!(
                plugin = %manifest.plugin.name,
                spawn_table_id,
                "plugin requested spawn-npc from on_load, which has no zone context — ignored; use on_zone_loaded instead"
            );
        }
        loaded_plugins.push(runtime);
    }
    let plugins = Arc::new(tokio::sync::Mutex::new(loaded_plugins));

    // Every zone-service actor's `on_tick` closure is wired up below
    // before the full `ZoneRegistry` can possibly exist (it needs every
    // actor's `WorldHandle` first) — but a `ZoneTransition` outcome needs
    // to reach a *different* zone's registry entry. This cell breaks
    // that chicken-and-egg: `on_tick` reads through it lazily, at tick
    // time, well after `.set()` below has run (in practice, before the
    // very first tick fires for any zone — `.set()` happens synchronously
    // right after every actor is spawned, all well under one tick
    // interval). `handle_tick_outcomes` treats a still-empty cell as "not
    // ready yet" and logs rather than panicking, just in case.
    let zone_registry_cell: Arc<OnceLock<Arc<ZoneRegistry>>> = Arc::new(OnceLock::new());

    let mut runtimes = HashMap::new();
    let mut manifests = HashMap::new();

    for manifest in zone_manifests.into_iter() {
        let zone_id = manifest.id.clone();
        let mut zone = Zone::new(manifest.clone(), world_config);
        manifests.insert(zone_id.clone(), manifest);

        // Created before the zone's actor starts (as before #95) — a
        // broadcast/roster lookup needs a `Sessions` handle from the
        // moment it's constructed, even though it's empty until a client
        // joins this particular zone.
        let sessions: Sessions = Arc::new(Mutex::new(HashMap::new()));

        if !discovered_plugins.is_empty() {
            // Zone-scope state for this zone, hydrated once here, before
            // any client can connect — shared across every plugin (#149,
            // see the earlier doc comment on why it's not namespaced per
            // plugin).
            let zone_state = plugin_state_store
                .zone_state(&zone_id)
                .await
                .expect("failed to load zone-scoped plugin state");
            {
                let mut cache = plugin_state_cache.lock().unwrap();
                for (key, value) in zone_state {
                    cache.insert(
                        plugin_state::cache_key(
                            &plugin_host::PluginStateScope::Zone(zone_id.clone()),
                            &key,
                        ),
                        value,
                    );
                }
            }

            // `on-zone-loaded` fan-out (#152) — every plugin that
            // declared it gets a chance to do zone-specific setup (e.g.
            // `spawn-npc` against this zone's own spawn tables) before
            // this zone's actor starts.
            let mut runtimes = plugins.lock().await;
            for runtime in runtimes.iter_mut() {
                if !runtime.wants("on-zone-loaded") {
                    continue;
                }
                if let Err(e) = runtime.plugin.on_zone_loaded(&zone_id) {
                    tracing::warn!(plugin = %runtime.name, zone_id, error = %e, "plugin on_zone_loaded hook failed");
                }
                for spawn_table_id in runtime.drain_pending_spawns() {
                    world_actor::spawn_npc_from_table(&mut zone, &spawn_table_id);
                }
            }
        }

        let registry_cell = zone_registry_cell.clone();
        let sessions_for_tick = sessions.clone();
        let zone_id_for_tick = zone_id.clone();
        let world = world_actor::spawn_world_actor(
            zone,
            world_config.tick_interval(),
            plugins.clone(),
            character_store.clone(),
            entity_characters.clone(),
            npc_stats.clone(),
            npc_attribute_schema.clone(),
            plugin_state_store.clone(),
            zone_id.clone(),
            metrics.clone(),
            global_sessions.clone(),
            move |zone, outcomes| {
                handle_tick_outcomes(
                    &registry_cell,
                    &zone_id_for_tick,
                    &sessions_for_tick,
                    zone,
                    outcomes,
                );
            },
        );

        runtimes.insert(zone_id, ZoneRuntime { world, sessions });
    }

    let layering_enabled: bool = std::env::var("WZ_LAYER_ENABLED")
        .ok()
        .map(|v| {
            v.parse()
                .expect("WZ_LAYER_ENABLED must be \"true\" or \"false\"")
        })
        .unwrap_or(true);
    let layer_population_threshold: usize = std::env::var("WZ_LAYER_POPULATION_THRESHOLD")
        .ok()
        .map(|v| {
            v.parse()
                .expect("WZ_LAYER_POPULATION_THRESHOLD must be a positive integer")
        })
        .unwrap_or(DEFAULT_LAYER_POPULATION_THRESHOLD);

    // Builds an additional layer for an already-running zone, on demand
    // (#50) — never called at startup, only from `ZoneRegistry::assign_layer`
    // once a zone's existing layers are all at or above
    // `layer_population_threshold`. Shares the same process-wide `plugins`
    // (#152) as every other layer/zone — a connection in this layer still
    // gets `on-player-join-zone`/`on-damage-calc`/etc — but never fires
    // `on-zone-loaded` for it (NPC seeding stays layer-0-only, a
    // deliberate v0 simplification predating #152, see `zone_registry`'s
    // doc comment); `plugin_message_types`/`plugin_chat_commands` are
    // likewise fixed at startup from the full discovered set, not
    // touched again here.
    let layer_spawner_world_config = world_config;
    let layer_spawner_character_store = character_store.clone();
    let layer_spawner_entity_characters = entity_characters.clone();
    let layer_spawner_npc_stats = npc_stats.clone();
    let layer_spawner_npc_attribute_schema = npc_attribute_schema.clone();
    let layer_spawner_metrics = metrics.clone();
    let layer_spawner_registry_cell = zone_registry_cell.clone();
    let layer_spawner_plugin_state_store = plugin_state_store.clone();
    let layer_spawner_plugins = plugins.clone();
    let layer_spawner_global_sessions = global_sessions.clone();
    let layer_spawner: zone_registry::LayerSpawner = Box::new(move |zone_id, manifest| {
        let zone = Zone::new(manifest.clone(), layer_spawner_world_config);
        let sessions: Sessions = Arc::new(Mutex::new(HashMap::new()));

        let registry_cell = layer_spawner_registry_cell.clone();
        let sessions_for_tick = sessions.clone();
        let zone_id_for_tick = zone_id.to_string();
        let world = world_actor::spawn_world_actor(
            zone,
            layer_spawner_world_config.tick_interval(),
            layer_spawner_plugins.clone(),
            layer_spawner_character_store.clone(),
            layer_spawner_entity_characters.clone(),
            layer_spawner_npc_stats.clone(),
            layer_spawner_npc_attribute_schema.clone(),
            layer_spawner_plugin_state_store.clone(),
            zone_id.to_string(),
            layer_spawner_metrics.clone(),
            layer_spawner_global_sessions.clone(),
            move |zone, outcomes| {
                handle_tick_outcomes(
                    &registry_cell,
                    &zone_id_for_tick,
                    &sessions_for_tick,
                    zone,
                    outcomes,
                );
            },
        );

        ZoneRuntime { world, sessions }
    });

    let zones = Arc::new(ZoneRegistry::new(
        runtimes,
        manifests,
        layering_enabled,
        layer_population_threshold,
        layer_spawner,
    ));
    zone_registry_cell
        .set(zones.clone())
        .unwrap_or_else(|_| unreachable!("zone_registry_cell is only ever set once, here"));

    let config_dir_for_cert = config_dir.clone();
    let cert = gateway::tcp::init_and_log_fingerprint(&config_dir_for_cert)
        .expect("failed to load/generate the gateway's TLS certificate");
    let acceptor = gateway::tcp::build_tls_acceptor(&cert).expect("failed to build TLS acceptor");

    let (local_addr, incoming) = gateway::tcp::listen(&addr, acceptor)
        .await
        .expect("failed to bind the gateway TCP listener");
    tracing::info!(%local_addr, "worldzero server listening");

    let deps = Arc::new(SessionDeps {
        auth_provider,
        character_store,
        realm_id,
        realm_name: realm.name,
        realm_open_or_bound: realm.open_or_bound,
        login_policy,
        character_lease,
        lease_ttl,
        realm_presence,
        max_characters_per_account,
        archetype_schema,
        crafting_schema,
        plugins: plugins.clone(),
        zones,
        default_zone_id,
        entity_characters,
        character_entities,
        party_store,
        pending_party_invites,
        guild_store,
        pending_guild_invites,
        entity_accounts,
        account_entities,
        role_store,
        entity_roles,
        plugin_message_types,
        plugin_chat_commands,
        chat: chat_deps,
        metrics,
        plugin_state_store,
        plugin_state_cache,
        global_sessions,
    });

    let mut incoming = Box::pin(incoming);
    while let Some(framed) = incoming.next().await {
        let deps = deps.clone();
        tokio::spawn(async move {
            if let Err(e) = session::handle_session(framed, deps).await {
                tracing::warn!(error = %e, "session ended with an error");
            }
        });
    }
}

/// `<config_dir>/content-pack.yaml` if present (multiple zones, #45);
/// otherwise falls back to the single `<config_dir>/zone.manifest.yaml`
/// (original Phase 1 behavior) — see this module's doc comment.
fn load_zone_manifests(config_dir: &std::path::Path) -> Vec<ZoneManifest> {
    let pack_path = config_dir.join("content-pack.yaml");
    if pack_path.exists() {
        let pack = ContentPack::from_file(&pack_path).unwrap_or_else(|e| {
            panic!(
                "failed to load the content pack at {} (see config/content-pack.example.yaml): {e}",
                pack_path.display()
            )
        });
        assert!(
            !pack.zones.is_empty(),
            "content pack at {} declares zero zones",
            pack_path.display()
        );
        return pack.zones;
    }

    let manifest_path = config_dir.join("zone.manifest.yaml");
    let manifest = ZoneManifest::from_file(&manifest_path).unwrap_or_else(|e| {
        panic!(
            "failed to load the zone manifest at {} (see config/zone.manifest.example.yaml): {e}",
            manifest_path.display()
        )
    });
    vec![manifest]
}

/// Discovers every plugin under `plugins_dir/<name>/{plugin.toml,*.wasm}`
/// (#152) — one subdirectory per plugin, auto-scanned; replaces the old
/// single `WZ_PLUGIN_MANIFEST_PATH`/`WZ_PLUGIN_WASM_PATH` env-var pair.
/// Returns `(manifest, wasm_path)` pairs. Each manifest is validated
/// individually (`PluginManifest::check_compatible`) and, as a whole set,
/// for cross-plugin `message_type`/`chat_command` collisions
/// (`plugin_host::check_no_collisions`) before any plugin is ever
/// instantiated — a config mistake here fails the whole process at
/// startup, not obscurely once a client connects. Empty (not an error,
/// not even a warning) if `plugins_dir` doesn't exist — "no plugins
/// configured" is the ordinary case.
fn discover_plugins(
    plugins_dir: &std::path::Path,
) -> Vec<(plugin_host::PluginManifest, std::path::PathBuf)> {
    if !plugins_dir.is_dir() {
        return Vec::new();
    }

    let mut discovered = Vec::new();
    let entries = std::fs::read_dir(plugins_dir)
        .unwrap_or_else(|e| panic!("failed to read {}: {e}", plugins_dir.display()));
    for entry in entries {
        let plugin_dir = entry
            .unwrap_or_else(|e| panic!("failed to read an entry in {}: {e}", plugins_dir.display()))
            .path();
        if !plugin_dir.is_dir() {
            continue;
        }

        let manifest_path = plugin_dir.join("plugin.toml");
        if !manifest_path.exists() {
            continue;
        }
        let manifest = plugin_host::PluginManifest::from_file(&manifest_path)
            .unwrap_or_else(|e| panic!("failed to parse {}: {e}", manifest_path.display()));
        manifest
            .check_compatible()
            .unwrap_or_else(|e| panic!("invalid plugin manifest {}: {e}", manifest_path.display()));

        let wasm_files: Vec<_> = std::fs::read_dir(&plugin_dir)
            .unwrap_or_else(|e| panic!("failed to read {}: {e}", plugin_dir.display()))
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|ext| ext == "wasm"))
            .collect();
        let wasm_path = match wasm_files.as_slice() {
            [one] => one.clone(),
            [] => panic!(
                "plugin directory {} has a plugin.toml but no .wasm file",
                plugin_dir.display()
            ),
            _ => panic!(
                "plugin directory {} has more than one .wasm file — ambiguous which one to load",
                plugin_dir.display()
            ),
        };

        discovered.push((manifest, wasm_path));
    }

    let manifests: Vec<_> = discovered.iter().map(|(m, _)| m.clone()).collect();
    plugin_host::check_no_collisions(&manifests).unwrap_or_else(|e| {
        panic!(
            "plugin configuration conflict in {}: {e}",
            plugins_dir.display()
        )
    });

    discovered
}

/// Reacts to one zone's tick outcomes — ordinary `Applied`/`Rejected`
/// broadcasts stay exactly as before #45; `ZoneTransition` hands the
/// entity off to a different zone (`complete_zone_transition`, spawned
/// as its own task since that needs an async round trip this
/// synchronous callback can't make itself).
fn handle_tick_outcomes(
    zone_registry_cell: &Arc<OnceLock<Arc<ZoneRegistry>>>,
    source_zone_id: &str,
    source_sessions: &Sessions,
    zone: &Zone,
    outcomes: Vec<(EntityId, MovementOutcome)>,
) {
    // Every outcome in this batch came from the same `zone.tick()` call
    // (#196) — one tick number for all of them, not resolved per-outcome.
    let tick = zone.current_tick();
    for (entity_id, outcome) in outcomes {
        match outcome {
            MovementOutcome::Applied { seq, to: (x, y) } => {
                broadcast_all(
                    source_sessions,
                    ServerMessage::Moved {
                        entity_id: entity_id.to_string(),
                        x,
                        y,
                        seq,
                        tick,
                    },
                );
            }
            MovementOutcome::Rejected { seq, rejection } => {
                send_to(
                    source_sessions,
                    entity_id,
                    ServerMessage::Rejected {
                        reason: format!("{rejection:?}"),
                        seq,
                        tick,
                    },
                );
            }
            MovementOutcome::ZoneTransition { target_zone } => {
                let Some(sender) = source_sessions.lock().unwrap().remove(&entity_id) else {
                    // The connection was already gone by the time this
                    // tick ran (e.g. disconnected the same tick it
                    // crossed) — nothing to hand off.
                    continue;
                };
                broadcast_except(
                    source_sessions,
                    entity_id,
                    ServerMessage::EntityDespawned {
                        entity_id: entity_id.to_string(),
                    },
                );

                let Some(zones) = zone_registry_cell.get().cloned() else {
                    tracing::error!(
                        target_zone,
                        "zone transition fired before the zone registry was ready — \
                         dropping this entity's world presence until they reconnect"
                    );
                    continue;
                };
                let source_zone_id = source_zone_id.to_string();
                tokio::spawn(async move {
                    complete_zone_transition(zones, source_zone_id, target_zone, entity_id, sender)
                        .await;
                });
            }
        }
    }
}

/// Finishes a zone handoff started by `handle_tick_outcomes` above: spawns
/// the entity into `target_zone_id`'s own `Zone`/`WorldHandle` at an
/// entry point resolved from that zone's own manifest links
/// (`ZoneRegistry::entry_point`), registers it in that zone's `Sessions`,
/// and sends the connection a `ZoneChanged` message carrying its new
/// roster — `server::session::handle_session` is what actually switches
/// which zone the connection's *own* task talks to from here on, reading
/// that same message back out of its outgoing channel.
async fn complete_zone_transition(
    zones: Arc<ZoneRegistry>,
    source_zone_id: String,
    target_zone_id: String,
    entity_id: EntityId,
    sender: mpsc::UnboundedSender<gateway::Envelope>,
) {
    let Some(target) = zones.get(&target_zone_id) else {
        tracing::error!(
            target_zone_id,
            "zone transition target isn't a zone this process runs — \
             the entity has no world presence until they reconnect"
        );
        return;
    };

    let entry: Point = zones.entry_point(&source_zone_id, &target_zone_id);
    let message = spawn_into_layer(&target, target_zone_id, entity_id, entry, sender.clone()).await;
    if let Ok(envelope) = message.into_envelope() {
        let _ = sender.send(envelope);
    }
}

/// Spawns `entity_id` into `target` (some layer of `target_zone_id`) at
/// `position`, registers `sender` in its `Sessions`, broadcasts
/// `EntitySpawned` to everyone else already there, and returns the
/// `ZoneChanged` message the connection itself should be told —
/// everything a zone/layer handoff needs on the *arrival* side, shared
/// by [`complete_zone_transition`] (crossing a manifest-declared zone
/// link) and `session`'s `JoinGroupLayer` handling (crossing layers of
/// the *same* zone, #142) — both are structurally the same "despawn
/// here, spawn there, tell the connection" move, differing only in
/// where `target`/`position` come from. Doesn't send the returned
/// message itself — callers differ in how they reach the connection
/// (a background task's `mpsc::Sender` vs. `session::handle_session`'s
/// own `sink`, already mid-await on this same connection).
async fn spawn_into_layer(
    target: &ZoneRuntime,
    target_zone_id: String,
    entity_id: EntityId,
    position: Point,
    sender: mpsc::UnboundedSender<gateway::Envelope>,
) -> ServerMessage {
    target.world.spawn(entity_id, EntityKind::Player, position);
    target.sessions.lock().unwrap().insert(entity_id, sender);

    let roster: Vec<RosterEntry> = target
        .world
        .entities_snapshot()
        .await
        .into_iter()
        .filter(|(id, ..)| *id != entity_id)
        .map(|(id, kind, position)| RosterEntry {
            entity_id: id.to_string(),
            entity_type: session::entity_type_label(kind),
            x: position.0,
            y: position.1,
        })
        .collect();

    broadcast_except(
        &target.sessions,
        entity_id,
        ServerMessage::EntitySpawned {
            entity_id: entity_id.to_string(),
            entity_type: session::entity_type_label(EntityKind::Player),
            x: position.0,
            y: position.1,
        },
    );

    // The destination layer's own tick counter (#196) — each
    // zone-service/layer instance ticks independently, so this is a
    // fresh baseline, not a continuation of wherever the entity was
    // before.
    let tick = target.world.current_tick().await;
    ServerMessage::ZoneChanged {
        zone_id: target_zone_id,
        entity_id: entity_id.to_string(),
        x: position.0,
        y: position.1,
        roster,
        tick,
    }
}

/// Despawns `entity_id` out of `source` (some layer/zone's `ZoneRuntime`)
/// and broadcasts `EntityDespawned` to everyone else there — the
/// departure-side counterpart to [`spawn_into_layer`], used wherever a
/// connection is leaving a `Sessions` map for good (a real zone-link
/// crossing already had this shape inline in `handle_tick_outcomes`,
/// since `Zone::tick` itself despawns from the world for that case; a
/// same-zone layer move (#142) needs it done explicitly here instead,
/// since nothing about the move is a simulated movement outcome).
fn despawn_from_layer(source: &ZoneRuntime, entity_id: EntityId) {
    source.world.despawn(entity_id);
    source.sessions.lock().unwrap().remove(&entity_id);
    broadcast_except(
        &source.sessions,
        entity_id,
        ServerMessage::EntityDespawned {
            entity_id: entity_id.to_string(),
        },
    );
}

fn broadcast_all(sessions: &Sessions, message: ServerMessage) {
    let Ok(envelope) = message.into_envelope() else {
        return;
    };
    for sender in sessions.lock().unwrap().values() {
        let _ = sender.send(envelope.clone());
    }
}

fn broadcast_except(sessions: &Sessions, exclude: EntityId, message: ServerMessage) {
    let Ok(envelope) = message.into_envelope() else {
        return;
    };
    for (id, sender) in sessions.lock().unwrap().iter() {
        if *id != exclude {
            let _ = sender.send(envelope.clone());
        }
    }
}

fn send_to(sessions: &Sessions, entity_id: common::id::EntityId, message: ServerMessage) {
    let Ok(envelope) = message.into_envelope() else {
        return;
    };
    if let Some(sender) = sessions.lock().unwrap().get(&entity_id) {
        let _ = sender.send(envelope);
    }
}
