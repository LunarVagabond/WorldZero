//! Combined-process runnable binary — the Phase 1 target (docs/PROPOSAL.md,
//! "Phased Roadmap"): wires `auth`, `character`, `world`, `gateway`, and
//! `content` into one process a self-hoster can run end to end.
//! `realm-directory`, `chat`, `transfer`, and `plugin-host` come online
//! more fully in later phases — `plugin-host` gets a minimal, optional
//! slice here already (spawning one NPC on startup via a configured
//! plugin's `on_load` hook), matching Phase 1's "minimal plugin hook."
//!
//! `cargo run -p server`. Needs, at minimum:
//! - `WZ_POSTGRES_*` / `WZ_REDIS_*` (`.env`)
//! - `<config_dir>/zone.manifest.yaml` (see
//!   `config/zone.manifest.example.yaml`) — the one zone this
//!   process runs
//! - `<config_dir>/stats.schema.yaml` (see
//!   `config/stats.schema.example.yaml`) — the declared
//!   character attribute schema
//!
//! Optional: `WZ_SERVER_ADDR` (default `127.0.0.1:7900`),
//! `WZ_PLUGIN_MANIFEST_PATH` + `WZ_PLUGIN_WASM_PATH` (both required
//! together — a plugin to run `on_load` against at startup).
//!
//! A connected client speaks `auth::gateway_protocol` first (login or
//! register), then `server::session_protocol` (move, see other entities
//! move) — same gateway-first-authenticate pattern as `chat`'s gateway
//! integration (docs/specs/Auth_Spec.md, "Gateway handshake").

mod plugin_startup;
mod session;
mod session_protocol;
mod world_actor;

use std::sync::{Arc, Mutex};

use character::{AttributeSchema, CharacterStore};
use common::config::{PostgresConfig, RedisConfig};
use common::id::RealmId;
use common::pool::{PoolOptions, postgres_pool, redis_pool};
use content::manifest::ZoneManifest;
use futures_util::StreamExt;
use session::{SessionDeps, Sessions};
use session_protocol::ServerMessage;
use world::{MovementOutcome, Zone};

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

    let manifest_path = config_dir.join("zone.manifest.yaml");
    let manifest = ZoneManifest::from_file(&manifest_path).unwrap_or_else(|e| {
        panic!(
            "failed to load the zone manifest at {} (see config/zone.manifest.example.yaml): {e}",
            manifest_path.display()
        )
    });
    let zone_id = manifest.id.clone();
    tracing::info!(zone_id, "loaded zone manifest");

    let schema_path = config_dir.join("stats.schema.yaml");
    let schema = AttributeSchema::from_file(&schema_path).unwrap_or_else(|e| {
        panic!(
            "failed to load the declared attribute schema at {} (see config/stats.schema.example.yaml): {e}",
            schema_path.display()
        )
    });

    let account_store: Arc<dyn auth::AccountStore> =
        Arc::new(auth::PostgresAccountStore::new(pool.clone()));
    let sessions_manager = auth::SessionManager::new(redis.clone());
    let auth_provider = Arc::new(auth::UsernamePasswordProvider::new(
        account_store,
        sessions_manager,
    ));
    let character_store = Arc::new(CharacterStore::new(pool.clone(), schema));
    let realm_id = placeholder_realm_id();

    let mut zone = Zone::new(manifest, world_config);

    // Created before the plugin loads (not just before the actor starts,
    // as before #95) — the plugin's `send_message` host call needs a
    // `Sessions` handle from the moment it's constructed, even though
    // it's empty (and every `send_message` call correctly errors "not
    // connected") until a client actually connects.
    let sessions: Sessions = Arc::new(Mutex::new(std::collections::HashMap::new()));

    let plugin_runtime = if let (Ok(plugin_manifest_path), Ok(plugin_wasm_path)) = (
        std::env::var("WZ_PLUGIN_MANIFEST_PATH"),
        std::env::var("WZ_PLUGIN_WASM_PATH"),
    ) {
        let plugin_manifest_path = std::path::PathBuf::from(plugin_manifest_path);
        let plugin_wasm_path = std::path::PathBuf::from(plugin_wasm_path);
        match plugin_startup::load_and_run_on_load(
            &plugin_manifest_path,
            &plugin_wasm_path,
            sessions.clone(),
        ) {
            Ok((runtime, spawn_table_ids)) => {
                for spawn_table_id in spawn_table_ids {
                    world_actor::spawn_npc_from_table(&mut zone, &spawn_table_id);
                }
                Some(runtime)
            }
            Err(e) => {
                panic!(
                    "failed to load the configured plugin ({} / {}): {e}",
                    plugin_manifest_path.display(),
                    plugin_wasm_path.display()
                );
            }
        }
    } else {
        tracing::info!(
            "no plugin configured (set WZ_PLUGIN_MANIFEST_PATH and WZ_PLUGIN_WASM_PATH to load one)"
        );
        None
    };
    let plugin_message_types = plugin_runtime
        .as_ref()
        .map(|runtime| runtime.message_types.clone())
        .unwrap_or_default();

    let sessions_for_tick = sessions.clone();
    let world = world_actor::spawn_world_actor(
        zone,
        world_config.tick_interval(),
        plugin_runtime,
        move |zone, outcomes| {
            for (entity_id, outcome) in outcomes {
                match outcome {
                    MovementOutcome::Applied => {
                        if let Some((x, y)) = zone.position_of(entity_id) {
                            broadcast_all(
                                &sessions_for_tick,
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
                            &sessions_for_tick,
                            entity_id,
                            ServerMessage::Rejected {
                                reason: format!("{rejection:?}"),
                            },
                        );
                    }
                }
            }
        },
    );

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
        zone_id,
        world,
        sessions,
        plugin_message_types,
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

fn broadcast_all(sessions: &Sessions, message: ServerMessage) {
    let Ok(envelope) = message.into_envelope() else {
        return;
    };
    for sender in sessions.lock().unwrap().values() {
        let _ = sender.send(envelope.clone());
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
