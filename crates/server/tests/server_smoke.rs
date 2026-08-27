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
const SESSION_RESUME_ADDR: &str = "127.0.0.1:7930";
const SESSION_RESUME_INVALID_ADDR: &str = "127.0.0.1:7931";
const MOVE_CORRELATION_ADDR: &str = "127.0.0.1:7932";
const PING_PONG_ADDR: &str = "127.0.0.1:7933";
const NPC_STATS_ADDR: &str = "127.0.0.1:7934";
const GROUP_LAYER_ADDR: &str = "127.0.0.1:7935";
const GROUP_LAYER_RECONNECT_ADDR: &str = "127.0.0.1:7936";
const PARTY_DECLINE_ADDR: &str = "127.0.0.1:7937";
const PARTY_LEAVE_ADDR: &str = "127.0.0.1:7938";
const GUILD_CHAT_SYNC_ADDR: &str = "127.0.0.1:7939";
const GUILD_NO_CHAT_ADDR: &str = "127.0.0.1:7940";
const ARCHETYPE_ADDR: &str = "127.0.0.1:7941";
const CRAFT_ADDR: &str = "127.0.0.1:7942";
const CRAFT_INSUFFICIENT_ADDR: &str = "127.0.0.1:7943";
const CRAFT_UNKNOWN_RECIPE_ADDR: &str = "127.0.0.1:7944";
const SPAWN_CORRELATION_ADDR: &str = "127.0.0.1:7945";
const TRANSFER_ADDR: &str = "127.0.0.1:7946";
const TRANSFER_REJECTED_ADDR: &str = "127.0.0.1:7947";

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

/// Reads an item stack's quantity straight from `items` (#216's crafting
/// tests) — a direct DB read, same "no client-facing read of my own
/// inventory today" reasoning as `read_character_stat`. `0` (not an
/// error) if the character owns no stack of `item_type` at all, same
/// convention `character::CharacterStore::item_quantity` itself uses.
async fn read_item_quantity(character_id: &str, item_type: &str) -> i64 {
    let pg_config = common::config::PostgresConfig::from_env().expect("WZ_POSTGRES_* env vars set");
    let pool = common::pool::postgres_pool(&pg_config, common::pool::PoolOptions::default())
        .await
        .expect("failed to connect to Postgres to read an item quantity");
    let character_id: uuid::Uuid = character_id.parse().unwrap();
    let quantity: Option<i64> =
        sqlx::query_scalar("SELECT quantity FROM items WHERE character_id = $1 AND item_type = $2")
            .bind(character_id)
            .bind(item_type)
            .fetch_optional(&pool)
            .await
            .unwrap();
    quantity.unwrap_or(0)
}

/// Reads an account's id straight from `accounts.username` (#179's chat
/// sync tests) — a direct DB read, same shape as `read_character_stat`,
/// since there's no client-facing "what's my account id" message.
async fn account_id_for_username(username: &str) -> uuid::Uuid {
    let pg_config = common::config::PostgresConfig::from_env().expect("WZ_POSTGRES_* env vars set");
    let pool = common::pool::postgres_pool(&pg_config, common::pool::PoolOptions::default())
        .await
        .expect("failed to connect to Postgres to read an account id");
    sqlx::query_scalar("SELECT id FROM accounts WHERE username = $1")
        .bind(username)
        .fetch_one(&pool)
        .await
        .unwrap()
}

/// Reads a guild's synced chat channel id straight from `guilds.name`
/// (#179's chat sync tests) — `None` if chat was disabled at creation
/// (the column stays `NULL`, see `guild::GuildStore::create`'s own
/// `chat_channel_id` parameter).
async fn guild_chat_channel_id(guild_name: &str) -> Option<uuid::Uuid> {
    let pg_config = common::config::PostgresConfig::from_env().expect("WZ_POSTGRES_* env vars set");
    let pool = common::pool::postgres_pool(&pg_config, common::pool::PoolOptions::default())
        .await
        .expect("failed to connect to Postgres to read a guild's chat channel id");
    sqlx::query_scalar("SELECT chat_channel_id FROM guilds WHERE name = $1")
        .bind(guild_name)
        .fetch_one(&pool)
        .await
        .unwrap()
}

/// Every account currently a member of chat channel `channel_id` (#179's
/// chat sync tests) — a direct DB read against `chat::ChannelStore`'s own
/// `chat_channel_members` table, proving the sync actually happened
/// rather than trusting the wire protocol's own account of itself.
async fn chat_channel_member_ids(channel_id: uuid::Uuid) -> Vec<uuid::Uuid> {
    let pg_config = common::config::PostgresConfig::from_env().expect("WZ_POSTGRES_* env vars set");
    let pool = common::pool::postgres_pool(&pg_config, common::pool::PoolOptions::default())
        .await
        .expect("failed to connect to Postgres to read chat channel members");
    sqlx::query_scalar("SELECT account_id FROM chat_channel_members WHERE channel_id = $1")
        .bind(channel_id)
        .fetch_all(&pool)
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
                archetype_key: String::new(),
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

/// Grants one of `item_type` to `stream`'s own character via the fixture
/// plugin's `/give` chat command (#57/#211's e2e `grant-item` coverage,
/// reused here to set up a #216 craft's declared inputs) — drains the
/// automatic `ItemChanged` push and the plugin's own `on-item-acquire`
/// confirmation, same two-message shape
/// `combat_item_use_npc_interact_and_death_respawn_hooks_fire_for_real`
/// already exercises for a single `/give`.
async fn give_item(stream: &mut ClientStream, item_type: &str) {
    send_chat(
        stream,
        &chat::gateway_protocol::ClientMessage::Send {
            channel_id: common::id::ChannelId::new(),
            body: format!("/give {item_type}"),
        },
    )
    .await;
    loop {
        match recv_world(stream).await {
            ServerMessage::ItemChanged { .. } => break,
            ServerMessage::Moved { .. } | ServerMessage::EntitySpawned { .. } => {}
            other => panic!("expected ItemChanged after /give {item_type}, got {other:?}"),
        }
    }
    loop {
        match recv_world(stream).await {
            ServerMessage::PluginMessage { .. } => break,
            ServerMessage::Moved { .. } | ServerMessage::EntitySpawned { .. } => {}
            other => {
                panic!(
                    "expected the on-item-acquire confirmation after /give {item_type}, got {other:?}"
                )
            }
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
/// plugin manifest declaring `message_types = [1000]` (#95) and
/// `chat_commands = ["give"]` (#57/#211's e2e `grant-item` coverage) — a
/// custom manifest rather than a copy of `config/plugin.example.toml`,
/// whose shipped `message_types`/`chat_commands` are empty (a generic
/// starting point, not this suite's fixture). `test_name` keeps
/// concurrently-run tests' temp dirs from colliding.
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
    std::fs::copy(
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../config/party.schema.example.yaml"),
        config_dir.join("party.schema.yaml"),
    )
    .unwrap();
    std::fs::copy(
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../config/guild.schema.example.yaml"),
        config_dir.join("guild.schema.yaml"),
    )
    .unwrap();
    std::fs::copy(
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../config/character.archetypes.example.yaml"),
        config_dir.join("character.archetypes.yaml"),
    )
    .unwrap();
    std::fs::copy(
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../config/crafting.schema.example.yaml"),
        config_dir.join("crafting.schema.yaml"),
    )
    .unwrap();
    std::fs::copy(
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../config/currency.schema.example.yaml"),
        config_dir.join("currency.schema.yaml"),
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
host_api_version = "0.10.0"
capabilities = ["spawning", "movement", "combat", "economy", "messaging"]
message_types = [1000]
chat_commands = ["give", "spawn-track", "which-wolf"]
hooks = [
    "on-zone-loaded",
    "on-character-create",
    "on-entity-spawn",
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
    "on-craft-complete",
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
    std::fs::copy(
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../config/party.schema.example.yaml"),
        config_dir.join("party.schema.yaml"),
    )
    .unwrap();
    std::fs::copy(
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../config/guild.schema.example.yaml"),
        config_dir.join("guild.schema.yaml"),
    )
    .unwrap();
    std::fs::copy(
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../config/character.archetypes.example.yaml"),
        config_dir.join("character.archetypes.yaml"),
    )
    .unwrap();
    std::fs::copy(
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../config/crafting.schema.example.yaml"),
        config_dir.join("crafting.schema.yaml"),
    )
    .unwrap();
    std::fs::copy(
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../config/currency.schema.example.yaml"),
        config_dir.join("currency.schema.yaml"),
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
host_api_version = "0.10.0"
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
host_api_version = "0.10.0"
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
    std::fs::copy(
        repo_config_dir.join("party.schema.example.yaml"),
        config_dir.join("party.schema.yaml"),
    )
    .unwrap();
    std::fs::copy(
        repo_config_dir.join("guild.schema.example.yaml"),
        config_dir.join("guild.schema.yaml"),
    )
    .unwrap();
    std::fs::copy(
        repo_config_dir.join("character.archetypes.example.yaml"),
        config_dir.join("character.archetypes.yaml"),
    )
    .unwrap();
    std::fs::copy(
        repo_config_dir.join("crafting.schema.example.yaml"),
        config_dir.join("crafting.schema.yaml"),
    )
    .unwrap();
    std::fs::copy(
        repo_config_dir.join("currency.schema.example.yaml"),
        config_dir.join("currency.schema.yaml"),
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
            seq: 1,
        },
    )
    .await;

    // Drain messages until we see our own Moved confirmation.
    loop {
        match recv_world(&mut stream).await {
            ServerMessage::Moved {
                entity_id,
                x,
                y,
                seq,
                ..
            } if entity_id == own_entity_id => {
                assert_eq!((x, y), MOVE_TO);
                assert_eq!(seq, 1, "should echo back the client's own sequence number");
                break;
            }
            ServerMessage::Rejected { reason, .. } => {
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
/// `on-message` commands. And #211: every `apply-stat-delta`/
/// `grant-item`/`remove-item`/`modify-currency` call this test already
/// exercises for real now also proves the corresponding
/// `StatChanged`/`ItemChanged`/`CurrencyChanged` push actually reaches
/// the connection that owns the affected entity, with no plugin-side
/// `send-message` involved for that half of it.
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

    // #211: the on-damage-calc hook's apply-stat-delta call above lands
    // on the *target*'s own "hp" stat (100 - 3 = 97) — this is the
    // connection that should receive StatChanged automatically, no
    // plugin-side send-message required for this any more (the plugin
    // fixture above only ever sends a PluginMessage to the attacker).
    loop {
        match recv_world(&mut target).await {
            ServerMessage::StatChanged { stat_key, value } => {
                assert_eq!(stat_key, "hp");
                assert_eq!(value, 97);
                break;
            }
            ServerMessage::Moved { .. } | ServerMessage::EntitySpawned { .. } => {}
            other => panic!(
                "expected StatChanged after on-damage-calc's apply-stat-delta, got {other:?}"
            ),
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

    // #211/#57: the "give" chat command (`test-plugin`'s `on_chat_command`)
    // exercises a real, successful `grant-item` — plugin.toml declares
    // `chat_commands = ["give"]`, so this is routed to the plugin instead
    // of published as an ordinary chat message. A successful grant should
    // push ItemChanged automatically, ahead of the plugin's own
    // on-item-acquire confirmation (`ItemChanged` is applied during the
    // drain that runs right after the hook call returns; the hook's own
    // `send-message` reply for `on-item-acquire` fires after that, once
    // the drain hands the acquired grant back to the caller).
    send_chat(
        &mut attacker,
        &chat::gateway_protocol::ClientMessage::Send {
            channel_id: common::id::ChannelId::new(),
            body: "/give torch".to_string(),
        },
    )
    .await;
    loop {
        match recv_world(&mut attacker).await {
            ServerMessage::ItemChanged {
                item_type,
                quantity,
            } => {
                assert_eq!(item_type, "torch");
                assert_eq!(quantity, 1);
                break;
            }
            ServerMessage::Moved { .. } | ServerMessage::EntitySpawned { .. } => {}
            other => {
                panic!("expected ItemChanged after the give command's grant-item, got {other:?}")
            }
        }
    }
    loop {
        match recv_world(&mut attacker).await {
            ServerMessage::PluginMessage { body } => {
                assert!(body.contains("acquired torch"), "{body}");
                break;
            }
            ServerMessage::Moved { .. } | ServerMessage::EntitySpawned { .. } => {}
            other => panic!("expected the on-item-acquire confirmation, got {other:?}"),
        }
    }

    // UseItem: the core never validates ownership itself — the hook
    // fires regardless, the plugin decides what happens. This connection
    // really does own one "torch" now (the give command above), so
    // on-item-use's remove-item call actually succeeds this time.
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
    // #211: on-item-use's remove-item (torch: 1 -> 0) and modify-currency
    // (+5) calls above both actually land — this connection should
    // receive ItemChanged and CurrencyChanged automatically, no
    // plugin-side send-message needed for either any more.
    loop {
        match recv_world(&mut attacker).await {
            ServerMessage::ItemChanged {
                item_type,
                quantity,
            } => {
                assert_eq!(item_type, "torch");
                assert_eq!(quantity, 0);
                break;
            }
            ServerMessage::Moved { .. } | ServerMessage::EntitySpawned { .. } => {}
            other => panic!("expected ItemChanged after on-item-use's remove-item, got {other:?}"),
        }
    }
    loop {
        match recv_world(&mut attacker).await {
            ServerMessage::CurrencyChanged {
                currency_key,
                balance,
            } => {
                assert_eq!(currency_key, "gold");
                assert_eq!(balance, 5);
                break;
            }
            ServerMessage::Moved { .. } | ServerMessage::EntitySpawned { .. } => {}
            other => panic!(
                "expected CurrencyChanged after on-item-use's modify-currency, got {other:?}"
            ),
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
    send_world(
        &mut stream,
        &ClientMessage::Move {
            x: 505.0,
            y: 250.0,
            seq: 1,
        },
    )
    .await;

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
            ServerMessage::Rejected { reason, .. } => {
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
            archetype_key: String::new(),
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
            archetype_key: String::new(),
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
            seq: 1,
        },
    )
    .await;
    loop {
        match recv_world(&mut stream).await {
            ServerMessage::Moved { x, y, .. } => {
                assert_eq!((x, y), MOVE_TO);
                break;
            }
            ServerMessage::Rejected { reason, .. } => panic!("move rejected: {reason}"),
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
                archetype_key: String::new(),
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
            archetype_key: String::new(),
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
            archetype_key: String::new(),
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

/// #213/#212, end to end through the real `server` binary:
/// `ListCharacterOptions` returns the declared archetype list from
/// `config/character.archetypes.example.yaml`, and `CreateCharacter`
/// with an explicit `archetype_key` applies that archetype's starting
/// stats — observed here the same way `plugin_sets_a_starting_stat_via_on_character_create`
/// does, by reading `characters.stats` straight out of Postgres right
/// after `CharacterCreated` comes back.
#[tokio::test]
#[ignore]
async fn list_character_options_and_create_with_an_archetype() {
    let config_dir = setup_config_dir("archetype");
    let _server = start_server(&config_dir, ARCHETYPE_ADDR).await;
    wait_for_port(ARCHETYPE_ADDR).await;

    let username = format!("archetype-{}", uuid::Uuid::now_v7());
    let password = "hunter2";

    let mut stream = connect(&config_dir, ARCHETYPE_ADDR).await;
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

    // Reachable before any character exists — same pre-join phase
    // ListCharacters already is.
    send_character(&mut stream, &CharacterClientMessage::ListCharacterOptions).await;
    let archetypes = match recv_character(&mut stream).await {
        CharacterServerMessage::CharacterOptions { archetypes } => archetypes,
        other => panic!("expected CharacterOptions, got {other:?}"),
    };
    let mut keys: Vec<_> = archetypes.iter().map(|a| a.key.clone()).collect();
    keys.sort();
    assert_eq!(
        keys,
        vec![
            "mage".to_string(),
            "rogue".to_string(),
            "warrior".to_string()
        ],
        "{archetypes:?}"
    );
    let mage = archetypes.iter().find(|a| a.key == "mage").unwrap();
    assert_eq!(mage.name, "Mage");
    assert!(!mage.description.is_empty());

    // Creating with an explicit archetype_key applies that archetype's
    // preset (hp: 50, mana: 50 per config/character.archetypes.example.yaml).
    send_character(
        &mut stream,
        &CharacterClientMessage::CreateCharacter {
            name: "Elowen".to_string(),
            archetype_key: "mage".to_string(),
        },
    )
    .await;
    let mage_id = match recv_character(&mut stream).await {
        CharacterServerMessage::CharacterCreated { character_id } => character_id,
        other => panic!("expected a CharacterCreated, got {other:?}"),
    };
    assert_eq!(read_character_stat(&mage_id, "hp").await, Some(50));
    assert_eq!(read_character_stat(&mage_id, "mana").await, Some(50));

    // An empty archetype_key resolves to the first declared entry
    // (warrior: hp 100, mana 10).
    send_character(
        &mut stream,
        &CharacterClientMessage::CreateCharacter {
            name: "Bram".to_string(),
            archetype_key: String::new(),
        },
    )
    .await;
    let default_id = match recv_character(&mut stream).await {
        CharacterServerMessage::CharacterCreated { character_id } => character_id,
        other => panic!("expected a CharacterCreated, got {other:?}"),
    };
    assert_eq!(read_character_stat(&default_id, "hp").await, Some(100));
    assert_eq!(read_character_stat(&default_id, "mana").await, Some(10));

    // An unknown key is rejected with a clear error, not a panic or
    // silent fallback — and no character is created for it.
    send_character(
        &mut stream,
        &CharacterClientMessage::CreateCharacter {
            name: "Ghost".to_string(),
            archetype_key: "necromancer".to_string(),
        },
    )
    .await;
    match recv_character(&mut stream).await {
        CharacterServerMessage::Error { message } => {
            assert!(message.contains("unknown archetype"), "{message}");
        }
        other => panic!("expected an Error for an unknown archetype, got {other:?}"),
    }
    send_character(&mut stream, &CharacterClientMessage::ListCharacters).await;
    match recv_character(&mut stream).await {
        CharacterServerMessage::CharacterList { characters } => {
            assert!(
                characters.iter().all(|c| c.name != "Ghost"),
                "{characters:?}"
            );
        }
        other => panic!("expected a CharacterList, got {other:?}"),
    }
}

/// #195, end to end: a disconnected client reconnects and resumes its
/// session using only the `session_token` an earlier `Authenticated`
/// reply issued — no `Login` message sent at all — and lands back in the
/// world at the same character/position, same as a real `Login`-based
/// reconnect would.
#[tokio::test]
#[ignore]
async fn a_client_resumes_a_session_with_only_the_token_no_login() {
    let config_dir = setup_config_dir("session-resume");
    let _server = start_server(&config_dir, SESSION_RESUME_ADDR).await;
    wait_for_port(SESSION_RESUME_ADDR).await;

    let username = format!("session-resume-{}", uuid::Uuid::now_v7());
    let password = "hunter2";

    // First connection: register, capture the issued session_token, move
    // somewhere observable, then disconnect.
    let mut stream = connect(&config_dir, SESSION_RESUME_ADDR).await;
    send_auth(
        &mut stream,
        &AuthClientMessage::Register {
            username: username.clone(),
            password: password.to_string(),
        },
    )
    .await;
    let session_token = match recv_auth(&mut stream).await {
        AuthServerMessage::Authenticated { session_token, .. } => session_token,
        other => panic!("expected Authenticated, got {other:?}"),
    };
    select_realm(&mut stream, _server.realm_id).await;
    select_or_create_character(&mut stream, &username).await;
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
            seq: 1,
        },
    )
    .await;
    loop {
        match recv_world(&mut stream).await {
            ServerMessage::Moved { x, y, .. } => {
                assert_eq!((x, y), MOVE_TO);
                break;
            }
            ServerMessage::Rejected { reason, .. } => panic!("move rejected: {reason}"),
            _ => {}
        }
    }
    drop(stream);
    tokio::time::sleep(Duration::from_millis(500)).await;

    // Second connection: Resume only — no Register, no Login.
    let mut stream = connect(&config_dir, SESSION_RESUME_ADDR).await;
    send_auth(
        &mut stream,
        &AuthClientMessage::Resume {
            session_token: session_token.clone(),
        },
    )
    .await;
    match recv_auth(&mut stream).await {
        AuthServerMessage::Authenticated {
            username: resumed_username,
            session_token: resumed_token,
            ..
        } => {
            assert_eq!(resumed_username, username);
            // Same token handed back, not rotated (#195's deliberate
            // "same token, sliding expiration" choice).
            assert_eq!(resumed_token, session_token);
        }
        other => panic!("expected Authenticated, got {other:?}"),
    }
    select_realm(&mut stream, _server.realm_id).await;
    select_or_create_character(&mut stream, &username).await;
    match recv_world(&mut stream).await {
        ServerMessage::Joined { x, y, .. } => {
            assert_eq!((x, y), MOVE_TO, "should resume at the same position")
        }
        other => panic!("expected Joined, got {other:?}"),
    }
}

/// #195: an unknown/expired token produces a clear error, not a silent
/// or ambiguous failure — the client is expected to fall back to `Login`.
#[tokio::test]
#[ignore]
async fn resuming_with_an_unknown_token_is_rejected() {
    let config_dir = setup_config_dir("session-resume-invalid");
    let _server = start_server(&config_dir, SESSION_RESUME_INVALID_ADDR).await;
    wait_for_port(SESSION_RESUME_INVALID_ADDR).await;

    let mut stream = connect(&config_dir, SESSION_RESUME_INVALID_ADDR).await;
    send_auth(
        &mut stream,
        &AuthClientMessage::Resume {
            session_token: "not-a-real-token".to_string(),
        },
    )
    .await;
    match recv_auth(&mut stream).await {
        AuthServerMessage::Error { message } => {
            assert!(message.contains("invalid or has expired"), "{message}");
        }
        other => panic!("expected an Error, got {other:?}"),
    }
}

/// #196: several `Move` requests in flight, each with its own
/// client-assigned `seq`, are correlated back to the right `Moved`/
/// `Rejected` by that `seq` — not by the order responses happen to
/// arrive in. Deliberately interleaves an accepted move between two
/// others so a test that only checked arrival order would still pass
/// even with a broken correlation (responses on a single connection
/// already arrive in send order); indexing the collected outcomes by
/// `seq` into a map, rather than reading them positionally off the
/// wire, is what actually exercises the acceptance criterion.
#[tokio::test]
#[ignore]
async fn several_in_flight_moves_correlate_to_the_right_outcome_by_sequence_number() {
    let config_dir = setup_config_dir("move-correlation");
    let _server = start_server(&config_dir, MOVE_CORRELATION_ADDR).await;
    wait_for_port(MOVE_CORRELATION_ADDR).await;

    let username = format!("smoke-{}", uuid::Uuid::now_v7());
    let mut stream = connect(&config_dir, MOVE_CORRELATION_ADDR).await;
    send_auth(
        &mut stream,
        &AuthClientMessage::Register {
            username: username.clone(),
            password: "hunter2".to_string(),
        },
    )
    .await;
    assert!(matches!(
        recv_auth(&mut stream).await,
        AuthServerMessage::Authenticated { .. }
    ));
    select_realm(&mut stream, _server.realm_id).await;
    select_or_create_character(&mut stream, &username).await;

    let (own_entity_id, start) = loop {
        if let ServerMessage::Joined {
            entity_id, x, y, ..
        } = recv_world(&mut stream).await
        {
            break (entity_id, (x, y));
        }
    };

    // seq 11 and seq 13: small, well within the speed cap — accepted.
    // seq 12, sandwiched between them: a huge jump — rejected as
    // `TooFast`. If a client (or this test) trusted arrival order
    // instead of `seq`, the middle rejection would still land second —
    // masking a correlation bug. Matching by `seq` is what actually
    // proves the mapping.
    send_world(
        &mut stream,
        &ClientMessage::Move {
            x: start.0 + 0.1,
            y: start.1,
            seq: 11,
        },
    )
    .await;
    send_world(
        &mut stream,
        &ClientMessage::Move {
            x: start.0 + 400.0,
            y: start.1,
            seq: 12,
        },
    )
    .await;
    send_world(
        &mut stream,
        &ClientMessage::Move {
            x: start.0 + 0.2,
            y: start.1,
            seq: 13,
        },
    )
    .await;

    let mut outcomes = std::collections::HashMap::new();
    while outcomes.len() < 3 {
        match recv_world(&mut stream).await {
            ServerMessage::Moved {
                entity_id, x, seq, ..
            } if entity_id == own_entity_id => {
                outcomes.insert(seq, Ok(x));
            }
            // `Rejected` doesn't carry an entity id — safe to take any of
            // them here since this connection is the only source of
            // traffic in the test.
            ServerMessage::Rejected { seq, .. } => {
                outcomes.insert(seq, Err(()));
            }
            _ => {}
        }
    }

    assert_eq!(outcomes.get(&11), Some(&Ok(start.0 + 0.1)));
    assert_eq!(outcomes.get(&12), Some(&Err(())));
    assert_eq!(outcomes.get(&13), Some(&Ok(start.0 + 0.2)));
}

/// #196: `Ping`/`Pong` is a standalone latency probe, independent of
/// movement traffic — a client should get a `Pong` echoing its
/// `client_sent_at` plus the server's own wall-clock time, with no
/// character/movement involvement at all beyond having already joined
/// a zone.
#[tokio::test]
#[ignore]
async fn ping_gets_a_pong_with_the_echoed_timestamp_and_a_server_time() {
    let config_dir = setup_config_dir("ping-pong");
    let _server = start_server(&config_dir, PING_PONG_ADDR).await;
    wait_for_port(PING_PONG_ADDR).await;

    let username = format!("smoke-{}", uuid::Uuid::now_v7());
    let mut stream = connect(&config_dir, PING_PONG_ADDR).await;
    send_auth(
        &mut stream,
        &AuthClientMessage::Register {
            username: username.clone(),
            password: "hunter2".to_string(),
        },
    )
    .await;
    assert!(matches!(
        recv_auth(&mut stream).await,
        AuthServerMessage::Authenticated { .. }
    ));
    select_realm(&mut stream, _server.realm_id).await;
    select_or_create_character(&mut stream, &username).await;

    loop {
        if let ServerMessage::Joined { .. } = recv_world(&mut stream).await {
            break;
        }
    }

    let client_sent_at = 123_456_789_i64;
    send_world(&mut stream, &ClientMessage::Ping { client_sent_at }).await;

    loop {
        if let ServerMessage::Pong {
            client_sent_at: echoed,
            server_time,
        } = recv_world(&mut stream).await
        {
            assert_eq!(echoed, client_sent_at, "should echo the client's timestamp");
            assert!(server_time > 0, "server_time should be a real timestamp");
            break;
        }
    }
}

/// #197: an NPC entity — no character row, no `entity_characters` entry
/// — can get real, declared-schema-validated stats through the exact
/// same `apply-stat-delta` path a player target does, and that composes
/// with `on-damage-calc`/`report-death` into a real "attack it, watch it
/// die" combat loop: three `Attack`s (the fixture's `on_damage_calc`
/// tracks a 3-hit combat counter per target — see its own doc comment)
/// against the plugin-spawned wolf NPC, `on-death` firing after the
/// third confirms the whole chain actually ran end to end, not just that
/// `apply-stat-delta` stopped silently no-op'ing.
#[tokio::test]
#[ignore]
async fn attacking_an_npc_applies_real_stats_and_kills_it_at_zero() {
    let config_dir = setup_config_dir("npc-stats");
    let _server = start_server(&config_dir, NPC_STATS_ADDR).await;
    wait_for_port(NPC_STATS_ADDR).await;

    let mut attacker = connect(&config_dir, NPC_STATS_ADDR).await;
    register_and_authenticate(
        &mut attacker,
        &format!("npc-attacker-{}", uuid::Uuid::now_v7()),
        "hunter2",
        _server.realm_id,
    )
    .await;
    let npc_id = loop {
        if let ServerMessage::Joined { roster, .. } = recv_world(&mut attacker).await {
            break roster
                .iter()
                .find(|entry| entry.entity_type == "npc")
                .map(|entry| entry.entity_id.clone())
                .expect("expected the plugin-spawned NPC in the join roster");
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

    // Two hits: on-damage-calc's real, schema-validated apply-stat-delta
    // write against the NPC's stats (#197) confirmed each time via the
    // fixture's own reply — this is the part that used to silently
    // no-op for an NPC target before this ticket.
    for hit in 1..=2 {
        send_world(
            &mut attacker,
            &ClientMessage::Attack {
                target_entity_id: npc_id.clone(),
                stat_key: "hp".to_string(),
            },
        )
        .await;
        loop {
            match recv_world(&mut attacker).await {
                ServerMessage::PluginMessage { body } => {
                    assert!(body.contains(&npc_id), "hit {hit}: {body}");
                    assert!(body.contains("hp"), "hit {hit}: {body}");
                    break;
                }
                ServerMessage::Moved { .. } | ServerMessage::EntitySpawned { .. } => {}
                other => {
                    panic!("hit {hit}: expected the on-damage-calc confirmation, got {other:?}")
                }
            }
        }
    }

    // Third hit crosses the fixture's own 3-hit combat threshold — it
    // calls report-death itself (core has no notion of HP or a death
    // condition), which the host applies and fires on-death back for.
    send_world(
        &mut attacker,
        &ClientMessage::Attack {
            target_entity_id: npc_id.clone(),
            stat_key: "hp".to_string(),
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
            other => panic!("expected the fatal hit's on-damage-calc confirmation, got {other:?}"),
        }
    }

    // `on_death`'s own `send_message` targets the entity that died — the
    // NPC, which has no connection to receive it on. Read back what it
    // recorded via zone-scope plugin state instead (same "last-left"
    // pattern #155 already established), proving on-death actually fired
    // for this specific NPC, not just that report-death didn't error.
    attacker
        .send(gateway::Envelope::new(1000, b"last-death".to_vec()))
        .await
        .unwrap();
    loop {
        match recv_world(&mut attacker).await {
            ServerMessage::PluginMessage { body } => {
                assert!(body.contains(&format!("last-death: {npc_id}")), "{body}");
                break;
            }
            ServerMessage::Moved { .. } | ServerMessage::EntitySpawned { .. } => {}
            other => panic!("expected the last-death query reply, got {other:?}"),
        }
    }
}

/// Sends a `PartyInvite` from `inviter` to `invitee_entity_id`, drains
/// `invitee`'s stream until its `PartyInviteReceived` confirms delivery,
/// then answers it with `PartyInviteResponse { accept }`. Shared by
/// every #178 party test below — the one real trigger this ticket adds.
async fn invite_and_respond(
    inviter: &mut ClientStream,
    invitee: &mut ClientStream,
    invitee_entity_id: &str,
    inviter_entity_id: &str,
    accept: bool,
) {
    send_world(
        inviter,
        &ClientMessage::PartyInvite {
            target_entity_id: invitee_entity_id.to_string(),
            party_type: String::new(),
        },
    )
    .await;
    loop {
        match recv_world(invitee).await {
            ServerMessage::PartyInviteReceived { from_entity_id } => {
                assert_eq!(from_entity_id, inviter_entity_id);
                break;
            }
            ServerMessage::Moved { .. }
            | ServerMessage::EntitySpawned { .. }
            | ServerMessage::PluginMessage { .. } => {}
            other => panic!("expected PartyInviteReceived, got {other:?}"),
        }
    }
    send_world(invitee, &ClientMessage::PartyInviteResponse { accept }).await;
}

/// #178's core end-to-end proof: two connections a low population
/// threshold forces onto separate layers of the same zone (same setup as
/// `a_low_population_threshold_isolates_two_joining_connections_onto_separate_layers`
/// above — proven isolated first, same as that test) end up truly
/// co-located once the second *accepts a real party invite* from the
/// first — not a raw `JoinGroupLayer` call standing in for party
/// formation, the actual invite/accept flow #178 adds firing #142's
/// placement primitive as a side effect. Both sides get a `PartyUpdate`
/// roster; the accepter gets a `ZoneChanged` proving the live layer move;
/// the inviter observes the accepter's `EntitySpawned` arrival.
#[tokio::test]
#[ignore]
async fn accepting_a_party_invite_moves_the_accepter_onto_the_inviters_live_layer() {
    let config_dir = setup_content_pack_config_dir("party-accept-layer");
    let _server = start_server_with_env(
        &config_dir,
        GROUP_LAYER_ADDR,
        false,
        &[("WZ_LAYER_POPULATION_THRESHOLD", "1")],
    )
    .await;
    wait_for_port(GROUP_LAYER_ADDR).await;

    let mut first = connect(&config_dir, GROUP_LAYER_ADDR).await;
    register_and_authenticate(
        &mut first,
        &format!("party-accept-a-{}", uuid::Uuid::now_v7()),
        "hunter2",
        _server.realm_id,
    )
    .await;
    let first_entity_id = loop {
        if let ServerMessage::Joined {
            entity_id, roster, ..
        } = recv_world(&mut first).await
        {
            assert!(roster.is_empty(), "{roster:?}");
            break entity_id;
        }
    };

    let mut second = connect(&config_dir, GROUP_LAYER_ADDR).await;
    register_and_authenticate(
        &mut second,
        &format!("party-accept-b-{}", uuid::Uuid::now_v7()),
        "hunter2",
        _server.realm_id,
    )
    .await;
    let second_entity_id = loop {
        if let ServerMessage::Joined {
            entity_id, roster, ..
        } = recv_world(&mut second).await
        {
            // With the threshold forcing a separate layer, the second
            // connection does *not* see the first yet — confirms the two
            // really do start on different layers, same as the
            // isolation test above.
            assert!(roster.is_empty(), "{roster:?}");
            break entity_id;
        }
    };

    invite_and_respond(
        &mut first,
        &mut second,
        &second_entity_id,
        &first_entity_id,
        true,
    )
    .await;

    // second: a PartyUpdate naming first, and a ZoneChanged landing it on
    // first's layer — order between the two isn't guaranteed (different
    // delivery paths), so drain until both are seen.
    let (mut saw_party_update, mut saw_zone_changed) = (false, false);
    while !(saw_party_update && saw_zone_changed) {
        match recv_world(&mut second).await {
            ServerMessage::PartyUpdate { members } => {
                assert_eq!(members, vec![first_entity_id.clone()]);
                saw_party_update = true;
            }
            ServerMessage::ZoneChanged { roster, .. } => {
                assert!(
                    roster
                        .iter()
                        .any(|entry| entry.entity_id == first_entity_id),
                    "{roster:?}"
                );
                saw_zone_changed = true;
            }
            ServerMessage::Moved { .. } | ServerMessage::EntitySpawned { .. } => {}
            other => panic!("expected PartyUpdate/ZoneChanged, got {other:?}"),
        }
    }

    // first: its own PartyUpdate naming second, and second's EntitySpawned
    // actually arriving live — the real, observable proof the move
    // happened, not just that second's own ZoneChanged said so.
    let (mut saw_first_party_update, mut saw_second_arrive) = (false, false);
    while !(saw_first_party_update && saw_second_arrive) {
        match recv_world(&mut first).await {
            ServerMessage::PartyUpdate { members } => {
                assert_eq!(members, vec![second_entity_id.clone()]);
                saw_first_party_update = true;
            }
            ServerMessage::EntitySpawned { entity_id, .. } if entity_id == second_entity_id => {
                saw_second_arrive = true;
            }
            ServerMessage::Moved { .. } | ServerMessage::EntitySpawned { .. } => {}
            other => panic!("expected PartyUpdate/EntitySpawned, got {other:?}"),
        }
    }
}

/// #178: declining an invite notifies the inviter and forms no party at
/// all — proven by a subsequent `JoinGroupLayer` from the would-be
/// inviter against the decliner being rejected as "not partied," the
/// same rejection an unrelated stranger would get.
#[tokio::test]
#[ignore]
async fn declining_a_party_invite_notifies_the_inviter_and_forms_no_party() {
    let config_dir = setup_config_dir("party-decline");
    let _server = start_server(&config_dir, PARTY_DECLINE_ADDR).await;
    wait_for_port(PARTY_DECLINE_ADDR).await;

    let mut first = connect(&config_dir, PARTY_DECLINE_ADDR).await;
    register_and_authenticate(
        &mut first,
        &format!("party-decline-a-{}", uuid::Uuid::now_v7()),
        "hunter2",
        _server.realm_id,
    )
    .await;
    let first_entity_id = loop {
        if let ServerMessage::Joined { entity_id, .. } = recv_world(&mut first).await {
            break entity_id;
        }
    };

    let mut second = connect(&config_dir, PARTY_DECLINE_ADDR).await;
    register_and_authenticate(
        &mut second,
        &format!("party-decline-b-{}", uuid::Uuid::now_v7()),
        "hunter2",
        _server.realm_id,
    )
    .await;
    let second_entity_id = loop {
        if let ServerMessage::Joined { entity_id, .. } = recv_world(&mut second).await {
            break entity_id;
        }
    };

    invite_and_respond(
        &mut first,
        &mut second,
        &second_entity_id,
        &first_entity_id,
        false,
    )
    .await;

    loop {
        match recv_world(&mut first).await {
            ServerMessage::PartyInviteDeclined { by_entity_id } => {
                assert_eq!(by_entity_id, second_entity_id);
                break;
            }
            ServerMessage::Moved { .. }
            | ServerMessage::EntitySpawned { .. }
            | ServerMessage::PluginMessage { .. } => {}
            other => panic!("expected PartyInviteDeclined, got {other:?}"),
        }
    }

    send_world(
        &mut first,
        &ClientMessage::JoinGroupLayer {
            other_entity_id: second_entity_id.clone(),
        },
    )
    .await;
    match recv_world(&mut first).await {
        ServerMessage::Error { message } => {
            assert!(message.contains("not in a party"), "{message}");
        }
        other => panic!("expected a not-in-a-party Error, got {other:?}"),
    }
}

/// #178: leaving a two-person party dissolves it entirely — the
/// remaining member's `PartyUpdate` comes back empty, not just missing
/// the leaver.
#[tokio::test]
#[ignore]
async fn leaving_a_two_person_party_notifies_the_remaining_member_with_an_empty_roster() {
    let config_dir = setup_config_dir("party-leave");
    let _server = start_server(&config_dir, PARTY_LEAVE_ADDR).await;
    wait_for_port(PARTY_LEAVE_ADDR).await;

    let mut first = connect(&config_dir, PARTY_LEAVE_ADDR).await;
    register_and_authenticate(
        &mut first,
        &format!("party-leave-a-{}", uuid::Uuid::now_v7()),
        "hunter2",
        _server.realm_id,
    )
    .await;
    let first_entity_id = loop {
        if let ServerMessage::Joined { entity_id, .. } = recv_world(&mut first).await {
            break entity_id;
        }
    };

    let mut second = connect(&config_dir, PARTY_LEAVE_ADDR).await;
    register_and_authenticate(
        &mut second,
        &format!("party-leave-b-{}", uuid::Uuid::now_v7()),
        "hunter2",
        _server.realm_id,
    )
    .await;
    let second_entity_id = loop {
        if let ServerMessage::Joined { entity_id, .. } = recv_world(&mut second).await {
            break entity_id;
        }
    };

    invite_and_respond(
        &mut first,
        &mut second,
        &second_entity_id,
        &first_entity_id,
        true,
    )
    .await;
    // Drain both sides' formation confirmations (PartyUpdate/ZoneChanged/
    // EntitySpawned, same as the acceptance test above) before leaving.
    loop {
        if let ServerMessage::PartyUpdate { members } = recv_world(&mut second).await {
            assert_eq!(members, vec![first_entity_id.clone()]);
            break;
        }
    }
    loop {
        if let ServerMessage::PartyUpdate { members } = recv_world(&mut first).await {
            assert_eq!(members, vec![second_entity_id.clone()]);
            break;
        }
    }

    send_world(&mut second, &ClientMessage::PartyLeave {}).await;

    loop {
        if let ServerMessage::PartyUpdate { members } = recv_world(&mut first).await {
            assert!(members.is_empty(), "{members:?}");
            break;
        }
    }
    loop {
        if let ServerMessage::PartyUpdate { members } = recv_world(&mut second).await {
            assert!(members.is_empty(), "{members:?}");
            break;
        }
    }
}

/// #142's second acceptance criterion, now driven by real party
/// formation (#178) rather than a raw `JoinGroupLayer` stand-in: a
/// player *reconnecting* to a party they were already in before
/// disconnecting lands back on that party's current layer, not wherever
/// population balancing alone would put them. `second` accepts a real
/// invite from `first` (landing them together live, same mechanism as
/// the acceptance test above), disconnects, then logs back in as the
/// *same* character — with the population threshold still forcing a
/// fresh connection onto a brand-new layer, landing back on `first`'s
/// layer at all is only possible because login itself consulted real
/// party membership.
#[tokio::test]
#[ignore]
async fn reconnecting_to_a_still_live_party_lands_on_its_current_layer() {
    let config_dir = setup_content_pack_config_dir("group-layer-reconnect");
    let _server = start_server_with_env(
        &config_dir,
        GROUP_LAYER_RECONNECT_ADDR,
        false,
        &[("WZ_LAYER_POPULATION_THRESHOLD", "1")],
    )
    .await;
    wait_for_port(GROUP_LAYER_RECONNECT_ADDR).await;

    let mut first = connect(&config_dir, GROUP_LAYER_RECONNECT_ADDR).await;
    register_and_authenticate(
        &mut first,
        &format!("group-reconnect-a-{}", uuid::Uuid::now_v7()),
        "hunter2",
        _server.realm_id,
    )
    .await;
    let first_entity_id = loop {
        if let ServerMessage::Joined { entity_id, .. } = recv_world(&mut first).await {
            break entity_id;
        }
    };

    let second_username = format!("group-reconnect-b-{}", uuid::Uuid::now_v7());
    let mut second = connect(&config_dir, GROUP_LAYER_RECONNECT_ADDR).await;
    register_and_authenticate(&mut second, &second_username, "hunter2", _server.realm_id).await;
    let second_entity_id = loop {
        if let ServerMessage::Joined {
            entity_id, roster, ..
        } = recv_world(&mut second).await
        {
            // Separate layer, same as the isolation/party-accept tests
            // above — confirms the threshold really did split them
            // before the invite below brings them together.
            assert!(roster.is_empty(), "{roster:?}");
            break entity_id;
        }
    };

    invite_and_respond(
        &mut first,
        &mut second,
        &second_entity_id,
        &first_entity_id,
        true,
    )
    .await;
    // Drain both sides' formation confirmations before disconnecting
    // second, so they don't confuse the reconnect roster check below.
    let (mut saw_party_update, mut saw_zone_changed) = (false, false);
    while !(saw_party_update && saw_zone_changed) {
        match recv_world(&mut second).await {
            ServerMessage::PartyUpdate { .. } => saw_party_update = true,
            ServerMessage::ZoneChanged { .. } => saw_zone_changed = true,
            _ => {}
        }
    }
    loop {
        if let ServerMessage::PartyUpdate { .. } = recv_world(&mut first).await {
            break;
        }
    }

    drop(second);
    // Give the session task a moment to notice the disconnect and clean
    // up before reconnecting as the same character.
    tokio::time::sleep(Duration::from_millis(500)).await;

    let mut second = connect(&config_dir, GROUP_LAYER_RECONNECT_ADDR).await;
    send_auth(
        &mut second,
        &AuthClientMessage::Login {
            username: second_username.clone(),
            password: "hunter2".to_string(),
        },
    )
    .await;
    assert!(matches!(
        recv_auth(&mut second).await,
        AuthServerMessage::Authenticated { .. }
    ));
    select_realm(&mut second, _server.realm_id).await;
    // Same character name resolves to the same existing character via
    // `select_or_create_character`'s "select if it already exists"
    // branch — the same character that was partied, not a new one.
    select_or_create_character(&mut second, &second_username).await;

    loop {
        if let ServerMessage::Joined { roster, .. } = recv_world(&mut second).await {
            assert!(
                roster
                    .iter()
                    .any(|entry| entry.entity_id == first_entity_id),
                "reconnect should have landed on first's layer, not a fresh one: {roster:?}"
            );
            break;
        }
    }
}

/// Sends a `GuildInvite` from `inviter` to `invitee_entity_id`, drains
/// `invitee`'s stream until its `GuildInviteReceived` confirms delivery,
/// then answers it with `GuildInviteResponse { accept }` — the guild
/// counterpart to `invite_and_respond` (#179).
async fn guild_invite_and_respond(
    inviter: &mut ClientStream,
    invitee: &mut ClientStream,
    invitee_entity_id: &str,
    inviter_entity_id: &str,
    accept: bool,
) {
    send_world(
        inviter,
        &ClientMessage::GuildInvite {
            target_entity_id: invitee_entity_id.to_string(),
        },
    )
    .await;

    loop {
        if let ServerMessage::GuildInviteReceived { from_entity_id } = recv_world(invitee).await {
            assert_eq!(from_entity_id, inviter_entity_id);
            break;
        }
    }

    send_world(invitee, &ClientMessage::GuildInviteResponse { accept }).await;
}

/// #179's acceptance criteria: creating a guild syncs a real chat
/// channel (not two independently-drifting membership lists), accepting
/// an invite adds the new member to that channel, and leaving removes
/// them again — proven against `chat_channel_members` directly, not just
/// the wire protocol's own account of itself.
#[tokio::test]
#[ignore]
async fn guild_creation_and_invite_sync_the_chat_channel_and_leaving_removes_it() {
    let config_dir = setup_config_dir("guild-chat-sync");
    let _server = start_server(&config_dir, GUILD_CHAT_SYNC_ADDR).await;
    wait_for_port(GUILD_CHAT_SYNC_ADDR).await;

    let mut founder = connect(&config_dir, GUILD_CHAT_SYNC_ADDR).await;
    let founder_username = format!("guild-chat-founder-{}", uuid::Uuid::now_v7());
    register_and_authenticate(&mut founder, &founder_username, "hunter2", _server.realm_id).await;
    let founder_entity_id = loop {
        if let ServerMessage::Joined { entity_id, .. } = recv_world(&mut founder).await {
            break entity_id;
        }
    };

    let guild_name = format!("Chat Sync Guild {}", uuid::Uuid::now_v7());
    send_world(
        &mut founder,
        &ClientMessage::GuildCreate {
            name: guild_name.clone(),
        },
    )
    .await;
    loop {
        if let ServerMessage::GuildUpdate { members, .. } = recv_world(&mut founder).await {
            assert_eq!(members.len(), 1);
            assert_eq!(members[0].rank_key, "leader");
            break;
        }
    }

    let founder_account_id = account_id_for_username(&founder_username).await;
    let channel_id = guild_chat_channel_id(&guild_name)
        .await
        .expect("chat is enabled, so guild creation should have synced a chat channel");
    assert_eq!(
        chat_channel_member_ids(channel_id).await,
        vec![founder_account_id]
    );

    let mut member = connect(&config_dir, GUILD_CHAT_SYNC_ADDR).await;
    let member_username = format!("guild-chat-member-{}", uuid::Uuid::now_v7());
    register_and_authenticate(&mut member, &member_username, "hunter2", _server.realm_id).await;
    let member_entity_id = loop {
        if let ServerMessage::Joined { entity_id, .. } = recv_world(&mut member).await {
            break entity_id;
        }
    };

    guild_invite_and_respond(
        &mut founder,
        &mut member,
        &member_entity_id,
        &founder_entity_id,
        true,
    )
    .await;

    loop {
        if let ServerMessage::GuildUpdate { members, .. } = recv_world(&mut member).await
            && members.len() == 2
        {
            break;
        }
    }
    loop {
        if let ServerMessage::GuildUpdate { members, .. } = recv_world(&mut founder).await
            && members.len() == 2
        {
            break;
        }
    }

    let member_account_id = account_id_for_username(&member_username).await;
    let mut synced_members = chat_channel_member_ids(channel_id).await;
    synced_members.sort();
    let mut expected = vec![founder_account_id, member_account_id];
    expected.sort();
    assert_eq!(synced_members, expected);

    send_world(&mut member, &ClientMessage::GuildLeave {}).await;
    loop {
        if let ServerMessage::GuildUpdate { members, .. } = recv_world(&mut founder).await
            && members.len() == 1
        {
            break;
        }
    }

    assert_eq!(
        chat_channel_member_ids(channel_id).await,
        vec![founder_account_id]
    );
}

/// #179's other acceptance requirement: a guild is core, not something
/// that secretly needs the optional `chat` service — create/invite/
/// accept/leave must all work correctly with `WZ_SERVICE_CHAT_ENABLED=false`,
/// and no chat channel id should ever get stored on a guild created while
/// chat is disabled.
#[tokio::test]
#[ignore]
async fn guild_create_invite_and_leave_all_work_with_chat_disabled() {
    let config_dir = setup_config_dir("guild-no-chat");
    let _server = start_server_with(&config_dir, GUILD_NO_CHAT_ADDR, false).await;
    wait_for_port(GUILD_NO_CHAT_ADDR).await;

    let mut founder = connect(&config_dir, GUILD_NO_CHAT_ADDR).await;
    let founder_username = format!("guild-no-chat-founder-{}", uuid::Uuid::now_v7());
    register_and_authenticate(&mut founder, &founder_username, "hunter2", _server.realm_id).await;
    let founder_entity_id = loop {
        if let ServerMessage::Joined { entity_id, .. } = recv_world(&mut founder).await {
            break entity_id;
        }
    };

    let guild_name = format!("No Chat Guild {}", uuid::Uuid::now_v7());
    send_world(
        &mut founder,
        &ClientMessage::GuildCreate {
            name: guild_name.clone(),
        },
    )
    .await;
    loop {
        if let ServerMessage::GuildUpdate { members, .. } = recv_world(&mut founder).await {
            assert_eq!(members.len(), 1);
            break;
        }
    }

    assert!(
        guild_chat_channel_id(&guild_name).await.is_none(),
        "chat is disabled, so no chat channel should have been synced"
    );

    let mut member = connect(&config_dir, GUILD_NO_CHAT_ADDR).await;
    let member_username = format!("guild-no-chat-member-{}", uuid::Uuid::now_v7());
    register_and_authenticate(&mut member, &member_username, "hunter2", _server.realm_id).await;
    let member_entity_id = loop {
        if let ServerMessage::Joined { entity_id, .. } = recv_world(&mut member).await {
            break entity_id;
        }
    };

    guild_invite_and_respond(
        &mut founder,
        &mut member,
        &member_entity_id,
        &founder_entity_id,
        true,
    )
    .await;
    loop {
        if let ServerMessage::GuildUpdate { members, .. } = recv_world(&mut member).await
            && members.len() == 2
        {
            break;
        }
    }

    send_world(&mut member, &ClientMessage::GuildLeave {}).await;
    loop {
        if let ServerMessage::GuildUpdate { members, .. } = recv_world(&mut founder).await
            && members.len() == 1
        {
            break;
        }
    }
}

/// #216, end to end through the real `server` binary: `CraftItem` with
/// every declared input present consumes them and grants the output
/// exactly once, and the fixture plugin's `on-craft-complete` hook fires
/// for real (`crates/plugin-host/tests/fixtures/test-plugin` applies a
/// `reputation.ironclad_guild` bonus via `apply-stat-delta-for-character`,
/// the one host function reachable from a hook that carries no entity
/// id) — observed via the `StatChanged` push it triggers, which can only
/// arrive after the hook actually ran.
#[tokio::test]
#[ignore]
async fn craft_item_with_sufficient_inputs_succeeds_and_fires_the_hook() {
    let config_dir = setup_config_dir("craft-success");
    let _server = start_server(&config_dir, CRAFT_ADDR).await;
    wait_for_port(CRAFT_ADDR).await;

    let username = format!("craft-{}", uuid::Uuid::now_v7());
    let mut stream = connect(&config_dir, CRAFT_ADDR).await;
    send_auth(
        &mut stream,
        &AuthClientMessage::Register {
            username: username.clone(),
            password: "hunter2".to_string(),
        },
    )
    .await;
    assert!(matches!(
        recv_auth(&mut stream).await,
        AuthServerMessage::Authenticated { .. }
    ));
    select_realm(&mut stream, _server.realm_id).await;
    let character_id = select_or_create_character(&mut stream, &username).await;

    loop {
        if let ServerMessage::Joined { .. } = recv_world(&mut stream).await {
            break;
        }
    }
    // Drain this connection's own join greeting (#155).
    loop {
        match recv_world(&mut stream).await {
            ServerMessage::PluginMessage { .. } => break,
            ServerMessage::Moved { .. } => {}
            other => panic!("expected the join greeting, got {other:?}"),
        }
    }

    // #194: on-character-create already set reputation.ironclad_guild to
    // 25 (no declared bounds, starts at 0) — the craft's own
    // on-craft-complete bonus below should land on top of that.
    assert_eq!(
        read_character_stat(&character_id, "reputation.ironclad_guild").await,
        Some(25)
    );

    // config/crafting.schema.example.yaml's "wolf-fang-dagger" recipe:
    // 3 wolf-fang + 2 iron-ore -> 1 wolf-fang-dagger.
    for _ in 0..3 {
        give_item(&mut stream, "wolf-fang").await;
    }
    for _ in 0..2 {
        give_item(&mut stream, "iron-ore").await;
    }

    send_world(
        &mut stream,
        &ClientMessage::CraftItem {
            recipe_key: "wolf-fang-dagger".to_string(),
        },
    )
    .await;

    // Every input consumed to 0, then the output granted, in recipe
    // declaration order (character::CharacterStore::craft_item's own
    // documented return order).
    let mut item_changes = Vec::new();
    for _ in 0..3 {
        loop {
            match recv_world(&mut stream).await {
                ServerMessage::ItemChanged {
                    item_type,
                    quantity,
                } => {
                    item_changes.push((item_type, quantity));
                    break;
                }
                ServerMessage::Moved { .. } | ServerMessage::EntitySpawned { .. } => {}
                other => panic!("expected ItemChanged after CraftItem, got {other:?}"),
            }
        }
    }
    assert_eq!(
        item_changes,
        vec![
            ("wolf-fang".to_string(), 0),
            ("iron-ore".to_string(), 0),
            ("wolf-fang-dagger".to_string(), 1),
        ]
    );

    // on-craft-complete's own apply-stat-delta-for-character (+5) —
    // waiting for this StatChanged push (rather than reading Postgres
    // immediately) is what actually proves the hook ran, not just that
    // the craft itself succeeded.
    loop {
        match recv_world(&mut stream).await {
            ServerMessage::StatChanged { stat_key, value } => {
                assert_eq!(stat_key, "reputation.ironclad_guild");
                assert_eq!(value, 30);
                break;
            }
            ServerMessage::Moved { .. } | ServerMessage::EntitySpawned { .. } => {}
            other => {
                panic!("expected StatChanged after on-craft-complete's hook fired, got {other:?}")
            }
        }
    }
    assert_eq!(
        read_character_stat(&character_id, "reputation.ironclad_guild").await,
        Some(30)
    );
}

/// #216: `CraftItem` with an insufficient input is rejected with a clear
/// `Error` and consumes nothing at all — verified by reading the
/// caller's inventory straight out of Postgres after the rejected
/// attempt, same "prove nothing was touched" discipline `crafting.rs`'s
/// own unit tests already apply at the storage layer.
#[tokio::test]
#[ignore]
async fn craft_item_with_an_insufficient_input_fails_and_consumes_nothing() {
    let config_dir = setup_config_dir("craft-insufficient");
    let _server = start_server(&config_dir, CRAFT_INSUFFICIENT_ADDR).await;
    wait_for_port(CRAFT_INSUFFICIENT_ADDR).await;

    let username = format!("craft-insufficient-{}", uuid::Uuid::now_v7());
    let mut stream = connect(&config_dir, CRAFT_INSUFFICIENT_ADDR).await;
    send_auth(
        &mut stream,
        &AuthClientMessage::Register {
            username: username.clone(),
            password: "hunter2".to_string(),
        },
    )
    .await;
    assert!(matches!(
        recv_auth(&mut stream).await,
        AuthServerMessage::Authenticated { .. }
    ));
    select_realm(&mut stream, _server.realm_id).await;
    let character_id = select_or_create_character(&mut stream, &username).await;

    loop {
        if let ServerMessage::Joined { .. } = recv_world(&mut stream).await {
            break;
        }
    }
    loop {
        match recv_world(&mut stream).await {
            ServerMessage::PluginMessage { .. } => break,
            ServerMessage::Moved { .. } => {}
            other => panic!("expected the join greeting, got {other:?}"),
        }
    }

    // Enough wolf-fang (3), but no iron-ore at all — "wolf-fang-dagger"
    // needs 2 of the latter.
    for _ in 0..3 {
        give_item(&mut stream, "wolf-fang").await;
    }

    send_world(
        &mut stream,
        &ClientMessage::CraftItem {
            recipe_key: "wolf-fang-dagger".to_string(),
        },
    )
    .await;
    loop {
        match recv_world(&mut stream).await {
            ServerMessage::Error { message } => {
                assert!(message.contains("iron-ore"), "{message}");
                break;
            }
            ServerMessage::Moved { .. } | ServerMessage::EntitySpawned { .. } => {}
            other => panic!("expected an Error for the insufficient craft, got {other:?}"),
        }
    }

    // Nothing consumed, nothing granted.
    assert_eq!(read_item_quantity(&character_id, "wolf-fang").await, 3);
    assert_eq!(read_item_quantity(&character_id, "iron-ore").await, 0);
    assert_eq!(
        read_item_quantity(&character_id, "wolf-fang-dagger").await,
        0
    );
}

/// #216: `CraftItem` naming a `recipe_key` the schema never declared is
/// rejected with a clear `Error`, not a panic or a silent no-op.
#[tokio::test]
#[ignore]
async fn craft_item_with_an_unknown_recipe_key_is_rejected() {
    let config_dir = setup_config_dir("craft-unknown-recipe");
    let _server = start_server(&config_dir, CRAFT_UNKNOWN_RECIPE_ADDR).await;
    wait_for_port(CRAFT_UNKNOWN_RECIPE_ADDR).await;

    let mut stream = connect(&config_dir, CRAFT_UNKNOWN_RECIPE_ADDR).await;
    register_and_authenticate(
        &mut stream,
        &format!("craft-unknown-{}", uuid::Uuid::now_v7()),
        "hunter2",
        _server.realm_id,
    )
    .await;
    loop {
        if let ServerMessage::Joined { .. } = recv_world(&mut stream).await {
            break;
        }
    }
    loop {
        match recv_world(&mut stream).await {
            ServerMessage::PluginMessage { .. } => break,
            ServerMessage::Moved { .. } => {}
            other => panic!("expected the join greeting, got {other:?}"),
        }
    }

    send_world(
        &mut stream,
        &ClientMessage::CraftItem {
            recipe_key: "does-not-exist".to_string(),
        },
    )
    .await;
    loop {
        match recv_world(&mut stream).await {
            ServerMessage::Error { message } => {
                assert!(message.contains("does-not-exist"), "{message}");
                break;
            }
            ServerMessage::Moved { .. } | ServerMessage::EntitySpawned { .. } => {}
            other => panic!("expected an Error for the unknown recipe, got {other:?}"),
        }
    }
}

/// #214: `spawn-npc`'s host callback can't synchronously return a real
/// entity id — the real entity is only created once the caller drains
/// the request outside the sandboxed WASM call (`wit/plugin.wit`'s
/// `spawn-npc`/`on-entity-spawn` doc comments). A plugin instead
/// correlates a specific `spawn-npc` call to the real entity it caused by
/// consuming `on-entity-spawn`'s `spawn-table-id` parameter.
/// `test-plugin`'s `spawn-track`/`which-wolf` chat commands (this file's
/// `setup_config_dir`) exercise that correlation for real: two
/// back-to-back `spawn-track` calls against the same spawn table
/// (`wolf-pack-01`) must resolve to two distinct, real entity ids, each
/// one correctly attributed to the specific call that requested it — not
/// the old `spawn_table_id`-echoed-back placeholder, and not just
/// whichever spawn happened to land last.
#[tokio::test]
#[ignore]
async fn spawn_npc_correlates_to_the_real_entity_via_on_entity_spawn() {
    let config_dir = setup_config_dir("spawn-correlation");
    let _server = start_server(&config_dir, SPAWN_CORRELATION_ADDR).await;
    wait_for_port(SPAWN_CORRELATION_ADDR).await;

    let mut client = connect(&config_dir, SPAWN_CORRELATION_ADDR).await;
    register_and_authenticate(
        &mut client,
        &format!("spawn-correlation-{}", uuid::Uuid::now_v7()),
        "hunter2",
        _server.realm_id,
    )
    .await;
    loop {
        if let ServerMessage::Joined { .. } = recv_world(&mut client).await {
            break;
        }
    }
    // Drain this connection's own join greeting (#155).
    loop {
        match recv_world(&mut client).await {
            ServerMessage::PluginMessage { .. } => break,
            ServerMessage::Moved { .. } | ServerMessage::EntitySpawned { .. } => {}
            other => panic!("expected the join greeting, got {other:?}"),
        }
    }

    // Two `spawn-track` calls, back to back, both against `wolf-pack-01`
    // — same table both times, so `spawn-table-id` alone can't tell them
    // apart; `on_entity_spawn`'s fixture handler uses the per-call label
    // this command records right before its own `spawn-npc` call (see
    // `test-plugin`'s doc comments) to attribute the resulting real
    // entity to the correct call.
    for label in ["a", "b"] {
        send_chat(
            &mut client,
            &chat::gateway_protocol::ClientMessage::Send {
                channel_id: common::id::ChannelId::new(),
                body: format!("/spawn-track {label}"),
            },
        )
        .await;
        loop {
            match recv_world(&mut client).await {
                ServerMessage::PluginMessage { body } => {
                    assert!(
                        body.starts_with(&format!("spawn-track {label}: requested")),
                        "{body}"
                    );
                    break;
                }
                ServerMessage::Moved { .. } | ServerMessage::EntitySpawned { .. } => {}
                other => panic!("expected the spawn-track {label} confirmation, got {other:?}"),
            }
        }
    }

    let mut which_wolf = Vec::new();
    for label in ["a", "b"] {
        send_chat(
            &mut client,
            &chat::gateway_protocol::ClientMessage::Send {
                channel_id: common::id::ChannelId::new(),
                body: format!("/which-wolf {label}"),
            },
        )
        .await;
        let prefix = format!("which-wolf {label}: ");
        let value = loop {
            match recv_world(&mut client).await {
                ServerMessage::PluginMessage { body } => {
                    break body
                        .strip_prefix(&prefix)
                        .unwrap_or_else(|| panic!("{body:?} missing prefix {prefix:?}"))
                        .to_string();
                }
                ServerMessage::Moved { .. } | ServerMessage::EntitySpawned { .. } => {}
                other => panic!("expected the which-wolf {label} reply, got {other:?}"),
            }
        };
        which_wolf.push(value);
    }
    let entity_a = which_wolf[0].clone();
    let entity_b = which_wolf[1].clone();

    // Real, distinct, parseable entity ids for both — not "<not spawned
    // yet>" (correlation never fired) and not the same id twice
    // (correlation collapsed both calls onto whichever spawn happened
    // last) — is the actual proof the mechanism works end to end.
    assert_ne!(
        entity_a, "<not spawned yet>",
        "spawn-track a never resolved"
    );
    assert_ne!(
        entity_b, "<not spawned yet>",
        "spawn-track b never resolved"
    );
    assert_ne!(
        entity_a, entity_b,
        "spawn-track's two calls against the same spawn table must correlate to two distinct real entities"
    );
    entity_a
        .parse::<common::id::EntityId>()
        .unwrap_or_else(|_| {
            panic!("expected a real entity id for spawn-track a, got {entity_a:?}")
        });
    entity_b
        .parse::<common::id::EntityId>()
        .unwrap_or_else(|_| {
            panic!("expected a real entity id for spawn-track b, got {entity_b:?}")
        });
}

/// #225, end to end: a player-initiated `RequestTransfer` between two
/// real bound realms goes through the real `transfer::TransferExecutor`
/// — not a stub — and is reflected on this same connection immediately,
/// with no reconnect: a `ListCharacters` sent right after the
/// `TransferComplete` no longer includes the transferred character, since
/// `login_policy::list_characters`'s bound-realm branch scopes strictly
/// by `characters.realm_id`, which the transfer just changed.
#[tokio::test]
#[ignore]
async fn a_player_can_transfer_their_own_character_to_a_bound_destination_realm() {
    let config_dir = setup_config_dir("transfer");
    let source_realm_id = create_realm(realm_directory::OpenOrBound::Bound).await;
    let _server = start_server_with_env(
        &config_dir,
        TRANSFER_ADDR,
        true,
        &[("WZ_REALM_ID", source_realm_id.to_string().as_str())],
    )
    .await;
    wait_for_port(TRANSFER_ADDR).await;
    let destination_realm_id = create_realm(realm_directory::OpenOrBound::Bound).await;

    let username = format!("transfer-{}", uuid::Uuid::now_v7());
    let password = "hunter2";

    let mut stream = connect(&config_dir, TRANSFER_ADDR).await;
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

    // Created but never selected/joined — `RequestTransfer` is only
    // handled in this pre-join character phase (a transfer only makes
    // sense for a character that isn't the one this connection has
    // already joined the world with).
    send_character(
        &mut stream,
        &CharacterClientMessage::CreateCharacter {
            name: username.clone(),
            archetype_key: String::new(),
        },
    )
    .await;
    let character_id = match recv_character(&mut stream).await {
        CharacterServerMessage::CharacterCreated { character_id } => character_id,
        other => panic!("expected a CharacterCreated, got {other:?}"),
    };

    send_character(
        &mut stream,
        &CharacterClientMessage::RequestTransfer {
            character_id: character_id.clone(),
            destination_realm_id: destination_realm_id.to_string(),
        },
    )
    .await;
    match recv_character(&mut stream).await {
        CharacterServerMessage::TransferComplete {
            character_id: confirmed_id,
            realm_id,
        } => {
            assert_eq!(confirmed_id, character_id);
            assert_eq!(realm_id, destination_realm_id.to_string());
        }
        other => panic!("expected a TransferComplete, got {other:?}"),
    }

    // Immediate, same-connection reflection: this process still serves
    // `source_realm_id`, and the character no longer belongs to it.
    send_character(&mut stream, &CharacterClientMessage::ListCharacters).await;
    match recv_character(&mut stream).await {
        CharacterServerMessage::CharacterList { characters } => {
            assert!(
                characters.iter().all(|c| c.character_id != character_id),
                "transferred character {character_id} still listed on the source realm: {characters:?}"
            );
        }
        other => panic!("expected a CharacterList, got {other:?}"),
    }
}

/// #225: a rejected transfer (here, a destination that's an open realm —
/// `transfer::TransferExecutor::transfer` never defines "transfer into an
/// open pool") returns a clear `Error` naming the real reason, not a
/// generic failure, and the character stays exactly where it was — a
/// second `RequestTransfer` naming the same character is still reachable
/// on the same connection afterward (never a closed connection over a
/// rejected transfer, unlike a rejected realm selection).
#[tokio::test]
#[ignore]
async fn a_transfer_into_an_open_realm_is_rejected_with_a_clear_reason() {
    let config_dir = setup_config_dir("transfer-rejected");
    let source_realm_id = create_realm(realm_directory::OpenOrBound::Bound).await;
    let _server = start_server_with_env(
        &config_dir,
        TRANSFER_REJECTED_ADDR,
        true,
        &[("WZ_REALM_ID", source_realm_id.to_string().as_str())],
    )
    .await;
    wait_for_port(TRANSFER_REJECTED_ADDR).await;
    let open_destination_realm_id = create_realm(realm_directory::OpenOrBound::Open).await;

    let username = format!("transfer-rejected-{}", uuid::Uuid::now_v7());
    let password = "hunter2";

    let mut stream = connect(&config_dir, TRANSFER_REJECTED_ADDR).await;
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
            name: username.clone(),
            archetype_key: String::new(),
        },
    )
    .await;
    let character_id = match recv_character(&mut stream).await {
        CharacterServerMessage::CharacterCreated { character_id } => character_id,
        other => panic!("expected a CharacterCreated, got {other:?}"),
    };

    send_character(
        &mut stream,
        &CharacterClientMessage::RequestTransfer {
            character_id: character_id.clone(),
            destination_realm_id: open_destination_realm_id.to_string(),
        },
    )
    .await;
    match recv_character(&mut stream).await {
        CharacterServerMessage::Error { message } => {
            assert!(message.contains("open"), "{message}");
        }
        other => panic!("expected a transfer Error, got {other:?}"),
    }

    // The connection stays usable — the character can still be selected
    // and joined normally, proving the rejection didn't half-apply.
    send_character(
        &mut stream,
        &CharacterClientMessage::SelectCharacter {
            character_id: character_id.clone(),
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
}
