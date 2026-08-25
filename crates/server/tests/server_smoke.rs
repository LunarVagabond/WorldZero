//! End-to-end smoke test against the real, compiled `server` binary
//! (#39's acceptance criteria) — not run by default (needs real
//! Postgres/Redis and, for the plugin-spawned-NPC part, the
//! `plugin-host` test fixtures built for `wasm32-wasip2`). Run with:
//!
//! ```sh
//! cd crates/plugin-host/tests/fixtures/test-plugin
//! cargo build --target wasm32-wasip2 --release
//! cd ../second-plugin
//! cargo build --target wasm32-wasip2 --release
//! cd ../../../../..
//! set -a; source .env; set +a
//! cargo test -p server -- --ignored
//! ```
//!
//! Covers: a client connects, registers/authenticates, receives a roster
//! that includes the NPC the configured plugin spawned via `on_load`,
//! moves, sees the move broadcast back, disconnects, then reconnects and
//! finds its character exactly where it left off — proving movement
//! validation, the plugin hook wiring, and cross-session position
//! persistence all work together over the real gateway transport. Also
//! covers real multi-plugin support (#152, `two_independent_plugins...`
//! below) — two distinct compiled `.wasm` fixtures loaded into the same
//! process at once.

use std::path::PathBuf;
use std::process::{Child, Command};
use std::sync::Arc;
use std::time::Duration;

use auth::gateway_protocol::{
    ClientMessage as AuthClientMessage, ServerMessage as AuthServerMessage,
};
use character_protocol_support::{
    ClientMessage as CharacterClientMessage, ServerMessage as CharacterServerMessage,
};
use futures_util::{SinkExt, StreamExt};
use realm_protocol_support::{
    ClientMessage as RealmClientMessage, ServerMessage as RealmServerMessage,
};
use server_test_support::{ClientMessage, ServerMessage};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio_rustls::TlsConnector;
use tokio_rustls::rustls::RootCertStore;
use tokio_rustls::rustls::pki_types::{CertificateDer, ServerName};

#[path = "../src/session_protocol.rs"]
mod server_test_support;

#[path = "../src/realm_protocol.rs"]
mod realm_protocol_support;

#[path = "../src/character_protocol.rs"]
mod character_protocol_support;

const ADDR: &str = "127.0.0.1:7910";
const CHAT_ADDR: &str = "127.0.0.1:7911";
const CHAT_DISABLED_ADDR: &str = "127.0.0.1:7912";
const ZONE_TRANSITION_ADDR: &str = "127.0.0.1:7913";
const LAYER_ADDR: &str = "127.0.0.1:7918";
const LAYER_DISABLED_ADDR: &str = "127.0.0.1:7919";
const PLAYER_SESSION_ADDR: &str = "127.0.0.1:7920";
const COMBAT_ADDR: &str = "127.0.0.1:7921";
const MULTI_PLUGIN_ADDR: &str = "127.0.0.1:7922";
const OPEN_LEASE_ADDR: &str = "127.0.0.1:7923";
const BOUND_REALM_ADDR: &str = "127.0.0.1:7924";
const REALM_LIST_ADDR: &str = "127.0.0.1:7925";
const REALM_MISMATCH_ADDR: &str = "127.0.0.1:7926";
const MULTI_CHARACTER_ADDR: &str = "127.0.0.1:7927";
const CHARACTER_CAP_ADDR: &str = "127.0.0.1:7928";
const CHARACTER_CREATE_HOOK_ADDR: &str = "127.0.0.1:7929";

struct ServerProcess {
    child: Child,
    /// The realm this process was started to serve (#136/#192) — tests
    /// need this to send `SelectRealm` after authenticating.
    realm_id: common::id::RealmId,
}

impl Drop for ServerProcess {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// A fresh realm for one test's server process to serve (#136 —
/// `WZ_REALM_ID` is required now, no more `placeholder_realm_id()`).
/// Every call gets its own uniquely-named realm, so concurrently-run
/// tests never share one.
async fn create_realm(open_or_bound: realm_directory::OpenOrBound) -> common::id::RealmId {
    let pg_config = common::config::PostgresConfig::from_env().expect("WZ_POSTGRES_* env vars set");
    let pool = common::pool::postgres_pool(&pg_config, common::pool::PoolOptions::default())
        .await
        .expect("failed to connect to Postgres to set up a test realm");
    let store = realm_directory::RealmStore::new(pool);
    let name = format!("smoke-test-realm-{}", common::id::RealmId::new());
    store.create(&name, open_or_bound).await.unwrap()
}

/// Reads a declared stat straight from `characters.stats` (#194's
/// `on-character-create` fixture test) — a direct DB read rather than
/// going through the wire protocol, since there's no client-facing "read
/// my own stats" message today; this is the same JSONB column
/// `character::CharacterStore::get_stat` reads, just queried directly so
/// this test doesn't need to build a matching `AttributeSchema` just to
/// read one key back.
async fn read_character_stat(character_id: &str, key: &str) -> Option<i64> {
    let pg_config = common::config::PostgresConfig::from_env().expect("WZ_POSTGRES_* env vars set");
    let pool = common::pool::postgres_pool(&pg_config, common::pool::PoolOptions::default())
        .await
        .expect("failed to connect to Postgres to read a character stat");
    let character_id: uuid::Uuid = character_id.parse().unwrap();
    sqlx::query_scalar("SELECT (stats->>$2)::bigint FROM characters WHERE id = $1")
        .bind(character_id)
        .bind(key)
        .fetch_one(&pool)
        .await
        .unwrap()
}

async fn start_server(config_dir: &std::path::Path, addr: &str) -> ServerProcess {
    start_server_with(config_dir, addr, true).await
}

/// `chat_enabled = false` sets `WZ_SERVICE_CHAT_ENABLED=false` (#104) —
/// otherwise identical to `start_server`.
async fn start_server_with(
    config_dir: &std::path::Path,
    addr: &str,
    chat_enabled: bool,
) -> ServerProcess {
    start_server_with_env(config_dir, addr, chat_enabled, &[]).await
}

/// Same as `start_server_with`, plus arbitrary extra env vars — used by
/// the zone-transition test (#45) to raise `WZ_WORLD_MAX_SPEED_MPS`
/// enough for one queued move to cross a link edge hundreds of meters
/// away in a single tick, without waiting out hundreds of real ticks at
/// the default walking-speed cap, and by the realm-policy tests (#136)
/// to pass a specific `WZ_REALM_ID` rather than the freshly-created open
/// realm every other test gets by default.
async fn start_server_with_env(
    config_dir: &std::path::Path,
    addr: &str,
    chat_enabled: bool,
    extra_env: &[(&str, &str)],
) -> ServerProcess {
    let mut command = Command::new(env!("CARGO_BIN_EXE_server"));
    command
        .env("WZ_CONFIG_DIR", config_dir)
        .env("WZ_SERVER_ADDR", addr)
        .env(
            "WZ_SERVICE_CHAT_ENABLED",
            if chat_enabled { "true" } else { "false" },
        )
        .env(
            "WZ_POSTGRES_HOST",
            std::env::var("WZ_POSTGRES_HOST").expect("WZ_POSTGRES_* env vars set"),
        )
        .env(
            "WZ_POSTGRES_PORT",
            std::env::var("WZ_POSTGRES_PORT").unwrap(),
        )
        .env(
            "WZ_POSTGRES_USER",
            std::env::var("WZ_POSTGRES_USER").unwrap(),
        )
        .env(
            "WZ_POSTGRES_PASSWORD",
            std::env::var("WZ_POSTGRES_PASSWORD").unwrap(),
        )
        .env(
            "WZ_POSTGRES_DATABASE",
            std::env::var("WZ_POSTGRES_DATABASE").unwrap(),
        )
        .env(
            "WZ_REDIS_HOST",
            std::env::var("WZ_REDIS_HOST").expect("WZ_REDIS_* env vars set"),
        )
        .env("WZ_REDIS_PORT", std::env::var("WZ_REDIS_PORT").unwrap());
    if let Ok(password) = std::env::var("WZ_REDIS_PASSWORD") {
        command.env("WZ_REDIS_PASSWORD", password);
    }

    // A fresh open realm by default (#136) — overridden below if
    // `extra_env` names its own `WZ_REALM_ID` (the realm-policy tests do
    // this to point at a realm they've set up with specific policy/zone
    // assignments).
    let default_realm_id = create_realm(realm_directory::OpenOrBound::Open).await;
    command.env("WZ_REALM_ID", default_realm_id.to_string());

    // #152: plugins are discovered from `<config_dir>/plugins/<name>/`,
    // not a single `WZ_PLUGIN_MANIFEST_PATH`/`WZ_PLUGIN_WASM_PATH` pair
    // anymore — `setup_config_dir` below writes the manifest and copies
    // the compiled wasm fixture into that layout for tests that want a
    // plugin; `setup_content_pack_config_dir`'s tests (e.g. the
    // zone-transition test) deliberately have no `plugins/` dir at all,
    // and `discover_plugins` treats that as the ordinary "no plugins
    // configured" case, not an error.
    let mut realm_id = default_realm_id;
    for (key, value) in extra_env {
        command.env(key, value);
        if *key == "WZ_REALM_ID" {
            realm_id = value
                .parse()
                .expect("extra_env WZ_REALM_ID must be a valid realm id");
        }
    }

    let child = command.spawn().expect("failed to start the server binary");
    ServerProcess { child, realm_id }
}

async fn wait_for_port(addr: &str) {
    for _ in 0..100 {
        if tokio::net::TcpStream::connect(addr).await.is_ok() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("server never started listening on {addr}");
}

type ClientStream = tokio_util::codec::Framed<
    tokio_rustls::client::TlsStream<tokio::net::TcpStream>,
    gateway::EnvelopeCodec,
>;

const STEP_TIMEOUT: Duration = Duration::from_secs(10);

async fn connect(config_dir: &std::path::Path, addr: &str) -> ClientStream {
    gateway::tcp::ensure_crypto_provider_installed();
    let cert = gateway::tls::load_or_generate(config_dir).unwrap();

    let mut roots = RootCertStore::empty();
    roots
        .add(CertificateDer::from(cert.cert_der.clone()))
        .unwrap();
    let client_config = tokio_rustls::rustls::ClientConfig::builder()
        .with_root_certificates(roots)
        .with_no_client_auth();
    let connector = TlsConnector::from(Arc::new(client_config));

    let tcp = tokio::time::timeout(STEP_TIMEOUT, tokio::net::TcpStream::connect(addr))
        .await
        .expect("timed out establishing TCP connection")
        .unwrap();
    let server_name = ServerName::try_from("localhost").unwrap();
    let tls = tokio::time::timeout(STEP_TIMEOUT, connector.connect(server_name, tcp))
        .await
        .expect("timed out during TLS handshake")
        .unwrap();
    tokio_util::codec::Framed::new(tls, gateway::EnvelopeCodec::default())
}

async fn send_auth(stream: &mut ClientStream, message: &AuthClientMessage) {
    stream.send(message.into_envelope().unwrap()).await.unwrap();
}

async fn recv_auth(stream: &mut ClientStream) -> AuthServerMessage {
    let envelope = tokio::time::timeout(STEP_TIMEOUT, stream.next())
        .await
        .expect("timed out waiting for an auth response")
        .expect("connection closed")
        .unwrap();
    AuthServerMessage::from_envelope(&envelope).unwrap()
}

async fn send_realm(stream: &mut ClientStream, message: &RealmClientMessage) {
    stream.send(message.into_envelope().unwrap()).await.unwrap();
}

async fn recv_realm(stream: &mut ClientStream) -> RealmServerMessage {
    let envelope = tokio::time::timeout(STEP_TIMEOUT, stream.next())
        .await
        .expect("timed out waiting for a realm response")
        .expect("connection closed")
        .unwrap();
    RealmServerMessage::from_envelope(&envelope).unwrap()
}

/// Sends `SelectRealm{ realm_id }` and asserts it's accepted — every test
/// does this immediately after authenticating (#192's realm-selection
/// step is mandatory on the wire, even though a single-realm deployment
/// never needs a picker UI client-side — see `realm_protocol`'s doc
/// comment).
async fn select_realm(stream: &mut ClientStream, realm_id: common::id::RealmId) {
    send_realm(
        stream,
        &RealmClientMessage::SelectRealm {
            realm_id: realm_id.to_string(),
        },
    )
    .await;
    assert!(matches!(
        recv_realm(stream).await,
        RealmServerMessage::RealmSelected { .. }
    ));
}

async fn send_character(stream: &mut ClientStream, message: &CharacterClientMessage) {
    stream.send(message.into_envelope().unwrap()).await.unwrap();
}

async fn recv_character(stream: &mut ClientStream) -> CharacterServerMessage {
    let envelope = tokio::time::timeout(STEP_TIMEOUT, stream.next())
        .await
        .expect("timed out waiting for a character response")
        .expect("connection closed")
        .unwrap();
    CharacterServerMessage::from_envelope(&envelope).unwrap()
}

/// Character selection (#193) — creates a character named `name` if the
/// account doesn't have one by that name yet on the already-selected
/// realm, otherwise selects the existing one, then confirms the
/// selection. Mirrors Phase 1's old "one auto-created character per
/// account" behavior for tests that don't care about multi-character
/// specifics; the dedicated multi-character tests below drive
/// `ListCharacters`/`CreateCharacter`/`SelectCharacter` directly instead.
async fn select_or_create_character(stream: &mut ClientStream, name: &str) -> String {
    send_character(stream, &CharacterClientMessage::ListCharacters).await;
    let characters = match recv_character(stream).await {
        CharacterServerMessage::CharacterList { characters } => characters,
        other => panic!("expected a CharacterList, got {other:?}"),
    };
    let character_id = if let Some(existing) = characters.into_iter().find(|c| c.name == name) {
        existing.character_id
    } else {
        send_character(
            stream,
            &CharacterClientMessage::CreateCharacter {
                name: name.to_string(),
            },
        )
        .await;
        match recv_character(stream).await {
            CharacterServerMessage::CharacterCreated { character_id } => character_id,
            other => panic!("expected a CharacterCreated, got {other:?}"),
        }
    };

    send_character(
        stream,
        &CharacterClientMessage::SelectCharacter {
            character_id: character_id.clone(),
        },
    )
    .await;
    assert!(matches!(
        recv_character(stream).await,
        CharacterServerMessage::CharacterSelected { .. }
    ));
    character_id
}

async fn send_world(stream: &mut ClientStream, message: &ClientMessage) {
    stream.send(message.into_envelope().unwrap()).await.unwrap();
}

async fn recv_world(stream: &mut ClientStream) -> ServerMessage {
    let envelope = tokio::time::timeout(STEP_TIMEOUT, stream.next())
        .await
        .expect("timed out waiting for a world message")
        .expect("connection closed")
        .unwrap();
    ServerMessage::from_envelope(&envelope).unwrap()
}

async fn send_chat(stream: &mut ClientStream, message: &chat::gateway_protocol::ClientMessage) {
    stream.send(message.into_envelope().unwrap()).await.unwrap();
}

/// Skips any interleaved non-chat envelope (e.g. the world `EntitySpawned`
/// broadcast another connection's join triggers) rather than assuming the
/// very next envelope on the wire is necessarily a chat one — both share
/// one connection/message loop (#104), so unrelated traffic can land
/// between a request and its reply.
async fn recv_chat(stream: &mut ClientStream) -> chat::gateway_protocol::ServerMessage {
    loop {
        let envelope = tokio::time::timeout(STEP_TIMEOUT, stream.next())
            .await
            .expect("timed out waiting for a chat message")
            .expect("connection closed")
            .unwrap();
        if envelope.message_type == chat::gateway_protocol::CHAT_MESSAGE_TYPE {
            return chat::gateway_protocol::ServerMessage::from_envelope(&envelope).unwrap();
        }
    }
}

async fn register_and_authenticate(
    stream: &mut ClientStream,
    username: &str,
    password: &str,
    realm_id: common::id::RealmId,
) {
    send_auth(
        stream,
        &AuthClientMessage::Register {
            username: username.to_string(),
            password: password.to_string(),
        },
    )
    .await;
    assert!(matches!(
        recv_auth(stream).await,
        AuthServerMessage::Authenticated { .. }
    ));
    select_realm(stream, realm_id).await;
    select_or_create_character(stream, username).await;
}

/// Shared per-test config dir: zone manifest, attribute schema, and a
/// plugin manifest declaring `message_types = [1000]` (#95) — a custom
/// manifest rather than a copy of `config/plugin.example.toml`, whose
/// shipped `message_types` is empty (a generic starting point, not this
/// suite's fixture). `test_name` keeps concurrently-run tests' temp dirs
/// from colliding.
fn setup_config_dir(test_name: &str) -> PathBuf {
    let config_dir = std::env::temp_dir().join(format!(
        "wz-server-smoke-{test_name}-{}",
        std::process::id()
    ));
    std::fs::create_dir_all(&config_dir).unwrap();
    std::fs::copy(
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../config/zone.manifest.example.yaml"),
        config_dir.join("zone.manifest.yaml"),
    )
    .unwrap();
    std::fs::copy(
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../config/stats.schema.example.yaml"),
        config_dir.join("stats.schema.yaml"),
    )
    .unwrap();
    // `<config_dir>/plugins/test-plugin/{plugin.toml,test_plugin.wasm}`
    // (#152's discovery convention) — only written if the compiled wasm
    // fixture actually exists, same "gracefully run with no plugin
    // attached" behavior `start_server_with_env` used to gate on before
    // #152 (building the fixture first is an extra manual step, not
    // something `cargo test` does on its own — see this file's own doc
    // comment).
    let plugin_wasm = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(
        "../plugin-host/tests/fixtures/test-plugin/target/wasm32-wasip2/release/test_plugin.wasm",
    );
    if plugin_wasm.exists() {
        let plugin_dir = config_dir.join("plugins").join("test-plugin");
        std::fs::create_dir_all(&plugin_dir).unwrap();
        std::fs::copy(&plugin_wasm, plugin_dir.join("test_plugin.wasm")).unwrap();
        std::fs::write(
            plugin_dir.join("plugin.toml"),
            r#"
[plugin]
name = "test-plugin"
host_api_version = "0.9.0"
capabilities = ["spawning", "movement", "combat", "economy", "messaging"]
message_types = [1000]
hooks = [
    "on-zone-loaded",
    "on-character-create",
    "on-player-join-zone",
    "on-player-leave-zone",
    "on-interact",
    "on-damage-calc",
    "on-death",
    "on-respawn",
    "on-npc-tick",
    "on-npc-interact",
    "on-item-use",
    "on-item-acquire",
]
"#,
        )
        .unwrap();
    }
    config_dir
}

/// Same shape as `setup_config_dir`, but loads *both* `test-plugin` and
/// `second-plugin` (#152) — two distinct, independently-authored compiled
/// `.wasm` fixtures, both loaded process-wide (there's no per-zone
/// scoping to opt into) and declaring non-colliding
/// `message_types`/`chat_commands` (1000/`give` vs 1001/`second-wave`).
/// Panics loudly (not silently
/// skips) if either fixture wasn't built — unlike `setup_config_dir`,
/// this test is specifically about multi-plugin behavior, so a missing
/// fixture should fail clearly rather than silently degrade to
/// single-plugin or no-plugin.
fn setup_multi_plugin_config_dir(test_name: &str) -> PathBuf {
    let config_dir = std::env::temp_dir().join(format!(
        "wz-server-smoke-{test_name}-{}",
        std::process::id()
    ));
    std::fs::create_dir_all(&config_dir).unwrap();
    std::fs::copy(
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../config/zone.manifest.example.yaml"),
        config_dir.join("zone.manifest.yaml"),
    )
    .unwrap();
    std::fs::copy(
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../config/stats.schema.example.yaml"),
        config_dir.join("stats.schema.yaml"),
    )
    .unwrap();

    let fixtures_dir =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../plugin-host/tests/fixtures");
    let test_plugin_wasm =
        fixtures_dir.join("test-plugin/target/wasm32-wasip2/release/test_plugin.wasm");
    let second_plugin_wasm =
        fixtures_dir.join("second-plugin/target/wasm32-wasip2/release/second_plugin.wasm");
    assert!(
        test_plugin_wasm.exists() && second_plugin_wasm.exists(),
        "both test-plugin and second-plugin must be built for wasm32-wasip2 first — see this file's own doc comment"
    );

    let test_plugin_dir = config_dir.join("plugins").join("test-plugin");
    std::fs::create_dir_all(&test_plugin_dir).unwrap();
    std::fs::copy(&test_plugin_wasm, test_plugin_dir.join("test_plugin.wasm")).unwrap();
    std::fs::write(
        test_plugin_dir.join("plugin.toml"),
        r#"
[plugin]
name = "test-plugin"
host_api_version = "0.9.0"
capabilities = ["spawning", "movement", "combat", "economy", "messaging"]
message_types = [1000]
hooks = ["on-zone-loaded", "on-player-join-zone"]
"#,
    )
    .unwrap();

    let second_plugin_dir = config_dir.join("plugins").join("second-plugin");
    std::fs::create_dir_all(&second_plugin_dir).unwrap();
    std::fs::copy(
        &second_plugin_wasm,
        second_plugin_dir.join("second_plugin.wasm"),
    )
    .unwrap();
    std::fs::write(
        second_plugin_dir.join("plugin.toml"),
        r#"
[plugin]
name = "second-plugin"
host_api_version = "0.9.0"
capabilities = ["messaging"]
message_types = [1001]
hooks = ["on-player-join-zone"]
"#,
    )
    .unwrap();

    config_dir
}

/// Same shape as `setup_config_dir`, but a `content-pack.yaml` (two
/// linked zones, #45) instead of a single `zone.manifest.yaml` — no
/// plugin, this test doesn't need one.
fn setup_content_pack_config_dir(test_name: &str) -> PathBuf {
    let config_dir = std::env::temp_dir().join(format!(
        "wz-server-smoke-{test_name}-{}",
        std::process::id()
    ));
    let example_zones_dir = config_dir.join("example-zones");
    std::fs::create_dir_all(&example_zones_dir).unwrap();

    let repo_config_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../config");
    std::fs::copy(
        repo_config_dir.join("content-pack.example.yaml"),
        config_dir.join("content-pack.yaml"),
    )
    .unwrap();
    for zone_file in ["greenwood-forest.yaml", "stonebridge-village.yaml"] {
        std::fs::copy(
            repo_config_dir.join("example-zones").join(zone_file),
            example_zones_dir.join(zone_file),
        )
        .unwrap();
    }
    std::fs::copy(
        repo_config_dir.join("stats.schema.example.yaml"),
        config_dir.join("stats.schema.yaml"),
    )
    .unwrap();
    config_dir
}

#[tokio::test]
#[ignore]
async fn connect_register_move_and_persist_across_reconnect() {
    let config_dir = setup_config_dir("move");

    let _server = start_server(&config_dir, ADDR).await;
    wait_for_port(ADDR).await;

    let username = format!("smoke-{}", uuid::Uuid::now_v7());
    let password = "hunter2";

    // First connection: register, expect the NPC the plugin spawned via
    // on_load to already be in the roster, then move.
    let mut stream = connect(&config_dir, ADDR).await;
    send_auth(
        &mut stream,
        &AuthClientMessage::Register {
            username: username.clone(),
            password: password.to_string(),
        },
    )
    .await;
    assert!(matches!(
        recv_auth(&mut stream).await,
        AuthServerMessage::Authenticated { .. }
    ));
    select_realm(&mut stream, _server.realm_id).await;
    select_or_create_character(&mut stream, &username).await;

    let own_entity_id = loop {
        if let ServerMessage::Joined {
            entity_id, roster, ..
        } = recv_world(&mut stream).await
        {
            assert!(
                roster.iter().any(|entry| entry.entity_type == "npc"),
                "expected the plugin-spawned NPC in the join roster: {roster:?}"
            );
            break entity_id;
        }
    };

    // Well within the default speed cap (10 m/s at a 20 Hz tick allows
    // ~0.5m per tick) — a real, acceptable move, not a rejected one.
    const MOVE_TO: (f64, f64) = (0.3, 0.2);
    send_world(
        &mut stream,
        &ClientMessage::Move {
            x: MOVE_TO.0,
            y: MOVE_TO.1,
        },
    )
    .await;

    // Drain messages until we see our own Moved confirmation.
    loop {
        match recv_world(&mut stream).await {
            ServerMessage::Moved { entity_id, x, y } if entity_id == own_entity_id => {
                assert_eq!((x, y), MOVE_TO);
                break;
            }
            ServerMessage::Rejected { reason } => {
                panic!("expected the move to be accepted, was rejected: {reason}");
            }
            _ => {}
        }
    }

    // Plugin-routed message (#95): message_type 1000 isn't part of
    // `server::session_protocol` at all — it only reaches the client
    // because the configured plugin declared it and its on_message hook
    // calls send_message back. Proves the full gateway → session →
    // world actor → plugin → session path, not just that the manifest
    // parses.
    stream
        .send(gateway::Envelope::new(1000, b"hello".to_vec()))
        .await
        .unwrap();
    loop {
        match recv_world(&mut stream).await {
            ServerMessage::PluginMessage { body } => {
                assert!(body.contains("1000"), "{body}");
                assert!(body.contains("hello"), "{body}");
                break;
            }
            ServerMessage::Moved { .. } => {}
            other => panic!("expected a PluginMessage reply, got {other:?}"),
        }
    }

    drop(stream);
    // Give the session task a moment to notice the disconnect and persist.
    tokio::time::sleep(Duration::from_millis(500)).await;

    // Second connection, same account: should reconnect to the same
    // character, spawned at the position it moved to.
    let mut stream = connect(&config_dir, ADDR).await;
    send_auth(
        &mut stream,
        &AuthClientMessage::Login {
            username: username.clone(),
            password: password.to_string(),
        },
    )
    .await;
    assert!(matches!(
        recv_auth(&mut stream).await,
        AuthServerMessage::Authenticated { .. }
    ));
    select_realm(&mut stream, _server.realm_id).await;
    // Same character name as the first connection created — resolves to
    // the same existing character via `select_or_create_character`'s
    // "select if it already exists" branch, not a fresh one.
    select_or_create_character(&mut stream, &username).await;

    loop {
        if let ServerMessage::Joined { x, y, .. } = recv_world(&mut stream).await {
            assert_eq!((x, y), MOVE_TO, "position should persist across reconnect");
            break;
        }
    }
}

/// #155: `on-player-join-zone` fires once a connection is fully joined
/// (the fixture plugin greets the newly-joined entity via `send-message`,
/// observable as the client's first `PluginMessage` right after `Joined`)
/// and `on-player-leave-zone` fires on clean disconnect — verified here
/// via the fixture recording the departing entity id under zone-scope
/// plugin state (#149) on leave, then a second connection reading it back
/// through the same `message_type` 1000 path the first smoke test already
/// exercises for `on-message` — the only network-observable proof a
/// black-box test has that the leave hook actually ran, since the
/// departing connection itself is already gone by the time it fires.
#[tokio::test]
#[ignore]
async fn player_join_and_leave_hooks_fire_for_real() {
    let config_dir = setup_config_dir("player-session");

    let _server = start_server(&config_dir, PLAYER_SESSION_ADDR).await;
    wait_for_port(PLAYER_SESSION_ADDR).await;

    let username = format!("smoke-{}", uuid::Uuid::now_v7());
    let password = "hunter2";

    let mut stream = connect(&config_dir, PLAYER_SESSION_ADDR).await;
    send_auth(
        &mut stream,
        &AuthClientMessage::Register {
            username: username.clone(),
            password: password.to_string(),
        },
    )
    .await;
    assert!(matches!(
        recv_auth(&mut stream).await,
        AuthServerMessage::Authenticated { .. }
    ));
    select_realm(&mut stream, _server.realm_id).await;
    select_or_create_character(&mut stream, &username).await;

    let own_entity_id = loop {
        if let ServerMessage::Joined { entity_id, .. } = recv_world(&mut stream).await {
            break entity_id;
        }
    };

    loop {
        match recv_world(&mut stream).await {
            ServerMessage::PluginMessage { body } => {
                assert!(body.contains("welcome"), "{body}");
                assert!(body.contains(&own_entity_id), "{body}");
                break;
            }
            ServerMessage::Moved { .. } => {}
            other => panic!("expected the on-player-join-zone greeting, got {other:?}"),
        }
    }

    drop(stream);
    // Give the session task a moment to notice the disconnect and run
    // on-player-leave-zone before the second connection asks about it.
    tokio::time::sleep(Duration::from_millis(500)).await;

    let mut stream = connect(&config_dir, PLAYER_SESSION_ADDR).await;
    let second_username = format!("smoke-{}", uuid::Uuid::now_v7());
    send_auth(
        &mut stream,
        &AuthClientMessage::Register {
            username: second_username.clone(),
            password: password.to_string(),
        },
    )
    .await;
    assert!(matches!(
        recv_auth(&mut stream).await,
        AuthServerMessage::Authenticated { .. }
    ));
    select_realm(&mut stream, _server.realm_id).await;
    select_or_create_character(&mut stream, &second_username).await;
    loop {
        if let ServerMessage::Joined { .. } = recv_world(&mut stream).await {
            break;
        }
    }
    // Drain this second connection's own join greeting before querying.
    loop {
        match recv_world(&mut stream).await {
            ServerMessage::PluginMessage { .. } => break,
            ServerMessage::Moved { .. } => {}
            other => panic!("expected this connection's own join greeting, got {other:?}"),
        }
    }

    stream
        .send(gateway::Envelope::new(1000, b"last-left".to_vec()))
        .await
        .unwrap();
    loop {
        match recv_world(&mut stream).await {
            ServerMessage::PluginMessage { body } => {
                assert!(body.starts_with("last-left:"), "{body}");
                assert!(body.contains(&own_entity_id), "{body}");
                break;
            }
            ServerMessage::Moved { .. } => {}
            other => panic!("expected the last-left query reply, got {other:?}"),
        }
    }
}

/// #154: real client-protocol call sites for `on-damage-calc`,
/// `on-item-use`, and `on-npc-interact` — each fires the fixture
/// plugin's hook with real, server-validated data (an `Attack`/
/// `InteractNpc` naming an entity id the client made up would be dropped
/// before the hook is ever called, not passed through). Also covers
/// `report-death`/`report-respawn` (the plugin-owned trigger for
/// `on-death`/`on-respawn`) via the fixture's `die`/`respawn`
/// `on-message` commands.
#[tokio::test]
#[ignore]
async fn combat_item_use_npc_interact_and_death_respawn_hooks_fire_for_real() {
    let config_dir = setup_config_dir("combat-hooks");

    let _server = start_server(&config_dir, COMBAT_ADDR).await;
    wait_for_port(COMBAT_ADDR).await;

    let mut attacker = connect(&config_dir, COMBAT_ADDR).await;
    register_and_authenticate(
        &mut attacker,
        &format!("attacker-{}", uuid::Uuid::now_v7()),
        "hunter2",
        _server.realm_id,
    )
    .await;
    let (_attacker_id, npc_id) = loop {
        if let ServerMessage::Joined {
            entity_id, roster, ..
        } = recv_world(&mut attacker).await
        {
            let npc_id = roster
                .iter()
                .find(|entry| entry.entity_type == "npc")
                .map(|entry| entry.entity_id.clone())
                .expect("expected the plugin-spawned NPC in the join roster");
            break (entity_id, npc_id);
        }
    };
    // Drain this connection's own join greeting (#155).
    loop {
        match recv_world(&mut attacker).await {
            ServerMessage::PluginMessage { .. } => break,
            ServerMessage::Moved { .. } => {}
            other => panic!("expected the join greeting, got {other:?}"),
        }
    }

    let mut target = connect(&config_dir, COMBAT_ADDR).await;
    register_and_authenticate(
        &mut target,
        &format!("target-{}", uuid::Uuid::now_v7()),
        "hunter2",
        _server.realm_id,
    )
    .await;
    let target_id = loop {
        if let ServerMessage::Joined { entity_id, .. } = recv_world(&mut target).await {
            break entity_id;
        }
    };
    loop {
        match recv_world(&mut target).await {
            ServerMessage::PluginMessage { .. } => break,
            ServerMessage::Moved { .. } => {}
            other => panic!("expected the join greeting, got {other:?}"),
        }
    }
    // The attacker also sees the target's own join broadcast — drain it.
    loop {
        match recv_world(&mut attacker).await {
            ServerMessage::EntitySpawned { .. } => break,
            ServerMessage::Moved { .. } => {}
            other => panic!("expected the target's EntitySpawned, got {other:?}"),
        }
    }

    // Attack: the server confirms target_id is real before ever calling
    // on-damage-calc — the client only ever requests the attack, never
    // reports a damage amount (#154).
    send_world(
        &mut attacker,
        &ClientMessage::Attack {
            target_entity_id: target_id.clone(),
            stat_key: "hp".to_string(),
        },
    )
    .await;
    loop {
        match recv_world(&mut attacker).await {
            ServerMessage::PluginMessage { body } => {
                assert!(body.contains(&target_id), "{body}");
                assert!(body.contains("hp"), "{body}");
                assert!(body.contains("base_amount was 0"), "{body}");
                break;
            }
            ServerMessage::Moved { .. } | ServerMessage::EntitySpawned { .. } => {}
            other => panic!("expected the on-damage-calc confirmation, got {other:?}"),
        }
    }

    // InteractNpc: distinct from the generic trigger-volume on-interact.
    send_world(
        &mut attacker,
        &ClientMessage::InteractNpc {
            npc_entity_id: npc_id.clone(),
        },
    )
    .await;
    loop {
        match recv_world(&mut attacker).await {
            ServerMessage::PluginMessage { body } => {
                assert!(body.contains(&npc_id), "{body}");
                break;
            }
            ServerMessage::Moved { .. } | ServerMessage::EntitySpawned { .. } => {}
            other => panic!("expected the on-npc-interact confirmation, got {other:?}"),
        }
    }

    // UseItem: the core never validates ownership itself — the hook
    // fires regardless, the plugin decides what happens.
    send_world(
        &mut attacker,
        &ClientMessage::UseItem {
            item_type: "torch".to_string(),
        },
    )
    .await;
    loop {
        match recv_world(&mut attacker).await {
            ServerMessage::PluginMessage { body } => {
                assert!(body.contains("torch"), "{body}");
                break;
            }
            ServerMessage::Moved { .. } | ServerMessage::EntitySpawned { .. } => {}
            other => panic!("expected the on-item-use confirmation, got {other:?}"),
        }
    }

    // report-death/report-respawn (#154): the plugin decides "died"/
    // "respawned" and reports it; on-death/on-respawn firing back is
    // what actually confirms it to the client.
    attacker
        .send(gateway::Envelope::new(1000, b"die".to_vec()))
        .await
        .unwrap();
    loop {
        match recv_world(&mut attacker).await {
            ServerMessage::PluginMessage { body } => {
                assert!(body.contains("you died"), "{body}");
                break;
            }
            ServerMessage::Moved { .. } | ServerMessage::EntitySpawned { .. } => {}
            other => panic!("expected on-death's confirmation, got {other:?}"),
        }
    }

    attacker
        .send(gateway::Envelope::new(1000, b"respawn".to_vec()))
        .await
        .unwrap();
    loop {
        match recv_world(&mut attacker).await {
            ServerMessage::PluginMessage { body } => {
                assert!(body.contains("you respawned"), "{body}");
                break;
            }
            ServerMessage::Moved { .. } | ServerMessage::EntitySpawned { .. } => {}
            other => panic!("expected on-respawn's confirmation, got {other:?}"),
        }
    }
}

/// #152: real multi-plugin support — `test-plugin` and `second-plugin`,
/// two distinct compiled `.wasm` fixtures, loaded into the same `server`
/// process at once via `WZ_PLUGINS_DIR`'s directory discovery. Proves:
/// (1) a shared lifecycle hook (`on-player-join-zone`) fans out to both,
/// independently — the host never picks a winner; (2) each plugin's own
/// declared `message_types` (1000 vs 1001) routes to that plugin alone,
/// never both, despite neither collision-checking having any way to know
/// that from message content; (3) `second-plugin`'s manifest declares
/// only `hooks = ["on-player-join-zone"]` — it never declares
/// `on-zone-loaded`, so no NPC of its own ever gets attributed to it
/// (the wolf-pack NPC in the roster is `test-plugin`'s doing alone, same
/// as every other single-plugin test in this file).
#[tokio::test]
#[ignore]
async fn two_independent_plugins_fan_out_a_shared_hook_and_keep_message_types_separate() {
    let config_dir = setup_multi_plugin_config_dir("multi-plugin");

    let _server = start_server(&config_dir, MULTI_PLUGIN_ADDR).await;
    wait_for_port(MULTI_PLUGIN_ADDR).await;

    let mut stream = connect(&config_dir, MULTI_PLUGIN_ADDR).await;
    register_and_authenticate(
        &mut stream,
        &format!("multi-plugin-{}", uuid::Uuid::now_v7()),
        "hunter2",
        _server.realm_id,
    )
    .await;
    let own_entity_id = loop {
        if let ServerMessage::Joined { entity_id, .. } = recv_world(&mut stream).await {
            break entity_id;
        }
    };

    // Both plugins' own on-player-join-zone greeting should arrive,
    // independently, in some order — collect until both are seen.
    let (mut saw_test_plugin, mut saw_second_plugin) = (false, false);
    while !(saw_test_plugin && saw_second_plugin) {
        match recv_world(&mut stream).await {
            ServerMessage::PluginMessage { body } => {
                // Check the second-plugin greeting first: "second-plugin
                // also welcomes ..." contains "welcome" as a substring
                // ("welcomes"), so checking the test-plugin's plain
                // "welcome, ..." pattern first would swallow both
                // messages into `saw_test_plugin` and this loop would
                // wait forever for a `saw_second_plugin` that already
                // arrived.
                if body.contains("second-plugin also welcomes") && body.contains(&own_entity_id) {
                    saw_second_plugin = true;
                } else if body.contains("welcome") && body.contains(&own_entity_id) {
                    saw_test_plugin = true;
                } else {
                    panic!("unexpected plugin message: {body}");
                }
            }
            ServerMessage::Moved { .. } | ServerMessage::EntitySpawned { .. } => {}
            other => panic!("expected a join greeting from one of the two plugins, got {other:?}"),
        }
    }

    // message_type 1000 belongs to test-plugin alone.
    stream
        .send(gateway::Envelope::new(1000, b"hello".to_vec()))
        .await
        .unwrap();
    loop {
        match recv_world(&mut stream).await {
            ServerMessage::PluginMessage { body } => {
                assert!(body.contains("on-message 1000: hello"), "{body}");
                assert!(!body.contains("second-plugin"), "{body}");
                break;
            }
            ServerMessage::Moved { .. } | ServerMessage::EntitySpawned { .. } => {}
            other => panic!("expected test-plugin's on-message reply, got {other:?}"),
        }
    }

    // message_type 1001 belongs to second-plugin alone.
    stream
        .send(gateway::Envelope::new(1001, b"hi".to_vec()))
        .await
        .unwrap();
    loop {
        match recv_world(&mut stream).await {
            ServerMessage::PluginMessage { body } => {
                assert!(body.contains("second-plugin on-message 1001: hi"), "{body}");
                break;
            }
            ServerMessage::Moved { .. } | ServerMessage::EntitySpawned { .. } => {}
            other => panic!("expected second-plugin's on-message reply, got {other:?}"),
        }
    }
}

/// #45: with a `content-pack.yaml` present, the combined process runs
/// *multiple* zone-service instances — a player walking through a
/// manifest-declared `links[]` edge crosses live, over the same TCP
/// connection/gateway session, no reconnect. `WZ_WORLD_MAX_SPEED_MPS` is
/// raised for this test process only so one queued move covers the
/// ~450m from spawn to the link edge in a single tick, rather than the
/// test waiting out hundreds of ticks at the default walking-speed cap.
#[tokio::test]
#[ignore]
async fn zone_transition_crosses_a_link_without_reconnecting() {
    let config_dir = setup_content_pack_config_dir("zone-transition");
    let _server = start_server_with_env(
        &config_dir,
        ZONE_TRANSITION_ADDR,
        true,
        &[("WZ_WORLD_MAX_SPEED_MPS", "1000000")],
    )
    .await;
    wait_for_port(ZONE_TRANSITION_ADDR).await;

    let mut stream = connect(&config_dir, ZONE_TRANSITION_ADDR).await;
    register_and_authenticate(
        &mut stream,
        &format!("zone-transition-{}", uuid::Uuid::now_v7()),
        "hunter2",
        _server.realm_id,
    )
    .await;

    // A freshly created character starts at (0, 0) in the pack's first
    // zone — greenwood-forest (config/content-pack.example.yaml's
    // declaration order).
    let own_entity_id = loop {
        if let ServerMessage::Joined { entity_id, .. } = recv_world(&mut stream).await {
            break entity_id;
        }
    };

    // greenwood-forest's link to stonebridge-village sits at x=500,
    // y in [200,300] (config/example-zones/greenwood-forest.yaml) —
    // (505, 250) is well past it, straight-line from the origin.
    send_world(&mut stream, &ClientMessage::Move { x: 505.0, y: 250.0 }).await;

    loop {
        match recv_world(&mut stream).await {
            ServerMessage::ZoneChanged {
                zone_id,
                entity_id,
                roster,
                ..
            } if entity_id == own_entity_id => {
                assert_eq!(zone_id, "stonebridge-village");
                // Nothing else has ever spawned into stonebridge-village
                // in this test — an empty roster confirms this is a
                // real, fresh arrival in the other zone, not e.g. a
                // stale/duplicated message replaying greenwood-forest's
                // own roster.
                assert!(roster.is_empty(), "{roster:?}");
                break;
            }
            ServerMessage::Rejected { reason } => {
                panic!("expected the cross-zone move to be accepted, was rejected: {reason}");
            }
            _ => {}
        }
    }
}

/// #50: with `WZ_LAYER_POPULATION_THRESHOLD=1`, a second connection into
/// an already-occupied zone must spin up a fresh layer rather than share
/// the first connection's — proven the same way the zone-transition test
/// above proves a fresh arrival: an empty roster. If both connections
/// had landed on the same layer, the second one's `Joined` roster would
/// include the first.
#[tokio::test]
#[ignore]
async fn a_low_population_threshold_isolates_two_joining_connections_onto_separate_layers() {
    let config_dir = setup_content_pack_config_dir("layer-isolation");
    let _server = start_server_with_env(
        &config_dir,
        LAYER_ADDR,
        false,
        &[("WZ_LAYER_POPULATION_THRESHOLD", "1")],
    )
    .await;
    wait_for_port(LAYER_ADDR).await;

    let mut first = connect(&config_dir, LAYER_ADDR).await;
    register_and_authenticate(
        &mut first,
        &format!("layer-isolation-a-{}", uuid::Uuid::now_v7()),
        "hunter2",
        _server.realm_id,
    )
    .await;
    let first_roster = loop {
        if let ServerMessage::Joined { roster, .. } = recv_world(&mut first).await {
            break roster;
        }
    };
    assert!(first_roster.is_empty(), "{first_roster:?}");

    let mut second = connect(&config_dir, LAYER_ADDR).await;
    register_and_authenticate(
        &mut second,
        &format!("layer-isolation-b-{}", uuid::Uuid::now_v7()),
        "hunter2",
        _server.realm_id,
    )
    .await;
    let second_roster = loop {
        if let ServerMessage::Joined { roster, .. } = recv_world(&mut second).await {
            break roster;
        }
    };
    // With the default (much higher) threshold this would contain the
    // first connection's entity — an empty roster here is only possible
    // because the population threshold forced a second, separate layer.
    assert!(second_roster.is_empty(), "{second_roster:?}");
}

/// #50: `WZ_LAYER_ENABLED=false` must override even a threshold of `1` —
/// a deployment that opts out of layering entirely should never see a
/// second layer, full stop. Same shape as the test above, but this time
/// the second connection's roster must *include* the first (same layer),
/// where the enabled case above proved the opposite.
#[tokio::test]
#[ignore]
async fn layering_disabled_keeps_connections_on_the_same_layer_regardless_of_threshold() {
    let config_dir = setup_content_pack_config_dir("layer-disabled");
    let _server = start_server_with_env(
        &config_dir,
        LAYER_DISABLED_ADDR,
        false,
        &[
            ("WZ_LAYER_ENABLED", "false"),
            ("WZ_LAYER_POPULATION_THRESHOLD", "1"),
        ],
    )
    .await;
    wait_for_port(LAYER_DISABLED_ADDR).await;

    let mut first = connect(&config_dir, LAYER_DISABLED_ADDR).await;
    register_and_authenticate(
        &mut first,
        &format!("layer-disabled-a-{}", uuid::Uuid::now_v7()),
        "hunter2",
        _server.realm_id,
    )
    .await;
    let first_entity_id = loop {
        if let ServerMessage::Joined { entity_id, .. } = recv_world(&mut first).await {
            break entity_id;
        }
    };

    let mut second = connect(&config_dir, LAYER_DISABLED_ADDR).await;
    register_and_authenticate(
        &mut second,
        &format!("layer-disabled-b-{}", uuid::Uuid::now_v7()),
        "hunter2",
        _server.realm_id,
    )
    .await;
    let second_roster = loop {
        if let ServerMessage::Joined { roster, .. } = recv_world(&mut second).await {
            break roster;
        }
    };

    assert!(
        second_roster
            .iter()
            .any(|entry| entry.entity_id == first_entity_id),
        "expected the first connection's entity in the roster (same layer), got {second_roster:?}"
    );
}

/// #104: chat wired into the combined `server` process (not just the
/// standalone `chat::bin::gateway_server` demo) — two connections join
/// the same channel over the *same* gateway/auth session that also
/// carries world/plugin traffic, and one's `Send` reaches the other via
/// the real Redis-backed pub/sub path (`chat::ChatBus`).
#[tokio::test]
#[ignore]
async fn chat_join_send_and_receive_across_two_connections() {
    let config_dir = setup_config_dir("chat");
    let _server = start_server(&config_dir, CHAT_ADDR).await;
    wait_for_port(CHAT_ADDR).await;

    let channel = format!("smoke-{}", uuid::Uuid::now_v7());

    let mut alice = connect(&config_dir, CHAT_ADDR).await;
    register_and_authenticate(
        &mut alice,
        &format!("chat-alice-{}", uuid::Uuid::now_v7()),
        "hunter2",
        _server.realm_id,
    )
    .await;
    // Drain the world `Joined` message every connection gets right after
    // auth, same as the movement test — chat traffic is a different
    // message_type on the same connection, not a separate handshake.
    assert!(matches!(
        recv_world(&mut alice).await,
        ServerMessage::Joined { .. }
    ));

    let mut bob = connect(&config_dir, CHAT_ADDR).await;
    register_and_authenticate(
        &mut bob,
        &format!("chat-bob-{}", uuid::Uuid::now_v7()),
        "hunter2",
        _server.realm_id,
    )
    .await;
    assert!(matches!(
        recv_world(&mut bob).await,
        ServerMessage::Joined { .. }
    ));

    send_chat(
        &mut alice,
        &chat::gateway_protocol::ClientMessage::Join {
            channel: channel.clone(),
        },
    )
    .await;
    let channel_id = match recv_chat(&mut alice).await {
        chat::gateway_protocol::ServerMessage::Joined { channel_id, .. } => channel_id,
        other => panic!("expected Joined, got {other:?}"),
    };

    send_chat(
        &mut bob,
        &chat::gateway_protocol::ClientMessage::Join {
            channel: channel.clone(),
        },
    )
    .await;
    assert!(matches!(
        recv_chat(&mut bob).await,
        chat::gateway_protocol::ServerMessage::Joined { .. }
    ));

    send_chat(
        &mut alice,
        &chat::gateway_protocol::ClientMessage::Send {
            channel_id,
            body: "hello from alice".to_string(),
        },
    )
    .await;

    match recv_chat(&mut bob).await {
        chat::gateway_protocol::ServerMessage::Chat { body, sender, .. } => {
            assert_eq!(body, "hello from alice");
            assert!(sender.starts_with("chat-alice-"), "{sender}");
        }
        other => panic!("expected Chat, got {other:?}"),
    }
}

/// #104: `WZ_SERVICE_CHAT_ENABLED=false` means chat's message_type is
/// unroutable, not silently ignored — a clear per-connection error, and
/// (per #92's design) chat's `ChannelStore`/`ChatBus` are never
/// constructed at all for this process (verified by code path, not
/// observable from the wire — this test proves the wire-visible half).
#[tokio::test]
#[ignore]
async fn chat_disabled_returns_a_clear_error() {
    let config_dir = setup_config_dir("chat-disabled");
    let _server = start_server_with(&config_dir, CHAT_DISABLED_ADDR, false).await;
    wait_for_port(CHAT_DISABLED_ADDR).await;

    let mut stream = connect(&config_dir, CHAT_DISABLED_ADDR).await;
    register_and_authenticate(
        &mut stream,
        &format!("chat-disabled-{}", uuid::Uuid::now_v7()),
        "hunter2",
        _server.realm_id,
    )
    .await;
    assert!(matches!(
        recv_world(&mut stream).await,
        ServerMessage::Joined { .. }
    ));

    stream
        .send(gateway::Envelope::new(
            chat::gateway_protocol::CHAT_MESSAGE_TYPE,
            b"{\"kind\":\"Join\",\"channel\":\"trade\"}".to_vec(),
        ))
        .await
        .unwrap();

    loop {
        match recv_world(&mut stream).await {
            ServerMessage::Error { message } => {
                assert!(message.contains("chat is disabled"), "{message}");
                break;
            }
            // The configured plugin's own on-player-join-zone greeting
            // (#155) — unrelated to this test, drain past it.
            ServerMessage::PluginMessage { .. } => {}
            other => panic!("expected an Error, got {other:?}"),
        }
    }
}

/// #48: `/metrics` is a real, separate HTTP listener — a plain GET
/// against it returns a Prometheus-exposition-format body naming every
/// metric this build exposes, once there's been at least one connection
/// and one tick to actually populate them.
#[tokio::test]
#[ignore]
async fn metrics_endpoint_serves_prometheus_text() {
    let config_dir = setup_config_dir("metrics");
    let metrics_addr = "127.0.0.1:7914";
    let _server = start_server_with_env(
        &config_dir,
        "127.0.0.1:7915",
        true,
        &[("WZ_METRICS_ADDR", metrics_addr)],
    )
    .await;
    wait_for_port("127.0.0.1:7915").await;
    wait_for_port(metrics_addr).await;

    // One real connection so worldzero_connection_count and the
    // per-zone gauges have something to report.
    let mut stream = connect(&config_dir, "127.0.0.1:7915").await;
    register_and_authenticate(
        &mut stream,
        &format!("metrics-{}", uuid::Uuid::now_v7()),
        "hunter2",
        _server.realm_id,
    )
    .await;
    assert!(matches!(
        recv_world(&mut stream).await,
        ServerMessage::Joined { .. }
    ));
    // Give the zone actor at least one tick to run and populate the
    // per-zone gauges/histogram before scraping.
    tokio::time::sleep(Duration::from_millis(200)).await;

    let mut metrics_stream =
        tokio::time::timeout(STEP_TIMEOUT, tokio::net::TcpStream::connect(metrics_addr))
            .await
            .expect("timed out connecting to the metrics listener")
            .unwrap();
    metrics_stream
        .write_all(b"GET /metrics HTTP/1.1\r\nHost: localhost\r\n\r\n")
        .await
        .unwrap();

    let mut response = Vec::new();
    tokio::time::timeout(STEP_TIMEOUT, metrics_stream.read_to_end(&mut response))
        .await
        .expect("timed out reading the metrics response")
        .unwrap();
    let response = String::from_utf8(response).unwrap();

    assert!(response.starts_with("HTTP/1.1 200 OK"), "{response}");
    assert!(
        response.contains("worldzero_zone_tick_duration_seconds"),
        "{response}"
    );
    assert!(
        response.contains("worldzero_zone_entity_count"),
        "{response}"
    );
    assert!(
        response.contains("worldzero_zone_world_command_queue_depth"),
        "{response}"
    );
    assert!(
        response.contains("worldzero_connection_count 1"),
        "{response}"
    );
}

/// #48: `WZ_SERVICE_METRICS_ENABLED=false` means no `/metrics` listener
/// binds at all — not a listener that's up but returns nothing.
#[tokio::test]
#[ignore]
async fn metrics_disabled_means_no_listener_at_all() {
    let config_dir = setup_config_dir("metrics-disabled");
    let metrics_addr = "127.0.0.1:7916";
    let _server = start_server_with_env(
        &config_dir,
        "127.0.0.1:7917",
        true,
        &[
            ("WZ_METRICS_ADDR", metrics_addr),
            ("WZ_SERVICE_METRICS_ENABLED", "false"),
        ],
    )
    .await;
    wait_for_port("127.0.0.1:7917").await;

    // Give the process a moment to have started (and *not* bound the
    // metrics port) before asserting the connection is refused.
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(
        tokio::net::TcpStream::connect(metrics_addr).await.is_err(),
        "metrics listener should not be bound when WZ_SERVICE_METRICS_ENABLED=false"
    );
}

/// #51/#136, end to end (not just `realm-directory`'s own crate-level
/// tests): two connections for the same account, both logging into the
/// same open realm at once, is exactly the split-brain scenario #21's
/// session lease exists to prevent — `login_policy::authorize_login`
/// must refuse the second one while the first is still connected and
/// holding the lease.
#[tokio::test]
#[ignore]
async fn a_second_login_to_the_same_open_realm_character_is_rejected_while_the_first_is_still_connected()
 {
    let config_dir = setup_config_dir("open-lease");
    let _server = start_server(&config_dir, OPEN_LEASE_ADDR).await;
    wait_for_port(OPEN_LEASE_ADDR).await;

    let username = format!("open-lease-{}", uuid::Uuid::now_v7());
    let password = "hunter2";

    // First connection stays open for the rest of this test — its lease
    // is never released.
    let mut first = connect(&config_dir, OPEN_LEASE_ADDR).await;
    register_and_authenticate(&mut first, &username, password, _server.realm_id).await;
    assert!(matches!(
        recv_world(&mut first).await,
        ServerMessage::Joined { .. }
    ));

    // Second connection, same account: `authorize_login` should refuse
    // it — the first connection's lease on this character is still held.
    // Realm selection itself still succeeds (that's a separate, earlier
    // step, #192) — the rejection only happens once login-policy
    // enforcement runs afterward.
    let mut second = connect(&config_dir, OPEN_LEASE_ADDR).await;
    send_auth(
        &mut second,
        &AuthClientMessage::Login {
            username,
            password: password.to_string(),
        },
    )
    .await;
    assert!(matches!(
        recv_auth(&mut second).await,
        AuthServerMessage::Authenticated { .. }
    ));
    select_realm(&mut second, _server.realm_id).await;

    // `authorize_login` now runs at `SelectCharacter` time (#193), not
    // right after realm selection — list, then select the same character
    // the first connection is using, and expect that specific selection
    // to be rejected.
    send_character(&mut second, &CharacterClientMessage::ListCharacters).await;
    let character_id = match recv_character(&mut second).await {
        CharacterServerMessage::CharacterList { characters } => {
            assert_eq!(characters.len(), 1, "{characters:?}");
            characters[0].character_id.clone()
        }
        other => panic!("expected a CharacterList, got {other:?}"),
    };
    send_character(
        &mut second,
        &CharacterClientMessage::SelectCharacter { character_id },
    )
    .await;
    match recv_character(&mut second).await {
        CharacterServerMessage::Error { message } => {
            assert!(message.contains("already logged in elsewhere"), "{message}");
        }
        other => panic!("expected the second selection to be rejected, got {other:?}"),
    }
}

/// A bound realm's login flow, end to end — separate from the open-realm
/// lease-contention test above, since a bound realm never touches #21's
/// lease table at all. Note: `authorize_login`'s bound-mismatch
/// *rejection* branch itself isn't reachable through this single-realm-
/// per-process login flow — `resolve_or_create_character` only ever
/// resolves a character whose `realm_id` already matches `deps.realm_id`
/// for a bound realm (`CharacterStore::find_by_account`'s realm-scoped
/// lookup), so the mismatch case can't fire here; it needs a real
/// multi-realm character-selection flow (#193) to reach, and is already
/// covered by `realm-directory`'s own crate-level tests
/// (`login_policy.rs`). This test instead proves the ordinary bound-realm
/// path — resolve, authorize, reconnect to the same character — is
/// really wired through `login_policy` rather than skipped.
#[tokio::test]
#[ignore]
async fn a_bound_realm_login_resolves_and_authorizes_through_the_real_policy() {
    let config_dir = setup_config_dir("bound-realm");
    let realm_id = create_realm(realm_directory::OpenOrBound::Bound).await;
    let realm_id = realm_id.to_string();
    let _server = start_server_with_env(
        &config_dir,
        BOUND_REALM_ADDR,
        true,
        &[("WZ_REALM_ID", realm_id.as_str())],
    )
    .await;
    wait_for_port(BOUND_REALM_ADDR).await;

    let username = format!("bound-realm-{}", uuid::Uuid::now_v7());
    let password = "hunter2";

    let mut stream = connect(&config_dir, BOUND_REALM_ADDR).await;
    register_and_authenticate(&mut stream, &username, password, _server.realm_id).await;
    assert!(matches!(
        recv_world(&mut stream).await,
        ServerMessage::Joined { .. }
    ));
    drop(stream);
    // Give the session task a moment to notice the disconnect, release
    // the lease (a harmless no-op here, since a bound realm never took
    // one), and persist.
    tokio::time::sleep(Duration::from_millis(500)).await;

    // Reconnect: listed via `login_policy::list_characters`'s bound
    // branch, then selected and authorized — the same character, not a
    // freshly created one (`select_or_create_character` finds it by name
    // rather than creating a second one).
    let mut stream = connect(&config_dir, BOUND_REALM_ADDR).await;
    send_auth(
        &mut stream,
        &AuthClientMessage::Login {
            username: username.clone(),
            password: password.to_string(),
        },
    )
    .await;
    assert!(matches!(
        recv_auth(&mut stream).await,
        AuthServerMessage::Authenticated { .. }
    ));
    select_realm(&mut stream, _server.realm_id).await;
    select_or_create_character(&mut stream, &username).await;
    assert!(matches!(
        recv_world(&mut stream).await,
        ServerMessage::Joined { .. }
    ));
}

/// #192, end to end: `ListRealms` reports the one realm this process
/// serves with real numbers, both before and after this connection
/// itself joins — proving `character_count`/`live_connection_count`
/// (#137) are read live, not hardcoded/cached at startup.
#[tokio::test]
#[ignore]
async fn list_realms_reports_the_one_served_realm_with_live_numbers() {
    let config_dir = setup_config_dir("realm-list");
    let _server = start_server(&config_dir, REALM_LIST_ADDR).await;
    wait_for_port(REALM_LIST_ADDR).await;

    // First connection: registers, selects, and fully joins — so there's
    // a real character and a real live connection on this realm by the
    // time the second connection asks about it. `ListRealms` is only
    // ever handled during the pre-join realm-selection phase (#192), so
    // this connection can't re-query after joining — a second, still-
    // unjoined connection is what actually exercises "live," not a
    // second request on the same one.
    let mut joined = connect(&config_dir, REALM_LIST_ADDR).await;
    register_and_authenticate(
        &mut joined,
        &format!("realm-list-a-{}", uuid::Uuid::now_v7()),
        "hunter2",
        _server.realm_id,
    )
    .await;
    assert!(matches!(
        recv_world(&mut joined).await,
        ServerMessage::Joined { .. }
    ));

    // Second connection: authenticated, but deliberately stops short of
    // selecting a realm — queries the list instead, and should see the
    // first connection's character and live presence already reflected.
    let mut observer = connect(&config_dir, REALM_LIST_ADDR).await;
    send_auth(
        &mut observer,
        &AuthClientMessage::Register {
            username: format!("realm-list-b-{}", uuid::Uuid::now_v7()),
            password: "hunter2".to_string(),
        },
    )
    .await;
    assert!(matches!(
        recv_auth(&mut observer).await,
        AuthServerMessage::Authenticated { .. }
    ));

    send_realm(&mut observer, &RealmClientMessage::ListRealms).await;
    match recv_realm(&mut observer).await {
        RealmServerMessage::RealmList { realms } => {
            assert_eq!(realms.len(), 1, "{realms:?}");
            let realm = &realms[0];
            assert_eq!(realm.realm_id, _server.realm_id.to_string());
            assert_eq!(realm.open_or_bound, "open");
            assert_eq!(realm.character_count, 1, "{realm:?}");
            assert_eq!(realm.live_connection_count, 1, "{realm:?}");
        }
        other => panic!("expected a RealmList, got {other:?}"),
    }
}

/// #192: `SelectRealm` naming a realm other than the one this process
/// serves is rejected with a clear error, and the connection is closed
/// rather than left hanging — the same "one process, one realm" rule
/// #136 enforces at login, enforced here one step earlier in the
/// handshake instead.
#[tokio::test]
#[ignore]
async fn select_realm_rejects_a_realm_this_process_does_not_serve() {
    let config_dir = setup_config_dir("realm-mismatch");
    let _server = start_server(&config_dir, REALM_MISMATCH_ADDR).await;
    wait_for_port(REALM_MISMATCH_ADDR).await;
    let other_realm_id = create_realm(realm_directory::OpenOrBound::Open).await;

    let username = format!("realm-mismatch-{}", uuid::Uuid::now_v7());
    let password = "hunter2";

    let mut stream = connect(&config_dir, REALM_MISMATCH_ADDR).await;
    send_auth(
        &mut stream,
        &AuthClientMessage::Register {
            username: username.clone(),
            password: password.to_string(),
        },
    )
    .await;
    assert!(matches!(
        recv_auth(&mut stream).await,
        AuthServerMessage::Authenticated { .. }
    ));

    send_realm(
        &mut stream,
        &RealmClientMessage::SelectRealm {
            realm_id: other_realm_id.to_string(),
        },
    )
    .await;
    match recv_realm(&mut stream).await {
        RealmServerMessage::Error { message } => {
            assert!(message.contains("only serves realm"), "{message}");
        }
        other => panic!("expected a realm Error, got {other:?}"),
    }

    // The connection is closed after a rejected selection, not left open
    // waiting for a valid one — `Ok(None)` is a clean EOF, `Ok(Some(Err(_)))`
    // a reset; either means "closed." A `Err(_)` (timeout) would mean it's
    // still open, which is the failure case this asserts against.
    let closed = tokio::time::timeout(STEP_TIMEOUT, stream.next())
        .await
        .expect("connection should have closed, not stayed open");
    assert!(
        matches!(closed, None | Some(Err(_))),
        "expected the connection to close after a rejected realm selection, got {closed:?}"
    );
}

/// #193, end to end, with more than one character on one account (the
/// ticket's own acceptance criteria): list, create two, list again, then
/// prove selection actually determines *which* character's state loads
/// — not just "a" character — by moving one of the two, reconnecting,
/// and confirming each selection loads that specific character's own
/// position.
#[tokio::test]
#[ignore]
async fn an_account_can_create_list_and_select_between_multiple_characters() {
    let config_dir = setup_config_dir("multi-character");
    let _server = start_server(&config_dir, MULTI_CHARACTER_ADDR).await;
    wait_for_port(MULTI_CHARACTER_ADDR).await;

    let username = format!("multi-character-{}", uuid::Uuid::now_v7());
    let password = "hunter2";

    let mut stream = connect(&config_dir, MULTI_CHARACTER_ADDR).await;
    send_auth(
        &mut stream,
        &AuthClientMessage::Register {
            username: username.clone(),
            password: password.to_string(),
        },
    )
    .await;
    assert!(matches!(
        recv_auth(&mut stream).await,
        AuthServerMessage::Authenticated { .. }
    ));
    select_realm(&mut stream, _server.realm_id).await;

    // No characters yet.
    send_character(&mut stream, &CharacterClientMessage::ListCharacters).await;
    match recv_character(&mut stream).await {
        CharacterServerMessage::CharacterList { characters } => {
            assert!(characters.is_empty(), "{characters:?}");
        }
        other => panic!("expected an empty CharacterList, got {other:?}"),
    }

    // Create two.
    send_character(
        &mut stream,
        &CharacterClientMessage::CreateCharacter {
            name: "Aria".to_string(),
        },
    )
    .await;
    let aria_id = match recv_character(&mut stream).await {
        CharacterServerMessage::CharacterCreated { character_id } => character_id,
        other => panic!("expected a CharacterCreated, got {other:?}"),
    };
    send_character(
        &mut stream,
        &CharacterClientMessage::CreateCharacter {
            name: "Bram".to_string(),
        },
    )
    .await;
    let bram_id = match recv_character(&mut stream).await {
        CharacterServerMessage::CharacterCreated { character_id } => character_id,
        other => panic!("expected a CharacterCreated, got {other:?}"),
    };
    assert_ne!(aria_id, bram_id);

    // List reflects both.
    send_character(&mut stream, &CharacterClientMessage::ListCharacters).await;
    match recv_character(&mut stream).await {
        CharacterServerMessage::CharacterList { characters } => {
            let mut names: Vec<_> = characters.iter().map(|c| c.name.clone()).collect();
            names.sort();
            assert_eq!(
                names,
                vec!["Aria".to_string(), "Bram".to_string()],
                "{characters:?}"
            );
        }
        other => panic!("expected a CharacterList, got {other:?}"),
    }

    // Select Bram specifically, move it, disconnect.
    send_character(
        &mut stream,
        &CharacterClientMessage::SelectCharacter {
            character_id: bram_id.clone(),
        },
    )
    .await;
    assert!(matches!(
        recv_character(&mut stream).await,
        CharacterServerMessage::CharacterSelected { .. }
    ));
    assert!(matches!(
        recv_world(&mut stream).await,
        ServerMessage::Joined { .. }
    ));
    const MOVE_TO: (f64, f64) = (0.3, 0.2);
    send_world(
        &mut stream,
        &ClientMessage::Move {
            x: MOVE_TO.0,
            y: MOVE_TO.1,
        },
    )
    .await;
    loop {
        match recv_world(&mut stream).await {
            ServerMessage::Moved { x, y, .. } => {
                assert_eq!((x, y), MOVE_TO);
                break;
            }
            ServerMessage::Rejected { reason } => panic!("move rejected: {reason}"),
            _ => {}
        }
    }
    drop(stream);
    tokio::time::sleep(Duration::from_millis(500)).await;

    // Reconnect, select Aria — untouched by Bram's move, still at origin.
    // Proves selection determines *which* character's state loads.
    let mut stream = connect(&config_dir, MULTI_CHARACTER_ADDR).await;
    send_auth(
        &mut stream,
        &AuthClientMessage::Login {
            username: username.clone(),
            password: password.to_string(),
        },
    )
    .await;
    assert!(matches!(
        recv_auth(&mut stream).await,
        AuthServerMessage::Authenticated { .. }
    ));
    select_realm(&mut stream, _server.realm_id).await;
    send_character(
        &mut stream,
        &CharacterClientMessage::SelectCharacter {
            character_id: aria_id,
        },
    )
    .await;
    assert!(matches!(
        recv_character(&mut stream).await,
        CharacterServerMessage::CharacterSelected { .. }
    ));
    match recv_world(&mut stream).await {
        ServerMessage::Joined { x, y, .. } => assert_eq!(
            (x, y),
            (0.0, 0.0),
            "Aria should be untouched by Bram's move"
        ),
        other => panic!("expected Joined, got {other:?}"),
    }
    drop(stream);
    tokio::time::sleep(Duration::from_millis(300)).await;

    // Reconnect once more, select Bram — should be where it was moved to.
    let mut stream = connect(&config_dir, MULTI_CHARACTER_ADDR).await;
    send_auth(
        &mut stream,
        &AuthClientMessage::Login {
            username,
            password: password.to_string(),
        },
    )
    .await;
    assert!(matches!(
        recv_auth(&mut stream).await,
        AuthServerMessage::Authenticated { .. }
    ));
    select_realm(&mut stream, _server.realm_id).await;
    send_character(
        &mut stream,
        &CharacterClientMessage::SelectCharacter {
            character_id: bram_id,
        },
    )
    .await;
    assert!(matches!(
        recv_character(&mut stream).await,
        CharacterServerMessage::CharacterSelected { .. }
    ));
    match recv_world(&mut stream).await {
        ServerMessage::Joined { x, y, .. } => {
            assert_eq!((x, y), MOVE_TO, "Bram should be where it was moved to")
        }
        other => panic!("expected Joined, got {other:?}"),
    }
}

/// #193's character-creation cap — a third `CreateCharacter` is rejected
/// once `WZ_CHARACTER_MAX_PER_ACCOUNT` is reached, and the connection
/// stays open and usable afterward (the account can still select one of
/// its existing characters).
#[tokio::test]
#[ignore]
async fn character_creation_is_rejected_once_the_per_account_cap_is_reached() {
    let config_dir = setup_config_dir("character-cap");
    let _server = start_server_with_env(
        &config_dir,
        CHARACTER_CAP_ADDR,
        true,
        &[("WZ_CHARACTER_MAX_PER_ACCOUNT", "2")],
    )
    .await;
    wait_for_port(CHARACTER_CAP_ADDR).await;

    let username = format!("character-cap-{}", uuid::Uuid::now_v7());
    let password = "hunter2";

    let mut stream = connect(&config_dir, CHARACTER_CAP_ADDR).await;
    send_auth(
        &mut stream,
        &AuthClientMessage::Register {
            username: username.clone(),
            password: password.to_string(),
        },
    )
    .await;
    assert!(matches!(
        recv_auth(&mut stream).await,
        AuthServerMessage::Authenticated { .. }
    ));
    select_realm(&mut stream, _server.realm_id).await;

    for name in ["Aria", "Bram"] {
        send_character(
            &mut stream,
            &CharacterClientMessage::CreateCharacter {
                name: name.to_string(),
            },
        )
        .await;
        assert!(matches!(
            recv_character(&mut stream).await,
            CharacterServerMessage::CharacterCreated { .. }
        ));
    }

    // A third exceeds the cap.
    send_character(
        &mut stream,
        &CharacterClientMessage::CreateCharacter {
            name: "Cato".to_string(),
        },
    )
    .await;
    match recv_character(&mut stream).await {
        CharacterServerMessage::Error { message } => {
            assert!(message.contains("character limit reached"), "{message}");
        }
        other => panic!("expected the cap to reject creation, got {other:?}"),
    }

    // The connection is still open and usable — the account can still
    // select one of its existing characters.
    send_character(&mut stream, &CharacterClientMessage::ListCharacters).await;
    match recv_character(&mut stream).await {
        CharacterServerMessage::CharacterList { characters } => {
            assert_eq!(characters.len(), 2, "{characters:?}");
        }
        other => panic!("expected a CharacterList, got {other:?}"),
    }
}

/// #194, end to end through the real `server` binary: the fixture
/// plugin's `on-character-create` hook calls
/// `apply-stat-delta-for-character` to set a starting stat on a
/// character that has no entity/session yet — observed here by reading
/// the value straight out of Postgres right after `CharacterCreated`
/// comes back, before the character is ever selected/spawned into the
/// world (proving the write landed pre-spawn, not as a side effect of
/// joining).
#[tokio::test]
#[ignore]
async fn plugin_sets_a_starting_stat_via_on_character_create() {
    let config_dir = setup_config_dir("character-create-hook");
    let _server = start_server(&config_dir, CHARACTER_CREATE_HOOK_ADDR).await;
    wait_for_port(CHARACTER_CREATE_HOOK_ADDR).await;

    let username = format!("character-create-hook-{}", uuid::Uuid::now_v7());
    let password = "hunter2";

    let mut stream = connect(&config_dir, CHARACTER_CREATE_HOOK_ADDR).await;
    send_auth(
        &mut stream,
        &AuthClientMessage::Register {
            username: username.clone(),
            password: password.to_string(),
        },
    )
    .await;
    assert!(matches!(
        recv_auth(&mut stream).await,
        AuthServerMessage::Authenticated { .. }
    ));
    select_realm(&mut stream, _server.realm_id).await;

    send_character(
        &mut stream,
        &CharacterClientMessage::CreateCharacter {
            name: "Aria".to_string(),
        },
    )
    .await;
    let character_id = match recv_character(&mut stream).await {
        CharacterServerMessage::CharacterCreated { character_id } => character_id,
        other => panic!("expected a CharacterCreated, got {other:?}"),
    };

    // The fixture plugin's on_character_create applies a +25 delta to
    // reputation.ironclad_guild (default 0, no declared bounds) — same
    // key/host-function `on_player_leave_zone` already exercises
    // elsewhere in this suite, just via the character-id-scoped variant.
    let stat = read_character_stat(&character_id, "reputation.ironclad_guild").await;
    assert_eq!(stat, Some(25), "starting stat should be set pre-spawn");
}
