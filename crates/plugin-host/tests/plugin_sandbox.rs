//! Integration tests against the real `wasmtime` sandbox and a real
//! compiled `.wasm` component (#37/#38's acceptance criteria) — not
//! run by default (`cargo test -p plugin-host` skips these), since they
//! need `tests/fixtures/test-plugin` built for `wasm32-wasip2` first,
//! which `cargo test` doesn't do on its own. Build the three variants,
//! then run with `-- --ignored`:
//!
//! ```sh
//! cd crates/plugin-host/tests/fixtures/test-plugin
//! cargo build --target wasm32-wasip2 --release
//! cargo build --target wasm32-wasip2 --release --features panic_on_load --target-dir target/panic
//! cargo build --target wasm32-wasip2 --release --features escape_attempt --target-dir target/escape
//! cd ../../../../..
//! cargo test -p plugin-host -- --ignored
//! ```

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use plugin_host::{HostCallbacks, PluginHost, PluginManifest};

fn fixture_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/test-plugin")
}

fn manifest() -> PluginManifest {
    PluginManifest::from_toml(
        r#"
[plugin]
name = "test-plugin"
host_api_version = "0.5.0"
message_types = [1000]
"#,
    )
    .unwrap()
}

#[derive(Default, Clone)]
struct RecordingCallbacks {
    spawned: Arc<Mutex<Vec<String>>>,
    messages: Arc<Mutex<Vec<(String, String)>>>,
    stat_deltas: Arc<Mutex<Vec<(String, String, i64)>>>,
    moves: Arc<Mutex<Vec<(String, f64, f64)>>>,
    item_grants: Arc<Mutex<Vec<(String, String, i64)>>>,
    item_removals: Arc<Mutex<Vec<(String, String, i64)>>>,
    currency_deltas: Arc<Mutex<Vec<(String, i64)>>>,
    roles: Arc<Mutex<HashMap<String, Vec<String>>>>,
}

impl HostCallbacks for RecordingCallbacks {
    fn spawn_npc(&mut self, spawn_table_id: &str) -> Result<String, String> {
        self.spawned
            .lock()
            .unwrap()
            .push(spawn_table_id.to_string());
        Ok("fake-entity-id".to_string())
    }

    fn send_message(&mut self, target_entity_id: &str, body: &str) -> Result<(), String> {
        self.messages
            .lock()
            .unwrap()
            .push((target_entity_id.to_string(), body.to_string()));
        Ok(())
    }

    fn apply_stat_delta(
        &mut self,
        entity_id: &str,
        stat_key: &str,
        delta: i64,
    ) -> Result<(), String> {
        self.stat_deltas
            .lock()
            .unwrap()
            .push((entity_id.to_string(), stat_key.to_string(), delta));
        Ok(())
    }

    fn move_entity(&mut self, entity_id: &str, x: f64, y: f64) -> Result<(), String> {
        self.moves
            .lock()
            .unwrap()
            .push((entity_id.to_string(), x, y));
        Ok(())
    }

    fn grant_item(
        &mut self,
        entity_id: &str,
        item_type: &str,
        quantity: i64,
    ) -> Result<(), String> {
        self.item_grants.lock().unwrap().push((
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
    ) -> Result<(), String> {
        self.item_removals.lock().unwrap().push((
            entity_id.to_string(),
            item_type.to_string(),
            quantity,
        ));
        Ok(())
    }

    fn modify_currency(&mut self, entity_id: &str, delta: i64) -> Result<(), String> {
        self.currency_deltas
            .lock()
            .unwrap()
            .push((entity_id.to_string(), delta));
        Ok(())
    }

    fn caller_role(&mut self, entity_id: &str) -> Result<Vec<String>, String> {
        Ok(self
            .roles
            .lock()
            .unwrap()
            .get(entity_id)
            .cloned()
            .unwrap_or_default())
    }
}

#[test]
#[ignore]
fn a_well_behaved_plugin_spawns_an_npc_and_responds_to_interaction() {
    let wasm_path = fixture_dir().join("target/wasm32-wasip2/release/test_plugin.wasm");
    let callbacks = RecordingCallbacks::default();

    let host = PluginHost::new();
    let mut plugin = host
        .load(&manifest(), &wasm_path, Box::new(callbacks.clone()))
        .expect("failed to load the well-behaved test plugin");

    plugin.on_load().expect("on_load should succeed");
    assert_eq!(
        callbacks.spawned.lock().unwrap().as_slice(),
        ["wolf-pack-01"]
    );

    plugin
        .on_interact("forest-entrance", "actor-1")
        .expect("on_interact should succeed");
    {
        let messages = callbacks.messages.lock().unwrap();
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].0, "actor-1");
        assert!(messages[0].1.contains("forest-entrance"));
    }

    plugin
        .on_message(1000, "actor-1", b"hello")
        .expect("on_message should succeed");
    let messages = callbacks.messages.lock().unwrap();
    assert_eq!(messages.len(), 2);
    assert_eq!(messages[1].0, "actor-1");
    assert!(messages[1].1.contains("1000"));
    assert!(messages[1].1.contains("hello"));
}

#[test]
#[ignore]
fn a_plugin_computes_damage_ticks_a_route_and_handles_a_chat_command() {
    let wasm_path = fixture_dir().join("target/wasm32-wasip2/release/test_plugin.wasm");
    let callbacks = RecordingCallbacks::default();

    let host = PluginHost::new();
    let mut plugin = host
        .load(&manifest(), &wasm_path, Box::new(callbacks.clone()))
        .expect("failed to load the well-behaved test plugin");

    plugin
        .on_damage_calc("attacker-1", "target-1", "hp", 12)
        .expect("on_damage_calc should succeed");
    assert_eq!(
        callbacks.stat_deltas.lock().unwrap().as_slice(),
        [("target-1".to_string(), "hp".to_string(), -12)]
    );

    plugin
        .on_npc_tick(
            "npc-1",
            0.0,
            0.0,
            &[(5.0, 5.0), (10.0, 10.0)],
            true,
            2.0,
            0.05,
        )
        .expect("on_npc_tick should succeed");
    assert_eq!(
        callbacks.moves.lock().unwrap().as_slice(),
        [("npc-1".to_string(), 5.0, 5.0)]
    );

    plugin
        .on_chat_command("roll", "2d6", "actor-1")
        .expect("on_chat_command should succeed");
    let messages = callbacks.messages.lock().unwrap();
    assert_eq!(messages.len(), 1);
    assert_eq!(messages[0].0, "actor-1");
    assert!(messages[0].1.contains("roll"));
    assert!(messages[0].1.contains("2d6"));
}

#[test]
#[ignore]
fn a_plugin_grants_removes_items_and_modifies_currency() {
    let wasm_path = fixture_dir().join("target/wasm32-wasip2/release/test_plugin.wasm");
    let callbacks = RecordingCallbacks::default();

    let host = PluginHost::new();
    let mut plugin = host
        .load(&manifest(), &wasm_path, Box::new(callbacks.clone()))
        .expect("failed to load the well-behaved test plugin");

    plugin
        .on_chat_command("give", "torch", "actor-1")
        .expect("on_chat_command should succeed");
    assert_eq!(
        callbacks.item_grants.lock().unwrap().as_slice(),
        [("actor-1".to_string(), "torch".to_string(), 1)]
    );

    plugin
        .on_item_use("actor-1", "torch")
        .expect("on_item_use should succeed");
    assert_eq!(
        callbacks.item_removals.lock().unwrap().as_slice(),
        [("actor-1".to_string(), "torch".to_string(), 1)]
    );
    assert_eq!(
        callbacks.currency_deltas.lock().unwrap().as_slice(),
        [("actor-1".to_string(), 5)]
    );
}

#[test]
#[ignore]
fn a_plugin_queries_the_caller_role() {
    let wasm_path = fixture_dir().join("target/wasm32-wasip2/release/test_plugin.wasm");
    let callbacks = RecordingCallbacks::default();
    callbacks.roles.lock().unwrap().insert(
        "actor-1".to_string(),
        vec!["admin".to_string(), "dev".to_string()],
    );

    let host = PluginHost::new();
    let mut plugin = host
        .load(&manifest(), &wasm_path, Box::new(callbacks.clone()))
        .expect("failed to load the well-behaved test plugin");

    plugin
        .on_chat_command("whoami", "", "actor-1")
        .expect("on_chat_command should succeed");
    let messages = callbacks.messages.lock().unwrap();
    assert_eq!(messages.len(), 1);
    assert_eq!(messages[0].0, "actor-1");
    assert!(messages[0].1.contains("admin"));
    assert!(messages[0].1.contains("dev"));
}

#[test]
#[ignore]
fn a_plugin_acquires_an_item() {
    let wasm_path = fixture_dir().join("target/wasm32-wasip2/release/test_plugin.wasm");
    let callbacks = RecordingCallbacks::default();

    let host = PluginHost::new();
    let mut plugin = host
        .load(&manifest(), &wasm_path, Box::new(callbacks.clone()))
        .expect("failed to load the well-behaved test plugin");

    plugin
        .on_item_acquire("actor-1", "torch", 3)
        .expect("on_item_acquire should succeed");
    let messages = callbacks.messages.lock().unwrap();
    assert_eq!(messages.len(), 1);
    assert_eq!(messages[0].0, "actor-1");
    assert!(messages[0].1.contains("torch"));
    assert!(messages[0].1.contains('3'));
}

#[test]
#[ignore]
fn a_plugin_panic_does_not_crash_the_host_process() {
    let wasm_path = fixture_dir().join("target/panic/wasm32-wasip2/release/test_plugin.wasm");
    let callbacks = RecordingCallbacks::default();

    let host = PluginHost::new();
    let mut plugin = host
        .load(&manifest(), &wasm_path, Box::new(callbacks))
        .expect("failed to load the panicking test plugin");

    // The trap surfaces as an ordinary `Err` here — the fact this
    // assertion runs at all (the test process is still alive to check
    // it) is the actual proof the guest's panic didn't crash the host.
    let result = plugin.on_load();
    assert!(
        result.is_err(),
        "a guest panic should surface as an Err, not a process crash"
    );
}

#[test]
#[ignore]
fn a_plugin_cannot_read_the_filesystem_with_no_preopens_granted() {
    let wasm_path = fixture_dir().join("target/escape/wasm32-wasip2/release/test_plugin.wasm");
    let callbacks = RecordingCallbacks::default();

    let host = PluginHost::new();
    let mut plugin = host
        .load(&manifest(), &wasm_path, Box::new(callbacks))
        .expect("failed to load the escape-attempt test plugin");

    // The fixture itself panics (surfacing as an Err) only if the
    // filesystem read unexpectedly *succeeded* — so `Ok(())` here is the
    // sandbox holding, not the absence of an attempt.
    let result = plugin.on_load();
    assert!(
        result.is_ok(),
        "expected the sandboxed filesystem read to fail (not succeed): {result:?}"
    );
}
