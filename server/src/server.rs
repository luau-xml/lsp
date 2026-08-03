//! The message loop.
//!
//! Two readers — the editor's stdin and luau-lsp's stdout — feed one channel,
//! and everything else happens on this thread. That is why no state here is
//! behind a lock: there is only ever one thing touching it.
//!
//! Nothing blocks on the child. A forwarded request is recorded against the id
//! we gave the child and answered when its response arrives, so a slow type
//! check never stops tag completion from being instant.

use crate::analysis::{Analysis, Compiled};
use crate::code_actions;
use crate::completion::{self, Completion};
use crate::document::Documents;
use crate::hover;
use crate::jsonrpc::{
    self,
    build::{self, CONTENT_MODIFIED, INTERNAL_ERROR, METHOD_NOT_FOUND},
    Message,
};
use crate::line_index::LineIndex;
use crate::project::{self, Projects};
use crate::proxy::{self, Event, Proxy};
use crate::remap::{Direction, Remap};
use crate::rename;
use crate::scan;
use crate::semantic_tokens;
use crate::symbols;
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::io::{self, Stdout};
use std::sync::mpsc::{channel, Receiver, Sender};

/// What a response from the child is for.
enum Pending {
    /// The editor is waiting. Map the result home and answer it.
    Editor {
        id: Value,
        uri: String,
        /// Our own contribution, merged with the child's when it arrives.
        ours: Option<Value>,
        /// Whether a partially-mappable answer is unusable. Rename is: applying
        /// half of one is data loss.
        all_or_nothing: bool,
    },
    /// A component's props: the child's expected keys for the generated call's
    /// table argument, rewritten into markup attributes.
    ///
    /// Not a `Editor` forward, because none of that applies — the answer is not
    /// remapped but rebuilt, and the range is one we already know.
    ComponentProps { id: Value, tag: String, range: Value, snippets: bool },
    /// The handshake with luau-lsp.
    Initialize,
}

pub struct Server {
    documents: Documents,
    projects: Projects,
    /// Last successful compile per document, kept as the stale fallback.
    compiled: HashMap<String, Compiled>,
    /// Our diagnostics, and luau-lsp's, published together.
    ours: HashMap<String, Vec<Value>>,
    theirs: HashMap<String, Vec<Value>>,
    /// Whether the compile behind `compiled` is older than the document.
    stale: HashMap<String, bool>,
    /// The build path each document maps to, resolved once at open rather than
    /// walked for every message.
    generated: HashMap<String, String>,

    /// Generated URIs the child has been told to open. It is the child's own
    /// view, not ours: a document it does not hold cannot be changed or asked
    /// about, only opened.
    opened_in_child: HashSet<String>,

    proxy: Option<Proxy>,
    pending: HashMap<i64, Pending>,
    /// Requests the child made that only the editor can answer, by the id we
    /// gave the editor.
    forwarded: HashMap<i64, Value>,
    /// Things already said once. Advice and explanations describe a state of the
    /// file, so repeating them on every keystroke is noise.
    said_once: HashSet<String>,
    /// Ids of our own `workspace/configuration` requests. Recorded rather than
    /// inferred: "not a forwarded id" also describes a response to a request we
    /// cancelled, and reading one of those as settings would replace them with
    /// an unrelated payload.
    configuration_requests: HashSet<i64>,
    next_editor_id: i64,

    /// The editor's own `initialize` params, replayed to the child.
    client: Value,
    /// The user's `luau-lsp.*` settings, used for the child's invocation.
    settings: Value,
    /// Our own `luaux.*` settings.
    ours_settings: Value,
    /// Whether the editor understands snippet placeholders in a completion.
    /// One that does not would insert `${1:p1}` as literal text.
    snippets: bool,
    child_ready: bool,

    writer: jsonrpc::Writer<Stdout>,
    events: Sender<Event>,
    trace: bool,
    shutting_down: bool,
}

/// Runs until the editor closes stdin.
pub fn run() -> io::Result<()> {
    let (sender, receiver) = channel();

    let reader_events = sender.clone();
    std::thread::spawn(move || {
        let mut reader = jsonrpc::reader(io::stdin());

        while let Ok(Some(message)) = reader.read() {
            if reader_events.send(Event::FromEditor(message)).is_err() {
                return;
            }
        }

        let _ = reader_events.send(Event::EditorClosed);
    });

    Server::new(sender).serve(receiver)
}

impl Server {
    fn new(events: Sender<Event>) -> Self {
        Self {
            documents: Documents::default(),
            projects: Projects::default(),
            compiled: HashMap::new(),
            ours: HashMap::new(),
            theirs: HashMap::new(),
            stale: HashMap::new(),
            generated: HashMap::new(),
            opened_in_child: HashSet::new(),
            proxy: None,
            pending: HashMap::new(),
            forwarded: HashMap::new(),
            configuration_requests: HashSet::new(),
            said_once: HashSet::new(),
            next_editor_id: 1,
            client: Value::Null,
            settings: Value::Null,
            ours_settings: Value::Null,
            snippets: false,
            child_ready: false,
            writer: jsonrpc::Writer::new(io::stdout()),
            events,
            trace: false,
            shutting_down: false,
        }
    }

    fn serve(mut self, events: Receiver<Event>) -> io::Result<()> {
        while let Ok(event) = events.recv() {
            match event {
                Event::FromEditor(value) => {
                    if let Some(message) = Message::from_value(value) {
                        self.guarded(message, true);
                    }
                }
                Event::FromChild(value) => {
                    if let Some(message) = Message::from_value(value) {
                        self.guarded(message, false);
                    }
                }
                Event::EditorClosed => break,
                Event::ChildClosed => {
                    self.proxy = None;
                    self.child_ready = false;
                    self.opened_in_child.clear();

                    if !self.shutting_down {
                        // Everything local keeps working; say so rather than
                        // leaving people wondering why hover went quiet.
                        self.log(2, "luau-lsp exited — markup features only");
                        self.fail_pending();
                    }
                }
            }
        }

        if let Some(proxy) = &mut self.proxy {
            proxy.shutdown();
        }

        Ok(())
    }

    /// Handles one message, surviving a panic in the handler.
    ///
    /// A server that dies takes every feature down with it and buries the cause
    /// under a wall of secondary errors — "stopping server failed", "cannot
    /// write after a stream was destroyed" — none of which name the one message
    /// that went wrong. Catching here turns the worst case into a single logged
    /// line and one failed request, with the session still usable.
    ///
    /// The state this leaves behind may be inconsistent, which is why it is not
    /// a substitute for the offset being right. It is the floor, not the design.
    fn guarded(&mut self, message: Message, from_editor: bool) {
        let method = message.method().unwrap_or("a response").to_string();
        let waiting = match &message {
            Message::Request { id, .. } => Some(id.clone()),
            _ => None,
        };

        let handled = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            if from_editor {
                self.handle_editor(message);
            } else {
                self.handle_child(message);
            }
        }));

        if handled.is_ok() {
            return;
        }

        self.log(1, &format!("internal error handling {method} — please report it"));

        // An unanswered request leaves the editor waiting forever.
        if let Some(id) = waiting {
            self.error(id, INTERNAL_ERROR, "luaux-lsp could not handle this request");
        }
    }

    // --- editor → us -------------------------------------------------------

    fn handle_editor(&mut self, message: Message) {
        match message {
            Message::Request { id, method, params } => self.request(id, &method, params),
            Message::Notification { method, params } => self.notification(&method, params),
            Message::Response { id, body } => self.editor_response(id, body),
        }
    }

    fn request(&mut self, id: Value, method: &str, params: Value) {
        match method {
            "initialize" => self.initialize(id, params),
            "shutdown" => {
                self.shutting_down = true;
                if let Some(proxy) = &mut self.proxy {
                    proxy.shutdown();
                }
                self.proxy = None;
                self.reply(id, Value::Null);
            }

            "textDocument/completion" => self.completion(id, params),
            "textDocument/hover" => self.hover(id, params),
            "textDocument/definition" => self.definition(id, params),
            "textDocument/documentSymbol" => self.document_symbols(id, params),
            "textDocument/semanticTokens/full" => self.semantic_tokens(id, params),
            "textDocument/codeAction" => self.code_action(id, params),
            // Ours, not LSP's. Auto-closing has to happen as the `>` is typed,
            // and the only standard hook for that is on-type formatting, which
            // is off by default in VS Code — so the client asks and inserts.
            "luaux/closingTag" => self.closing_tag(id, params),

            "textDocument/prepareRename" => self.prepare_rename(id, params),
            "textDocument/rename" => self.rename(id, params),

            // Purely about Luau, and only meaningful inside a captured
            // expression. Forwarding handles the "not in one" case by refusing
            // to map the position.
            "textDocument/typeDefinition"
            | "textDocument/references"
            | "textDocument/signatureHelp"
            | "textDocument/documentHighlight"
            | "textDocument/inlayHint"
            | "textDocument/foldingRange"
            | "textDocument/selectionRange" => self.forward(id, method, params, None, false),

            _ => self.error(id, METHOD_NOT_FOUND, &format!("{method} is not supported")),
        }
    }

    fn notification(&mut self, method: &str, params: Value) {
        match method {
            "initialized" => {
                // The child's invocation depends on settings only the editor
                // has, so it cannot start until they arrive.
                self.request_configuration();
            }
            "exit" => std::process::exit(if self.shutting_down { 0 } else { 1 }),

            "textDocument/didOpen" => self.did_open(params),
            "textDocument/didChange" => self.did_change(params),
            "textDocument/didClose" => self.did_close(params),
            "textDocument/didSave" => self.did_save(params),

            "workspace/didChangeConfiguration" => self.request_configuration(),
            "workspace/didChangeWatchedFiles" => {
                // A `luaux.toml` may have changed, and it decides what compiles.
                self.projects.clear();
                self.reanalyse_all();
                self.send_child(&build::notification(method, params));
            }

            "$/setTrace" => {
                self.trace = params.get("value").and_then(Value::as_str) != Some("off");
            }
            "$/cancelRequest" => self.cancel_in_child(&params),

            _ => {}
        }
    }

    /// Cancels the child's request, not the one that happens to share a number.
    ///
    /// The editor numbers its requests and [`Proxy`] numbers ours, so the two id
    /// spaces collide by construction — which is why a forwarded request is given
    /// an id of our own in the first place. A cancellation is the same problem
    /// and needs the same translation.
    ///
    /// Sending it through untranslated carries the editor's number into a space
    /// where it means something else, and request 1 there is the **handshake**:
    /// the child answers `initialize` with "request cancelled by client", we take
    /// that for a refusal, and the session degrades to markup features for as
    /// long as the window lives. Reloading with a `.luaux` already open is enough
    /// to do it, because the editor issues requests and cancels them while the
    /// child is still starting.
    ///
    /// A request that was never forwarded — answered here, or dropped because its
    /// position did not map — has nothing to cancel, and silence is the answer.
    fn cancel_in_child(&mut self, params: &Value) {
        let Some(wanted) = params.get("id") else { return };

        let forwarded = self.pending.iter().find_map(|(child, pending)| match pending {
            Pending::Editor { id, .. } | Pending::ComponentProps { id, .. } if id == wanted => {
                Some(*child)
            }
            _ => None,
        });

        if let Some(id) = forwarded {
            self.send_child(&build::notification("$/cancelRequest", json!({ "id": id })));
        }
    }

    /// The same translation the other way, for a request the child made of the
    /// editor. Rarer — it is `workspace/configuration` and little else — but the
    /// id spaces collide in both directions.
    fn cancel_in_editor(&mut self, params: &Value) {
        let Some(wanted) = params.get("id") else { return };

        let ours =
            self.forwarded.iter().find_map(|(ours, child)| (child == wanted).then_some(*ours));

        if let Some(id) = ours {
            self.forwarded.remove(&id);
            self.send(&build::notification("$/cancelRequest", json!({ "id": id })));
        }
    }

    /// The editor answering something the child asked for.
    fn editor_response(&mut self, id: Value, body: Value) {
        let Some(number) = id.as_i64() else { return };

        // Our own `workspace/configuration`, which is what starts luau-lsp.
        if self.configuration_requests.remove(&number) {
            if let Some(result) = body.get("result") {
                self.configured(result.clone());
            }
            return;
        }

        let Some(child_id) = self.forwarded.remove(&number) else { return };
        let mut answer = body;
        answer["id"] = child_id;
        self.send_child(&answer);
    }

    // --- child → us --------------------------------------------------------

    fn handle_child(&mut self, message: Message) {
        match message {
            Message::Notification { method, params } => self.child_notification(&method, params),

            // It wants something only the editor can answer — configuration
            // above all, which is how the user's existing `luau-lsp.*` settings
            // reach it without being configured twice.
            Message::Request { id, method, params } => {
                if !proxy::needs_the_editor(&Message::Request {
                    id: id.clone(),
                    method: method.clone(),
                    params: params.clone(),
                }) {
                    self.send_child(&build::error(id, METHOD_NOT_FOUND, "not supported"));
                    return;
                }

                let ours = self.next_editor_id;
                self.next_editor_id += 1;
                self.forwarded.insert(ours, id);
                self.send(&build::request(ours.into(), &method, params));
            }

            Message::Response { id, body } => self.child_response(id, body),
        }
    }

    fn child_notification(&mut self, method: &str, params: Value) {
        if method == "textDocument/publishDiagnostics" {
            self.child_diagnostics(params);
            return;
        }

        if method == "$/cancelRequest" {
            self.cancel_in_editor(&params);
            return;
        }

        // Logs and progress belong to the editor, unchanged.
        self.send(&build::notification(method, params));
    }

    fn child_response(&mut self, id: Value, body: Value) {
        let Some(number) = id.as_i64() else { return };
        let Some(pending) = self.pending.remove(&number) else { return };

        match pending {
            Pending::Initialize => match body.get("error") {
                // A handshake that failed is not a handshake. Treating it as one
                // marks the child ready, and it then answers "server not
                // initialized" to every request for the rest of the session —
                // one clear message turned into an endless stream of confusing
                // ones.
                Some(error) => self.child_failed(error),
                None => self.child_initialized(),
            },
            Pending::Editor { id, uri, ours, all_or_nothing } => {
                self.answer_forwarded(id, &uri, ours, all_or_nothing, body)
            }
            Pending::ComponentProps { id, tag, range, snippets } => {
                self.answer_component_props(id, &tag, range, snippets, body)
            }
        }
    }

    /// luau-lsp refused to start up. Say why, once, and carry on without it.
    fn child_failed(&mut self, error: &Value) {
        let message = error.get("message").and_then(Value::as_str).unwrap_or("no reason given");

        self.log(1, &format!("luau-lsp refused to initialize ({message}) — markup features only"));

        self.child_ready = false;
        self.opened_in_child.clear();
        if let Some(proxy) = &mut self.proxy {
            proxy.shutdown();
        }
        self.proxy = None;
        self.fail_pending();
    }

    /// Maps a forwarded response home and answers the editor.
    fn answer_forwarded(
        &mut self,
        id: Value,
        uri: &str,
        ours: Option<Value>,
        all_or_nothing: bool,
        body: Value,
    ) {
        if let Some(error) = body.get("error") {
            // An error about the *child's own state* is not an answer to the
            // question that was asked, and the editor can do nothing with it but
            // show it. Those become "nothing"; a real error — a malformed
            // request, an internal failure — still travels, because it says
            // something true about what was asked.
            if about_the_child(error) {
                // It is not holding this document, whatever we believed. Forget
                // that we opened it, so the next request opens it again rather
                // than failing the same way for the rest of the session.
                if lost_the_document(error) {
                    if let Some(generated) = self.generated_uri(uri) {
                        self.opened_in_child.remove(&generated);
                    }
                }

                self.reply(id, merged(ours, Value::Null));
                return;
            }

            // Ours stands on its own. The child failing says nothing about the
            // half we answered ourselves, and relaying the error in its place
            // turns a working hover into an empty one.
            if let Some(ours) = ours {
                self.reply(id, ours);
                return;
            }

            self.send(&json!({ "jsonrpc": "2.0", "id": id, "error": error }));
            return;
        }

        let result = body.get("result").cloned().unwrap_or(Value::Null);

        let mapped = if result.is_null() {
            Value::Null
        } else {
            let Some((mapped, dropped)) = self.remap_up(uri, &result) else {
                // Nothing to map against — no compile has ever succeeded — so
                // there is no honest answer but "nothing".
                self.reply(id, merged(ours, Value::Null));
                return;
            };

            // Half a rename is worse than none: the editor applies what it is
            // given and cannot take it back.
            if dropped && all_or_nothing {
                self.error(
                    id,
                    CONTENT_MODIFIED,
                    "some of this rename lands in generated code, so it cannot be applied here",
                );
                return;
            }

            self.mark_stale(uri, mapped)
        };

        self.reply(id, merged(ours, mapped));
    }

    // --- lifecycle ---------------------------------------------------------

    fn initialize(&mut self, id: Value, params: Value) {
        self.snippets = params
            .pointer("/capabilities/textDocument/completion/completionItem/snippetSupport")
            == Some(&Value::Bool(true));
        self.client = params;

        self.reply(
            id,
            json!({
                "capabilities": capabilities(),
                "serverInfo": {
                    "name": "luaux-lsp",
                    // Both versions, because a server built against a different
                    // compiler than the one producing `build/` reports
                    // diagnostics the build does not.
                    "version": format!("{} (luaux {})", crate::VERSION, crate::LUAUX_VERSION),
                },
            }),
        );
    }

    /// Asks the editor for settings.
    ///
    /// `luau-lsp.*` first, because those are the ones the user already has and
    /// making them configure the same definition files twice is a bug. Ours
    /// second, for the two paths that are ours to name.
    fn request_configuration(&mut self) {
        let id = self.next_editor_id;
        self.next_editor_id += 1;

        self.configuration_requests.insert(id);
        self.send(&build::request(
            id.into(),
            "workspace/configuration",
            json!({ "items": [{ "section": "luau-lsp" }, { "section": "luaux" }] }),
        ));
    }

    fn configured(&mut self, result: Value) {
        self.settings = result.get(0).cloned().unwrap_or(Value::Null);
        self.ours_settings = result.get(1).cloned().unwrap_or(Value::Null);

        // The same Roblox definitions luau-lsp is about to be given, so that a
        // tooltip of ours and one of its own describe the same member the same
        // way.
        let (definitions, docs) = proxy::roblox_type_files(&self.settings);
        crate::api::install(definitions, docs);

        if self.proxy.is_some() {
            return;
        }

        self.start_child();
    }

    fn start_child(&mut self) {
        // Ours wins: someone who named a binary in `luaux.luauLsp.path` meant
        // that one, whatever the luau-lsp extension is configured to use.
        let configured = self
            .ours_settings
            .pointer("/luauLsp/path")
            .or_else(|| self.settings.pointer("/server/path"))
            .and_then(Value::as_str)
            .filter(|path| !path.trim().is_empty())
            .map(str::to_string);

        let Some(command) = proxy::locate("luau-lsp", configured.as_deref()) else {
            // Correct to degrade, wrong to refuse.
            self.log(2, "luau-lsp not found — markup features only, no Luau types");
            return;
        };

        let arguments = proxy::arguments(&self.settings);

        match Proxy::spawn(&command, &arguments, self.events.clone()) {
            Ok(mut proxy) => {
                self.log(3, &format!("luau-lsp: {}", command.display()));

                // The editor's own initialize params: same roots, same
                // capabilities, so requires, the rojo sourcemap and `.luaurc`
                // all resolve exactly as they would without us.
                let mut params = self.client.clone();
                params["processId"] = json!(std::process::id());

                // Push, not pull. luau-lsp offers diagnostics both ways and
                // chooses from the *client's* capabilities — which are the
                // editor's, replayed. An editor that supports pull (VS Code
                // does) silently switches the child to pull-only, and since this
                // server never asks that way, its diagnostics stop arriving
                // altogether: no Luau errors in `.luaux`, nothing logged, no
                // request failed. The document is handed over and simply never
                // answered about.
                //
                // Push is what this design is built on — ours and the child's
                // are merged and published together — so the capability is
                // withheld rather than the plumbing rebuilt.
                for (section, capability) in [
                    ("/capabilities/textDocument", "diagnostic"),
                    ("/capabilities/workspace", "diagnostics"),
                ] {
                    if let Some(object) = params.pointer_mut(section).and_then(Value::as_object_mut)
                    {
                        object.remove(capability);
                    }
                }

                // Its Luau flags travel here rather than on the command line,
                // which is how its own extension delivers them. Without this the
                // child runs with every flag off and answers *worse* than the
                // luau-lsp extension would about the same generated file — Vide's
                // `create` degrading to `*error-type*` is what that looks like.
                let flags = proxy::fflags(
                    &self.settings,
                    self.client.pointer("/initializationOptions/fflags"),
                );

                if !flags.is_empty() {
                    if !params["initializationOptions"].is_object() {
                        params["initializationOptions"] = json!({});
                    }
                    params["initializationOptions"]["fflags"] = Value::Object(flags);
                }

                match proxy.request("initialize", params) {
                    Ok(id) => {
                        self.pending.insert(id, Pending::Initialize);
                        self.proxy = Some(proxy);
                    }
                    Err(error) => self.log(1, &format!("luau-lsp: {error}")),
                }
            }
            Err(error) => self.log(1, &format!("could not start luau-lsp: {error}")),
        }
    }

    fn child_initialized(&mut self) {
        self.child_ready = true;
        self.send_child(&build::notification("initialized", json!({})));

        // Documents opened before it was ready still have to reach it.
        for uri in self.documents.uris() {
            self.sync_child(&uri);
        }
    }

    // --- documents ---------------------------------------------------------

    fn did_open(&mut self, params: Value) {
        let Some(document) = params.get("textDocument") else { return };
        let (Some(uri), Some(text)) = (
            document.get("uri").and_then(Value::as_str),
            document.get("text").and_then(Value::as_str),
        ) else {
            return;
        };

        let version = document.get("version").and_then(Value::as_i64).unwrap_or(0);
        let uri = uri.to_string();
        self.documents.open(uri.clone(), version, text.to_string());

        self.resolve_generated(&uri);
        self.analyse(&uri);
        self.sync_child(&uri);
    }

    fn did_change(&mut self, params: Value) {
        let Some(uri) = params.pointer("/textDocument/uri").and_then(Value::as_str) else {
            return;
        };
        let uri = uri.to_string();
        let version = params.pointer("/textDocument/version").and_then(Value::as_i64).unwrap_or(0);
        let changes: Vec<Value> =
            params.get("contentChanges").and_then(Value::as_array).cloned().unwrap_or_default();

        if !self.documents.change(&uri, version, &changes) {
            // Out of step with the editor, and every position from here on would
            // be wrong. Ask for the whole file rather than guessing.
            self.log(1, "lost track of an edit — reopen the file to resynchronise");
            return;
        }

        self.analyse(&uri);
        self.sync_child(&uri);
    }

    fn did_close(&mut self, params: Value) {
        let Some(uri) = params.pointer("/textDocument/uri").and_then(Value::as_str) else {
            return;
        };
        let uri = uri.to_string();

        if let Some(generated) = self.generated_uri(&uri) {
            self.close_in_child(&generated);
        }

        self.documents.close(&uri);
        self.compiled.remove(&uri);
        self.ours.remove(&uri);
        self.theirs.remove(&uri);
        self.stale.remove(&uri);
        self.generated.remove(&uri);

        // Diagnostics outlive a closed document unless they are cleared.
        self.send(&build::notification(
            "textDocument/publishDiagnostics",
            json!({ "uri": uri, "diagnostics": [] }),
        ));
    }

    fn did_save(&mut self, params: Value) {
        let Some(uri) = params.pointer("/textDocument/uri").and_then(Value::as_str) else {
            return;
        };
        let uri = uri.to_string();

        // Deliberately *not* clearing the project cache. `Project::is_stale`
        // already notices a `luaux.toml` that changed on disk, and clearing here
        // throws away the casing vocabulary with it — which costs ~90ms to
        // rebuild in a project with `[elements] all`, on every save of any file.
        self.resolve_generated(&uri);
        self.analyse(&uri);
        self.sync_child(&uri);
    }

    fn reanalyse_all(&mut self) {
        for uri in self.documents.uris() {
            // `[build] out` may have moved, which changes the path luau-lsp
            // sees and so what its answers are about.
            self.resolve_generated(&uri);
            self.analyse(&uri);
            self.sync_child(&uri);
        }
    }

    /// Compiles, publishes our diagnostics, and keeps the map for forwarding.
    fn analyse(&mut self, uri: &str) {
        let Some(document) = self.documents.get(uri) else { return };
        let Some(path) = project::uri_to_path(uri) else { return };

        let project = self.projects.for_file(&path);
        let previous = self.compiled.remove(uri);
        let analysis = Analysis::run(document, &project, previous);

        self.ours.insert(uri.to_string(), analysis.diagnostics);
        self.stale.insert(uri.to_string(), analysis.stale);

        if let Some(compiled) = analysis.compiled {
            self.compiled.insert(uri.to_string(), compiled);
        }

        self.publish(uri);
    }

    fn publish(&mut self, uri: &str) {
        let mut diagnostics = self.ours.get(uri).cloned().unwrap_or_default();
        diagnostics.extend(self.theirs.get(uri).cloned().unwrap_or_default());

        self.send(&build::notification(
            "textDocument/publishDiagnostics",
            json!({ "uri": uri, "diagnostics": diagnostics }),
        ));
    }

    /// luau-lsp's diagnostics, mapped home and merged with ours.
    fn child_diagnostics(&mut self, params: Value) {
        let Some(generated) = params.get("uri").and_then(Value::as_str) else { return };

        let Some(uri) = self.documents.uris().into_iter().find(|uri| {
            self.generated_uri(uri).is_some_and(|ours| project::same_file(&ours, generated))
        }) else {
            // A file we do not own — luau-lsp analyses dependencies too, and
            // publishing those against a generated path the author never opened
            // is noise. Unless it is a document we *did* hand over, in which
            // case our own bookkeeping disagrees with itself and that is worth
            // knowing about.
            if self.opened_in_child.iter().any(|ours| project::same_file(ours, generated)) {
                let generated = generated.to_string();
                self.log(
                    2,
                    &format!(
                        "luau-lsp published diagnostics for {generated}, which is open in it but \
                         matches no document here — they cannot be shown"
                    ),
                );
            }
            return;
        };

        let list = params.get("diagnostics").cloned().unwrap_or(json!([]));
        let received = list.as_array().map_or(0, Vec::len);

        if self.said_once.insert(format!("heard:{uri}")) {
            self.log(
                3,
                &format!("luau-lsp answered about this file with {received} diagnostic(s)"),
            );
        }

        let mapped = match self.remap_up(&uri, &list) {
            // Anything landing in generated text is dropped, never relocated to
            // whatever is nearest.
            Some((mapped, _)) => mapped.as_array().cloned().unwrap_or_default(),
            None => Vec::new(),
        };

        // Received and then lost is the one failure that looks exactly like
        // "luau-lsp had nothing to say", and the difference decides where to go
        // looking. It is said once per document per session, because it is a
        // property of the file rather than of the keystroke.
        if received > 0 && mapped.is_empty() && self.said_once.insert(format!("dropped:{uri}")) {
            let reason = match self.compiled.contains_key(&uri) {
                true => "none of them mapped back into the source",
                false => "there is no successful compile to map them through",
            };
            self.log(
                2,
                &format!("luau-lsp reported {received} diagnostic(s) for this file; {reason}"),
            );
        }

        if self.trace {
            eprintln!("[diagnostics] {uri}: {received} received, {} shown", mapped.len());
        }

        self.theirs.insert(uri.clone(), mapped);
        self.publish(&uri);
    }

    // --- forwarding --------------------------------------------------------

    /// Sends a request on to luau-lsp, with positions and URIs translated.
    ///
    /// A position that lands in generated text is not forwarded at all — there
    /// is nothing on the other side for it to mean.
    fn forward(
        &mut self,
        id: Value,
        method: &str,
        params: Value,
        ours: Option<Value>,
        all_or_nothing: bool,
    ) {
        let Some(uri) = params.pointer("/textDocument/uri").and_then(Value::as_str) else {
            self.reply(id, merged(ours, Value::Null));
            return;
        };
        let uri = uri.to_string();

        let Some(mapped) = self.remap_down(&uri, &params) else {
            self.reply(id, merged(ours, Value::Null));
            return;
        };

        if !self.child_ready {
            self.reply(id, merged(ours, Value::Null));
            return;
        }

        // The child cannot answer about a document it does not hold — it refuses
        // with `No managed text document`, an error naming a generated path the
        // author never opened and cannot act on. Checking costs a set lookup and
        // only ever sends anything in the case that would otherwise fail.
        if self
            .generated_uri(&uri)
            .is_some_and(|generated| !self.opened_in_child.contains(&generated))
        {
            self.sync_child(&uri);
        }

        let Some(proxy) = &mut self.proxy else {
            self.reply(id, merged(ours, Value::Null));
            return;
        };

        match proxy.request(method, mapped) {
            Ok(child_id) => {
                self.pending.insert(child_id, Pending::Editor { id, uri, ours, all_or_nothing });
            }
            Err(error) => {
                self.log(1, &format!("luau-lsp: {error}"));
                self.reply(id, merged(ours, Value::Null));
            }
        }
    }

    /// Answers everything still in flight when the child goes away.
    fn fail_pending(&mut self) {
        for (_, pending) in std::mem::take(&mut self.pending) {
            match pending {
                Pending::Editor { id, ours, .. } => self.reply(id, merged(ours, Value::Null)),
                Pending::ComponentProps { id, .. } => {
                    self.reply(id, json!({ "isIncomplete": false, "items": [] }))
                }
                Pending::Initialize => {}
            }
        }
    }

    fn remap_down(&self, uri: &str, value: &Value) -> Option<Value> {
        self.with_remap(uri, Direction::Down, value).map(|(value, _)| value)
    }

    fn remap_up(&self, uri: &str, value: &Value) -> Option<(Value, bool)> {
        self.with_remap(uri, Direction::Up, value)
    }

    fn with_remap(&self, uri: &str, direction: Direction, value: &Value) -> Option<(Value, bool)> {
        let document = self.documents.get(uri)?;
        let compiled = self.compiled.get(uri)?;
        let generated = self.generated_uri(uri)?;
        let output_index = LineIndex::new(&compiled.output);

        let mut remap = Remap {
            map: &compiled.map,
            source: &document.text,
            output: &compiled.output,
            source_index: &document.index,
            output_index: &output_index,
            source_uri: uri,
            output_uri: &generated,
            direction,
            dropped: false,
        };

        let mapped = remap.message(value)?;
        Some((mapped, remap.dropped))
    }

    /// Notes on an answer built from a compile older than the document.
    ///
    /// Marked rather than hidden: a stale hover beats no hover, but only if the
    /// person reading it knows which it is.
    fn mark_stale(&self, uri: &str, mut value: Value) -> Value {
        if self.stale.get(uri) != Some(&true) {
            return value;
        }

        if let Some(text) = value.pointer("/contents/value").and_then(Value::as_str) {
            let marked = format!("{text}\n\n---\n\n*From the last version that compiled.*");
            value["contents"]["value"] = json!(marked);
        }

        value
    }

    /// The path the build writes for this document — the URI luau-lsp sees.
    fn generated_uri(&self, uri: &str) -> Option<String> {
        self.generated.get(uri).cloned()
    }

    /// Resolves and caches it. Walking the tree for `luaux.toml` on every
    /// message would put a filesystem stat on the keystroke path.
    fn resolve_generated(&mut self, uri: &str) {
        let Some(path) = project::uri_to_path(uri) else { return };
        let project = self.projects.for_file(&path);
        let generated = project::path_to_uri(&project.build_path(&path));

        // `[build] out` can move, which renames this document out from under the
        // child. Leaving the old URI open has it holding a file nothing writes
        // any more, and the new one is a document it was never given.
        if let Some(previous) = self.generated.insert(uri.to_string(), generated.clone()) {
            if previous != generated {
                self.close_in_child(&previous);
            }
        }
    }

    /// Brings the child's copy of a document up to date, opening it if it does
    /// not have it.
    ///
    /// Which notification to send is decided by **what the child actually
    /// holds**, not by which of ours we are handling. A `didChange` for a
    /// document it was never given is not an opening: it answers
    /// `No managed text document` to every request against that URI afterwards,
    /// and nothing about the failure names the file that was never opened.
    ///
    /// More than one ordinary path arrives here without an opening having
    /// happened — a file that did not compile when the child started, so there
    /// was no Luau to hand over; a `[build] out` that moved, which renames the
    /// document out from under it; a save that produced the first compile that
    /// ever succeeded. Deciding from the child's own state covers all of them,
    /// including the ones not yet thought of.
    fn sync_child(&mut self, uri: &str) {
        // Nothing may be sent before the handshake finishes. Documents opened in
        // the meantime are replayed by `child_initialized`.
        if !self.child_ready {
            return;
        }

        let (Some(generated), Some(text), Some(version)) = (
            self.generated_uri(uri),
            self.compiled.get(uri).map(|compiled| compiled.output.clone()),
            self.documents.get(uri).map(|document| document.version),
        ) else {
            // No generated Luau means nothing to hand over, and luau-lsp then has
            // nothing to say about this file — which looks exactly like it having
            // no complaints. Said once, because it is a property of the file.
            if self.said_once.insert(format!("unsent:{uri}")) {
                self.log(3, "no compiled output yet, so luau-lsp has not been given this file");
            }
            return;
        };

        if self.opened_in_child.contains(&generated) {
            // Whole-text sync: an edit to one `.luaux` character can move any
            // amount of generated code, so there is no useful incremental change.
            self.send_child(&build::notification(
                "textDocument/didChange",
                json!({
                    "textDocument": { "uri": generated, "version": version },
                    "contentChanges": [{ "text": text }],
                }),
            ));
            return;
        }

        self.opened_in_child.insert(generated.clone());

        if self.said_once.insert(format!("sent:{generated}")) {
            self.log(3, &format!("handed {generated} to luau-lsp ({} bytes)", text.len()));
        }

        self.send_child(&build::notification(
            "textDocument/didOpen",
            json!({
                "textDocument": {
                    "uri": generated,
                    "languageId": "luau",
                    "version": version,
                    "text": text,
                },
            }),
        ));
    }

    /// Tells the child to forget a document, by the URI it knows it under.
    fn close_in_child(&mut self, generated: &str) {
        if !self.opened_in_child.remove(generated) {
            return;
        }

        self.send_child(&build::notification(
            "textDocument/didClose",
            json!({ "textDocument": { "uri": generated } }),
        ));
    }

    // --- features ----------------------------------------------------------

    fn completion(&mut self, id: Value, params: Value) {
        let Some((uri, offset)) = self.locate(&params) else {
            return self.reply(id, Value::Null);
        };

        let Some(document) = self.documents.get(&uri) else {
            return self.reply(id, Value::Null);
        };
        let Some(path) = project::uri_to_path(&uri) else {
            return self.reply(id, Value::Null);
        };
        let project = self.projects.for_file(&path);

        let snippets = self.snippets;

        match completion::complete(document, &project, offset, snippets) {
            Completion::Ours(list) => self.reply(id, list),
            Completion::Nothing => self.reply(id, json!({ "isIncomplete": false, "items": [] })),
            Completion::Forward => self.forward(id, "textDocument/completion", params, None, false),
            Completion::ComponentProps { start, prefix } => {
                self.component_props(id, &uri, offset, start, &prefix, snippets)
            }
        }
    }

    /// Asks luau-lsp for a component's props, at the generated call it becomes.
    ///
    /// The position is *derived* rather than mapped: `<Row |/>` compiles to
    /// `Row({})`, and the empty slot between those braces — the one place the
    /// question can be asked — is generated text with no source counterpart, so
    /// there is nothing to map. It is computed from the component name's own run
    /// and then **checked** against the output before it is used, so a shape this
    /// does not recognise costs the feature rather than asking about somewhere
    /// else entirely.
    ///
    /// This is not the snapping decision 6 forbids. Nothing unmappable is
    /// reported as mapped: the answer is rebuilt as our own items, over a range
    /// in the source that the cursor is actually on.
    fn component_props(
        &mut self,
        id: Value,
        uri: &str,
        offset: usize,
        start: usize,
        prefix: &str,
        snippets: bool,
    ) {
        let empty = json!({ "isIncomplete": false, "items": [] });

        let (Some(document), Some(compiled), Some(generated)) =
            (self.documents.get(uri), self.compiled.get(uri), self.generated_uri(uri))
        else {
            return self.reply(id, empty);
        };

        // The element under the cursor, from the tree rather than the scan: the
        // name's *offset* is what the map is keyed by, and only the tree has it.
        let tags = crate::tree::tree(&document.text);
        let Some(tag) = crate::tree::innermost_at(&tags, offset) else {
            return self.reply(id, empty);
        };

        let Some(inside) = props_table(&compiled.map, &compiled.output, tag) else {
            return self.reply(id, empty);
        };

        let range = document.range_at(start, start + prefix.len());
        let name = tag.name.clone();
        let position = LineIndex::new(&compiled.output).position(&compiled.output, inside);

        if !self.child_ready {
            return self.reply(id, empty);
        }

        // Same reason as in `forward`: a document the child does not hold cannot
        // be asked about, and here the refusal would be logged as "no props".
        if !self.opened_in_child.contains(&generated) {
            self.sync_child(uri);
        }

        let request = json!({
            "textDocument": { "uri": generated },
            "position": { "line": position.line, "character": position.character },
        });

        let Some(proxy) = &mut self.proxy else {
            return self.reply(id, empty);
        };

        match proxy.request("textDocument/completion", request) {
            Ok(child_id) => {
                self.pending
                    .insert(child_id, Pending::ComponentProps { id, tag: name, range, snippets });
            }
            Err(error) => {
                self.log(1, &format!("luau-lsp: {error}"));
                self.reply(id, empty);
            }
        }
    }

    /// The child's answer about a component's props, as markup attributes.
    fn answer_component_props(
        &mut self,
        id: Value,
        tag: &str,
        range: Value,
        snippets: bool,
        body: Value,
    ) {
        let empty = json!({ "isIncomplete": false, "items": [] });

        // An error describes the child's condition, not this component. Reading
        // it as "no props" would put a cause on the record that was never
        // established.
        if body.get("error").is_some() {
            return self.reply(id, empty);
        }

        let result = body.get("result").cloned().unwrap_or(Value::Null);

        let items = match &result {
            Value::Array(items) => items.clone(),
            other => other.get("items").and_then(Value::as_array).cloned().unwrap_or_default(),
        };

        let mut list = completion::props(&items, &range, snippets);

        // A list it truncated is one we truncated too, whatever our filter kept.
        if result.get("isIncomplete") == Some(&Value::Bool(true)) {
            list["isIncomplete"] = json!(true);
        }

        if list["items"].as_array().is_none_or(Vec::is_empty) {
            self.no_props(tag);
        }

        self.reply(id, list);
    }

    /// Says that a component offered nothing, once per component per session.
    ///
    /// Silence is the wrong answer: an empty list is indistinguishable from a
    /// component that genuinely has no props. Naming *the* cause would be worse,
    /// though — luau-lsp reports "nothing is expected here" identically whether
    /// the props parameter is untyped, inferred as `any`, or declared behind a
    /// `__call` it will not infer through. So this states the fact and the two
    /// things worth checking, and asserts neither.
    fn no_props(&mut self, tag: &str) {
        if !self.said_once.insert(format!("props:{tag}")) {
            return;
        }

        self.log(
            3,
            &format!(
                "<{tag}>: luau-lsp knows of no props for this component. Either its props \
                 parameter is untyped or typed `any`, or it is a table made callable with \
                 `__call` — which luau-lsp type-checks but infers no argument type through. \
                 For the second, annotating the module's return with its call signature, as \
                 in `return (M :: any) :: (props: Props) -> Instance`, makes the props known \
                 here."
            ),
        );
    }

    fn hover(&mut self, id: Value, params: Value) {
        let Some((uri, offset)) = self.locate(&params) else {
            return self.reply(id, Value::Null);
        };
        let Some(path) = project::uri_to_path(&uri) else {
            return self.reply(id, Value::Null);
        };
        let project = self.projects.for_file(&path);
        let Some(document) = self.documents.get(&uri) else {
            return self.reply(id, Value::Null);
        };

        match hover::hover(document, &project, offset) {
            hover::Answer::Ours(value) => self.reply(id, value),
            hover::Answer::Nothing => self.reply(id, Value::Null),
            hover::Answer::Forward => self.forward(id, "textDocument/hover", params, None, false),
            // Ours travels as the fallback: `forward` answers with it alone when
            // the position does not map or there is no child to ask.
            //
            // `at` is asked about instead of the cursor, because `</Row>` names
            // the same component as `<Row>` while only the opening tag exists in
            // the generated code. This is not the snapping decision 6 forbids —
            // that is about reporting an unmappable position *as* a nearby one.
            // Here the question moves to where the symbol is and the answer's
            // range stays on the name the cursor is actually on.
            hover::Answer::Both { ours, at } => {
                let mut params = params;

                if let Some(position) =
                    self.documents.get(&uri).map(|document| document.range_at(at, at))
                {
                    params["position"] = position["start"].clone();
                }

                self.forward(id, "textDocument/hover", params, Some(ours), false)
            }
        }
    }

    fn definition(&mut self, id: Value, params: Value) {
        let Some((uri, offset)) = self.locate(&params) else {
            return self.reply(id, Value::Null);
        };
        let Some(path) = project::uri_to_path(&uri) else {
            return self.reply(id, Value::Null);
        };
        let project = self.projects.for_file(&path);
        let Some(document) = self.documents.get(&uri) else {
            return self.reply(id, Value::Null);
        };

        match hover::definition(document, &project, offset) {
            // Definition never asks for both — a component tag's binding is in
            // the `.luaux`, and that is the whole answer.
            hover::Answer::Ours(value) | hover::Answer::Both { ours: value, .. } => {
                self.reply(id, value)
            }
            hover::Answer::Nothing => self.reply(id, Value::Null),
            hover::Answer::Forward => {
                self.forward(id, "textDocument/definition", params, None, false)
            }
        }
    }

    fn document_symbols(&mut self, id: Value, params: Value) {
        let Some(uri) = params.pointer("/textDocument/uri").and_then(Value::as_str) else {
            return self.reply(id, Value::Null);
        };
        let Some(document) = self.documents.get(uri) else {
            return self.reply(id, Value::Null);
        };

        self.reply(id, symbols::symbols(document));
    }

    fn semantic_tokens(&mut self, id: Value, params: Value) {
        let Some(uri) = params.pointer("/textDocument/uri").and_then(Value::as_str) else {
            return self.reply(id, Value::Null);
        };
        let uri = uri.to_string();

        let Some(path) = project::uri_to_path(&uri) else {
            return self.reply(id, Value::Null);
        };
        let project = self.projects.for_file(&path);
        let Some(document) = self.documents.get(&uri) else {
            return self.reply(id, Value::Null);
        };

        self.reply(id, semantic_tokens::tokens(document, &project));
    }

    fn code_action(&mut self, id: Value, params: Value) {
        let Some(uri) = params.pointer("/textDocument/uri").and_then(Value::as_str) else {
            return self.reply(id, Value::Null);
        };
        let uri = uri.to_string();

        let context = params.get("context").cloned().unwrap_or(Value::Null);
        let mine = code_actions::actions(&uri, &code_actions::ours(&context));

        // Ours and luau-lsp's, merged — the two answer about different halves of
        // the same file.
        self.forward(id, "textDocument/codeAction", params, Some(mine), false);
    }

    /// What a `>` just typed leaves unclosed, so the client can close it.
    fn closing_tag(&mut self, id: Value, params: Value) {
        let Some((uri, offset)) = self.locate(&params) else {
            return self.reply(id, Value::Null);
        };
        let Some(document) = self.documents.get(&uri) else {
            return self.reply(id, Value::Null);
        };

        match scan::closing_tag(&document.text, offset) {
            Some(name) => self.reply(id, json!({ "tagName": name })),
            None => self.reply(id, Value::Null),
        }
    }

    fn prepare_rename(&mut self, id: Value, params: Value) {
        let Some((uri, offset)) = self.locate(&params) else {
            return self.reply(id, Value::Null);
        };
        let Some(document) = self.documents.get(&uri) else {
            return self.reply(id, Value::Null);
        };

        match rename::prepare(document, offset) {
            rename::Answer::Ours(range) => self.reply(id, range),
            rename::Answer::Nothing => self.reply(id, Value::Null),
            rename::Answer::Forward => {
                self.forward(id, "textDocument/prepareRename", params, None, false)
            }
        }
    }

    fn rename(&mut self, id: Value, params: Value) {
        let Some((uri, offset)) = self.locate(&params) else {
            return self.reply(id, Value::Null);
        };
        let new_name =
            params.get("newName").and_then(Value::as_str).unwrap_or_default().to_string();

        let Some(document) = self.documents.get(&uri) else {
            return self.reply(id, Value::Null);
        };

        match rename::rename(document, offset, &new_name) {
            rename::Answer::Ours(edit) => self.reply(id, edit),
            rename::Answer::Nothing => self.reply(id, Value::Null),
            // A Luau symbol. Its edits come back against the generated file, and
            // one that touches generated text makes the whole rename
            // inapplicable — hence all-or-nothing.
            rename::Answer::Forward => self.forward(id, "textDocument/rename", params, None, true),
        }
    }

    /// The document and byte offset a positional request is about.
    fn locate(&self, params: &Value) -> Option<(String, usize)> {
        let uri = params.pointer("/textDocument/uri")?.as_str()?.to_string();
        let document = self.documents.get(&uri)?;
        let offset = document.byte_offset(params.get("position")?)?;

        Some((uri, offset))
    }

    // --- plumbing ----------------------------------------------------------

    fn send(&mut self, message: &Value) {
        if self.trace {
            eprintln!("-> {message}");
        }
        let _ = self.writer.write(message);
    }

    fn send_child(&mut self, message: &Value) {
        if let Some(proxy) = &mut self.proxy {
            let _ = proxy.send(message);
        }
    }

    fn reply(&mut self, id: Value, result: Value) {
        self.send(&build::result(id, result));
    }

    fn error(&mut self, id: Value, code: i64, message: &str) {
        self.send(&build::error(id, code, message));
    }

    fn log(&mut self, level: u8, message: &str) {
        self.send(&build::notification(
            "window/logMessage",
            json!({ "type": level, "message": format!("luaux: {message}") }),
        ));
    }
}

/// Where the props table of a component's generated call begins.
///
/// `<Row Name={x}/>` becomes `Row({ Name = x })`, so the question "what may go
/// here" is asked just inside the `({`. That offset is derived from the one run
/// this element does have — its name — rather than searched for, and then
/// **checked**: the two bytes after the name must actually be `({`. A component
/// whose call is emitted in some other shape, or whose name did not map at all,
/// yields `None`, and the feature is simply unavailable there.
///
/// Deriving is what makes the empty case work. `<Row |/>` compiles to `Row({})`
/// — there is no attribute yet, so nothing in the source maps into the table,
/// and the position where the answer lives has no source counterpart at all.
fn props_table(
    map: &crate::sourcemap::SourceMap,
    output: &str,
    tag: &crate::tree::Tag,
) -> Option<usize> {
    let name = map.to_output(tag.open_name.0)?;
    let after = name + (tag.open_name.1 - tag.open_name.0);

    // The map may describe an *older* revision of the source: a compile that
    // fails keeps the last good one while the line count holds, and for LuauX a
    // failed compile is usually a name that is not in scope — which is to say,
    // exactly this edit. A run is byte-identical for the revision it was built
    // from and says nothing about this one, so the only way to know it still
    // describes this tag is to look.
    //
    // Without that, renaming `<Row/>` to `<Col/>` — same offset, same length,
    // and `Col` not yet defined, so it does not compile — derives a position
    // inside `Row(`'s call and offers Row's props under Col's name. Missing
    // beats wrong (decision 6).
    if output.get(name..after) != Some(tag.name.as_str()) {
        return None;
    }

    // `Row` then `({`, and nothing between: an intrinsic's `create("Frame")({`
    // does not reach here, since a class tag name records no run.
    (output.get(after..after + 2) == Some("({")).then_some(after + 2)
}

/// Whether an error from luau-lsp describes its own condition rather than the
/// request.
///
/// `ServerNotInitialized` and `MethodNotFound` mean "not me, not now" — the
/// person hovering a symbol cannot act on either, and relaying them puts a red
/// entry in the log for something they did not do wrong.
fn about_the_child(error: &Value) -> bool {
    matches!(error.get("code").and_then(Value::as_i64), Some(-32002 | METHOD_NOT_FOUND))
        || lost_the_document(error)
}

/// `RequestFailed`, because it is not holding the document that was asked about.
///
/// This names a generated path the author never opened and cannot do anything
/// about — `No managed text document for …/build/App.luau` — so relaying it puts
/// a red entry in their log for a bookkeeping mistake of ours. It is also
/// recoverable, which is the better reason not to pass it on: the document is
/// re-opened and the next request succeeds.
fn lost_the_document(error: &Value) -> bool {
    error.get("code").and_then(Value::as_i64) == Some(-32803)
        && error
            .get("message")
            .and_then(Value::as_str)
            .is_some_and(|message| message.contains("No managed text document"))
}

/// Joins our contribution to the child's, for the requests where both have
/// something to say.
fn merged(ours: Option<Value>, theirs: Value) -> Value {
    let Some(ours) = ours else { return theirs };

    match (ours, theirs) {
        (Value::Array(mut mine), Value::Array(theirs)) => {
            mine.extend(theirs);
            Value::Array(mine)
        }
        (mine, Value::Null) => mine,
        (mine, theirs) => match hovers(&mine, &theirs) {
            Some(both) => both,
            None => theirs,
        },
    }
}

/// Two hovers about the same thing, as one.
///
/// A component tag is both a tag and a Luau identifier, and the interesting half
/// is luau-lsp's: the inferred type, and the doc comment above the binding. Ours
/// is the provenance — that this resolved to a component rather than a class,
/// and where it came from — which is the one thing the generated code cannot
/// say. So its answer leads and ours follows it.
///
/// The range stays ours. Theirs is against the generated file, and although the
/// remap brings it home it covers the identifier as the *call* spells it, not
/// the tag the cursor is actually on.
fn hovers(ours: &Value, theirs: &Value) -> Option<Value> {
    let mine = ours.pointer("/contents/value")?.as_str()?;
    let theirs_text = theirs.pointer("/contents/value")?.as_str()?;

    // Everything ours says beyond the bare name it repeats. Without this the
    // reader gets the same identifier twice, in two code fences, for nothing.
    let note = mine.rsplit("```").next().unwrap_or_default().trim();

    let value = match note.is_empty() {
        true => theirs_text.to_string(),
        false => format!("{theirs_text}\n\n---\n\n{note}"),
    };

    Some(json!({
        "contents": { "kind": "markdown", "value": value },
        "range": ours.get("range").cloned().unwrap_or(Value::Null),
    }))
}

fn capabilities() -> Value {
    json!({
        // Incremental: a keystroke should not resend the file, since a compile
        // and a forward already run on every one.
        "textDocumentSync": { "openClose": true, "change": 2, "save": { "includeText": false } },
        "completionProvider": {
            // `<` and `/` are ours; the rest are luau-lsp's, and a completion
            // inside `{ … }` is forwarded to it.
            "triggerCharacters": ["<", "/", " ", ".", ":", "'", "\"", "\n"],
            "resolveProvider": false,
        },
        "hoverProvider": true,
        "definitionProvider": true,
        "typeDefinitionProvider": true,
        "referencesProvider": true,
        "documentHighlightProvider": true,
        "documentSymbolProvider": true,
        "signatureHelpProvider": { "triggerCharacters": ["(", ","] },
        "codeActionProvider": { "codeActionKinds": ["quickfix"] },
        "renameProvider": { "prepareProvider": true },
        "inlayHintProvider": true,
        "foldingRangeProvider": true,
        "selectionRangeProvider": true,
        "semanticTokensProvider": {
            "legend": { "tokenTypes": semantic_tokens::TYPES, "tokenModifiers": [] },
            "full": true,
        },
        // Deliberately absent: formatting. stylua formats Luau, and `.luaux` is
        // not Luau — running it over the generated file would produce edits that
        // cannot be mapped back, since formatting rewrites the text a map is
        // made of. Claiming the capability and mangling the file is worse than
        // not claiming it.
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capabilities_do_not_claim_formatting() {
        // Offering it would mean applying stylua's edits to a file it has never
        // seen. See the note beside the declaration.
        let capabilities = capabilities();
        assert!(capabilities.get("documentFormattingProvider").is_none());
        assert!(capabilities.get("documentRangeFormattingProvider").is_none());
    }

    #[test]
    fn the_semantic_token_legend_matches_what_is_emitted() {
        let legend = capabilities()["semanticTokensProvider"]["legend"]["tokenTypes"].clone();
        assert_eq!(legend, json!(semantic_tokens::TYPES));
    }

    /// The stale map is kept while line numbers still correspond, and for LuauX
    /// a failed compile is usually a name that is not in scope — so the revision
    /// being asked about routinely differs from the one the map describes.
    /// Renaming a component to another of the same length is enough to derive a
    /// position inside the *other* one's call.
    #[test]
    fn a_stale_map_does_not_offer_one_components_props_as_anothers() {
        use luaux::backend::Vide;
        use luaux::config::Config;

        let good =
            "local create = f()\nlocal function Row(p) return p end\nlocal e = <Row Name={n} />\n";
        let broken = good.replace("<Row Name", "<Col Name");

        let config = Config::with_create("create");
        let (output, _) =
            luaux::compile::compile_configured(good, &Vide, config.clone()).expect("compile");
        let map = crate::map_builder::build(good, &output, &config);

        // Unchanged, so the run still describes it: the feature keeps working
        // through an unrelated failure elsewhere in the file.
        let same = crate::tree::tree(good);
        assert!(props_table(&map, &output, &same[0]).is_some());

        // Renamed to the same length. `Col` maps onto `Row(` and every check but
        // the text agrees, which is what makes this worth pinning.
        let renamed = crate::tree::tree(&broken);
        assert_eq!(renamed[0].name, "Col");
        assert_eq!(renamed[0].open_name, same[0].open_name);
        assert_eq!(props_table(&map, &output, &renamed[0]), None);
    }

    #[test]
    fn errors_about_the_childs_state_are_not_the_editors_business() {
        // The cascade this prevents: a failed handshake, then every request for
        // the rest of the session logged as an error the user did not cause.
        assert!(about_the_child(&json!({ "code": -32002, "message": "server not initialized" })));
        assert!(about_the_child(&json!({ "code": METHOD_NOT_FOUND })));

        // Not holding the document is bookkeeping of ours, named against a
        // generated path the author never opened and cannot act on.
        assert!(about_the_child(&json!({
            "code": -32803,
            "message": "No managed text document for file:///p/build/App.luau",
        })));

        // But `RequestFailed` is not blanket-swallowed: a real one still travels.
        assert!(!about_the_child(&json!({ "code": -32803, "message": "internal failure" })));

        // A real complaint about the request still travels.
        assert!(!about_the_child(&json!({ "code": -32602, "message": "invalid params" })));
        assert!(!about_the_child(&json!({ "code": -32603, "message": "internal error" })));
        assert!(!about_the_child(&json!({ "message": "no code at all" })));
    }

    #[test]
    fn merging_keeps_both_contributions_in_order() {
        // Ours first: a quick fix for the thing under the cursor beats a
        // refactor for the enclosing function.
        let merged = merged(Some(json!([1, 2])), json!([3]));
        assert_eq!(merged, json!([1, 2, 3]));
    }

    /// A component tag hover: luau-lsp's type and doc comment lead, our note on
    /// where the tag resolved follows, and the range stays the tag's own.
    #[test]
    fn two_hovers_about_one_tag_become_one() {
        let ours = json!({
            "contents": { "kind": "markdown", "value": "```luau\nRow\n```\n\nComponent, bound on line 1." },
            "range": { "start": { "line": 5, "character": 11 }, "end": { "line": 5, "character": 14 } },
        });
        let theirs = json!({
            "contents": { "kind": "markdown", "value": "```luau\nlocal Row: (props: any) -> Frame\n```\n\nA row." },
            "range": { "start": { "line": 5, "character": 4 }, "end": { "line": 5, "character": 7 } },
        });

        let merged = merged(Some(ours.clone()), theirs);
        let text = merged["contents"]["value"].as_str().expect("markdown");

        // Theirs in full — the type and whatever doc comment came with it.
        assert!(text.contains("local Row: (props: any) -> Frame"), "{text}");
        assert!(text.contains("A row."), "{text}");
        // Ours reduced to what it alone knows: not the name over again.
        assert!(text.contains("Component, bound on line 1."), "{text}");
        assert!(!text.contains("```luau\nRow\n```"), "{text}");
        // Their range is against the generated call, not the tag under the cursor.
        assert_eq!(merged["range"], ours["range"]);
    }

    #[test]
    fn a_hover_with_nothing_of_ours_to_add_is_left_alone() {
        let theirs = json!({ "contents": { "kind": "markdown", "value": "```luau\nRow\n```" } });
        let ours = json!({ "contents": { "kind": "markdown", "value": "```luau\nRow\n```" } });

        let merged = merged(Some(ours), theirs.clone());
        assert_eq!(merged["contents"]["value"], theirs["contents"]["value"]);
    }

    #[test]
    fn merging_survives_the_child_having_nothing() {
        assert_eq!(merged(Some(json!([1])), Value::Null), json!([1]));
        assert_eq!(merged(None, json!([2])), json!([2]));
        assert_eq!(merged(None, Value::Null), Value::Null);
    }
}
