//! Compiles `proto/session.proto` and `proto/realm.proto` into `OUT_DIR`
//! at build time via `prost-build` (docs/specs/Networking_Spec.md, "Wire
//! schema", #109/#123) — see `crates/auth/build.rs` for why a vendored
//! `protoc` is used instead of requiring one on the host.

fn main() {
    let protoc_path =
        protoc_bin_vendored::protoc_bin_path().expect("failed to locate vendored protoc binary");
    // SAFETY: build scripts run single-threaded before any other code in
    // this process gets a chance to read/write the environment.
    unsafe {
        std::env::set_var("PROTOC", protoc_path);
    }

    prost_build::compile_protos(&["proto/session.proto", "proto/realm.proto"], &["proto/"])
        .expect("failed to compile proto/session.proto and proto/realm.proto");
}
