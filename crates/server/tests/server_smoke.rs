//! End-to-end smoke test against the real, compiled `server` binary
//! (#39's acceptance criteria) — not run by default (needs real
//! Postgres/Redis and, for the plugin-spawned-NPC part, the
//! `plugin-host` test fixture built for `wasm32-wasip2`). Run with:
//!
//! ```sh
//! cd crates/plugin-host/tests/fixtures/test-plugin
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
//! persistence all work together over the real gateway transport.

use std::path::PathBuf;
use std::process::{Child, Command};
use std::sync::Arc;
use std::time::Duration;

use auth::gateway_protocol::{
    ClientMessage as AuthClientMessage, ServerMessage as AuthServerMessage,
};
use futures_util::{SinkExt, StreamExt};
use server_test_support::{ClientMessage, ServerMessage};
use tokio_rustls::TlsConnector;
use tokio_rustls::rustls::RootCertStore;
use tokio_rustls::rustls::pki_types::{CertificateDer, ServerName};

#[path = "../src/session_protocol.rs"]
mod server_test_support;

const ADDR: &str = "127.0.0.1:7910";
const CHAT_ADDR: &str = "127.0.0.1:7911";
const CHAT_DISABLED_ADDR: &str = "127.0.0.1:7912";

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

    let plugin_wasm = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(
        "../plugin-host/tests/fixtures/test-plugin/target/wasm32-wasip2/release/test_plugin.wasm",
    );
    if plugin_wasm.exists() {
        command
            .env("WZ_PLUGIN_MANIFEST_PATH", config_dir.join("plugin.toml"))
            .env("WZ_PLUGIN_WASM_PATH", plugin_wasm);
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
    std::fs::write(
        config_dir.join("plugin.toml"),
        r#"
[plugin]
name = "test-plugin"
host_api_version = "0.2.0"
message_types = [1000]
"#,
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

    match recv_world(&mut stream).await {
        ServerMessage::Error { message } => {
            assert!(message.contains("chat is disabled"), "{message}");
        }
        other => panic!("expected an Error, got {other:?}"),
    }
}
