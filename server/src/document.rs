//! Open documents and their text.
//!
//! Sync is incremental where the editor offers it: a keystroke should not resend
//! the file, because everything downstream — compile, map, forward — runs on
//! every change, and the proxy is already paying for one round trip.

use crate::line_index::{LineIndex, Position};
use serde_json::Value;
use std::collections::HashMap;

pub struct Document {
    pub uri: String,
    pub version: i64,
    pub text: String,
    pub index: LineIndex,
}

impl Document {
    pub fn new(uri: String, version: i64, text: String) -> Self {
        let index = LineIndex::new(&text);
        Self { uri, version, text, index }
    }

    /// Applies one `textDocument/didChange`.
    ///
    /// A change with no range replaces the document; one with a range splices.
    /// A range that will not resolve is refused rather than applied at a guess —
    /// a document silently out of step with the editor is worse than a dropped
    /// edit, because every position after it is then wrong.
    pub fn apply(&mut self, change: &Value) -> bool {
        let Some(text) = change.get("text").and_then(Value::as_str) else {
            return false;
        };

        let Some(range) = change.get("range") else {
            self.text = text.to_string();
            self.index = LineIndex::new(&self.text);
            return true;
        };

        let Some((start, end)) = self.byte_range(range) else {
            return false;
        };

        self.text.replace_range(start..end, text);
        self.index = LineIndex::new(&self.text);
        true
    }

    /// LSP range → byte range in this document.
    pub fn byte_range(&self, range: &Value) -> Option<(usize, usize)> {
        let start = self.byte_offset(range.get("start")?)?;
        let end = self.byte_offset(range.get("end")?)?;
        (start <= end).then_some((start, end))
    }

    /// LSP position → byte offset.
    pub fn byte_offset(&self, position: &Value) -> Option<usize> {
        let line = position.get("line")?.as_u64()? as u32;
        let character = position.get("character")?.as_u64()? as u32;
        self.index.offset(&self.text, Position::new(line, character))
    }

    /// Byte offset → LSP position, as JSON.
    pub fn position_at(&self, offset: usize) -> Value {
        let position = self.index.position(&self.text, offset);
        serde_json::json!({ "line": position.line, "character": position.character })
    }

    /// Byte range → LSP range, as JSON.
    pub fn range_at(&self, start: usize, end: usize) -> Value {
        serde_json::json!({ "start": self.position_at(start), "end": self.position_at(end) })
    }
}

#[derive(Default)]
pub struct Documents {
    open: HashMap<String, Document>,
}

impl Documents {
    pub fn open(&mut self, uri: String, version: i64, text: String) {
        self.open.insert(uri.clone(), Document::new(uri, version, text));
    }

    pub fn close(&mut self, uri: &str) {
        self.open.remove(uri);
    }

    pub fn get(&self, uri: &str) -> Option<&Document> {
        self.open.get(uri)
    }

    pub fn get_mut(&mut self, uri: &str) -> Option<&mut Document> {
        self.open.get_mut(uri)
    }

    pub fn uris(&self) -> Vec<String> {
        self.open.keys().cloned().collect()
    }

    /// Applies a `didChange`, returning whether every part of it landed.
    pub fn change(&mut self, uri: &str, version: i64, changes: &[Value]) -> bool {
        let Some(document) = self.open.get_mut(uri) else {
            return false;
        };

        let mut applied = true;
        for change in changes {
            applied &= document.apply(change);
        }

        document.version = version;
        applied
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn document(text: &str) -> Document {
        Document::new("file:///a.luaux".into(), 1, text.into())
    }

    #[test]
    fn a_full_change_replaces_everything() {
        let mut document = document("old");
        assert!(document.apply(&json!({ "text": "new" })));
        assert_eq!(document.text, "new");
    }

    #[test]
    fn an_incremental_change_splices() {
        let mut document = document("local e = <Fra/>\n");
        let change = json!({
            "range": {
                "start": { "line": 0, "character": 14 },
                "end": { "line": 0, "character": 14 },
            },
            "text": "me",
        });

        assert!(document.apply(&change));
        assert_eq!(document.text, "local e = <Frame/>\n");
    }

    #[test]
    fn the_line_index_keeps_up_with_edits() {
        let mut document = document("a\n");
        document.apply(&json!({
            "range": { "start": { "line": 1, "character": 0 }, "end": { "line": 1, "character": 0 } },
            "text": "b\nc\n",
        }));

        assert_eq!(document.text, "a\nb\nc\n");
        assert_eq!(document.byte_offset(&json!({ "line": 2, "character": 0 })), Some(4));
    }

    #[test]
    fn a_change_spanning_lines_applies() {
        let mut document = document("one\ntwo\nthree\n");
        assert!(document.apply(&json!({
            "range": { "start": { "line": 0, "character": 1 }, "end": { "line": 2, "character": 2 } },
            "text": "X",
        })));
        assert_eq!(document.text, "oXree\n");
    }

    #[test]
    fn an_unresolvable_range_is_refused_rather_than_guessed() {
        let mut document = document("a\n");
        // Line 99 does not exist; applying this anywhere would desynchronise us
        // from the editor for every position after it.
        assert!(!document.apply(&json!({
            "range": { "start": { "line": 99, "character": 0 }, "end": { "line": 99, "character": 0 } },
            "text": "x",
        })));
        assert_eq!(document.text, "a\n");
    }

    #[test]
    fn an_inverted_range_is_refused() {
        let document = document("abc\n");
        assert_eq!(
            document.byte_range(&json!({
                "start": { "line": 0, "character": 3 },
                "end": { "line": 0, "character": 1 },
            })),
            None
        );
    }
}
