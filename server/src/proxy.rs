//! Spawning and supervising a stock luau-lsp.
//!
//! It is handed ordinary Luau at an ordinary path — `build/App.luau`, the file
//! the build already writes — and never learns that LuauX exists. No fork, no
//! patch, no upstream PR to wait on (decision 1).
//!
//! Its absence is not fatal. Degrading to "markup features only" is correct;
//! refusing to start is not, so every call here is fallible in the ordinary
//! sense and the server checks for `None` rather than assuming a child.

use crate::jsonrpc::{self, Message};
use serde_json::{json, Map, Value};
use std::ffi::OsString;
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::Sender;

/// Where a message came from. The server's loop is single-threaded over both, so
/// no state needs a lock.
pub enum Event {
    FromEditor(Value),
    FromChild(Value),
    /// The editor's stdin closed — time to exit.
    EditorClosed,
    /// luau-lsp went away. Everything local keeps working.
    ChildClosed,
}

pub struct Proxy {
    process: Child,
    writer: jsonrpc::Writer<ChildStdin>,
    /// Ids we hand the child. Ours, not the editor's — two clients numbering
    /// their own requests will collide otherwise.
    next_id: i64,
    pub command: PathBuf,
}

impl Proxy {
    /// Starts luau-lsp, wiring its stdout into `events`.
    pub fn spawn(command: &Path, arguments: &[String], events: Sender<Event>) -> io::Result<Self> {
        let mut process = Command::new(command)
            .arg("lsp")
            .args(arguments)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            // Inherited, so its own logging lands in the same place ours does
            // rather than filling a pipe nobody drains.
            .stderr(Stdio::inherit())
            .spawn()?;

        let stdout = process.stdout.take().expect("piped stdout");
        let stdin = process.stdin.take().expect("piped stdin");

        std::thread::spawn(move || {
            let mut reader = jsonrpc::reader(stdout);

            while let Ok(Some(message)) = reader.read() {
                if events.send(Event::FromChild(message)).is_err() {
                    return;
                }
            }

            let _ = events.send(Event::ChildClosed);
        });

        Ok(Self {
            process,
            writer: jsonrpc::Writer::new(stdin),
            next_id: 1,
            command: command.to_path_buf(),
        })
    }

    /// Sends a notification or a pre-built message.
    pub fn send(&mut self, message: &Value) -> io::Result<()> {
        self.writer.write(message)
    }

    /// Sends a request under a fresh id, which the caller records against
    /// whatever it means to do with the response.
    pub fn request(&mut self, method: &str, params: Value) -> io::Result<i64> {
        let id = self.next_id;
        self.next_id += 1;

        self.send(&jsonrpc::build::request(id.into(), method, params))?;
        Ok(id)
    }

    pub fn notify(&mut self, method: &str, params: Value) -> io::Result<()> {
        self.send(&jsonrpc::build::notification(method, params))
    }

    /// Asks it to stop, then makes sure it did.
    pub fn shutdown(&mut self) {
        let _ = self.request("shutdown", Value::Null);
        let _ = self.notify("exit", Value::Null);

        // It has been asked politely and told to exit; if it is still here it
        // is not going to leave on its own, and an orphan holding a pipe open
        // outlives the editor session.
        let _ = self.process.kill();
        let _ = self.process.wait();
    }
}

impl Drop for Proxy {
    fn drop(&mut self) {
        let _ = self.process.kill();
        let _ = self.process.wait();
    }
}

/// Finds a binary: an explicit setting, then `PATH`, then rokit's tool storage,
/// then the copy inside the luau-lsp VS Code extension.
///
/// The order is deliberate — a project that pins its own tools should get the
/// one it pinned — and the choice is reported, because a version mismatch
/// between server and compiler is otherwise invisible.
///
/// Every candidate but an explicitly configured one is **checked by running
/// it**. rokit's shim sits on `PATH` under the tool's own name and exits with an
/// error outside a project that lists the tool, so "the file is there and
/// executable" is not the same question as "this will start". Finding a binary
/// that then dies immediately looks identical to a crash, and costs a startup
/// and a confusing log to discover.
pub fn locate(name: &str, configured: Option<&str>) -> Option<PathBuf> {
    locate_on(name, configured, std::env::var_os("PATH"))
}

/// The same, searching a `PATH` given rather than read.
///
/// Separated so a test can supply a directory it controls. Whether discovery
/// works cannot honestly be asserted against whatever binaries a machine
/// happens to ship: `/bin/sh` is dash on Debian and answers `--version` with
/// "Illegal option", so the `runs` check rejects it — a fact about the runner,
/// not about this code.
fn locate_on(name: &str, configured: Option<&str>, path: Option<OsString>) -> Option<PathBuf> {
    // Named explicitly: use it, or say it is missing. Quietly running a
    // different one is the version mismatch §2.1 is about.
    if let Some(path) = configured.filter(|path| !path.trim().is_empty()) {
        let path = PathBuf::from(path);
        return path.is_file().then_some(path);
    }

    on_path(name, path)
        .into_iter()
        .chain(rokit_tool(name))
        .chain(vscode_extension(name))
        .find(|candidate| runs(candidate))
}

/// Whether the binary starts and reports a version.
fn runs(path: &Path) -> bool {
    Command::new(path)
        .arg("--version")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}

fn on_path(name: &str, path: Option<OsString>) -> Option<PathBuf> {
    let path = path?;

    std::env::split_paths(&path)
        .flat_map(|directory| filenames(name).map(move |file| directory.join(file)))
        .find(|candidate| is_executable(candidate))
}

/// What the binary may be called on this platform.
///
/// Windows carries the extension in the filename, so a `luau-lsp` on `PATH` is
/// `luau-lsp.exe` there and joining the bare name finds nothing at all.
fn filenames(name: &str) -> impl Iterator<Item = String> + '_ {
    #[cfg(windows)]
    let endings: &[&str] = &[".exe", ".cmd", ".bat", ""];
    #[cfg(not(windows))]
    let endings: &[&str] = &[""];

    endings.iter().map(move |ending| format!("{name}{ending}"))
}

/// The server bundled inside the luau-lsp VS Code extension.
///
/// A last resort, and a good one: someone editing `.luaux` almost certainly has
/// that extension, and its copy is the exact build their editor already uses for
/// `.luau`. It is installed as `bin/server` rather than under the tool's name,
/// so nothing above finds it.
fn vscode_extension(name: &str) -> Option<PathBuf> {
    if name != "luau-lsp" {
        return None;
    }

    let home = std::env::home_dir()?;
    let mut newest: Option<(String, PathBuf)> = None;

    for editor in [".vscode", ".vscode-insiders", ".vscode-oss", ".cursor", ".windsurf"] {
        let extensions = home.join(editor).join("extensions");
        let Ok(entries) = std::fs::read_dir(&extensions) else { continue };

        for entry in entries.flatten() {
            let label = entry.file_name().to_string_lossy().into_owned();
            if !label.starts_with("johnnymorganz.luau-lsp-") {
                continue;
            }

            let binary = entry.path().join("bin").join("server");
            if is_executable(&binary) && newest.as_ref().is_none_or(|(best, _)| label > *best) {
                newest = Some((label, binary));
            }
        }
    }

    newest.map(|(_, path)| path)
}

/// rokit's shim in `~/.rokit/bin` refuses to run outside a project that lists
/// the tool, so the stored binary is what actually works from here.
///
/// `home_dir` rather than `$HOME`: Windows does not set that variable, and
/// keying off it would leave this and [`vscode_extension`] returning `None`
/// there — every fallback but `PATH` silently gone on one platform.
fn rokit_tool(name: &str) -> Option<PathBuf> {
    let home = std::env::home_dir()?;
    let storage = home.join(".rokit").join("tool-storage");

    let mut newest: Option<(String, PathBuf)> = None;

    for author in std::fs::read_dir(&storage).ok()?.flatten() {
        let tool = author.path().join(name);
        let Ok(versions) = std::fs::read_dir(&tool) else { continue };

        for version in versions.flatten() {
            let binary = version.path().join(name);
            if !is_executable(&binary) {
                continue;
            }

            let label = version.file_name().to_string_lossy().into_owned();
            if newest.as_ref().is_none_or(|(best, _)| label > *best) {
                newest = Some((label, binary));
            }
        }
    }

    newest.map(|(_, path)| path)
}

fn is_executable(path: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::metadata(path)
            .map(|data| data.is_file() && data.permissions().mode() & 0o111 != 0)
            .unwrap_or(false)
    }

    #[cfg(not(unix))]
    {
        path.is_file()
    }
}

/// Command-line arguments for luau-lsp, built from the client's own
/// `luau-lsp.*` settings.
///
/// Making people configure the same definition files twice is a bug, so these
/// come from the settings the user already has for the luau-lsp extension
/// rather than from settings of ours.
pub fn arguments(settings: &Value) -> Vec<String> {
    let mut arguments = vec!["--stdio".to_string()];

    // Roblox's own API types come first, as they do from the luau-lsp
    // extension, so a user's own definitions layer on top of them.
    let (definitions, documentation) = roblox_types(settings);

    for path in definitions.into_iter().chain(strings(settings.pointer("/types/definitionFiles"))) {
        arguments.push(format!("--definitions={path}"));
    }

    for path in
        documentation.into_iter().chain(strings(settings.pointer("/types/documentationFiles")))
    {
        arguments.push(format!("--docs={path}"));
    }

    if let Some(path) = settings.pointer("/platform/baseLuaurc").and_then(Value::as_str) {
        arguments.push(format!("--base-luaurc={path}"));
    }

    // Which flags are *on* is not a command-line matter — see [`fflags`].
    if settings.pointer("/fflags/enableByDefault") == Some(&Value::Bool(false)) {
        arguments.push("--no-flags-enabled".to_string());
    }

    arguments
}

/// Flag names carry a type prefix in Roblox's own tables that luau-lsp does not
/// want: `FFlagLuauSolverV2` is `LuauSolverV2` to it.
const FLAG_PREFIXES: [&str; 4] = ["FFlag", "FInt", "DFFlag", "DFInt"];

/// The Luau FFlags luau-lsp should run with.
///
/// These do not travel on the command line. luau-lsp's own extension resolves
/// them and hands them to the server in `initializationOptions`; the binary does
/// not fetch them itself. So a proxy that forwards settings but not flags starts
/// a child with every Luau flag off, and `.luaux` then gets *worse* answers than
/// the identical generated `.luau` — which is exactly backwards, since they are
/// the same file.
///
/// Vide's `create` is the case that shows it. It is typed with `keyof<>` and
/// user-defined type functions, both of which need the new solver, so without it
/// the whole signature collapses to `*error-type*` — and an error type has no
/// members to complete and no signature to show.
///
/// `synced` is what the client fetched from Roblox; ours does that in the
/// extension, because a network call has no business on this side. Settings then
/// layer on top, in the order luau-lsp's own extension applies them: the
/// new-solver switch, then explicit overrides, which win.
pub fn fflags(settings: &Value, synced: Option<&Value>) -> Map<String, Value> {
    let mut flags = synced.and_then(Value::as_object).cloned().unwrap_or_default();

    if settings.pointer("/fflags/enableNewSolver") == Some(&Value::Bool(true)) {
        flags.insert("LuauSolverV2".to_string(), json!("true"));
    }

    for (name, value) in
        settings.pointer("/fflags/override").and_then(Value::as_object).into_iter().flatten()
    {
        // luau-lsp's own setting is documented as a map of strings, and a value
        // it cannot read is worse than one it never receives.
        let Some(value) = value.as_str().map(str::trim).filter(|value| !value.is_empty()) else {
            continue;
        };

        let name = name.trim();
        let name =
            FLAG_PREFIXES.iter().find_map(|prefix| name.strip_prefix(prefix)).unwrap_or(name);

        if !name.is_empty() {
            flags.insert(name.to_string(), json!(value));
        }
    }

    flags
}

/// The Roblox API definitions and documentation the luau-lsp extension has
/// already downloaded, as `(definitions, documentation)`.
///
/// Without these, luau-lsp has never heard of `UDim2` or `Color3`, nothing
/// constrains a component's props, and every inferred type degrades to a free
/// variable. The generated file then gets *worse* answers than the same code
/// written by hand — which is exactly backwards, since the whole point is that
/// they are the same file.
///
/// They are not in settings, so passing settings through is not enough: the
/// luau-lsp extension fetches them into its own global storage and passes them
/// on the command line. Making people configure them a second time to get the
/// same answers is a bug.
pub fn roblox_type_files(settings: &Value) -> (Option<PathBuf>, Option<PathBuf>) {
    let (definitions, docs) = roblox_types(settings);
    (definitions.first().map(PathBuf::from), docs.first().map(PathBuf::from))
}

fn roblox_types(settings: &Value) -> (Vec<String>, Vec<String>) {
    let roblox = settings.pointer("/platform/type").and_then(Value::as_str).unwrap_or("roblox")
        == "roblox"
        && settings.pointer("/types/roblox") != Some(&Value::Bool(false));

    if !roblox {
        return (Vec::new(), Vec::new());
    }

    let level = settings
        .pointer("/types/robloxSecurityLevel")
        .and_then(Value::as_str)
        .unwrap_or("PluginSecurity");

    let Some(storage) = luau_lsp_storage() else {
        return (Vec::new(), Vec::new());
    };

    // The exact security level if it is there, and any of them if it is not —
    // stale globals beat none, and luau-lsp says so itself on startup.
    let definitions = [format!("globalTypes.{level}.d.luau")]
        .into_iter()
        .chain(
            ["PluginSecurity", "None", "LocalUserSecurity", "RobloxScriptSecurity"]
                .map(|other| format!("globalTypes.{other}.d.luau")),
        )
        .map(|name| storage.join(name))
        .find(|path| path.is_file())
        .map(|path| vec![path.display().to_string()])
        .unwrap_or_default();

    let documentation = Some(storage.join("api-docs.json"))
        .filter(|path| path.is_file())
        .map(|path| vec![path.display().to_string()])
        .unwrap_or_default();

    (definitions, documentation)
}

/// Where the luau-lsp extension keeps what it has downloaded.
///
/// Editors put per-extension storage under `<config>/User/globalStorage/<id>`,
/// and only the `<config>` part differs between them and between platforms.
pub fn luau_lsp_storage() -> Option<PathBuf> {
    const EDITORS: &[&str] = &["Code", "Code - Insiders", "VSCodium", "Cursor", "Windsurf"];

    let home = std::env::home_dir();

    let roots: Vec<PathBuf> = [
        // Windows.
        std::env::var_os("APPDATA").map(PathBuf::from),
        // macOS.
        home.as_ref().map(|home| home.join("Library").join("Application Support")),
        // Linux.
        home.as_ref().map(|home| home.join(".config")),
    ]
    .into_iter()
    .flatten()
    .collect();

    roots
        .iter()
        .flat_map(|root| EDITORS.iter().map(move |editor| root.join(editor)))
        .map(|editor| editor.join("User").join("globalStorage").join("johnnymorganz.luau-lsp"))
        .find(|storage| storage.is_dir())
}

fn strings(value: Option<&Value>) -> Vec<String> {
    value
        .and_then(Value::as_array)
        .map(|items| items.iter().filter_map(Value::as_str).map(str::to_string).collect())
        .unwrap_or_default()
}

/// Whether a message from the child is a request that needs the editor's answer
/// rather than ours — `workspace/configuration` above all, which is how it asks
/// for the very `luau-lsp.*` settings the user already has.
pub fn needs_the_editor(message: &Message) -> bool {
    matches!(
        message.method(),
        Some(
            "workspace/configuration"
                | "workspace/applyEdit"
                | "workspace/workspaceFolders"
                | "window/showMessageRequest"
                | "window/workDoneProgress/create"
                | "client/registerCapability"
                | "client/unregisterCapability"
        )
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn settings_become_command_line_arguments() {
        let settings = json!({
            "types": {
                "definitionFiles": ["/defs/globalTypes.d.luau", "/defs/testez.d.luau"],
                "documentationFiles": ["/docs/api.json"],
            },
            "platform": { "baseLuaurc": "/p/.luaurc" },
        });

        let arguments = arguments(&settings);
        assert!(arguments.contains(&"--stdio".to_string()));
        assert!(arguments.contains(&"--definitions=/defs/globalTypes.d.luau".to_string()));
        assert!(arguments.contains(&"--definitions=/defs/testez.d.luau".to_string()));
        assert!(arguments.contains(&"--docs=/docs/api.json".to_string()));
        assert!(arguments.contains(&"--base-luaurc=/p/.luaurc".to_string()));
    }

    /// Everything but the Roblox globals, which depend on what this machine has
    /// downloaded and are covered separately.
    fn without_roblox_types(settings: Value) -> Vec<String> {
        arguments(&settings)
            .into_iter()
            .filter(|argument| !argument.contains("globalTypes.") && !argument.contains("api-docs"))
            .collect()
    }

    #[test]
    fn absent_settings_produce_a_bare_invocation() {
        assert_eq!(without_roblox_types(json!({})), ["--stdio"]);
        assert_eq!(without_roblox_types(Value::Null), ["--stdio"]);
    }

    #[test]
    fn a_standard_luau_project_gets_no_roblox_types() {
        // Someone who has said this is not a Roblox project should not have
        // `game` and `workspace` appear out of nowhere.
        assert_eq!(
            roblox_types(&json!({ "platform": { "type": "standard" } })).0,
            Vec::<String>::new()
        );
        assert_eq!(roblox_types(&json!({ "types": { "roblox": false } })).0, Vec::<String>::new());
    }

    #[test]
    fn the_configured_security_level_is_preferred() {
        // Only meaningful where the extension has actually downloaded them.
        let Some(storage) = luau_lsp_storage() else {
            eprintln!("skipped: the luau-lsp extension has downloaded no definitions here");
            return;
        };

        for level in ["None", "PluginSecurity"] {
            if !storage.join(format!("globalTypes.{level}.d.luau")).is_file() {
                continue;
            }

            let (definitions, _) =
                roblox_types(&json!({ "types": { "robloxSecurityLevel": level } }));
            assert!(
                definitions[0].ends_with(&format!("globalTypes.{level}.d.luau")),
                "{definitions:?}"
            );
        }
    }

    #[test]
    fn roblox_definitions_come_before_the_users_own() {
        let Some(_) = luau_lsp_storage() else {
            eprintln!("skipped: the luau-lsp extension has downloaded no definitions here");
            return;
        };

        let arguments = arguments(&json!({
            "types": { "definitionFiles": ["/mine/extra.d.luau"] },
        }));
        let definitions: Vec<&String> =
            arguments.iter().filter(|a| a.starts_with("--definitions=")).collect();

        // Theirs first, so a project's own definitions layer on top rather than
        // being buried under Roblox's.
        assert!(definitions.len() >= 2, "{definitions:?}");
        assert!(definitions[0].contains("globalTypes."), "{definitions:?}");
        assert!(definitions.last().unwrap().ends_with("/mine/extra.d.luau"), "{definitions:?}");
    }

    #[test]
    fn flags_being_off_by_default_is_passed_through() {
        let arguments = arguments(&json!({ "fflags": { "enableByDefault": false } }));
        assert!(arguments.contains(&"--no-flags-enabled".to_string()));
    }

    /// Without this the child runs with the new solver off, and Vide's `create`
    /// — typed with `keyof<>` and user-defined type functions — is
    /// `*error-type*` in `.luaux` while the identical generated `.luau` is fine.
    #[test]
    fn the_new_solver_setting_becomes_a_flag() {
        let flags = fflags(&json!({ "fflags": { "enableNewSolver": true } }), None);
        assert_eq!(flags.get("LuauSolverV2"), Some(&json!("true")));
    }

    #[test]
    fn synced_flags_are_kept_and_an_override_wins_over_one() {
        let synced = json!({ "LuauSolverV2": "false", "LuauAutocompleteRefactor": "true" });
        let flags =
            fflags(&json!({ "fflags": { "override": { "LuauSolverV2": "true" } } }), Some(&synced));

        // Explicit beats fetched, as it does in luau-lsp's own extension...
        assert_eq!(flags.get("LuauSolverV2"), Some(&json!("true")));
        // ...and the rest of what was fetched still travels.
        assert_eq!(flags.get("LuauAutocompleteRefactor"), Some(&json!("true")));
    }

    #[test]
    fn an_override_may_be_written_with_its_type_prefix() {
        // Both spellings are in the wild and name the same flag; luau-lsp wants
        // it without the prefix.
        for name in ["FFlagLuauSolverV2", "  LuauSolverV2  "] {
            let flags = fflags(&json!({ "fflags": { "override": { name: "true" } } }), None);
            assert_eq!(flags.get("LuauSolverV2"), Some(&json!("true")), "{name}");
        }
    }

    #[test]
    fn nothing_asked_for_means_nothing_sent() {
        assert!(fflags(&json!({}), None).is_empty());
        assert!(fflags(&Value::Null, None).is_empty());
        // The switch being present and off is not a request for the new solver.
        assert!(fflags(&json!({ "fflags": { "enableNewSolver": false } }), None).is_empty());
        // Neither is an override with nothing in it to apply.
        assert!(fflags(&json!({ "fflags": { "override": { "Luau": "" } } }), None).is_empty());
    }

    #[test]
    fn an_explicit_path_that_is_not_there_is_not_silently_replaced() {
        // Falling back to `PATH` would run a different binary than the one the
        // user named, which is exactly the version mismatch §2.1 warns about.
        assert_eq!(locate("luau-lsp", Some("/definitely/not/here")), None);
    }

    /// A stand-in on `PATH` that starts and reports a version, which is all
    /// `locate` asks of a candidate.
    fn discoverable(directory: &Path, name: &str) -> PathBuf {
        #[cfg(windows)]
        let (path, script) = (directory.join(format!("{name}.cmd")), "@echo 0.0.0\r\n".to_string());
        #[cfg(not(windows))]
        let (path, script) = (directory.join(name), "#!/bin/sh\necho 0.0.0\n".to_string());

        std::fs::write(&path, script).expect("write stand-in");

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).expect("chmod");
        }

        path
    }

    #[test]
    fn a_blank_setting_falls_back_to_discovery() {
        // An empty string in settings.json means "unset", not "this path".
        let directory = std::env::temp_dir().join(format!("luaux-locate-{}", std::process::id()));
        std::fs::create_dir_all(&directory).expect("temp directory");
        discoverable(&directory, "luaux-test-probe");

        let found = locate_on(
            "luaux-test-probe",
            Some("  "),
            Some(OsString::from(directory.display().to_string())),
        );

        assert!(found.is_some(), "discovery did not find the stand-in");
        let _ = std::fs::remove_dir_all(&directory);
    }

    /// Windows keeps the extension in the filename, so a bare join finds
    /// nothing — which would mean never locating a `luau-lsp.exe` on `PATH`.
    #[test]
    fn a_binary_is_looked_for_under_its_platform_name() {
        let names: Vec<String> = filenames("luau-lsp").collect();
        assert!(names.contains(&"luau-lsp".to_string()), "{names:?}");

        #[cfg(windows)]
        assert!(names.contains(&"luau-lsp.exe".to_string()), "{names:?}");
    }

    #[test]
    fn a_candidate_that_will_not_start_is_passed_over() {
        // `false` is executable and exits non-zero, standing in for rokit's shim
        // outside a project that lists the tool. Finding it and then watching it
        // die is indistinguishable from a crash.
        assert_eq!(locate("false", None), None);
    }

    #[test]
    fn only_luau_lsp_is_looked_for_in_editor_extensions() {
        // Our own binary is never shipped inside somebody else's extension.
        assert_eq!(vscode_extension("luaux-lsp"), None);
    }

    #[test]
    fn requests_the_editor_must_answer_are_recognised() {
        let configuration =
            Message::from_value(json!({ "id": 1, "method": "workspace/configuration" }))
                .expect("request");
        assert!(needs_the_editor(&configuration));

        let diagnostics =
            Message::from_value(json!({ "method": "textDocument/publishDiagnostics" }))
                .expect("notification");
        assert!(!needs_the_editor(&diagnostics));
    }
}
