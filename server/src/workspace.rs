//! Every `.luaux` in the project, not just the ones the editor opened.
//!
//! A `.luaux` requiring another `.luaux` was the one thing the proxy could not
//! answer, and the reason is worth writing down because it decides the whole
//! shape of this file. luau-lsp resolves a require in **two steps, from two
//! different sources**:
//!
//! - *existence* is checked on the filesystem, and
//! - *content* comes from the open document, when it holds one.
//!
//! Measured against a real luau-lsp 1.69.0, both string requires and Roblox
//! instance requires through a rojo sourcemap. A module handed over by
//! `textDocument/didOpen` and **not present on disk** is `Unknown require`; one
//! present on disk as a *zero-byte file* resolves, and is then typed entirely
//! from the text that was handed over. The tests at the bottom of this file and
//! in `tests/with_luau_lsp.rs` pin both halves.
//!
//! So two things are needed, and neither is a fork:
//!
//! 1. Something has to exist at the build path. [`Module::materialise`] writes
//!    the compiled output there — **only when nothing is there already**, so a
//!    real `luaux build` always wins and this never fights `--watch`.
//! 2. Every `.luaux` has to be compiled and handed over, not only the ones the
//!    editor happens to have open. That is what [`Workspace::scan`] is for.
//!
//! Without (2), a require of an unopened file gets the types from the last
//! build rather than from the file as it is now, which is the same class of
//! wrong answer as a stale hover — but silent.

use crate::project::Project;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

/// Directories never worth walking into. Package trees are the expensive ones —
/// `Packages/` in a Roblox project is routinely thousands of files, and none of
/// them are `.luaux` this project compiles.
const SKIPPED: &[&str] =
    &["node_modules", "Packages", "ServerPackages", "DevPackages", "target", "dist"];

/// How many `.luaux` files to take on before giving up on being exhaustive.
///
/// A bound rather than a promise: the walk happens on the message loop, and a
/// pathological tree should cost a log line rather than a server that stops
/// answering. Reaching it is reported, because silently covering nine tenths of
/// a project would look exactly like the bug this module fixes.
const LIMIT: usize = 4096;

/// One `.luaux` that is not open in the editor, compiled and handed to the
/// child so that a require of it resolves.
pub struct Module {
    /// `src/Card.luaux`.
    pub source: PathBuf,
    /// `build/Card.luau` — the path the build already writes.
    pub build: PathBuf,
    /// The generated Luau last handed over.
    pub output: String,
    /// Mtime of the source when it was last compiled, so an unchanged file is
    /// not recompiled on every watcher event.
    mtime: Option<SystemTime>,
}

impl Module {
    /// Ensures something exists at the build path, so the require resolves.
    ///
    /// **Only when nothing is there.** The content luau-lsp actually type-checks
    /// arrives over LSP, so this write exists purely to satisfy an existence
    /// check — which means overwriting would buy nothing and cost plenty: a
    /// concurrent `luaux build --watch` writes these same paths, and a language
    /// server racing the build over the project's own output is not a trade
    /// worth making.
    ///
    /// Returns whether a file was created.
    pub fn materialise(&self) -> bool {
        if self.build.exists() {
            return false;
        }

        if let Some(parent) = self.build.parent() {
            if std::fs::create_dir_all(parent).is_err() {
                return false;
            }
        }

        // Exactly what was compiled, and nothing else. A banner marking the file
        // as ours would make it differ from what `luaux build` writes to the
        // same path, and the first real build would then show a diff nobody
        // authored.
        std::fs::write(&self.build, &self.output).is_ok()
    }
}

/// The `.luaux` files behind the open ones.
#[derive(Default)]
pub struct Workspace {
    /// By source path.
    modules: HashMap<PathBuf, Module>,
    /// Whether the file count has already been complained about, so the warning
    /// is a property of the project rather than of every watcher event.
    warned: bool,
}

/// What one scan changed, for the caller to act on and log.
#[derive(Default)]
pub struct Changes {
    /// Modules to hand to the child — new, or compiled to something different.
    pub updated: Vec<PathBuf>,
    /// Modules whose `.luaux` is gone; the child should forget them.
    pub removed: Vec<PathBuf>,
    /// How many build outputs were created because nothing was there.
    pub materialised: usize,
    /// Files seen in total.
    pub seen: usize,
    /// Whether [`LIMIT`] cut the walk short.
    pub truncated: bool,
}

impl Workspace {
    pub fn get(&self, source: &Path) -> Option<&Module> {
        self.modules.get(source)
    }

    pub fn modules(&self) -> impl Iterator<Item = &Module> {
        self.modules.values()
    }

    /// Re-walks `project`, compiling what changed.
    ///
    /// `open` is the set of source paths the editor has open. Those are already
    /// synced from their live buffers by the server's own document handling, and
    /// compiling them a second time from disk would hand the child the *saved*
    /// text — undoing the freshness this module exists to provide.
    pub fn scan(&mut self, project: &Project, open: &HashSet<PathBuf>) -> Changes {
        let mut changes = Changes::default();
        let mut found = HashSet::new();

        let root = project.source_root.clone().unwrap_or_else(|| project.root.clone());
        let mut sources = Vec::new();
        collect(&root, project.out_root.as_deref(), &mut sources, &mut changes);

        changes.seen = sources.len();

        for source in sources {
            found.insert(source.clone());

            if open.contains(&source) {
                // The editor's buffer is the truth for this one, and the server
                // syncs it on every keystroke.
                continue;
            }

            let mtime = std::fs::metadata(&source).and_then(|data| data.modified()).ok();

            if let Some(existing) = self.modules.get(&source) {
                // An mtime that has not moved means the text has not either.
                // `None` is "cannot tell", which is a reason to recompile rather
                // than to assume.
                if mtime.is_some() && existing.mtime == mtime {
                    continue;
                }
            }

            let Ok(text) = std::fs::read_to_string(&source) else { continue };

            // Recovering, for the same reason [`crate::analysis`] uses it: a
            // half-written dependency should cost the broken part of its own
            // types, not every type in every file that requires it.
            //
            // A hard `Err` is a file that produced no Luau at all — an import
            // deleted, so `create` is out of scope, or a lex error. There is
            // nothing to hand over, so the previous good output is deliberately
            // left in place and still open in the child: last-known types for a
            // dependency someone is mid-edit beats none. `mtime` is left stale
            // with it, so the next scan tries again.
            let Ok(compiled) = luaux::compile::compile_recovering(
                &text,
                crate::backend(&project.config),
                project.config.clone(),
            ) else {
                continue;
            };

            let build = project.build_path(&source);
            let unchanged =
                self.modules.get(&source).is_some_and(|module| module.output == compiled.output);

            let module = Module { source: source.clone(), build, output: compiled.output, mtime };

            if module.materialise() {
                changes.materialised += 1;
            }

            // A touched file that compiles to what it already compiled to is not
            // a change the child needs told about.
            if !unchanged {
                changes.updated.push(source.clone());
            }

            self.modules.insert(source, module);
        }

        self.modules.retain(|source, _| {
            // Opened in the editor since the last scan. Dropped, but *not*
            // reported as removed: the child holds that build URI already, and
            // the server keeps it current from the live buffer. Telling it to
            // close the document would leave the file open in the editor and
            // absent from the type checker.
            if open.contains(source) {
                return false;
            }

            // Genuinely gone.
            if !found.contains(source) {
                changes.removed.push(source.clone());
                return false;
            }

            true
        });

        changes
    }

    pub fn clear(&mut self) {
        self.modules.clear();
        self.warned = false;
    }

    /// Whether the file-count warning still needs saying.
    pub fn warn_once(&mut self) -> bool {
        !std::mem::replace(&mut self.warned, true)
    }
}

/// Every `.luaux` under `directory`, skipping the build output and the usual
/// package trees.
fn collect(
    directory: &Path,
    out_root: Option<&Path>,
    found: &mut Vec<PathBuf>,
    changes: &mut Changes,
) {
    if found.len() >= LIMIT {
        changes.truncated = true;
        return;
    }

    // The build output holds generated `.luau`, and on a project without
    // `[build] out` it is the source tree itself — so this is a check against
    // descending into a directory, not a reason to skip the tree.
    if out_root.is_some_and(|out| out == directory) {
        return;
    }

    let Ok(entries) = std::fs::read_dir(directory) else { return };

    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(kind) = entry.file_type() else { continue };

        let name = entry.file_name();
        let name = name.to_string_lossy();

        if kind.is_dir() {
            // Symlinks are not followed: a link pointing at an ancestor turns
            // this walk into a loop, and there is no cheap way to notice.
            if kind.is_symlink() || name.starts_with('.') || SKIPPED.contains(&name.as_ref()) {
                continue;
            }
            collect(&path, out_root, found, changes);
            if found.len() >= LIMIT {
                changes.truncated = true;
                return;
            }
            continue;
        }

        if path.extension().is_some_and(|extension| extension == "luaux") {
            found.push(path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Temporary(PathBuf);

    impl Temporary {
        fn new(label: &str) -> Self {
            let path = std::env::temp_dir().join(format!(
                "luaux-workspace-{label}-{}-{:?}",
                std::process::id(),
                std::thread::current().id()
            ));
            let _ = std::fs::remove_dir_all(&path);
            std::fs::create_dir_all(&path).expect("temp directory");
            Self(path)
        }

        fn write(&self, relative: &str, text: &str) -> PathBuf {
            let path = self.0.join(relative);
            std::fs::create_dir_all(path.parent().expect("parent")).expect("parent");
            std::fs::write(&path, text).expect("write");
            path
        }
    }

    impl Drop for Temporary {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn project(root: &Path) -> Project {
        // `[factory]` names a backend because since luaux 0.2.0 it has to: a
        // block that configures a factory without saying which arrangement it
        // is for is a config error, not a default.
        std::fs::write(
            root.join("luaux.toml"),
            "[build]\nin = \"src\"\nout = \"build\"\n\n[factory]\nbackend = \"table\"\ncreate = \"create\"\n",
        )
        .expect("luaux.toml");
        Project::discover(&root.join("src"))
    }

    /// `create` has to be in scope or the backend refuses the whole file — the
    /// same shape every real `.luaux` has after its import.
    const CARD: &str =
        "local create = nil :: any\nlocal function Card()\n  return <Frame/>\nend\nreturn Card\n";

    #[test]
    fn a_luaux_nobody_opened_is_found_and_compiled() {
        let temporary = Temporary::new("found");
        temporary.write("src/Card.luaux", CARD);
        let project = project(&temporary.0);

        let mut workspace = Workspace::default();
        let changes = workspace.scan(&project, &HashSet::new());

        assert_eq!(changes.seen, 1, "the walk did not find src/Card.luaux");
        assert_eq!(changes.updated.len(), 1);

        let module = workspace.get(&temporary.0.join("src/Card.luaux")).expect("module");
        assert_eq!(module.build, temporary.0.join("build/Card.luau"));
        assert!(module.output.contains("Card"), "{}", module.output);
    }

    /// The existence check is the entire reason this module writes anything at
    /// all: without a file at the build path, luau-lsp answers `Unknown require`
    /// however good the text we hand it is.
    #[test]
    fn a_missing_build_output_is_created() {
        let temporary = Temporary::new("materialise");
        temporary.write("src/Card.luaux", CARD);
        let project = project(&temporary.0);

        let mut workspace = Workspace::default();
        let changes = workspace.scan(&project, &HashSet::new());

        assert_eq!(changes.materialised, 1);
        let built = temporary.0.join("build/Card.luau");
        assert!(built.is_file(), "nothing was written to {}", built.display());
        assert_eq!(
            std::fs::read_to_string(&built).expect("read"),
            workspace.get(&temporary.0.join("src/Card.luaux")).expect("module").output
        );
    }

    /// A real `luaux build` — or a `--watch` running right now — owns these
    /// paths. Racing it would be worse than doing nothing.
    #[test]
    fn an_existing_build_output_is_never_overwritten() {
        let temporary = Temporary::new("nooverwrite");
        temporary.write("src/Card.luaux", CARD);
        let existing = temporary.write("build/Card.luau", "-- written by luaux build\n");
        let project = project(&temporary.0);

        let changes = Workspace::default().scan(&project, &HashSet::new());

        assert_eq!(changes.materialised, 0);
        assert_eq!(
            std::fs::read_to_string(&existing).expect("read"),
            "-- written by luaux build\n"
        );
    }

    /// The editor's buffer is fresher than the file. Compiling from disk here
    /// would hand the child the *saved* text and undo the point of the exercise.
    #[test]
    fn a_file_open_in_the_editor_is_left_to_the_editor() {
        let temporary = Temporary::new("open");
        let source = temporary.write("src/Card.luaux", CARD);
        let project = project(&temporary.0);

        let open = HashSet::from([source.clone()]);
        let changes = Workspace::default().scan(&project, &open);

        assert_eq!(changes.seen, 1);
        assert!(changes.updated.is_empty(), "an open document was compiled from disk");
    }

    #[test]
    fn an_unchanged_file_is_not_recompiled() {
        let temporary = Temporary::new("unchanged");
        temporary.write("src/Card.luaux", CARD);
        let project = project(&temporary.0);

        let mut workspace = Workspace::default();
        assert_eq!(workspace.scan(&project, &HashSet::new()).updated.len(), 1);
        // Same mtime, same text: nothing to tell the child.
        assert!(workspace.scan(&project, &HashSet::new()).updated.is_empty());
    }

    #[test]
    fn a_deleted_luaux_is_reported_as_removed() {
        let temporary = Temporary::new("deleted");
        let source = temporary.write("src/Card.luaux", CARD);
        let project = project(&temporary.0);

        let mut workspace = Workspace::default();
        workspace.scan(&project, &HashSet::new());

        std::fs::remove_file(&source).expect("remove");
        let changes = workspace.scan(&project, &HashSet::new());

        assert_eq!(changes.removed, vec![source]);
        assert_eq!(workspace.modules().count(), 0);
    }

    /// Generated `.luau` under `build/` is not input, and a project whose output
    /// lands beside its input must not walk into its own results.
    #[test]
    fn the_build_output_is_not_walked() {
        let temporary = Temporary::new("outroot");
        temporary.write("src/Card.luaux", CARD);
        temporary.write("build/Stray.luaux", CARD);
        let project = project(&temporary.0);

        let changes = Workspace::default().scan(&project, &HashSet::new());
        assert_eq!(changes.seen, 1, "the walk descended into build/");
    }

    #[test]
    fn package_trees_are_skipped() {
        let temporary = Temporary::new("packages");
        temporary.write("src/Card.luaux", CARD);
        temporary.write("src/Packages/Vendor.luaux", CARD);
        temporary.write("src/node_modules/Vendor.luaux", CARD);
        let project = project(&temporary.0);

        let changes = Workspace::default().scan(&project, &HashSet::new());
        assert_eq!(changes.seen, 1, "a package tree was walked");
    }
}
