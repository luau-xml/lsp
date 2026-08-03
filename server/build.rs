//! Records which compiler this server was built against.
//!
//! A language server built against a different compiler version than the one
//! producing `build/` reports diagnostics the build does not, which is worse
//! than reporting none. That is invisible unless both versions are stated, so
//! the version is baked in here and reported at startup.
//!
//! The lockfile is the authority — it is what Cargo actually resolved, so it
//! cannot drift from what was linked.

use std::path::PathBuf;

fn main() {
    let root = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").expect("manifest dir"));
    let lock = root.join("..").join("Cargo.lock");

    println!("cargo:rerun-if-changed={}", lock.display());
    println!(
        "cargo:rustc-env=LUAUX_VERSION={}",
        locked_version(&lock, "luaux").unwrap_or_else(|| "unknown".to_string())
    );
}

/// Version of `name` as resolved in a Cargo lockfile.
fn locked_version(lock: &std::path::Path, name: &str) -> Option<String> {
    let text = std::fs::read_to_string(lock).ok()?;
    let mut in_package = false;

    for line in text.lines() {
        let line = line.trim();

        if line == "[[package]]" {
            in_package = false;
            continue;
        }

        if line == format!("name = \"{name}\"") {
            in_package = true;
            continue;
        }

        if in_package {
            if let Some(version) = line.strip_prefix("version = ") {
                return Some(version.trim_matches('"').to_string());
            }
        }
    }

    None
}
