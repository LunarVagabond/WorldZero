//! `cargo run -p content --bin validate -- <path>` — validates a single
//! `zone.manifest.yaml` or a `content-pack.yaml` (auto-detected by
//! filename) without starting any server process
//! (docs/specs/Content_Manifest_Spec.md, "validate CLI").

use std::path::PathBuf;

use content::content_pack::ContentPack;
use content::manifest::ZoneManifest;

fn main() {
    let Some(path) = std::env::args().nth(1) else {
        eprintln!("usage: validate <path/to/zone.manifest.yaml | path/to/content-pack.yaml>");
        std::process::exit(2);
    };
    let path = PathBuf::from(path);

    let is_pack = path
        .file_name()
        .and_then(|n| n.to_str())
        .map(|n| n == "content-pack.yaml")
        .unwrap_or(false);

    let result = if is_pack {
        ContentPack::from_file(&path).map(|pack| pack.zones.len())
    } else {
        ZoneManifest::from_file(&path).map(|_| 1)
    };

    match result {
        Ok(count) => {
            println!(
                "{}: OK ({count} zone{})",
                path.display(),
                if count == 1 { "" } else { "s" }
            );
        }
        Err(e) => {
            // `Error`'s Display already prefixes with the crate and joins
            // every collected problem with "; " — split those back onto
            // their own lines here for readability.
            eprintln!("{}: FAILED", path.display());
            for problem in e.to_string().trim_start_matches("[content] ").split("; ") {
                eprintln!("  - {problem}");
            }
            std::process::exit(1);
        }
    }
}
