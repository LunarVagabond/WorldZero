//! Combined-process runnable binary — the Phase 1 target (docs/PROPOSAL.md,
//! "Phased Roadmap"): wires `auth`, `character`, `world`, `gateway`,
//! `content`, and `chat` into one process a self-hoster can run end to
//! end. `realm-directory` and `transfer` come online in later phases —
//! `plugin-host` gets a minimal, optional slice here already (spawning
//! one NPC on startup via a configured plugin's `on_load` hook, plus
//! live `on-message` routing, #95), matching Phase 1's "minimal plugin
//! hook." `chat` is the first optional-service crate wired in, gated by
//! `WZ_SERVICE_CHAT_ENABLED` per the #91/#92 runtime-toggle decision (#104).
//!
//! `cargo run -p server`. Needs, at minimum:
//! - `WZ_POSTGRES_*` / `WZ_REDIS_*` (`.env`)
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
//!
//! Optional: `WZ_SERVER_ADDR` (default `127.0.0.1:7900`),
//! `WZ_PLUGIN_MANIFEST_PATH` + `WZ_PLUGIN_WASM_PATH` (both required
//! together — a plugin to run `on_load` against at startup; attached to
//! only the *first* zone loaded when running multiple, see
//! `zone_registry`'s doc comment), `WZ_SERVICE_CHAT_ENABLED` (default
//! `true`).
//!
//! A connected client speaks `auth::gateway_protocol` first (login or
//! register), then `server::session_protocol` (move, see other entities
//! move, zone transitions) and, when chat is enabled,
//! `chat::gateway_protocol` (join/leave/send) over the same connection —
//! same gateway-first-authenticate pattern as `chat`'s standalone
//! gateway demo (docs/specs/Auth_Spec.md, "Gateway handshake").

mod chat_session;
mod plugin_startup;
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
use session::{EntityCharacters, SessionDeps, Sessions};
use session_protocol::{RosterEntry, ServerMessage};
use tokio::sync::mpsc;
use world::{EntityKind, MovementOutcome, Point, Zone};
use zone_registry::{ZoneRegistry, ZoneRuntime};

const DEFAULT_ADDR: &str = "127.0.0.1:7900";

/// Placeholder until `realm-directory` (#47) exists — one fixed, nil
/// realm id every character in this phase-1 process belongs to. Safe as
/// a placeholder specifically because it's deterministic across restarts
/// (a random one would orphan every previously-created character from
/// `CharacterStore::find_by_account`'s realm-scoped lookup on every
/// restart).
fn placeholder_realm_id() -> RealmId {
    RealmId::from_uuid(uuid::Uuid::nil())
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
    let inventory_config =
        InventoryConfig::from_env().expect("invalid WZ_INVENTORY_MAX_ITEM_TYPES");

    let account_store: Arc<dyn auth::AccountStore> =
        Arc::new(auth::PostgresAccountStore::new(pool.clone()));
    let sessions_manager = auth::SessionManager::new(redis.clone());
    let auth_provider = Arc::new(auth::UsernamePasswordProvider::new(
        account_store,
        sessions_manager,
    ));
    let character_store = Arc::new(CharacterStore::new(pool.clone(), schema, inventory_config));
    let realm_id = placeholder_realm_id();

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
    let mut plugin_message_types = Vec::new();
    let mut plugin_chat_commands = Vec::new();

    for (index, manifest) in zone_manifests.into_iter().enumerate() {
        let zone_id = manifest.id.clone();
        let mut zone = Zone::new(manifest.clone(), world_config);
        manifests.insert(zone_id.clone(), manifest);

        // Created before the plugin loads (not just before the actor
        // starts, as before #95) — the plugin's `send_message` host call
        // needs a `Sessions` handle from the moment it's constructed,
        // even though it's empty (and every `send_message` call
        // correctly errors "not connected") until a client connects.
        let sessions: Sessions = Arc::new(Mutex::new(HashMap::new()));

        // The configured plugin (if any) attaches to only the *first*
        // zone loaded — real per-zone plugin instantiation is out of
        // scope for #45; see `zone_registry`'s doc comment for the gap
        // this leaves against `docs/specs/Plugin_API.md`'s "instantiated
        // for a zone-service" wording.
        let plugin_runtime = if index == 0 {
            load_configured_plugin(&mut zone, sessions.clone())
        } else {
            None
        };
        if let Some(runtime) = &plugin_runtime {
            plugin_message_types = runtime.message_types.clone();
            plugin_chat_commands = runtime.chat_commands.clone();
        }

        let registry_cell = zone_registry_cell.clone();
        let sessions_for_tick = sessions.clone();
        let zone_id_for_tick = zone_id.clone();
        let world = world_actor::spawn_world_actor(
            zone,
            world_config.tick_interval(),
            plugin_runtime,
            character_store.clone(),
            entity_characters.clone(),
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

    let zones = Arc::new(ZoneRegistry::new(runtimes, manifests));
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
        zones,
        default_zone_id,
        entity_characters,
        plugin_message_types,
        plugin_chat_commands,
        chat: chat_deps,
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

/// Loads the plugin named by `WZ_PLUGIN_MANIFEST_PATH`/`WZ_PLUGIN_WASM_PATH`
/// (both unset is the ordinary "no plugin configured" case, not an
/// error) and seeds `zone` with any `spawn-npc` calls its `on_load` hook
/// made. Panics on a configured-but-invalid plugin, same as before #45
/// — a self-hoster who set these vars wants to know immediately if the
/// plugin they pointed at doesn't load, not have it silently skipped.
fn load_configured_plugin(
    zone: &mut Zone,
    sessions: Sessions,
) -> Option<plugin_startup::PluginRuntime> {
    let (Ok(plugin_manifest_path), Ok(plugin_wasm_path)) = (
        std::env::var("WZ_PLUGIN_MANIFEST_PATH"),
        std::env::var("WZ_PLUGIN_WASM_PATH"),
    ) else {
        tracing::info!(
            "no plugin configured (set WZ_PLUGIN_MANIFEST_PATH and WZ_PLUGIN_WASM_PATH to load one)"
        );
        return None;
    };
    let plugin_manifest_path = std::path::PathBuf::from(plugin_manifest_path);
    let plugin_wasm_path = std::path::PathBuf::from(plugin_wasm_path);

    match plugin_startup::load_and_run_on_load(&plugin_manifest_path, &plugin_wasm_path, sessions) {
        Ok((runtime, spawn_table_ids)) => {
            for spawn_table_id in spawn_table_ids {
                world_actor::spawn_npc_from_table(zone, &spawn_table_id);
            }
            Some(runtime)
        }
        Err(e) => panic!(
            "failed to load the configured plugin ({} / {}): {e}",
            plugin_manifest_path.display(),
            plugin_wasm_path.display()
        ),
    }
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
    for (entity_id, outcome) in outcomes {
        match outcome {
            MovementOutcome::Applied => {
                if let Some((x, y)) = zone.position_of(entity_id) {
                    broadcast_all(
                        source_sessions,
                        ServerMessage::Moved {
                            entity_id: entity_id.to_string(),
                            x,
                            y,
                        },
                    );
                }
            }
            MovementOutcome::Rejected(rejection) => {
                send_to(
                    source_sessions,
                    entity_id,
                    ServerMessage::Rejected {
                        reason: format!("{rejection:?}"),
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
    target.world.spawn(entity_id, EntityKind::Player, entry);
    target
        .sessions
        .lock()
        .unwrap()
        .insert(entity_id, sender.clone());

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
            x: entry.0,
            y: entry.1,
        },
    );

    let message = ServerMessage::ZoneChanged {
        zone_id: target_zone_id,
        entity_id: entity_id.to_string(),
        x: entry.0,
        y: entry.1,
        roster,
    };
    if let Ok(envelope) = message.into_envelope() {
        let _ = sender.send(envelope);
    }
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
