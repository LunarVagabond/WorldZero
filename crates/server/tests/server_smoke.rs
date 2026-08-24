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

    let plugin_wasm = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(
        "../plugin-host/tests/fixtures/test-plugin/target/wasm32-wasip2/release/test_plugin.wasm",
    );
    let plugin_manifest_path = config_dir.join("plugin.toml");
    // Gated on *this test's own* config dir actually declaring a
    // plugin.toml, not just on the wasm fixture existing somewhere on
    // disk — `setup_content_pack_config_dir`'s tests (e.g. the
    // zone-transition test) deliberately have no plugin.toml, and
    // wiring WZ_PLUGIN_MANIFEST_PATH at them anyway made the spawned
    // server panic trying to read a file that was never written. Only
    // possible once the wasm fixture is actually built (previously never
    // true in CI, so this path was never exercised there before).
    if plugin_wasm.exists() && plugin_manifest_path.exists() {
        command
            .env("WZ_PLUGIN_MANIFEST_PATH", plugin_manifest_path)
            .env("WZ_PLUGIN_WASM_PATH", plugin_wasm);
    }

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
    std::fs::write(
        config_dir.join("plugin.toml"),
        r#"
[plugin]
name = "test-plugin"
host_api_version = "0.6.0"
message_types = [1000]
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

    let _server = start_server(&config_dir, ADDR);
    wait_for_port(ADDR).await;

    let username = format!("smoke-{}", uuid::Uuid::now_v7());
    let password = "hunter2";

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

    let mut stream = connect(&config_dir, ADDR).await;
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

    match recv_world(&mut stream).await {
        ServerMessage::Error { message } => {
            assert!(message.contains("chat is disabled"), "{message}");
        }
        other => panic!("expected an Error, got {other:?}"),
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
