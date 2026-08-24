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
use futures_util::{SinkExt, StreamExt};
use server_test_support::{ClientMessage, ServerMessage};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio_rustls::TlsConnector;
use tokio_rustls::rustls::RootCertStore;
use tokio_rustls::rustls::pki_types::{CertificateDer, ServerName};

#[path = "../src/session_protocol.rs"]
mod server_test_support;

const ADDR: &str = "127.0.0.1:7910";
const CHAT_ADDR: &str = "127.0.0.1:7911";
const CHAT_DISABLED_ADDR: &str = "127.0.0.1:7912";
const ZONE_TRANSITION_ADDR: &str = "127.0.0.1:7913";
const LAYER_ADDR: &str = "127.0.0.1:7918";
const LAYER_DISABLED_ADDR: &str = "127.0.0.1:7919";
const PLAYER_SESSION_ADDR: &str = "127.0.0.1:7920";
const COMBAT_ADDR: &str = "127.0.0.1:7921";
const MULTI_PLUGIN_ADDR: &str = "127.0.0.1:7922";

struct ServerProcess {
    child: Child,
}

impl Drop for ServerProcess {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn start_server(config_dir: &std::path::Path, addr: &str) -> ServerProcess {
    start_server_with(config_dir, addr, true)
}

/// `chat_enabled = false` sets `WZ_SERVICE_CHAT_ENABLED=false` (#104) —
/// otherwise identical to `start_server`.
fn start_server_with(
    config_dir: &std::path::Path,
    addr: &str,
    chat_enabled: bool,
) -> ServerProcess {
    start_server_with_env(config_dir, addr, chat_enabled, &[])
}

/// Same as `start_server_with`, plus arbitrary extra env vars — used by
/// the zone-transition test (#45) to raise `WZ_WORLD_MAX_SPEED_MPS`
/// enough for one queued move to cross a link edge hundreds of meters
/// away in a single tick, without waiting out hundreds of real ticks at
/// the default walking-speed cap.
fn start_server_with_env(
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

    // #152: plugins are discovered from `<config_dir>/plugins/<name>/`,
    // not a single `WZ_PLUGIN_MANIFEST_PATH`/`WZ_PLUGIN_WASM_PATH` pair
    // anymore — `setup_config_dir` below writes the manifest and copies
    // the compiled wasm fixture into that layout for tests that want a
    // plugin; `setup_content_pack_config_dir`'s tests (e.g. the
    // zone-transition test) deliberately have no `plugins/` dir at all,
    // and `discover_plugins` treats that as the ordinary "no plugins
    // configured" case, not an error.
    for (key, value) in extra_env {
        command.env(key, value);
    }

    let child = command.spawn().expect("failed to start the server binary");
    ServerProcess { child }
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

async fn register_and_authenticate(stream: &mut ClientStream, username: &str, password: &str) {
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
host_api_version = "0.8.0"
capabilities = ["spawning", "movement", "combat", "economy", "messaging"]
message_types = [1000]
hooks = [
    "on-zone-loaded",
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
host_api_version = "0.8.0"
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
host_api_version = "0.8.0"
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

    let _server = start_server(&config_dir, ADDR);
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
            username,
            password: password.to_string(),
        },
    )
    .await;
    assert!(matches!(
        recv_auth(&mut stream).await,
        AuthServerMessage::Authenticated { .. }
    ));

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

    let _server = start_server(&config_dir, PLAYER_SESSION_ADDR);
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
    send_auth(
        &mut stream,
        &AuthClientMessage::Register {
            username: format!("smoke-{}", uuid::Uuid::now_v7()),
            password: password.to_string(),
        },
    )
    .await;
    assert!(matches!(
        recv_auth(&mut stream).await,
        AuthServerMessage::Authenticated { .. }
    ));
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

    let _server = start_server(&config_dir, COMBAT_ADDR);
    wait_for_port(COMBAT_ADDR).await;

    let mut attacker = connect(&config_dir, COMBAT_ADDR).await;
    register_and_authenticate(
        &mut attacker,
        &format!("attacker-{}", uuid::Uuid::now_v7()),
        "hunter2",
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

    let _server = start_server(&config_dir, MULTI_PLUGIN_ADDR);
    wait_for_port(MULTI_PLUGIN_ADDR).await;

    let mut stream = connect(&config_dir, MULTI_PLUGIN_ADDR).await;
    register_and_authenticate(
        &mut stream,
        &format!("multi-plugin-{}", uuid::Uuid::now_v7()),
        "hunter2",
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
    );
    wait_for_port(ZONE_TRANSITION_ADDR).await;

    let mut stream = connect(&config_dir, ZONE_TRANSITION_ADDR).await;
    register_and_authenticate(
        &mut stream,
        &format!("zone-transition-{}", uuid::Uuid::now_v7()),
        "hunter2",
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
    );
    wait_for_port(LAYER_ADDR).await;

    let mut first = connect(&config_dir, LAYER_ADDR).await;
    register_and_authenticate(
        &mut first,
        &format!("layer-isolation-a-{}", uuid::Uuid::now_v7()),
        "hunter2",
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
    );
    wait_for_port(LAYER_DISABLED_ADDR).await;

    let mut first = connect(&config_dir, LAYER_DISABLED_ADDR).await;
    register_and_authenticate(
        &mut first,
        &format!("layer-disabled-a-{}", uuid::Uuid::now_v7()),
        "hunter2",
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
    let _server = start_server(&config_dir, CHAT_ADDR);
    wait_for_port(CHAT_ADDR).await;

    let channel = format!("smoke-{}", uuid::Uuid::now_v7());

    let mut alice = connect(&config_dir, CHAT_ADDR).await;
    register_and_authenticate(
        &mut alice,
        &format!("chat-alice-{}", uuid::Uuid::now_v7()),
        "hunter2",
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
    let _server = start_server_with(&config_dir, CHAT_DISABLED_ADDR, false);
    wait_for_port(CHAT_DISABLED_ADDR).await;

    let mut stream = connect(&config_dir, CHAT_DISABLED_ADDR).await;
    register_and_authenticate(
        &mut stream,
        &format!("chat-disabled-{}", uuid::Uuid::now_v7()),
        "hunter2",
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
    );
    wait_for_port("127.0.0.1:7915").await;
    wait_for_port(metrics_addr).await;

    // One real connection so worldzero_connection_count and the
    // per-zone gauges have something to report.
    let mut stream = connect(&config_dir, "127.0.0.1:7915").await;
    register_and_authenticate(
        &mut stream,
        &format!("metrics-{}", uuid::Uuid::now_v7()),
        "hunter2",
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
    );
    wait_for_port("127.0.0.1:7917").await;

    // Give the process a moment to have started (and *not* bound the
    // metrics port) before asserting the connection is refused.
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(
        tokio::net::TcpStream::connect(metrics_addr).await.is_err(),
        "metrics listener should not be bound when WZ_SERVICE_METRICS_ENABLED=false"
    );
}
