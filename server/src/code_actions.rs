//! Quick fixes.
//!
//! The compiler already computes the hard part: `closest_class` and
//! `closest_members` back the did-you-mean in `<Frmae>` and in an unknown
//! property. [`crate::analysis`] attaches those candidates to the diagnostic, so
//! here there is nothing to work out — only an edit to offer.
//!
//! Reading the candidates off the diagnostic rather than out of its message is
//! what keeps this from being English-parsing: the message is for a person, the
//! `data` is for us.

use serde_json::{json, Value};

/// LSP `CodeActionKind`.
const QUICKFIX: &str = "quickfix";

pub fn actions(uri: &str, diagnostics: &[Value]) -> Value {
    let mut out = Vec::new();

    for diagnostic in diagnostics {
        let Some(data) = diagnostic.get("data") else { continue };
        let Some(range) = data.get("range") else { continue };
        let Some(candidates) = data.get("candidates").and_then(Value::as_array) else {
            continue;
        };

        let what = match data.get("kind").and_then(Value::as_str) {
            Some("element") => "element",
            Some("member") => "attribute",
            _ => continue,
        };

        for candidate in candidates {
            let Some(name) = candidate.as_str() else { continue };

            out.push(json!({
                "title": format!("Change {what} to {name}"),
                "kind": QUICKFIX,
                "diagnostics": [diagnostic],
                // The first suggestion is the compiler's best guess, so it is
                // the one an editor's "fix all" or single-keystroke apply takes.
                "isPreferred": candidate == &candidates[0],
                "edit": {
                    "changes": { uri: [{ "range": range, "newText": name }] },
                },
            }));
        }
    }

    Value::Array(out)
}

/// The diagnostics an editor sent in `context`, narrowed to ours.
///
/// luau-lsp's diagnostics come back through here too, and its own code actions
/// are forwarded rather than reinvented.
pub fn ours(context: &Value) -> Vec<Value> {
    context
        .get("diagnostics")
        .and_then(Value::as_array)
        .map(|diagnostics| {
            diagnostics
                .iter()
                .filter(|diagnostic| diagnostic.get("source") == Some(&json!("luaux")))
                .cloned()
                .collect()
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::analysis::Analysis;
    use crate::document::Document;
    use crate::project::Project;
    use std::path::Path;

    fn fixes(source: &str) -> Value {
        let document = Document::new("file:///a.luaux".into(), 1, source.into());
        let project = Project::discover(Path::new("/nonexistent-luaux-project/a.luaux"));
        let analysis = Analysis::run(&document, &project, None);

        actions(&document.uri, &analysis.diagnostics)
    }

    #[test]
    fn a_misspelt_element_offers_the_class() {
        let actions = fixes("local create = f()\nlocal e = <Frmae/>\n");
        assert_eq!(actions[0]["title"], json!("Change element to Frame"));
        assert_eq!(actions[0]["kind"], json!("quickfix"));
        assert_eq!(actions[0]["isPreferred"], json!(true));
    }

    #[test]
    fn the_edit_replaces_only_the_name() {
        let source = "local create = f()\nlocal e = <Frmae/>\n";
        let actions = fixes(source);
        let edit = &actions[0]["edit"]["changes"]["file:///a.luaux"][0];

        // `<` stays; only `Frmae` is replaced.
        assert_eq!(edit["range"]["start"]["character"], json!(11));
        assert_eq!(edit["range"]["end"]["character"], json!(16));
        assert_eq!(edit["newText"], json!("Frame"));
    }

    #[test]
    fn a_misspelt_attribute_offers_every_candidate() {
        let actions = fixes("local create = f()\nlocal e = <Frame Color3={c}/>\n");
        let titles: Vec<&str> = actions
            .as_array()
            .expect("actions")
            .iter()
            .map(|action| action["title"].as_str().unwrap_or_default())
            .collect();

        // `Color3` sits inside several qualified names and nothing can tell
        // which was meant, so all of them are offered.
        assert!(titles.contains(&"Change attribute to BackgroundColor3"), "{titles:?}");
        assert!(titles.contains(&"Change attribute to BorderColor3"), "{titles:?}");
        assert!(titles.len() > 1);
    }

    #[test]
    fn a_diagnostic_with_no_suggestion_offers_no_fix() {
        // `Receipt` is nothing like any class, and inventing a fix would be worse
        // than none.
        assert_eq!(fixes("local create = f()\nlocal e = <Receipt/>\n"), json!([]));
    }

    #[test]
    fn only_our_diagnostics_are_claimed() {
        let context = json!({
            "diagnostics": [
                { "source": "luaux", "message": "ours" },
                { "source": "Luau", "message": "theirs" },
            ],
        });

        let mine = ours(&context);
        assert_eq!(mine.len(), 1);
        assert_eq!(mine[0]["message"], json!("ours"));
    }
}
