//! Rename.
//!
//! Two distinct operations, and conflating them produces data loss:
//!
//! * **A tag pair** — `<Frame>…</Frame>` renamed together. Ours, and handled here.
//! * **A Luau symbol** — forwarded. The edits come back against the *generated*
//!   file and have to be mapped home, and an edit touching generated text is not
//!   applicable: the rename is refused rather than partially applied. That half
//!   lives in [`crate::proxy`], where the response is.

use crate::document::Document;
use crate::scan::{self, Context};
use crate::tree;
use serde_json::{json, Value};

pub enum Answer {
    Ours(Value),
    /// A Luau symbol; luau-lsp knows its occurrences.
    Forward,
    /// Not renameable here.
    Nothing,
}

/// `textDocument/prepareRename` — the range that would be renamed.
pub fn prepare(document: &Document, offset: usize) -> Answer {
    match pair(document, offset) {
        Some((open, _)) => Answer::Ours(document.range_at(open.0, open.1)),
        None if forwards(&document.text, offset) => Answer::Forward,
        None => Answer::Nothing,
    }
}

pub fn rename(document: &Document, offset: usize, new_name: &str) -> Answer {
    let Some((open, close)) = pair(document, offset) else {
        return if forwards(&document.text, offset) { Answer::Forward } else { Answer::Nothing };
    };

    if !is_element_name(new_name) {
        return Answer::Nothing;
    }

    let mut edits = vec![json!({
        "range": document.range_at(open.0, open.1),
        "newText": new_name,
    })];

    if let Some((start, end)) = close {
        edits.push(json!({
            "range": document.range_at(start, end),
            "newText": new_name,
        }));
    }

    Answer::Ours(json!({ "changes": { &document.uri: edits } }))
}

/// Byte range of a name as written.
type Name = (usize, usize);

/// The element name at `offset`, and its closing half if it has one.
fn pair(document: &Document, offset: usize) -> Option<(Name, Option<Name>)> {
    for tag in tree::flatten(&tree::tree(&document.text)) {
        // A fragment has no name to rename.
        if tag.name.is_empty() || tag.name_at(offset).is_none() {
            continue;
        }

        return Some((tag.open_name, tag.close_name));
    }

    None
}

fn forwards(source: &str, offset: usize) -> bool {
    matches!(scan::scan(source, offset).context, Context::Luau | Context::Expression { .. })
}

/// Whether a name can legally be a tag.
///
/// Renaming to something unwritable would produce a file that does not parse,
/// and an editor cannot undo half of a rename it has already applied.
fn is_element_name(name: &str) -> bool {
    !name.is_empty()
        && name.split('.').all(|part| {
            !part.is_empty()
                && part.starts_with(|c: char| c == '_' || c.is_ascii_alphabetic())
                && part.chars().all(|c| c == '_' || c.is_ascii_alphanumeric())
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn document(text: &str) -> Document {
        Document::new("file:///a.luaux".into(), 1, text.into())
    }

    fn edits(marked: &str, new_name: &str) -> Option<Vec<(u64, u64)>> {
        let cursor = marked.find('|').expect("a cursor marker");
        let document = document(&marked.replace('|', ""));

        match rename(&document, cursor, new_name) {
            Answer::Ours(value) => Some(
                value["changes"]["file:///a.luaux"]
                    .as_array()
                    .expect("edits")
                    .iter()
                    .map(|edit| {
                        (
                            edit["range"]["start"]["line"].as_u64().expect("line"),
                            edit["range"]["start"]["character"].as_u64().expect("character"),
                        )
                    })
                    .collect(),
            ),
            _ => None,
        }
    }

    #[test]
    fn a_pair_is_renamed_together() {
        let edits = edits("local e = <Fr|ame></Frame>", "Panel").expect("edits");
        assert_eq!(edits, [(0, 11), (0, 19)]);
    }

    #[test]
    fn renaming_from_the_closing_half_renames_both() {
        let edits = edits("local e = <Frame></Fr|ame>", "Panel").expect("edits");
        assert_eq!(edits.len(), 2);
    }

    #[test]
    fn a_self_closing_element_has_one_edit() {
        let edits = edits("local e = <Fr|ame/>", "Panel").expect("edits");
        assert_eq!(edits, [(0, 11)]);
    }

    #[test]
    fn a_nested_element_renames_only_itself() {
        let edits =
            edits("local e = <Frame><Text|Label></TextLabel></Frame>", "Row").expect("edits");
        assert_eq!(edits.len(), 2);
        // Neither edit touches the outer `Frame`.
        assert!(edits.iter().all(|(_, character)| *character > 16));
    }

    #[test]
    fn a_name_that_would_not_parse_is_refused() {
        // Applying half of this would leave a file that does not compile, and
        // the editor has no way to take it back.
        for name in ["", "1Frame", "a b", "Frame>"] {
            assert!(edits("local e = <Fr|ame/>", name).is_none(), "{name:?}");
        }
        // A dotted component name is legal.
        assert!(edits("local e = <Fr|ame/>", "Foo.Bar").is_some());
    }

    #[test]
    fn luau_positions_are_forwarded() {
        let plain = document("local x = 1");
        assert!(matches!(rename(&plain, 7, "y"), Answer::Forward));

        let source = "local e = <Frame Size={size}/>";
        let inside = document(source);
        let cursor = source.find("size").expect("expression");
        assert!(matches!(rename(&inside, cursor, "y"), Answer::Forward));
    }

    #[test]
    fn prepare_reports_the_name_it_would_rename() {
        let source = "local e = <Frame></Frame>";
        let document = document(source);

        let Answer::Ours(range) = prepare(&document, source.find("Frame").expect("tag")) else {
            panic!("expected a range");
        };
        assert_eq!(range["start"]["character"], json!(11));
        assert_eq!(range["end"]["character"], json!(16));
    }

    #[test]
    fn a_fragment_is_not_renameable() {
        let source = "local e = (<><Frame/></>)";
        let document = document(source);
        assert!(matches!(
            prepare(&document, source.find("<>").expect("fragment") + 1),
            Answer::Nothing
        ));
    }
}
