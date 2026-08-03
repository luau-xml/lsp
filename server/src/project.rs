//! `luaux.toml` discovery, and where a `.luaux` file's build output lands.
//!
//! Do not invent a virtual URI. Map `src/App.luaux` to the path the build
//! already writes, `build/App.luau`, and open *that* against luau-lsp.
//! Requires resolve, the rojo sourcemap lines up, `.luaurc` aliases apply, and
//! definition files apply — all for free, because as far as luau-lsp is
//! concerned this is the file it would have analysed anyway.

use crate::naming::Vocabulary;
use luaux::config::Config;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::SystemTime;

/// A `luaux.toml` and the roots it implies.
#[derive(Clone)]
pub struct Project {
    /// This project's own spelling of the Roblox names, shared between the
    /// clones `for_file` hands out and rebuilt only when the config is.
    pub vocabulary: Arc<Vocabulary>,
    /// Directory holding `luaux.toml`, or the file's own directory if there is
    /// none. `[build] in`/`out` are relative to it.
    pub root: PathBuf,
    pub config: Config,
    /// Absolute `[build] in`, if set.
    pub source_root: Option<PathBuf>,
    /// Absolute `[build] out`, if set.
    pub out_root: Option<PathBuf>,
    /// `luaux.toml`'s path and mtime, for noticing edits to it.
    stamp: Option<(PathBuf, Option<SystemTime>)>,
    /// The parse error, if `luaux.toml` is currently broken. Reported rather
    /// than swallowed — silently falling back to defaults would give diagnostics
    /// that disagree with the build.
    pub error: Option<String>,
}

impl Project {
    fn bare(root: PathBuf) -> Self {
        Self {
            vocabulary: Arc::default(),
            root,
            config: Config::default(),
            source_root: None,
            out_root: None,
            stamp: None,
            error: None,
        }
    }

    /// Walks up from `file` looking for `luaux.toml`, as the CLI does, so the
    /// config can sit at the project root while sources live under `src/`.
    pub fn discover(file: &Path) -> Self {
        let start = if file.is_dir() { file } else { file.parent().unwrap_or(file) };

        for directory in start.ancestors() {
            let path = directory.join("luaux.toml");
            if !path.is_file() {
                continue;
            }

            let mtime = std::fs::metadata(&path).and_then(|data| data.modified()).ok();
            let mut project = Self::bare(directory.to_path_buf());
            project.stamp = Some((path, mtime));

            match Config::load(directory) {
                Ok(config) => {
                    project.source_root =
                        config.build.input.as_ref().map(|input| directory.join(input));
                    project.out_root =
                        config.build.output.as_ref().map(|output| directory.join(output));
                    project.config = config;
                }
                Err(error) => project.error = Some(error.message),
            }

            return project;
        }

        Self::bare(start.to_path_buf())
    }

    /// Whether the `luaux.toml` behind this project has changed on disk.
    pub fn is_stale(&self) -> bool {
        match &self.stamp {
            Some((path, mtime)) => {
                let now = std::fs::metadata(path).and_then(|data| data.modified()).ok();
                now != *mtime
            }
            // No config was found; one may have appeared since.
            None => self.root.join("luaux.toml").is_file(),
        }
    }

    /// Where `luaux build` writes the output for `source`.
    ///
    /// Mirrors the CLI's own rule: with no `[build] out` the output lands beside
    /// its input, and with one it mirrors the tree under `[build] in`.
    pub fn build_path(&self, source: &Path) -> PathBuf {
        let Some(out_root) = &self.out_root else {
            return source.with_extension("luau");
        };

        let source_root = self.source_root.as_deref().unwrap_or(&self.root);

        // A file outside the source root is not part of this build at all, so
        // there is no output path to point at. Beside itself is the honest
        // answer, and it matches what `luaux build <that file>` would do.
        let Ok(relative) = source.strip_prefix(source_root) else {
            return source.with_extension("luau");
        };

        out_root.join(relative).with_extension("luau")
    }
}

/// Projects, cached by root, reloaded when their `luaux.toml` changes.
#[derive(Default)]
pub struct Projects {
    by_root: HashMap<PathBuf, Project>,
}

impl Projects {
    /// The project owning `file`, loading or reloading it as needed.
    pub fn for_file(&mut self, file: &Path) -> Project {
        let start = if file.is_dir() { file } else { file.parent().unwrap_or(file) };

        // A cached project applies to any file under its root.
        let cached = start.ancestors().find_map(|directory| self.by_root.get(directory).cloned());

        if let Some(project) = cached {
            if !project.is_stale() {
                return project;
            }
        }

        let project = Project::discover(file);
        self.by_root.insert(project.root.clone(), project.clone());
        project
    }

    /// Drops every cached project, so the next request re-reads config.
    pub fn clear(&mut self) {
        self.by_root.clear();
    }
}

/// `file://` URI → path.
///
/// Percent-decoding is not optional: a project under `My Documents` arrives as
/// `My%20Documents`, and every path built from it would otherwise miss.
pub fn uri_to_path(uri: &str) -> Option<PathBuf> {
    let rest = uri.strip_prefix("file://")?;
    // `file:///c:/…` on Windows, `file:///home/…` elsewhere.
    let rest = rest.strip_prefix('/').map(|tail| {
        if tail.len() > 1 && tail.as_bytes()[1] == b':' {
            tail.to_string()
        } else {
            format!("/{tail}")
        }
    })?;

    Some(PathBuf::from(percent_decode(&rest)))
}

/// Path → `file://` URI.
pub fn path_to_uri(path: &Path) -> String {
    let text = path.to_string_lossy();
    let mut out = String::from("file://");

    if !text.starts_with('/') {
        out.push('/');
    }

    for byte in text.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' | b'/' | b':' => {
                out.push(byte as char)
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }

    out
}

fn percent_decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut at = 0;

    while at < bytes.len() {
        if bytes[at] == b'%' && at + 2 < bytes.len() {
            let hex = std::str::from_utf8(&bytes[at + 1..at + 3]).ok();
            if let Some(byte) = hex.and_then(|hex| u8::from_str_radix(hex, 16).ok()) {
                out.push(byte);
                at += 3;
                continue;
            }
        }

        out.push(bytes[at]);
        at += 1;
    }

    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn output_lands_beside_the_input_without_a_build_out() {
        let project = Project::bare(PathBuf::from("/p"));
        assert_eq!(
            project.build_path(Path::new("/p/src/App.luaux")),
            PathBuf::from("/p/src/App.luau")
        );
    }

    #[test]
    fn output_mirrors_the_tree_under_build_in() {
        let mut project = Project::bare(PathBuf::from("/p"));
        project.source_root = Some(PathBuf::from("/p/src"));
        project.out_root = Some(PathBuf::from("/p/build"));

        assert_eq!(
            project.build_path(Path::new("/p/src/ui/App.luaux")),
            PathBuf::from("/p/build/ui/App.luau")
        );
    }

    #[test]
    fn a_file_outside_the_source_root_lands_beside_itself() {
        let mut project = Project::bare(PathBuf::from("/p"));
        project.source_root = Some(PathBuf::from("/p/src"));
        project.out_root = Some(PathBuf::from("/p/build"));

        // This file is not in the build at all, so inventing a path under
        // `build/` would claim an output that nothing ever writes.
        assert_eq!(
            project.build_path(Path::new("/elsewhere/App.luaux")),
            PathBuf::from("/elsewhere/App.luau")
        );
    }

    #[test]
    fn uris_round_trip_through_paths() {
        for path in ["/tmp/App.luaux", "/tmp/My Documents/App.luaux", "/tmp/ünïcode.luaux"] {
            let uri = path_to_uri(Path::new(path));
            assert_eq!(uri_to_path(&uri).as_deref(), Some(Path::new(path)), "{uri}");
        }
    }

    #[test]
    fn a_space_arrives_percent_encoded() {
        assert_eq!(path_to_uri(Path::new("/a b")), "file:///a%20b");
        assert_eq!(uri_to_path("file:///a%20b"), Some(PathBuf::from("/a b")));
    }

    #[test]
    fn a_non_file_uri_has_no_path() {
        assert_eq!(uri_to_path("untitled:Untitled-1"), None);
        assert_eq!(uri_to_path("https://example.com/x.luaux"), None);
    }
}
