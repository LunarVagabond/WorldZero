//! Compiles `proto/auth.proto` into `OUT_DIR` at build time via
//! `prost-build` (docs/specs/Networking_Spec.md, "Wire schema", #109/#123)
//! — the `.proto` file, not the generated Rust here, is the checked-in
//! source of truth. `protoc-bin-vendored` supplies a prebuilt `protoc`
//! binary so building this crate never depends on the host having one
//! installed, keeping the "clone to running world" DX bar (#44) intact.

fn main() {
    let protoc_path =
        protoc_bin_vendored::protoc_bin_path().expect("failed to locate vendored protoc binary");
    // SAFETY: build scripts run single-threaded before any other code in
    // this process gets a chance to read/write the environment.
    unsafe {
        std::env::set_var("PROTOC", protoc_path);
    }

    prost_build::compile_protos(&["proto/auth.proto"], &["proto/"])
        .expect("failed to compile proto/auth.proto");
}
