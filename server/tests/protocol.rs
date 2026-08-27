//! Driving a real server over stdio.
//!
//! Asserts on responses rather than internals, so these keep meaning something
//! when the inside is rearranged. Everything here runs without luau-lsp — the
//! configuration answer points at a path that does not exist — which also pins
//! the behaviour §9 asks for: no luau-lsp means markup features only, not a
//! refusal to start.

mod harness;

use harness::Server;
use serde_json::json;

const SOURCE: &str = "local create = f()\nlocal e = <Frame Size={size}/>\n";

#[test]
fn initialize_reports_both_versions() {
    let mut server = Server::start();
    let result = server.initialize();

    let version = result["serverInfo"]["version"].as_str().expect("a version");
    // A server built against a different compiler than the one producing
    // `build/` reports diagnostics the build does not, and that is invisible
    // unless both are stated.
    assert!(version.contains("luaux "), "{version}");
    assert!(result["capabilities"]["completionProvider"].is_object());
    assert!(result["capabilities"]["hoverProvider"].as_bool().unwrap_or(false));
}

#[test]
fn formatting_is_not_offered() {
    let mut server = Server::start();
    let result = server.initialize();

    // stylua formats Luau, and `.luaux` is not Luau. Claiming the capability
    // and mangling the file would be worse than not claiming it.
    assert!(result["capabilities"]["documentFormattingProvider"].is_null());
}

#[test]
fn a_compile_error_is_published_as_a_diagnostic() {
    let mut server = Server::started();
    server.open("local create = f()\nlocal e = <Frmae/>\n");

    let diagnostics = server.diagnostics();
    assert_eq!(diagnostics.len(), 1, "{diagnostics:#?}");
    assert_eq!(diagnostics[0]["source"], json!("luaux"));
    assert_eq!(diagnostics[0]["range"]["start"]["line"], json!(1));

    let related = diagnostics[0]["relatedInformation"][0]["message"].as_str().unwrap_or_default();
    assert!(related.contains("did you mean <Frame>"), "{related}");
}

#[test]
fn a_clean_file_publishes_nothing() {
    let mut server = Server::started();
    server.open(SOURCE);
    assert!(server.diagnostics().is_empty());
}

#[test]
fn editing_a_file_republishes() {
    let mut server = Server::started();
    server.open(SOURCE);
    assert!(server.diagnostics().is_empty());

    // Break it.
    server.change("local create = f()\nlocal e = <Frmae Size={size}/>\n");
    assert_eq!(server.diagnostics().len(), 1);

    // Fix it.
    server.change(SOURCE);
    assert!(server.diagnostics().is_empty());
}

#[test]
fn completion_offers_classes_on_a_half_typed_tag() {
    let mut server = Server::started();
    server.open("local create = f()\nlocal e = <Fra\n");

    let items = server.completion(1, 14);
    let labels: Vec<&str> = items.iter().filter_map(|item| item["label"].as_str()).collect();

    assert!(labels.contains(&"Frame"), "{labels:?}");
    // The file does not compile, and that is the normal state of one being
    // typed.
    assert!(!server.diagnostics().is_empty());
}

#[test]
fn completion_offers_a_classes_own_members() {
    let mut server = Server::started();
    server.open("local create = f()\nlocal e = <TextLabel Te\n");

    let items = server.completion(1, 23);
    let labels: Vec<&str> = items.iter().filter_map(|item| item["label"].as_str()).collect();

    assert!(labels.contains(&"Text"), "{labels:?}");
    assert!(labels.contains(&"BackgroundColor3"), "{labels:?}");
}

/// Completion is the one feature another language server can be given, so
/// switching it off has to take nothing else with it.
#[test]
fn completion_can_be_switched_off() {
    let mut server = Server::start();
    server.initialize();
    server.luaux = json!({ "completion": { "enabled": false } });
    server.initialized();
    server.open(SOURCE);

    // The attribute name, which normally offers the class's own members.
    assert!(server.completion(1, 17).is_empty());

    // The rest of the server is untouched. This setting is about completion,
    // not about whether to run.
    assert!(server.diagnostics().is_empty());
    let hover = server.request("textDocument/hover", server.at(1, 12));
    let text = hover["contents"]["value"].as_str().expect("hover text");
    assert!(text.contains("Roblox class"), "{text}");

    // Switching it back on takes effect where it was switched off: in the
    // settings, without a restart.
    server.luaux = json!({ "completion": { "enabled": true } });
    server.notify("workspace/didChangeConfiguration", json!({ "settings": {} }));
    server.answer_configuration();

    assert!(!server.completion(1, 17).is_empty());
}

#[test]
fn hover_on_a_tag_says_what_it_resolved_to() {
    let mut server = Server::started();
    server.open("local create = f()\nlocal Row = f()\nlocal e = <Row/>\n");

    let hover = server.request("textDocument/hover", server.at(2, 12));
    let text = hover["contents"]["value"].as_str().expect("hover text");
    assert!(text.contains("Component"), "{text}");
}

#[test]
fn document_symbols_are_the_element_tree() {
    let mut server = Server::started();
    server.open("local create = f()\nlocal e = <Frame><TextLabel/></Frame>\n");

    let symbols = server.request(
        "textDocument/documentSymbol",
        json!({
            "textDocument": { "uri": server.uri() },
        }),
    );

    assert_eq!(symbols[0]["name"], json!("Frame"));
    assert_eq!(symbols[0]["children"][0]["name"], json!("TextLabel"));
}

#[test]
fn a_quick_fix_is_offered_for_a_misspelt_element() {
    let mut server = Server::started();
    server.open("local create = f()\nlocal e = <Frmae/>\n");

    let diagnostics = server.diagnostics();
    let actions = server.request(
        "textDocument/codeAction",
        json!({
            "textDocument": { "uri": server.uri() },
            "range": diagnostics[0]["range"],
            "context": { "diagnostics": diagnostics },
        }),
    );

    assert_eq!(actions[0]["title"], json!("Change element to Frame"));
    assert_eq!(actions[0]["edit"]["changes"][server.uri()][0]["newText"], json!("Frame"));
}

#[test]
fn renaming_a_tag_renames_both_halves() {
    let mut server = Server::started();
    server.open("local create = f()\nlocal e = <Frame></Frame>\n");

    let mut params = server.at(1, 12);
    params["newName"] = json!("Panel");
    let edit = server.request("textDocument/rename", params);

    let edits = edit["changes"][server.uri()].as_array().expect("edits");
    assert_eq!(edits.len(), 2);
    assert!(edits.iter().all(|edit| edit["newText"] == json!("Panel")));
}

#[test]
fn semantic_tokens_distinguish_a_component_from_a_class() {
    let mut server = Server::started();
    server.open("local create = f()\nlocal Row = f()\nlocal e = <Frame><Row/></Frame>\n");

    let tokens = server.request(
        "textDocument/semanticTokens/full",
        json!({
            "textDocument": { "uri": server.uri() },
        }),
    );

    let data: Vec<u64> = serde_json::from_value(tokens["data"].clone()).expect("data");
    // Three names, five numbers each: Frame, Row, Frame.
    assert_eq!(data.len(), 15, "{data:?}");
    // Token type: class, function, class.
    assert_eq!([data[3], data[8], data[13]], [0, 1, 0]);
}

#[test]
fn without_luau_lsp_a_forwarded_request_answers_rather_than_hangs() {
    let mut server = Server::started();
    server.open(SOURCE);

    // Inside the captured expression, which is luau-lsp's to answer. It is not
    // there, so the honest answer is nothing — arriving promptly.
    let hover = server.request("textDocument/hover", server.at(1, 23));
    assert!(hover.is_null(), "{hover}");
}

/// A stand-in luau-lsp that answers `initialize` with an error and then refuses
/// everything, the way a real one does when its handshake fails.
///
/// Treating that as a successful handshake used to mark the child ready, and
/// every request for the rest of the session came back as
/// `-32002 server not initialized` — one clear failure turned into an endless
/// stream of confusing ones.
///
/// Written as a Node script plus a launcher, rather than one file with a
/// shebang: Windows has no shebang, so a single script is not something
/// `Command::new` can start there at all.
fn refusing_child(root: &std::path::Path) -> String {
    let script = root.join("refusing-luau-lsp.js");

    std::fs::write(
        &script,
        r#"
if (process.argv.includes("--version")) { console.log("0.0.0-refuses"); process.exit(0); }
let buffer = "";
process.stdin.on("data", (chunk) => {
  buffer += chunk;
  for (;;) {
    const split = buffer.indexOf("\r\n\r\n");
    if (split < 0) return;
    const length = Number(/Content-Length: (\d+)/i.exec(buffer.slice(0, split))[1]);
    if (buffer.length < split + 4 + length) return;
    const message = JSON.parse(buffer.slice(split + 4, split + 4 + length));
    buffer = buffer.slice(split + 4 + length);
    if (message.id === undefined) continue;
    const body = JSON.stringify({
      jsonrpc: "2.0",
      id: message.id,
      error: { code: -32002, message: "server not initialized" },
    });
    process.stdout.write(`Content-Length: ${Buffer.byteLength(body)}\r\n\r\n${body}`);
  }
});
"#,
    )
    .expect("write stand-in");

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        let launcher = root.join("refusing-luau-lsp");
        std::fs::write(
            &launcher,
            format!("#!/bin/sh\nexec node \"{}\" \"$@\"\n", script.display()),
        )
        .expect("write launcher");
        std::fs::set_permissions(&launcher, std::fs::Permissions::from_mode(0o755)).expect("chmod");

        launcher.display().to_string()
    }

    #[cfg(not(unix))]
    {
        let launcher = root.join("refusing-luau-lsp.cmd");
        std::fs::write(&launcher, format!("@node \"{}\" %*\r\n", script.display()))
            .expect("write launcher");

        launcher.display().to_string()
    }
}

#[test]
fn a_child_that_refuses_to_initialize_is_not_treated_as_ready() {
    let directory = harness::TempDirectory::new();
    let command = refusing_child(&directory.path);

    let mut server = Server::with_luau_lsp(Some(command));
    server.initialize();
    server.initialized();
    server.open(SOURCE);

    // Said once, plainly, and not once per keystroke afterwards.
    let complaint = server.log_containing("refused to initialize");
    assert_eq!(complaint["params"]["type"], json!(1), "{complaint}");

    // And a forwarded request answers rather than surfacing the child's state as
    // an error against the user's document.
    let hover = server.request("textDocument/hover", server.at(1, 23));
    assert!(hover.is_null(), "{hover}");
}

/// Option+Space types `\u{a0}`, which the LuauX parser does not count as
/// whitespace — so it fails at that byte with a zero-length error. Widening that
/// by one *byte* landed inside the character, and the slice panicked: the server
/// exited with code 101 and took every feature with it.
#[test]
fn a_non_breaking_space_does_not_kill_the_server() {
    let mut server = Server::started();
    server.open("local create = f()\nlocal e = <TextButton\u{a0}Text='x'/>\n");

    assert!(!server.diagnostics().is_empty());

    // Still answering afterwards, which is the part that used to be untrue.
    let items = server.completion(1, 21);
    assert!(!items.is_empty(), "the server stopped answering");

    let symbols = server
        .request("textDocument/documentSymbol", json!({ "textDocument": { "uri": server.uri() } }));
    assert!(symbols.is_array());
}

#[test]
fn multi_byte_text_survives_every_feature() {
    let mut server = Server::started();
    server.open("local create = f()\nlocal e = <TextLabel>héllo — 😀 {n}</TextLabel>\n");

    let _ = server.diagnostics();

    // Positions in and around the astral character, in every feature that takes
    // one. None of these may bring the process down.
    for character in 20..40 {
        let _ = server.request("textDocument/hover", server.at(1, character));
        let _ = server.completion(1, character);
    }

    let symbols = server
        .request("textDocument/documentSymbol", json!({ "textDocument": { "uri": server.uri() } }));
    assert_eq!(symbols[0]["name"], json!("TextLabel"), "{symbols}");
}

#[test]
fn an_opened_tag_reports_what_would_close_it() {
    let mut server = Server::started();
    server.open("local create = f()\nlocal e = <Frame>\n");

    // Just after the `>` on line 1.
    let answer = server.request("luaux/closingTag", server.at(1, 17));
    assert_eq!(answer["tagName"], json!("Frame"));

    // One that closed itself has nothing to add.
    server.change("local create = f()\nlocal e = <Frame/>\n");
    assert!(server.request("luaux/closingTag", server.at(1, 18)).is_null());

    // Nor does a comparison, whatever it ends in.
    server.change("local create = f()\nlocal ok = a > b\n");
    assert!(server.request("luaux/closingTag", server.at(1, 14)).is_null());
}

#[test]
fn an_unknown_request_is_refused_rather_than_ignored() {
    let mut server = Server::started();
    let error = server.request_error("textDocument/somethingNew", json!({}));
    assert_eq!(error["code"], json!(-32601));
}

/// `[elements] all` renames every class at once, and a rename retires the
/// original — so the same file's completion has to change wholesale when
/// `luaux.toml` does, not just for the file that was edited (lsp-update.md §4).
#[test]
fn a_casing_scheme_takes_effect_when_luaux_toml_changes() {
    let mut server = Server::started();
    server.open("local create = f()\nlocal e = <\n");

    let before: Vec<String> = server
        .completion(1, 11)
        .iter()
        .filter_map(|item| item["label"].as_str())
        .map(str::to_string)
        .collect();

    assert!(before.contains(&"TextLabel".to_string()), "{:?}", &before[..5.min(before.len())]);

    std::fs::write(
        server.root().join("luaux.toml"),
        "[build]\nin = \"src\"\nout = \"build\"\n\n[elements]\nall = \"snake_case\"\n",
    )
    .expect("luaux.toml");
    server.notify("workspace/didChangeWatchedFiles", json!({ "changes": [] }));

    let after: Vec<String> = server
        .completion(1, 11)
        .iter()
        .filter_map(|item| item["label"].as_str())
        .map(str::to_string)
        .collect();

    assert!(after.contains(&"text_label".to_string()), "{:?}", &after[..5.min(after.len())]);
    // The canonical spelling is now an error, so offering it would offer the
    // one thing that cannot be written.
    assert!(!after.contains(&"TextLabel".to_string()), "the retired spelling survived");
}
