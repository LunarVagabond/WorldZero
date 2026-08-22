//! `content-pack.yaml` loading — bundles many zones for a game, versioned
//! as a unit (docs/specs/Content_Manifest_Spec.md, "content-pack.yaml").

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use common::{Error, Result};
use serde::Deserialize;

use crate::manifest::{SUPPORTED_SCHEMA_VERSION, ZoneManifest};

#[derive(Debug, Clone, Deserialize)]
struct ZoneEntry {
    id: String,
    path: PathBuf,
}

#[derive(Debug, Clone, Deserialize)]
struct RawContentPack {
    schema_version: u32,
    id: String,
    display_name: String,
    version: String,
    zones: Vec<ZoneEntry>,
}

#[derive(Debug, Clone)]
pub struct ContentPack {
    pub id: String,
    pub display_name: String,
    pub version: String,
    pub zones: Vec<ZoneManifest>,
}

impl ContentPack {
    /// Loads a `content-pack.yaml` and every zone manifest it references,
    /// resolving `zones[].path` relative to `path`'s own directory.
    pub fn from_file(path: &Path) -> Result<Self> {
        let contents = std::fs::read_to_string(path)
            .map_err(|e| Error::wrap("content", format!("failed to read {}", path.display()), e))?;

        let raw: RawContentPack = serde_yaml::from_str(&contents)
            .map_err(|e| Error::wrap("content", "failed to parse content pack", e))?;

        if raw.schema_version != SUPPORTED_SCHEMA_VERSION {
            return Err(Error::new(
                "content",
                format!(
                    "schema_version: unsupported version {} (this build understands {SUPPORTED_SCHEMA_VERSION})",
                    raw.schema_version
                ),
            ));
        }

        let base_dir = path.parent().unwrap_or_else(|| Path::new("."));
        let mut zones = Vec::with_capacity(raw.zones.len());
        let mut problems = Vec::new();

        for entry in &raw.zones {
            match ZoneManifest::from_file(&base_dir.join(&entry.path)) {
                Ok(manifest) if manifest.id != entry.id => {
                    problems.push(format!(
                        "zones: entry id {:?} does not match {}'s own id {:?}",
                        entry.id,
                        entry.path.display(),
                        manifest.id
                    ));
                }
                Ok(manifest) => zones.push(manifest),
                Err(e) => problems.push(format!("{}: {e}", entry.path.display())),
            }
        }

        let zone_ids: HashSet<&str> = zones.iter().map(|z| z.id.as_str()).collect();
        for zone in &zones {
            for link in &zone.links {
                if !zone_ids.contains(link.target_zone.as_str()) {
                    problems.push(format!(
                        "{}: links[].target_zone {:?} does not match any zone in this pack",
                        zone.id, link.target_zone
                    ));
                }
            }
        }

        if !problems.is_empty() {
            return Err(Error::new("content", problems.join("; ")));
        }

        Ok(Self {
            id: raw.id,
            display_name: raw.display_name,
            version: raw.version,
            zones,
        })
    }

    /// Reads `content-pack.yaml` from the dev's config directory
    /// (`common::config::config_dir` — `WZ_CONFIG_DIR` or `./config`).
    pub fn from_config_dir() -> Result<Self> {
        Self::from_file(&common::config::config_dir().join("content-pack.yaml"))
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;

    fn write_pack(dir: &Path, pack_yaml: &str, zones: &[(&str, &str)]) {
        fs::write(dir.join("content-pack.yaml"), pack_yaml).unwrap();
        fs::create_dir_all(dir.join("zones")).unwrap();
        for (filename, contents) in zones {
            fs::write(dir.join("zones").join(filename), contents).unwrap();
        }
    }

    fn zone_yaml(id: &str, links: &str) -> String {
        format!(
            r#"
schema_version: 1
id: {id}
display_name: "{id}"
bounds:
  shape: polygon
  coordinate_system: {{ units: meters, origin: [0, 0] }}
  points: [[0,0], [10,0], [10,10]]
collision:
  asset_ref: "sha256:9f2ac1b3e4d5c6a7b8c9d0e1f2a3b4c5d6e7f8a9b0c1d2e3f4a5b6c7d8e9f0a1"
  format: navmesh_v1
{links}
"#
        )
    }

    #[test]
    fn loads_a_pack_with_two_linked_zones() {
        let dir = tempdir();
        write_pack(
            dir.path(),
            r#"
schema_version: 1
id: my-game
display_name: "My Game"
version: "0.1.0"
zones:
  - id: greenwood-forest
    path: zones/greenwood-forest.yaml
  - id: stonebridge-village
    path: zones/stonebridge-village.yaml
"#,
            &[
                (
                    "greenwood-forest.yaml",
                    &zone_yaml(
                        "greenwood-forest",
                        "links:\n  - target_zone: stonebridge-village\n    edge: [[0,0],[1,1]]\n    bidirectional: true",
                    ),
                ),
                (
                    "stonebridge-village.yaml",
                    &zone_yaml("stonebridge-village", ""),
                ),
            ],
        );

        let pack = ContentPack::from_file(&dir.path().join("content-pack.yaml")).unwrap();
        assert_eq!(pack.zones.len(), 2);
    }

    #[test]
    fn dangling_link_target_fails_pack_validation() {
        let dir = tempdir();
        write_pack(
            dir.path(),
            r#"
schema_version: 1
id: my-game
display_name: "My Game"
version: "0.1.0"
zones:
  - id: greenwood-forest
    path: zones/greenwood-forest.yaml
"#,
            &[(
                "greenwood-forest.yaml",
                &zone_yaml(
                    "greenwood-forest",
                    "links:\n  - target_zone: nowhere\n    edge: [[0,0],[1,1]]\n    bidirectional: true",
                ),
            )],
        );

        let err = ContentPack::from_file(&dir.path().join("content-pack.yaml")).unwrap_err();
        assert!(
            err.to_string()
                .contains("does not match any zone in this pack"),
            "{err}"
        );
    }

    #[test]
    fn mismatched_zone_id_is_rejected() {
        let dir = tempdir();
        write_pack(
            dir.path(),
            r#"
schema_version: 1
id: my-game
display_name: "My Game"
version: "0.1.0"
zones:
  - id: wrong-id
    path: zones/greenwood-forest.yaml
"#,
            &[("greenwood-forest.yaml", &zone_yaml("greenwood-forest", ""))],
        );

        let err = ContentPack::from_file(&dir.path().join("content-pack.yaml")).unwrap_err();
        assert!(err.to_string().contains("does not match"), "{err}");
    }

    // A tiny throwaway-directory helper — avoids pulling in a `tempfile`
    // dependency for three tests.
    struct TempDir(PathBuf);
    impl TempDir {
        fn path(&self) -> &Path {
            &self.0
        }
    }
    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }
    fn tempdir() -> TempDir {
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);

        let unique = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "wz-content-pack-test-{}-{unique}",
            std::process::id()
        ));
        fs::create_dir_all(&dir).unwrap();
        TempDir(dir)
    }
}
