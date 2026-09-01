//! The proxy, against a real luau-lsp.
//!
//! Opt-in, and skipped with a reason when the binary is absent — mirroring how
//! the compiler's golden runtime tests treat Vide and Lune. Integration claims
//! that are never executed are not claims, and a test that passes because it
//! quietly did nothing is worse than no test.
//!
//! Set `LUAU_LSP` to point at a specific binary; otherwise the same discovery
//! the server itself uses applies.

mod harness;

use harness::Server;
use serde_json::{json, Value};

/// A type error inside a captured expression, so mapping back has to move a
/// column and not just trust the line.
const SOURCE: &str = "\
local create = nil :: any
local n: number = 1
local e = <Frame Size={n + \"x\"}/>
";

/// Where `n + "x"` sits on line 2 of the source.
const EXPRESSION: (u64, u64) = (23, 31);

macro_rules! server {
    () => {
        match harness::find_luau_lsp() {
            Some(path) => {
                let mut server = Server::with_luau_lsp(Some(path));
                server.initialize();
                server.initialized();
                server
            }
            None => {
                eprintln!(
                    "skipped: no working luau-lsp found. Set LUAU_LSP=<path> to run this test."
                );
                return;
            }
        }
    };
}

/// Diagnostics until one of luau-lsp's own arrives.
fn theirs(server: &mut Server) -> Vec<Value> {
    server
        .diagnostics_until(|diagnostics| {
            diagnostics.iter().any(|diagnostic| diagnostic["source"] == json!("Luau"))
        })
        .into_iter()
        .filter(|diagnostic| diagnostic["source"] == json!("Luau"))
        .collect()
}

/// Interpolating something that is not a source is not a type error.
///
/// `{count}` in text is compiled into a call to the generated `__luaux_read`,
/// and a bad signature on that helper blames the *author* for writing an
/// ordinary value: a number, a string, a table. Every generated helper is a type
/// the user never wrote and cannot fix, so its signature is this repository's
/// problem even though it is emitted by the compiler.
///
/// Asserting an absence needs a positive signal first, or the test passes while
/// luau-lsp is still starting and has said nothing at all. The annotation on
/// line 2 is a type error we know arrives; once it has, the file has been
/// analysed and the silence on line 3 means something.
///
/// The signal has to be a *type* error specifically. `bad` is also unused, so
/// line 2 carries a lint as well — and a lint arrives even when type checking
/// has stopped running, which is exactly the regression that would make this
/// test pass while proving nothing.
#[test]
fn a_plain_value_in_interpolated_text_is_not_a_type_error() {
    let mut server = server!();
    server.open(
        "local create = nil :: any\n\
         local n = 30\n\
         local bad: string = 1\n\
         local e = <TextLabel>Ammo {n}</TextLabel>\n\
         return e\n",
    );

    let diagnostics = theirs(&mut server);

    let seeded = diagnostics.iter().any(|diagnostic| {
        diagnostic["range"]["start"]["line"] == json!(2)
            && diagnostic["message"].as_str().is_some_and(|message| message.contains("TypeError"))
    });
    assert!(seeded, "the seeded type error never arrived: {diagnostics:#?}");

    // Line 3 is the interpolation, and there is nothing wrong with it.
    for diagnostic in &diagnostics {
        assert_ne!(
            diagnostic["range"]["start"]["line"],
            json!(3),
            "a plain value in interpolated text was blamed: {diagnostic:#?}"
        );
    }
}

/// A spread shared by sibling elements answers on every one of them.
///
/// Sharing one props table across siblings is the ordinary way to write this.
/// The spread was the only captured expression with no generated text in front
/// of it to anchor on, so it fell back to searching the region — which refuses
/// any match that occurs again later. Every sibling but the last was silent.
#[test]
fn a_spread_repeated_across_siblings_answers_on_each() {
    let mut server = server!();
    server.open(
        "local create = nil :: any\n\
         local row = { Size = 1 }\n\
         local e = (\n\
         \t<Frame>\n\
         \t\t<TextButton {row} />\n\
         \t\t<TextButton {row} />\n\
         \t\t<TextButton {row} />\n\
         \t</Frame>\n\
         )\n",
    );
    let _ = theirs(&mut server);

    // `row` starts at character 15 on each of lines 4, 5 and 6.
    for line in 4..=6 {
        let hover = server.request("textDocument/hover", server.at(line, 15));
        let text = hover["contents"]["value"].as_str().unwrap_or_default();

        assert!(text.contains("Size"), "line {line} did not answer: {hover:#?}");
    }
}

/// A one-character expression answers.
///
/// Anything under `MIN_SEARCH` bytes could not be searched for at all, so on the
/// unanchored paths it was unmappable however unique it was — and `i`, `n` and
/// `x` are ordinary names.
#[test]
fn a_short_spread_answers() {
    let mut server = server!();
    server.open(
        "local create = nil :: any\n\
         local p = { Size = 1 }\n\
         local e = <TextButton {p} />\n",
    );
    let _ = theirs(&mut server);

    let hover = server.request("textDocument/hover", server.at(2, 23));
    let text = hover["contents"]["value"].as_str().unwrap_or_default();

    assert!(text.contains("Size"), "{hover:#?}");
}

#[test]
fn luau_type_errors_come_back_on_the_luaux_line() {
    let mut server = server!();
    server.open(SOURCE);

    let diagnostics = theirs(&mut server);

    // The type error is reported against the expression as the author wrote it,
    // not against the `create("Frame")({ Size = ` that surrounds it in the
    // generated file — which is at a different column entirely.
    let on_the_expression = diagnostics.iter().any(|diagnostic| {
        let start = &diagnostic["range"]["start"];
        start["line"] == json!(2)
            && start["character"]
                .as_u64()
                .is_some_and(|at| (EXPRESSION.0..=EXPRESSION.1).contains(&at))
    });

    assert!(on_the_expression, "{diagnostics:#?}");

    // And nothing landed anywhere the author did not write.
    for diagnostic in &diagnostics {
        let line = diagnostic["range"]["start"]["line"].as_u64().expect("line");
        assert!(line <= 2, "line {line} is past the end of the file: {diagnostic:#?}");
    }
}

/// luau-lsp offers diagnostics push *or* pull, and chooses from the client's
/// capabilities — which, here, are the editor's, replayed to it.
///
/// VS Code advertises pull. Forwarded unaltered, that switches the child to
/// pull-only: it accepts the document, analyses it, and never publishes, while
/// this server — which asks for nothing that way — waits for a push that never
/// comes. Every Luau diagnostic disappears from `.luaux` with nothing logged and
/// no request failed, which is the hardest possible shape of failure to find.
///
/// The harness advertises what VS Code does, so this is really pinned by every
/// diagnostic test at once; it exists to say *why* they would all fail.
#[test]
fn a_pull_capable_editor_still_gets_the_childs_diagnostics() {
    let mut server = server!();
    server.open(SOURCE);

    assert!(!theirs(&mut server).is_empty(), "luau-lsp published nothing");
}

#[test]
fn the_generated_file_need_not_exist_on_disk() {
    // `build/` may be stale, or absent entirely on a fresh clone. The server
    // hands luau-lsp the text over `didOpen`, and Phase 0 established that it
    // honours that over what is on disk.
    let mut server = server!();
    assert!(!harness::build_path(server.root()).exists());

    server.open(SOURCE);
    assert!(!theirs(&mut server).is_empty());
}

#[test]
fn hover_inside_an_expression_is_answered_by_luau_lsp() {
    let mut server = server!();
    server.open(SOURCE);
    // Let it finish indexing before asking.
    let _ = theirs(&mut server);

    // On `n` inside `{n + "x"}`.
    let hover = server.request("textDocument/hover", server.at(2, 23));
    let text = hover["contents"]["value"].as_str().unwrap_or_default();

    assert!(text.contains("number"), "{hover:#?}");
}

#[test]
fn completion_inside_an_expression_is_forwarded() {
    let mut server = server!();
    server.open("local create = nil :: any\nlocal count = 1\nlocal e = <Frame Size={cou}/>\n");
    // Its diagnostics arriving is the signal that it has the document.
    let _ = theirs(&mut server);

    // The caret sits at the end of `cou`, which is the run's last byte plus one
    // — the ordinary case, and the one an exclusive range would refuse.
    let items = server.completion(2, 26);
    let labels: Vec<&str> = items.iter().filter_map(|item| item["label"].as_str()).collect();

    // `count` is a Luau symbol; we do not know it and never claimed to.
    assert!(labels.contains(&"count"), "{labels:?}");
}

#[test]
fn a_definition_inside_an_expression_maps_home() {
    let mut server = server!();
    server.open("local create = nil :: any\nlocal count = 1\nlocal e = <Frame Size={count}/>\n");
    let _ = theirs(&mut server);

    let result = server.request("textDocument/definition", server.at(2, 23));
    let location = if result.is_array() { result[0].clone() } else { result };

    assert_eq!(location["uri"], json!(server.uri()), "{location:#?}");
    // `count` is declared on line 1 of the `.luaux`, not wherever it landed in
    // the generated file.
    assert_eq!(location["range"]["start"]["line"], json!(1), "{location:#?}");
}

#[test]
fn our_diagnostics_and_theirs_are_published_together() {
    let mut server = server!();

    // The static-conditional-child lint is ours; the unassignable `n` is
    // luau-lsp's. Deliberately a file that *compiles* — a fatal error of ours
    // means there is no generated Luau for luau-lsp to have an opinion about,
    // and the merge is only observable when both halves exist.
    server.open(
        "local create = nil :: any\nlocal n: number = \"a\"\nlocal e = <Frame>{n() and <TextLabel/> or nil}</Frame>\n",
    );

    let diagnostics = server.diagnostics_until(|diagnostics| {
        diagnostics.iter().any(|d| d["source"] == json!("luaux"))
            && diagnostics.iter().any(|d| d["source"] == json!("Luau"))
    });

    assert!(diagnostics.len() >= 2, "{diagnostics:#?}");
}

/// Roblox's API types have to reach luau-lsp, or the generated file gets worse
/// answers than the same code written by hand: `UDim2` is an unknown global,
/// nothing constrains a component's props, and every inferred type collapses to
/// a free variable.
///
/// They are not in settings — the luau-lsp extension downloads them into its own
/// storage — so passing settings through is not enough.
#[test]
fn roblox_globals_reach_luau_lsp() {
    if luaux_lsp::proxy::luau_lsp_storage().is_none() {
        eprintln!("skipped: the luau-lsp extension has downloaded no Roblox definitions here");
        return;
    }

    let mut server = server!();
    server.open(
        "local create = (nil :: any) :: (n: string) -> (p: any) -> Instance\nlocal e = <Frame Size={UDim2.fromScale(1, 1)}/>\n",
    );

    // A clean file gives luau-lsp nothing to publish, so there is no diagnostic
    // to wait on — ask until it has indexed, the way an editor's next keystroke
    // would. Asking something only the definitions can answer is the point: an
    // empty diagnostic list is not proof that it looked.
    let mut hover = Value::Null;
    for _ in 0..40 {
        hover = server.request("textDocument/hover", server.at(1, 23));
        if hover["contents"]["value"].as_str().is_some_and(|text| text.contains("UDim2")) {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(250));
    }

    let text = hover["contents"]["value"].as_str().unwrap_or_default();
    assert!(text.contains("UDim2"), "UDim2 is unknown to luau-lsp: {hover:#?}");

    let complaints: Vec<String> = server
        .diagnostics()
        .iter()
        .filter_map(|diagnostic| diagnostic["message"].as_str())
        .filter(|message| message.contains("Unknown global"))
        .map(str::to_string)
        .collect();
    assert!(complaints.is_empty(), "{complaints:?}");
}

/// Spawned and told `initialized`, but with configuration unanswered — so
/// luau-lsp does not exist yet, and anything opened now arrives before it.
macro_rules! server_before_the_child {
    () => {
        match harness::find_luau_lsp() {
            Some(path) => {
                let mut server = Server::with_luau_lsp(Some(path));
                server.initialize();
                server.notify("initialized", json!({}));
                server
            }
            None => {
                eprintln!(
                    "skipped: no working luau-lsp found. Set LUAU_LSP=<path> to run this test."
                );
                return;
            }
        }
    };
}

/// Reloading a window with a `.luaux` already open lands the document here:
/// before the editor has answered configuration, so before there is a child to
/// hand it to. It has to be replayed once one exists, or the file that was open
/// when the editor started is the one file luau-lsp never hears about.
#[test]
fn a_document_opened_before_the_child_existed_still_reaches_it() {
    let mut server = server_before_the_child!();

    server.open(SOURCE);
    server.answer_configuration();

    assert!(!theirs(&mut server).is_empty());
}

/// The same ordering, but the file does not compile when the child arrives.
///
/// There is no generated Luau to hand over at that moment, so the replay has
/// nothing to send — and a file being mid-edit when the editor starts is
/// ordinary, not exotic. Whatever happens, the first compile that succeeds after
/// that has to reach luau-lsp, or the document stays invisible to it for the
/// rest of the session while every forwarded request quietly answers nothing.
#[test]
fn a_file_that_only_compiles_later_still_reaches_luau_lsp() {
    let mut server = server_before_the_child!();

    server.open("local create = nil :: any\nlocal e = <Frmae/>\n");
    server.answer_configuration();

    // Ours arrives regardless; it is luau-lsp that has been handed nothing.
    let _ = server.diagnostics_until(|diagnostics| !diagnostics.is_empty());

    server.change(SOURCE);
    assert!(!theirs(&mut server).is_empty());
}

/// Hovering a component tag should say what the *thing* is, not just that it is
/// a component.
///
/// `<Row/>` compiles to `Row(...)` — the identifier the author bound — so its
/// type and the doc comment above it are luau-lsp's to answer, and asking is the
/// only way to get them. Ours adds the one thing the generated code destroys:
/// that this resolved to a component rather than a class, and where from.
#[test]
fn a_component_tag_hovers_with_its_luau_type() {
    let mut server = server!();

    server.open(COMPONENT);
    let _ = theirs(&mut server);

    // On `Row` in the *opening* tag.
    let hover = hover_until_typed(&mut server, 5, 12);
    let text = hover["contents"]["value"].as_str().unwrap_or_default();

    // luau-lsp's half: the signature, and the doc comment above the binding.
    assert!(text.contains("Row(props: { Name: string })"), "{text}");
    assert!(text.contains("A row of things."), "{text}");
    // Ours: that it is a component at all, which `Row(...)` cannot say.
    assert!(text.contains("Component"), "{text}");

    // And the range is the tag under the cursor, not wherever the call landed.
    assert_eq!(hover["range"]["start"]["line"], json!(5), "{hover:#?}");
    assert_eq!(hover["range"]["start"]["character"], json!(11), "{hover:#?}");
}

const COMPONENT: &str = "\
local create = nil :: any
--- A row of things.
local function Row(props: { Name: string }): number
\treturn 1
end
local e = <Row Name='a'></Row>
";

fn hover_until_typed(server: &mut Server, line: u64, character: u64) -> Value {
    let mut hover = Value::Null;

    for _ in 0..40 {
        hover = server.request("textDocument/hover", server.at(line, character));
        if hover["contents"]["value"].as_str().is_some_and(|text| text.contains("props")) {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(250));
    }

    hover
}

/// The closing tag names the same component, and a person hovering it is asking
/// the same question — but an element is emitted once, so `</Row>` has no
/// counterpart in the generated code to forward to. The question moves to the
/// opening tag; the answer stays about the name under the cursor.
#[test]
fn a_closing_component_tag_hovers_like_its_opening_one() {
    let mut server = server!();

    server.open(COMPONENT);
    let _ = theirs(&mut server);

    // `local e = <Row Name='a'></Row>` — the closing name starts at 26.
    let character = 26;
    let hover = hover_until_typed(&mut server, 5, character);
    let text = hover["contents"]["value"].as_str().unwrap_or_default();

    assert!(text.contains("Row(props: { Name: string })"), "{text}");
    assert!(text.contains("A row of things."), "{text}");

    // The range is the *closing* name, so the editor underlines what was hovered.
    assert_eq!(hover["range"]["start"]["character"], json!(character), "{hover:#?}");
}

/// The editor's request ids and the child's are different number spaces, so a
/// cancellation has to be translated like every other id.
///
/// Untranslated, `$/cancelRequest` for the editor's request 1 cancels *our*
/// request 1 to the child — which is the handshake. Reloading a window with a
/// `.luaux` already open is enough to produce it: the editor fires requests and
/// cancels them while the child is still starting, luau-lsp answers `initialize`
/// with "request cancelled by client", and the whole session degrades to markup
/// features for as long as the window lives.
#[test]
fn a_cancelled_editor_request_does_not_cancel_the_childs_handshake() {
    let mut server = server_before_the_child!();

    server.open(SOURCE);
    // Starts the child, which is sent `initialize` under *our* id 1.
    server.answer_configuration();

    // The editor cancelling its own request 1, which is a different request
    // entirely — and on a reload it has usually issued several by now.
    server.notify("$/cancelRequest", json!({ "id": 1 }));

    assert!(!theirs(&mut server).is_empty(), "luau-lsp never answered");
}

/// A document the child was never told to open cannot be asked about.
///
/// The opening is skipped when there is no generated Luau yet — a file that does
/// not compile has none — and a later `didChange` is not a substitute: luau-lsp
/// takes the text well enough to publish diagnostics, but the document is not
/// *managed*, so every request against it fails with "No managed text document"
/// for the rest of the session.
#[test]
fn a_file_that_compiled_late_can_still_be_asked_about() {
    let mut server = server_before_the_child!();

    // Broken when the child arrives, so there is nothing to hand it.
    server.open("local create = nil :: any\nlocal e = <Frmae/>\n");
    server.answer_configuration();
    let _ = server.diagnostics_until(|diagnostics| !diagnostics.is_empty());

    // Fixed. This is the first time there has ever been Luau to give it.
    server.change(SOURCE);
    let _ = theirs(&mut server);

    // Anything forwarded whole-document is enough to show it: no position to
    // map, so nothing but the child's own state can refuse it.
    server.request("textDocument/foldingRange", json!({ "textDocument": { "uri": server.uri() } }));
}

/// A type error in a prop is reported against the value the author wrote.
///
/// `<Button Label={3}/>` becomes `Button({ Label = 3 })`, so the complaint comes
/// back against the generated table — and lands on the `3` inside the braces,
/// which is the only part of it the author typed.
///
/// Needs the new solver: without it luau-lsp blames the whole table constructor,
/// which is generated text, and a diagnostic there is dropped rather than moved
/// to whatever is nearest. Needs strict mode too — nonstrict reports no type
/// errors at all.
#[test]
fn a_type_error_in_a_component_prop_lands_on_the_value() {
    let mut server = match harness::find_luau_lsp() {
        Some(path) => {
            let mut server = Server::with_luau_lsp(Some(path));
            server.settings = json!({ "fflags": { "enableNewSolver": true } });
            server.initialize();
            server.initialized();
            server
        }
        None => {
            eprintln!("skipped: no working luau-lsp found. Set LUAU_LSP=<path> to run this test.");
            return;
        }
    };

    server.open(
        "\
--!strict
local create = (nil :: any) :: (n: string) -> (p: any) -> any
local function Button(props: { Label: string })
\treturn create(\"TextButton\")({ Text = props.Label })
end
local e = <Button Label={3} />
",
    );

    let mismatch = server
        .diagnostics_until(|diagnostics| {
            diagnostics
                .iter()
                .any(|d| d["message"].as_str().is_some_and(|m| m.contains("but got 'number'")))
        })
        .into_iter()
        .find(|d| d["message"].as_str().is_some_and(|m| m.contains("but got 'number'")))
        .expect("the mismatch");

    // On the `3` the author wrote, not on the `Button({ ` around it:
    // `local e = <Button Label={3} />` puts the `{` at 24 and the `3` at 25.
    assert_eq!(mismatch["range"]["start"]["line"], json!(5), "{mismatch:#?}");
    assert_eq!(mismatch["range"]["start"]["character"], json!(25), "{mismatch:#?}");
    assert_eq!(mismatch["range"]["end"]["character"], json!(26), "{mismatch:#?}");
}

/// A component's props are declared in Luau, so they are luau-lsp's to know —
/// the tag compiles to a call and its attributes to that call's table argument.
///
/// The position asked about is *derived*: `<Row |/>` becomes `Row({})`, and the
/// empty slot between those braces has no source counterpart to map to.
#[test]
fn a_functional_components_props_are_offered_as_attributes() {
    let mut server = server!();

    server.open(
        "\
local create = nil :: any
local function Row(props: { Name: string, OnClick: () -> () })
\treturn create(\"Frame\")(props)
end
local e = <Row />
",
    );
    let _ = theirs(&mut server);

    // Inside the tag, where an attribute goes.
    let mut labels: Vec<String> = Vec::new();
    for _ in 0..40 {
        labels = server
            .completion(4, 15)
            .iter()
            .filter_map(|item| item["label"].as_str())
            .map(str::to_string)
            .collect();

        if labels.iter().any(|label| label == "Name") {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(250));
    }

    assert!(labels.contains(&"Name".to_string()), "{labels:?}");
    assert!(labels.contains(&"OnClick".to_string()), "{labels:?}");
    // And *only* the props: the same position in Luau would offer the whole
    // scope, which is nonsense as an attribute list.
    assert!(!labels.iter().any(|label| label == "print"), "{labels:?}");
    assert!(labels.len() < 10, "{labels:?}");
}

/// A table made callable with `__call` is the shape luau-lsp will not infer an
/// argument type through, so there are no props to offer.
///
/// Answering an empty list is right — inventing props would be worse — but doing
/// it silently is not, because an empty list is indistinguishable from a
/// component that has none. The note says so and says what to check.
///
/// It is also the proof that the child was *asked and answered*: an error, a
/// child that is not ready, and a file that does not compile all reply without
/// logging, so the note arriving means luau-lsp looked at the right place and
/// had nothing.
#[test]
fn a_call_component_says_it_has_no_props() {
    let mut server = server!();

    server.open(
        "\
local create = nil :: any
type Props = { Name: string }
local Card = {}
setmetatable(Card, { __call = function(class, props: Props) return create(\"Frame\")(props) end })
local e = <Card />
",
    );
    let _ = theirs(&mut server);

    // No props, and no globals leaking in as attributes either.
    let items = server.completion(4, 16);
    assert!(items.is_empty(), "{items:#?}");

    let note = server.log_containing("<Card>");
    let text = note["params"]["message"].as_str().unwrap_or_default();

    // Both causes, because luau-lsp reports them identically and this server
    // cannot tell which it is.
    assert!(text.contains("untyped"), "{text}");
    assert!(text.contains("__call"), "{text}");
    // And the fix for the one that has one.
    assert!(text.contains("(props: Props) -> Instance"), "{text}");
}

/// The same note, for a component whose props are simply untyped — which is the
/// commoner cause, and the reason the note may not assert `__call` as fact.
#[test]
fn an_untyped_component_is_not_blamed_on_a_metamethod() {
    let mut server = server!();

    server.open(
        "\
local create = nil :: any
local function Card(props)
\treturn create(\"Frame\")(props)
end
local e = <Card />
",
    );
    let _ = theirs(&mut server);

    assert!(server.completion(4, 16).is_empty());

    let text = server.log_containing("<Card>")["params"]["message"]
        .as_str()
        .unwrap_or_default()
        .to_string();

    // There is no metamethod anywhere in that file, so the note may name one
    // only as an alternative — asserting it as *the* cause would send someone
    // looking for something that is not there.
    assert!(text.contains("untyped"), "{text}");
    assert!(text.contains("Either"), "{text}");
}

/// The annotation the note recommends is one luau-lsp *does* resolve, so the
/// advice has to actually work — otherwise it is a wild goose chase.
#[test]
fn the_annotation_the_note_recommends_makes_props_resolve() {
    let mut server = server!();

    server.open(
        "\
local create = nil :: any
type Props = { Name: string }
local Card = {}
setmetatable(Card, { __call = function(class, props: Props) return create(\"Frame\")(props) end })
local Callable = (Card :: any) :: (props: Props) -> any
local e = <Callable />
",
    );
    let _ = theirs(&mut server);

    let mut labels: Vec<String> = Vec::new();
    for _ in 0..40 {
        labels = server
            .completion(5, 20)
            .iter()
            .filter_map(|item| item["label"].as_str())
            .map(str::to_string)
            .collect();

        if !labels.is_empty() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(250));
    }

    assert_eq!(labels, ["Name"], "{labels:?}");
}

/// `[build] out` moving renames the document out from under the child.
///
/// The URI we ask about afterwards is one it was never given, and the one it
/// still holds is a file nothing writes any more. Both have to be put right, or
/// every request against the new name fails with "No managed text document".
#[test]
fn moving_the_build_output_re_opens_the_document_under_its_new_name() {
    let mut server = server!();

    server.open(SOURCE);
    let _ = theirs(&mut server);

    std::fs::write(
        server.root().join("luaux.toml"),
        "[build]\nin = \"src\"\nout = \"elsewhere\"\n",
    )
    .expect("luaux.toml");
    server.notify("workspace/didChangeWatchedFiles", json!({ "changes": [] }));

    // Whole-document, so nothing but the child's own state can refuse it.
    server.request("textDocument/foldingRange", json!({ "textDocument": { "uri": server.uri() } }));

    // And it still answers about the contents, under the new name.
    let _ = theirs(&mut server);
}

/// A fixture that only analyses under Luau's new solver, plus one error that is
/// reported either way.
///
/// The second half is what makes this testable: it gives luau-lsp something to
/// publish in both runs, so the flag's effect is a diagnostic that disappears
/// rather than an absence to race against.
const NEEDS_THE_NEW_SOLVER: &str = r#"--!strict
local create = nil :: any
type function keep(t) return t end
local x: keep<number> = 1
local bad: number = "a"
local e = <Frame/>
"#;

fn complaints(server: &mut Server) -> Vec<String> {
    theirs(server)
        .iter()
        .filter_map(|diagnostic| diagnostic["message"].as_str())
        .map(str::to_string)
        .collect()
}

/// For the half of luau-lsp's configuration that is not settings.
///
/// Its Luau flags never travel on the command line — its own extension resolves
/// them and hands them over in `initializationOptions` — so forwarding settings
/// is not enough on its own, and a child started without them runs with every
/// Luau flag off. Vide's `create` is what that costs in practice: typed with
/// `keyof<>` and user-defined type functions, it collapses to `*error-type*` in
/// `.luaux` while the identical generated `.luau` answers correctly.
#[test]
fn the_new_solver_setting_reaches_luau_lsp() {
    let mut without = server!();
    without.open(NEEDS_THE_NEW_SOLVER);

    // The fixture has to actually need the flag, or the assertion below would
    // hold whatever we sent.
    let unsupported = complaints(&mut without);
    assert!(unsupported.iter().any(|message| message.contains("keep")), "{unsupported:#?}");

    let mut with = Server::with_luau_lsp(harness::find_luau_lsp());
    with.settings = json!({ "fflags": { "enableNewSolver": true } });
    with.initialize();
    with.initialized();
    with.open(NEEDS_THE_NEW_SOLVER);

    let remaining = complaints(&mut with);
    assert!(!remaining.iter().any(|message| message.contains("keep")), "{remaining:#?}");
    // And it is still analysing rather than quietly saying nothing.
    assert!(remaining.iter().any(|message| message.contains("number")), "{remaining:#?}");
}

/// A parse error genuinely leaves nothing to hand over, so luau-lsp has nothing
/// to say — and saying nothing is right. Ours still arrives.
#[test]
fn a_file_that_does_not_parse_still_reports_our_own_diagnostic() {
    let mut server = server!();
    server.open("local create = nil :: any\nlocal e = <Frame\n");

    let diagnostics = server.diagnostics_until(|diagnostics| !diagnostics.is_empty());
    assert!(diagnostics.iter().all(|d| d["source"] == json!("luaux")), "{diagnostics:#?}");
}

/// The point of recovering rather than stopping: one bad tag costs its own
/// diagnostic, not the rest of the file's type checking.
///
/// Before, an unknown element produced no generated Luau at all, so luau-lsp was
/// handed nothing and every Luau error in the file vanished with it — a typo in
/// one tag silently switched off type checking for everything else.
#[test]
fn a_bad_tag_does_not_cost_the_file_its_type_checking() {
    let mut server = server!();

    server.open(
        "\
--!strict
local create = nil :: any
local n: number = 1
local bad = <Frmae/>
local e = <Frame Size={n + \"x\"}/>
",
    );

    let diagnostics = server.diagnostics_until(|diagnostics| {
        diagnostics.iter().any(|d| d["source"] == json!("luaux"))
            && diagnostics.iter().any(|d| d["source"] == json!("Luau"))
    });

    // Ours, about the tag.
    let ours: Vec<&Value> = diagnostics.iter().filter(|d| d["source"] == json!("luaux")).collect();
    assert!(
        ours.iter().any(|d| d["message"].as_str().is_some_and(|m| m.contains("Frmae"))),
        "{ours:#?}"
    );

    // And luau-lsp's, about the *other* line, which used to disappear entirely.
    let theirs: Vec<&Value> = diagnostics.iter().filter(|d| d["source"] == json!("Luau")).collect();
    assert!(
        theirs.iter().any(|d| d["range"]["start"]["line"] == json!(4)),
        "no type error on line 4: {theirs:#?}"
    );
}

// --- requiring one `.luaux` from another -----------------------------------
//
// The case the proxy could not answer until the workspace scan existed.
// luau-lsp resolves a require in two steps from two different sources:
// *existence* is checked on the filesystem, *content* comes from the open
// document. So a `.luaux` that has never been built is `Unknown require`
// however good the text we hand over, and one that has been built is typed
// from the last build rather than from the file as it is now.
//
// `crate::workspace` closes both halves. These tests are what say so, because
// neither half is observable from unit tests: only a real luau-lsp decides
// whether a require resolved.

/// A dependency that is genuinely LuauX — it has to be compiled before it is
/// Luau at all — exporting one field with a type worth getting wrong.
const CARD: &str = "\
--!strict
local create = nil :: any
local element = <Frame/>
return { element = element, title = \"Card\" }
";

/// Requires it and assigns its `string` field to a `number`.
const APP: &str = "\
--!strict
local create = nil :: any
local Card = require(\"./Card\")
local n: number = Card.title
local e = <Frame Size={n}/>
";

/// A started server with extra `.luaux` files already on disk, as a window
/// opened on an existing project would find them.
macro_rules! server_with {
    ($($name:expr => $text:expr),* $(,)?) => {{
        match harness::find_luau_lsp() {
            Some(path) => {
                let mut server = Server::with_luau_lsp(Some(path));
                $(
                    std::fs::write(server.root().join("src").join($name), $text)
                        .expect("write dependency");
                )*
                server.initialize();
                server.initialized();
                server
            }
            None => {
                eprintln!(
                    "skipped: no working luau-lsp found. Set LUAU_LSP=<path> to run this test."
                );
                return;
            }
        }
    }};
}

/// The headline: a `.luaux` requiring a `.luaux` **that has never been built**
/// still gets its types.
///
/// Before the workspace scan this was `TypeError: Unknown require`, because
/// nothing had ever written `build/Card.luau` and luau-lsp checks the
/// filesystem for existence. The assertion is deliberately on the *specific*
/// error — assigning `string` to `number` — rather than "some error", because
/// `Unknown require` is also an error and would pass a weaker test.
#[test]
fn a_luaux_requiring_an_unbuilt_luaux_gets_its_types() {
    let mut server = server_with!("Card.luaux" => CARD);
    server.open(APP);

    let messages = complaints(&mut server);

    assert!(
        messages.iter().any(|message| message.contains("number") && message.contains("string")),
        "no type error from the required .luaux: {messages:#?}"
    );
    assert!(
        !messages.iter().any(|message| message.contains("Unknown require")),
        "the require did not resolve: {messages:#?}"
    );
}

/// And the build output it needed is written where the build would write it.
///
/// The write is the whole reason the require resolves, so it is asserted
/// directly rather than inferred from the types working.
#[test]
fn the_missing_build_output_is_written_for_it() {
    let mut server = server_with!("Card.luaux" => CARD);
    server.open(APP);
    let _ = complaints(&mut server);

    let built = server.root().join("build").join("Card.luau");
    assert!(built.is_file(), "{} was never written", built.display());

    let text = std::fs::read_to_string(&built).expect("read");
    assert!(text.contains("title"), "{text}");
    // Compiled, not copied: the markup is gone by the time it is on disk.
    assert!(!text.contains("<Frame"), "the .luaux was written through uncompiled: {text}");
}

/// The dependency is open in another tab, which is the ordinary way two
/// `.luaux` get written.
///
/// Being open decides where the *text* comes from and nothing else: existence
/// is still checked on the filesystem, and an open document luau-lsp was handed
/// is not on it. The workspace scan used to skip such a file entirely — the
/// write along with the compile — so a project that had never been built typed
/// the required module as `*error-type*` for as long as its tab stayed open,
/// and closing the tab fixed it.
///
/// Both documents arrive before the child is answered into existence, which is
/// what a window reloaded with two `.luaux` open actually produces.
#[test]
fn a_dependency_open_in_another_tab_still_resolves() {
    let Some(path) = harness::find_luau_lsp() else {
        eprintln!("skipped: no working luau-lsp found. Set LUAU_LSP=<path> to run this test.");
        return;
    };

    let mut server = Server::with_luau_lsp(Some(path));
    let card = server.root().join("src").join("Card.luaux");
    std::fs::write(&card, CARD).expect("write dependency");

    server.initialize();
    server.notify("initialized", json!({}));

    server.notify(
        "textDocument/didOpen",
        json!({
            "textDocument": {
                "uri": harness::uri_for(&card),
                "languageId": "luaux",
                "version": 1,
                "text": CARD,
            },
        }),
    );

    server.answer_configuration();
    server.open(APP);

    let messages = complaints(&mut server);

    assert!(
        !messages.iter().any(|message| message.contains("Unknown require")),
        "an open dependency did not resolve: {messages:#?}"
    );
    assert!(
        messages.iter().any(|message| message.contains("number") && message.contains("string")),
        "no type error from the required .luaux: {messages:#?}"
    );
}

/// A dependency the editor never opened, edited on disk — a branch switch is
/// the ordinary case — is recompiled and re-typed.
///
/// This is the half that makes the answers *current* rather than merely
/// present. Without it the types come from whatever was last written to
/// `build/`, which is exactly the stale answer the design refuses elsewhere.
#[test]
fn editing_an_unopened_dependency_on_disk_updates_the_types() {
    let mut server = server_with!("Card.luaux" => CARD);
    server.open(APP);

    // The mismatch is there to begin with.
    let messages = complaints(&mut server);
    assert!(
        messages.iter().any(|message| message.contains("number") && message.contains("string")),
        "{messages:#?}"
    );

    // `title` becomes a number, which makes the assignment in App legal.
    let card = server.root().join("src").join("Card.luaux");
    std::fs::write(&card, CARD.replace("\"Card\"", "1")).expect("rewrite dependency");

    server.notify(
        "workspace/didChangeWatchedFiles",
        json!({ "changes": [{ "uri": harness::uri_for(&card), "type": 2 }] }),
    );

    // Nudge the open file so a fresh round of diagnostics is published.
    server.change(APP);

    for _ in 0..20 {
        let messages = complaints(&mut server);
        if !messages.iter().any(|m| m.contains("number") && m.contains("string")) {
            return;
        }
    }

    panic!("the type error survived an edit to the dependency it came from");
}

/// A `.luaux` that has been built already must not have its output replaced.
///
/// `luaux build --watch` owns these paths, and a language server racing the
/// build over the project's own output is not a trade worth making. The
/// content luau-lsp type-checks arrives over LSP regardless, so there is
/// nothing to gain by writing.
#[test]
fn an_existing_build_output_is_left_alone() {
    let mut server = server_with!("Card.luaux" => CARD);

    let built = server.root().join("build").join("Card.luau");
    std::fs::create_dir_all(built.parent().expect("parent")).expect("build directory");
    std::fs::write(&built, "-- written by luaux build\n").expect("existing output");

    server.open(APP);
    let _ = complaints(&mut server);

    assert_eq!(
        std::fs::read_to_string(&built).expect("read"),
        "-- written by luaux build\n",
        "the server overwrote the build's own output"
    );
}

/// The Roblox form of the same question: `require(script.Parent.Card)`.
///
/// Kept separate from the string-require test because it resolves by a
/// completely different route — the rojo sourcemap maps an *instance* to a file
/// path, and only then is the file read. Both routes end at the same existence
/// check, which is what `crate::workspace` satisfies, but a test of one is not a
/// test of the other.
#[test]
fn a_roblox_instance_require_of_a_luaux_gets_its_types() {
    let mut server = match harness::find_luau_lsp() {
        Some(path) => Server::with_luau_lsp(Some(path)),
        None => {
            eprintln!("skipped: no working luau-lsp found. Set LUAU_LSP=<path> to run this test.");
            return;
        }
    };

    std::fs::write(server.root().join("src").join("Card.luaux"), CARD).expect("dependency");

    // What rojo would generate for a project syncing `build/`. Both modules are
    // siblings under ReplicatedStorage, so `script.Parent.Card` names one from
    // the other.
    std::fs::write(
        server.root().join("sourcemap.json"),
        json!({
            "name": "Game",
            "className": "DataModel",
            "children": [{
                "name": "ReplicatedStorage",
                "className": "ReplicatedStorage",
                "children": [
                    { "name": "App", "className": "ModuleScript",
                      "filePaths": ["build/App.luau"] },
                    { "name": "Card", "className": "ModuleScript",
                      "filePaths": ["build/Card.luau"] },
                ],
            }],
        })
        .to_string(),
    )
    .expect("sourcemap");

    server.settings = json!({
        "platform": { "type": "roblox" },
        "sourcemap": { "enabled": true, "autogenerate": false },
    });

    server.initialize();
    server.initialized();

    server.open(
        "\
--!strict
local create = nil :: any
local Card = require(script.Parent.Card)
local n: number = Card.title
local e = <Frame Size={n}/>
",
    );

    let messages = complaints(&mut server);

    assert!(
        messages.iter().any(|message| message.contains("number") && message.contains("string")),
        "no type error through the sourcemap: {messages:#?}"
    );
    assert!(
        !messages.iter().any(|message| message.contains("Unknown require")),
        "the instance require did not resolve: {messages:#?}"
    );
}
