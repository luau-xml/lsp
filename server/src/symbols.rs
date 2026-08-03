//! Document symbols — the element tree as an outline.
//!
//! Cheap, because the tree already exists, and it is what makes a long file
//! navigable: a UI file is a shape, and a flat list of `local`s does not show it.

use crate::document::Document;
use crate::tree::{self, Tag};
use serde_json::{json, Value};

/// LSP `SymbolKind`.
const CLASS: u8 = 5;
const OBJECT: u8 = 19;

pub fn symbols(document: &Document) -> Value {
    Value::Array(tree::tree(&document.text).iter().map(|tag| symbol(document, tag)).collect())
}

fn symbol(document: &Document, tag: &Tag) -> Value {
    let (name, kind) =
        if tag.name.is_empty() { ("<>".to_string(), OBJECT) } else { (tag.name.clone(), CLASS) };

    let mut value = json!({
        "name": name,
        "kind": kind,
        "range": document.range_at(tag.start, tag.end),
        // Selecting the element highlights its opening name, not the whole
        // subtree, which is what makes the outline usable to jump with.
        "selectionRange": document.range_at(tag.open_name.0, tag.open_name.1.max(tag.open_name.0 + 1)),
        "children": tag.children.iter().map(|child| symbol(document, child)).collect::<Vec<_>>(),
    });

    if let Some(label) = &tag.label {
        value["detail"] = json!(label);
    }

    value
}

#[cfg(test)]
mod tests {
    use super::*;

    fn outline(source: &str) -> Value {
        symbols(&Document::new("file:///a.luaux".into(), 1, source.into()))
    }

    #[test]
    fn nesting_is_preserved() {
        let value = outline("local e = <Frame><TextLabel/></Frame>");

        assert_eq!(value[0]["name"], json!("Frame"));
        assert_eq!(value[0]["children"][0]["name"], json!("TextLabel"));
    }

    #[test]
    fn the_name_attribute_becomes_the_detail() {
        let value = outline("local e = <Frame Name=\"Header\"/>");
        assert_eq!(value[0]["detail"], json!("Header"));
    }

    #[test]
    fn the_selection_range_covers_the_opening_name() {
        let source = "local e = <Frame></Frame>";
        let value = outline(source);

        assert_eq!(value[0]["selectionRange"]["start"]["character"], json!(11));
        assert_eq!(value[0]["selectionRange"]["end"]["character"], json!(16));
    }

    #[test]
    fn a_fragment_is_shown_as_one() {
        let value = outline("local e = (<><Frame/></>)");
        assert_eq!(value[0]["name"], json!("<>"));
        assert_eq!(value[0]["children"][0]["name"], json!("Frame"));
    }

    #[test]
    fn a_file_without_luaux_has_no_outline_of_ours() {
        // luau-lsp still provides the Luau symbols; ours is additive.
        assert_eq!(outline("local x = 1"), json!([]));
    }

    #[test]
    fn a_half_typed_file_still_outlines_what_parsed() {
        let value = outline("local a = <Frame/>\nlocal b = <Fra");
        assert_eq!(value.as_array().expect("array").len(), 1);
    }
}
