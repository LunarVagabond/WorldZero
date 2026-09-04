//! A minimal local content-addressed asset store (#279, split out of #242
//! with the design decision recorded on that now-closed issue) — resolves
//! a manifest's `asset_ref` (`sha256:<64 hex>`, `manifest::is_valid_asset_ref`)
//! to real bytes, verifying the file's actual digest matches the reference
//! rather than trusting it.
//!
//! Deliberately minimal, per the closed issue's own scope: a flat,
//! content-addressed directory (`<config_dir>/assets/<sha256-hex>`) a
//! self-hoster drops files into by hand — no upload pipeline, no CDN, no
//! importer tooling. Generic, not navmesh-specific: any future binary
//! asset reference resolves through the same mechanism.

use std::path::{Path, PathBuf};

use common::{Error, Result};
use sha2::{Digest, Sha256};

use crate::manifest::is_valid_asset_ref;

pub struct AssetStore {
    assets_dir: PathBuf,
}

impl AssetStore {
    pub fn new(assets_dir: PathBuf) -> Self {
        Self { assets_dir }
    }

    /// `<config_dir>/assets` (`common::config::config_dir` — `WZ_CONFIG_DIR`
    /// or `./config`), same "one flat directory per dev-facing concern"
    /// convention `config/plugins/` already uses.
    pub fn from_config_dir() -> Self {
        Self::new(common::config::config_dir().join("assets"))
    }

    /// Resolves `asset_ref` to its file's bytes. Rejected with a clear,
    /// specific error for each distinct failure mode — a malformed
    /// reference, a missing file, and a hash mismatch are never
    /// conflated into one generic "not found":
    /// - `asset_ref` isn't `sha256:<64 lowercase hex>` at all
    /// - `<assets_dir>/<hash>` doesn't exist (or isn't readable)
    /// - the file exists but its real SHA-256 digest doesn't match `hash`
    ///   — the reference is never trusted at face value.
    pub fn resolve(&self, asset_ref: &str) -> Result<Vec<u8>> {
        if !is_valid_asset_ref(asset_ref) {
            return Err(Error::new(
                "content",
                format!("{asset_ref:?} is not a valid sha256:<64 hex chars> asset reference"),
            ));
        }
        // Safe to strip unconditionally — is_valid_asset_ref already
        // confirmed the "sha256:" prefix.
        let hash = asset_ref.strip_prefix("sha256:").unwrap();
        let path = self.assets_dir.join(hash);

        let bytes = std::fs::read(&path).map_err(|e| {
            Error::wrap(
                "content",
                format!("asset file for {asset_ref} not found at {}", path.display()),
                e,
            )
        })?;

        let actual = to_hex(&Sha256::digest(&bytes));
        if actual != hash {
            return Err(Error::new(
                "content",
                format!(
                    "asset file at {} does not match {asset_ref}: its real digest is sha256:{actual}",
                    path.display()
                ),
            ));
        }

        Ok(bytes)
    }

    /// The path `resolve` would read for `asset_ref` — for a caller that
    /// wants to check existence, stream the file itself, or report the
    /// path in a message, without loading the full contents into memory.
    /// Doesn't validate `asset_ref`'s shape or that the file exists.
    pub fn path_for(&self, asset_ref: &str) -> Option<PathBuf> {
        let hash = asset_ref.strip_prefix("sha256:")?;
        Some(self.assets_dir.join(hash))
    }

    pub fn assets_dir(&self) -> &Path {
        &self.assets_dir
    }
}

fn to_hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store_in(dir: &Path) -> AssetStore {
        AssetStore::new(dir.to_path_buf())
    }

    fn write_asset(dir: &Path, contents: &[u8]) -> String {
        std::fs::create_dir_all(dir).unwrap();
        let hash = to_hex(&Sha256::digest(contents));
        std::fs::write(dir.join(&hash), contents).unwrap();
        format!("sha256:{hash}")
    }

    #[test]
    fn resolves_a_real_asset_whose_digest_matches() {
        let dir = tempdir();
        let asset_ref = write_asset(dir.path(), b"navmesh bytes");
        let store = store_in(dir.path());
        assert_eq!(store.resolve(&asset_ref).unwrap(), b"navmesh bytes");
    }

    #[test]
    fn rejects_a_malformed_asset_ref() {
        let dir = tempdir();
        let store = store_in(dir.path());
        let err = store.resolve("md5:not-sha256").unwrap_err();
        assert!(err.to_string().contains("not a valid"), "{err}");
    }

    #[test]
    fn rejects_a_missing_file() {
        let dir = tempdir();
        let store = store_in(dir.path());
        let asset_ref = "sha256:9f2ac1b3e4d5c6a7b8c9d0e1f2a3b4c5d6e7f8a9b0c1d2e3f4a5b6c7d8e9f0a1";
        let err = store.resolve(asset_ref).unwrap_err();
        assert!(err.to_string().contains("not found"), "{err}");
    }

    #[test]
    fn rejects_a_file_whose_real_digest_does_not_match_the_reference() {
        let dir = tempdir();
        // Write real content under a hash that doesn't actually match it
        // — simulates a corrupted or mislabeled asset file.
        let wrong_hash = "9f2ac1b3e4d5c6a7b8c9d0e1f2a3b4c5d6e7f8a9b0c1d2e3f4a5b6c7d8e9f0a1";
        std::fs::write(dir.path().join(wrong_hash), b"different bytes").unwrap();
        let store = store_in(dir.path());
        let err = store.resolve(&format!("sha256:{wrong_hash}")).unwrap_err();
        assert!(err.to_string().contains("does not match"), "{err}");
    }

    #[test]
    fn path_for_names_the_expected_file_without_touching_disk() {
        let dir = tempdir();
        let store = store_in(dir.path());
        let asset_ref = "sha256:9f2ac1b3e4d5c6a7b8c9d0e1f2a3b4c5d6e7f8a9b0c1d2e3f4a5b6c7d8e9f0a1";
        assert_eq!(
            store.path_for(asset_ref),
            Some(
                dir.path()
                    .join("9f2ac1b3e4d5c6a7b8c9d0e1f2a3b4c5d6e7f8a9b0c1d2e3f4a5b6c7d8e9f0a1")
            )
        );
    }

    // A tiny throwaway-directory helper — same "avoid pulling in a
    // `tempfile` dependency" convention `content_pack.rs`'s own tests use.
    struct TempDir(PathBuf);
    impl TempDir {
        fn path(&self) -> &Path {
            &self.0
        }
    }
    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    fn tempdir() -> TempDir {
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);

        let unique = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "wz-asset-store-test-{}-{unique}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        TempDir(dir)
    }
}
