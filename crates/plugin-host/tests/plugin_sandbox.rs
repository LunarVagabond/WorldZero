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
host_api_version = "0.2.0"
message_types = [1000]
"#,
    )
    .unwrap()
}

#[derive(Default, Clone)]
struct RecordingCallbacks {
    spawned: Arc<Mutex<Vec<String>>>,
    messages: Arc<Mutex<Vec<(String, String)>>>,
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
