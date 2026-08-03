//! A test client: spawns the real binary and talks LSP to it over stdio.
//!
//! Each test binary compiles its own copy, so anything only one of them uses
//! looks unused to the others.
#![allow(dead_code)]

use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{channel, Receiver, RecvTimeoutError};
use std::time::Duration;

/// How long to wait for any one message. Generous, because a real luau-lsp
/// indexing a workspace is doing real work — but finite, so a server that stops
/// answering fails the test instead of hanging the suite.
const TIMEOUT: Duration = Duration::from_secs(20);

pub struct Server {
    process: Child,
    stdin: ChildStdin,
    incoming: Receiver<Value>,
    /// Arrived, but not what the last wait was looking for.
    held: Vec<Value>,
    next_id: i64,
    root: PathBuf,
    file: PathBuf,
    version: i64,
    /// Kept alive so the temporary project outlives the server.
    _directory: TempDirectory,
    /// Where luau-lsp is, if the test wants a real one.
    luau_lsp: Option<String>,
    /// Further `luau-lsp.*` settings the editor answers with, merged over the
    /// server path. Set before [`Server::initialized`].
    pub settings: Value,
}

impl Server {
    /// Spawned but not yet initialized.
    pub fn start() -> Self {
        Self::with_luau_lsp(None)
    }

    /// Spawned, initialized, and told there is no luau-lsp.
    pub fn started() -> Self {
        let mut server = Self::start();
        server.initialize();
        server.initialized();
        server
    }

    pub fn with_luau_lsp(luau_lsp: Option<String>) -> Self {
        let directory = TempDirectory::new();
        let root = directory.path.clone();

        std::fs::create_dir_all(root.join("src")).expect("src");
        std::fs::write(root.join("luaux.toml"), "[build]\nin = \"src\"\nout = \"build\"\n")
            .expect("luaux.toml");
        std::fs::create_dir_all(root.join("build")).expect("build");
        // Strict mode, so luau-lsp reports the type errors these tests are about
        // rather than shrugging at them.
        std::fs::write(root.join(".luaurc"), "{\"languageMode\": \"strict\"}\n").expect(".luaurc");

        let mut process = Command::new(env!("CARGO_BIN_EXE_luaux-lsp"))
            .arg("--stdio")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn luaux-lsp");

        let stdin = process.stdin.take().expect("stdin");
        let stdout = process.stdout.take().expect("stdout");

        let (sender, incoming) = channel();
        std::thread::spawn(move || {
            let mut stdout = BufReader::new(stdout);

            while let Some(message) = read(&mut stdout) {
                if sender.send(message).is_err() {
                    return;
                }
            }
        });

        Self {
            process,
            stdin,
            incoming,
            held: Vec::new(),
            next_id: 1,
            file: root.join("src").join("App.luaux"),
            root,
            version: 1,
            _directory: directory,
            luau_lsp,
            settings: json!({}),
        }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn uri(&self) -> String {
        format!("file://{}", self.file.display())
    }

    pub fn build_uri(&self) -> String {
        format!("file://{}", self.root.join("build").join("App.luau").display())
    }

    /// A `{ textDocument, position }` for this document.
    pub fn at(&self, line: u64, character: u64) -> Value {
        json!({
            "textDocument": { "uri": self.uri() },
            "position": { "line": line, "character": character },
        })
    }

    // --- protocol ----------------------------------------------------------

    pub fn initialize(&mut self) -> Value {
        self.request(
            "initialize",
            json!({
                "processId": std::process::id(),
                "rootUri": format!("file://{}", self.root.display()),
                "workspaceFolders": [{
                    "uri": format!("file://{}", self.root.display()),
                    "name": "test",
                }],
                // What VS Code actually advertises. Pull diagnostics above
                // all: the child chooses push or pull from these, so a harness
                // that omits them tests a client no user has — and every
                // diagnostic test passes while the feature is dead in the editor.
                "capabilities": {
                    "textDocument": {
                        "diagnostic": { "dynamicRegistration": true, "relatedDocumentSupport": true },
                        "completion": { "completionItem": { "snippetSupport": true } },
                    },
                    "workspace": { "diagnostics": { "refreshSupport": true } },
                },
            }),
        )
    }

    /// Sends `initialized` and answers the configuration request that follows.
    pub fn initialized(&mut self) {
        self.notify("initialized", json!({}));
        self.answer_configuration();
    }

    /// Answers `workspace/configuration`, which is what actually starts luau-lsp.
    ///
    /// Split out from [`Server::initialized`] so a test can open a document
    /// *before* it is answered — the ordering a window reloaded with a `.luaux`
    /// already open produces, where the document arrives before the child exists.
    pub fn answer_configuration(&mut self) {
        let request = self
            .wait_for(|message| message.get("method") == Some(&json!("workspace/configuration")));

        let mut settings = match &self.luau_lsp {
            Some(path) => json!({ "server": { "path": path } }),
            // A path that is not there, so discovery cannot quietly find a real
            // one and make the test depend on the machine.
            None => json!({ "server": { "path": "/nonexistent/luau-lsp" } }),
        };

        for (key, value) in self.settings.as_object().into_iter().flatten() {
            settings[key.as_str()] = value.clone();
        }

        let id = request["id"].clone();
        self.send(&json!({ "jsonrpc": "2.0", "id": id, "result": [settings] }));
    }

    pub fn open(&mut self, text: &str) {
        std::fs::write(&self.file, text).expect("write source");

        self.notify(
            "textDocument/didOpen",
            json!({
                "textDocument": {
                    "uri": self.uri(),
                    "languageId": "luaux",
                    "version": self.version,
                    "text": text,
                },
            }),
        );
    }

    pub fn change(&mut self, text: &str) {
        self.version += 1;

        self.notify(
            "textDocument/didChange",
            json!({
                "textDocument": { "uri": self.uri(), "version": self.version },
                "contentChanges": [{ "text": text }],
            }),
        );
    }

    /// The next `publishDiagnostics` for our document.
    pub fn diagnostics(&mut self) -> Vec<Value> {
        let uri = self.uri();
        let message = self.wait_for(|message| {
            message.get("method") == Some(&json!("textDocument/publishDiagnostics"))
                && message["params"]["uri"] == json!(uri)
        });

        message["params"]["diagnostics"].as_array().cloned().unwrap_or_default()
    }

    /// The next `window/logMessage` whose text contains `needle`.
    pub fn log_containing(&mut self, needle: &str) -> Value {
        self.wait_for(|message| {
            message.get("method") == Some(&json!("window/logMessage"))
                && message["params"]["message"].as_str().is_some_and(|text| text.contains(needle))
        })
    }

    /// Diagnostics that keep arriving until one satisfies `wanted`.
    pub fn diagnostics_until(&mut self, wanted: impl Fn(&[Value]) -> bool) -> Vec<Value> {
        for _ in 0..20 {
            let diagnostics = self.diagnostics();
            if wanted(&diagnostics) {
                return diagnostics;
            }
        }

        panic!("no diagnostics matched");
    }

    /// Completion items, from either shape the protocol allows: we answer with
    /// a `CompletionList`, luau-lsp answers with a bare array.
    pub fn completion(&mut self, line: u64, character: u64) -> Vec<Value> {
        let result = self.request("textDocument/completion", self.at(line, character));

        match result {
            Value::Array(items) => items,
            other => other["items"].as_array().cloned().unwrap_or_default(),
        }
    }

    pub fn request(&mut self, method: &str, params: Value) -> Value {
        let id = self.send_request(method, params);
        let response = self.wait_for(|message| {
            message.get("id") == Some(&json!(id)) && message.get("method").is_none()
        });

        assert!(response.get("error").is_none(), "{method}: {response}");
        response.get("result").cloned().unwrap_or(Value::Null)
    }

    pub fn request_error(&mut self, method: &str, params: Value) -> Value {
        let id = self.send_request(method, params);
        let response = self.wait_for(|message| {
            message.get("id") == Some(&json!(id)) && message.get("method").is_none()
        });

        response.get("error").cloned().expect("an error")
    }

    fn send_request(&mut self, method: &str, params: Value) -> i64 {
        let id = self.next_id;
        self.next_id += 1;

        self.send(&json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }));
        id
    }

    pub fn notify(&mut self, method: &str, params: Value) {
        self.send(&json!({ "jsonrpc": "2.0", "method": method, "params": params }));
    }

    // --- transport ---------------------------------------------------------

    fn send(&mut self, message: &Value) {
        let body = serde_json::to_vec(message).expect("serialise");
        write!(self.stdin, "Content-Length: {}\r\n\r\n", body.len()).expect("header");
        self.stdin.write_all(&body).expect("body");
        self.stdin.flush().expect("flush");
    }

    /// Reads until a message matches, so unrelated logs and progress do not
    /// have to be anticipated by every test.
    ///
    /// Messages that do not match are *kept*, not discarded. Waiting for a
    /// response would otherwise swallow the diagnostics that arrived first, and
    /// the next wait would hang for something it had already been sent.
    fn wait_for(&mut self, matches: impl Fn(&Value) -> bool) -> Value {
        if let Some(at) = self.held.iter().position(&matches) {
            return self.held.remove(at);
        }

        loop {
            match self.incoming.recv_timeout(TIMEOUT) {
                Ok(message) => {
                    if matches(&message) {
                        return message;
                    }
                    self.held.push(message);
                }
                Err(RecvTimeoutError::Timeout) => {
                    panic!("no matching message in {TIMEOUT:?}; held {:#?}", self.held)
                }
                Err(RecvTimeoutError::Disconnected) => {
                    panic!("the server closed its output; held {:#?}", self.held)
                }
            }
        }
    }
}

/// One `Content-Length`-framed message, or `None` at end of stream.
fn read(stdout: &mut BufReader<std::process::ChildStdout>) -> Option<Value> {
    let mut length = None;

    loop {
        let mut line = String::new();
        if stdout.read_line(&mut line).ok()? == 0 {
            return None;
        }

        let line = line.trim_end_matches(['\r', '\n']);
        if line.is_empty() {
            break;
        }

        if let Some(value) = line.strip_prefix("Content-Length: ") {
            length = value.trim().parse::<usize>().ok();
        }
    }

    let mut body = vec![0u8; length?];
    stdout.read_exact(&mut body).ok()?;

    serde_json::from_slice(&body).ok()
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.process.kill();
        let _ = self.process.wait();
    }
}

/// A directory that removes itself. Small enough not to justify a dependency.
pub struct TempDirectory {
    pub path: PathBuf,
}

impl TempDirectory {
    pub fn new() -> Self {
        // A counter rather than a random number: tests run in one process, and
        // the pid keeps concurrent runs apart.
        use std::sync::atomic::{AtomicUsize, Ordering};
        static NEXT: AtomicUsize = AtomicUsize::new(0);

        let path = std::env::temp_dir().join(format!(
            "luaux-lsp-test-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));

        std::fs::create_dir_all(&path).expect("temp directory");
        Self { path }
    }
}

impl Drop for TempDirectory {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

/// Where a real, *working* luau-lsp is, if this machine has one.
///
/// Integration claims that are never executed are not claims, so the tests that
/// need it run when it is present and say why when it is not — rather than
/// passing vacuously.
///
/// "Working" is checked rather than assumed: a rokit shim on `PATH` refuses to
/// run outside a project that lists the tool, and a test that hangs against one
/// tells nobody anything.
pub fn find_luau_lsp() -> Option<String> {
    let candidates = [
        std::env::var("LUAU_LSP").ok().map(PathBuf::from),
        luaux_lsp::proxy::locate("luau-lsp", None),
        rokit_luau_lsp(),
    ];

    candidates.into_iter().flatten().find(|path| runs(path)).map(|path| path.display().to_string())
}

fn runs(path: &Path) -> bool {
    Command::new(path)
        .arg("--version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}

/// The stored binary, past the shim.
fn rokit_luau_lsp() -> Option<PathBuf> {
    let home = std::env::var_os("HOME").map(PathBuf::from)?;
    let tool = home.join(".rokit/tool-storage/johnnymorganz/luau-lsp");

    let mut versions: Vec<PathBuf> = std::fs::read_dir(tool)
        .ok()?
        .flatten()
        .map(|entry| entry.path().join("luau-lsp"))
        .filter(|path| path.is_file())
        .collect();

    versions.sort();
    versions.pop()
}

/// The path luau-lsp is handed for this project's one document.
pub fn build_path(root: &Path) -> PathBuf {
    root.join("build").join("App.luau")
}
