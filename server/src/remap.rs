//! Translating positions and URIs across the proxy boundary.
//!
//! Requests going down are rewritten from `src/App.luaux` to `build/App.luau`;
//! responses coming back are rewritten the other way. Both directions walk the
//! JSON generically rather than knowing each message's shape, which is what
//! keeps us decoupled from luau-lsp's protocol surface — an unknown field is
//! carried through untouched instead of being dropped by a struct that had
//! never heard of it.
//!
//! **`None` means none** (decision 6). A range that lands in generated text —
//! `create(`, `Text = `, the `__luaux_read` wrapper — has no source counterpart,
//! and anything carrying it is dropped rather than moved to whatever is nearest.
//! A wrong position sends people to code they did not write.

use crate::line_index::{LineIndex, Position};
use crate::project;
use crate::sourcemap::SourceMap;
use serde_json::{Map, Value};

/// Which way a translation runs.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    /// `.luaux` → generated `.luau`.
    Down,
    /// generated `.luau` → `.luaux`.
    Up,
}

pub struct Remap<'a> {
    pub map: &'a SourceMap,
    pub source: &'a str,
    pub output: &'a str,
    pub source_index: &'a LineIndex,
    pub output_index: &'a LineIndex,
    /// The `.luaux` URI as the editor knows it.
    pub source_uri: &'a str,
    /// The generated `.luau` URI as luau-lsp knows it.
    pub output_uri: &'a str,
    pub direction: Direction,
    /// Set when anything had to be dropped. Callers that must be all-or-nothing
    /// — rename, above all — check it before applying.
    pub dropped: bool,
}

impl Remap<'_> {
    /// Rewrites a whole message.
    ///
    /// `Some` is a message that survived translation intact enough to use;
    /// `None` means it does not apply to the other side at all.
    pub fn message(&mut self, value: &Value) -> Option<Value> {
        // A message is about our document until something says otherwise.
        self.value(value, true)
    }

    /// Rewrites only the parts of a message that name *this* pair.
    ///
    /// The opposite default to [`Remap::message`], and the difference matters:
    /// an answer about one document can carry locations in another `.luaux`, and
    /// those are translated by running the message through a second `Remap`
    /// built for that file. Such a pass must assume nothing — a `Hover` has no
    /// `uri` at all, and mapping its range through a foreign file's map would
    /// move it somewhere meaningless.
    pub fn foreign_message(&mut self, value: &Value) -> Option<Value> {
        self.value(value, false)
    }

    fn value(&mut self, value: &Value, ours: bool) -> Option<Value> {
        match value {
            Value::Array(items) => Some(Value::Array(
                items
                    .iter()
                    .filter_map(|item| {
                        let mapped = self.value(item, ours);
                        if mapped.is_none() {
                            self.dropped = true;
                        }
                        mapped
                    })
                    .collect(),
            )),
            Value::Object(object) => self.object(object, ours),
            other => Some(other.clone()),
        }
    }

    fn object(&mut self, object: &Map<String, Value>, ours: bool) -> Option<Value> {
        // A `uri` re-anchors everything beneath it: a location in another file
        // is not ours to translate, and touching its positions would corrupt it.
        let ours = match object.get("uri").and_then(Value::as_str) {
            Some(uri) => self.is_ours(uri),
            None => ours,
        };

        if ours {
            if let Some(range) = self.range(object) {
                return range;
            }
            if let Some(position) = self.position_object(object) {
                return position;
            }
        }

        let mut out = Map::with_capacity(object.len());

        for (key, value) in object {
            if key == "uri" || key == "targetUri" {
                out.insert(key.clone(), Value::String(self.rewrite_uri(value)?));
                continue;
            }

            let mapped = match self.value(value, ours) {
                Some(mapped) => mapped,
                None => {
                    self.dropped = true;
                    // A container whose *position* did not survive is not
                    // relocatable; one that merely lost an optional field is.
                    if POSITIONAL.contains(&key.as_str()) {
                        return None;
                    }
                    continue;
                }
            };

            out.insert(key.clone(), mapped);
        }

        Some(Value::Object(out))
    }

    /// Compared as files, not as strings: everything travelling *up* was written
    /// by luau-lsp, which re-encodes URIs into its own normal form.
    fn is_ours(&self, uri: &str) -> bool {
        project::same_file(uri, self.source_uri) || project::same_file(uri, self.output_uri)
    }

    fn rewrite_uri(&self, value: &Value) -> Option<String> {
        let uri = value.as_str()?;

        Some(match self.direction {
            Direction::Down if project::same_file(uri, self.source_uri) => {
                self.output_uri.to_string()
            }
            Direction::Up if project::same_file(uri, self.output_uri) => {
                self.source_uri.to_string()
            }
            _ => uri.to_string(),
        })
    }

    /// Translates `{ start, end }`, requiring both edges to land in one run.
    ///
    /// Two positions that map individually can still straddle generated text,
    /// and the range between them would then cover code the author never wrote.
    fn range(&mut self, object: &Map<String, Value>) -> Option<Option<Value>> {
        let start = self.offset(object.get("start")?)?;
        let end = self.offset(object.get("end")?)?;

        let mapped = match self.direction {
            Direction::Down => self.map.to_output_range(start, end),
            Direction::Up => self.map.to_source_range(start, end),
        };

        Some(mapped.map(
            |(start, end)| serde_json::json!({ "start": self.emit(start), "end": self.emit(end) }),
        ))
    }

    fn position_object(&mut self, object: &Map<String, Value>) -> Option<Option<Value>> {
        if object.len() != 2 || !object.contains_key("line") || !object.contains_key("character") {
            return None;
        }

        let offset = self.offset(&Value::Object(object.clone()))?;

        // A caret just past the end of a run is still in it — which is exactly
        // where someone completing `{count|}` has their cursor.
        let mapped = match self.direction {
            Direction::Down => {
                self.map.to_output(offset).or_else(|| self.map.to_output_end(offset))
            }
            Direction::Up => self.map.to_source(offset).or_else(|| self.map.to_source_end(offset)),
        };

        Some(mapped.map(|offset| self.emit(offset)))
    }

    /// LSP position on the *incoming* side → byte offset.
    fn offset(&self, value: &Value) -> Option<usize> {
        let line = value.get("line")?.as_u64()? as u32;
        let character = value.get("character")?.as_u64()? as u32;
        let position = Position::new(line, character);

        match self.direction {
            Direction::Down => self.source_index.offset(self.source, position),
            Direction::Up => self.output_index.offset(self.output, position),
        }
    }

    /// Byte offset on the *outgoing* side → LSP position.
    fn emit(&self, offset: usize) -> Value {
        let position = match self.direction {
            Direction::Down => self.output_index.position(self.output, offset),
            Direction::Up => self.source_index.position(self.source, offset),
        };

        serde_json::json!({ "line": position.line, "character": position.character })
    }
}

/// Keys whose value *is* the thing's location. Losing one of these makes the
/// whole object meaningless, so it goes rather than arriving without a place.
const POSITIONAL: &[&str] = &[
    "range",
    "position",
    "selectionRange",
    "targetRange",
    "targetSelectionRange",
    "originSelectionRange",
];

#[cfg(test)]
mod tests {
    use super::*;
    use crate::map_builder;
    use crate::project::Project;
    use serde_json::json;

    struct Fixture {
        source: String,
        output: String,
        map: SourceMap,
        source_index: LineIndex,
        output_index: LineIndex,
    }

    fn fixture(source: &str) -> Fixture {
        let project = Project::without_config(luaux::Config::with_create("create"));
        let (output, _) = luaux::compile::compile_configured(
            source,
            crate::backend(&project.config),
            project.config.clone(),
        )
        .expect("compile");
        let map = map_builder::build(source, &output, &project.config);

        Fixture {
            source_index: LineIndex::new(source),
            output_index: LineIndex::new(&output),
            source: source.to_string(),
            map,
            output,
        }
    }

    impl Fixture {
        fn remap(&self, direction: Direction) -> Remap<'_> {
            Remap {
                map: &self.map,
                source: &self.source,
                output: &self.output,
                source_index: &self.source_index,
                output_index: &self.output_index,
                source_uri: "file:///src/App.luaux",
                output_uri: "file:///build/App.luau",
                direction,
                dropped: false,
            }
        }
    }

    const SOURCE: &str = "local create = f()\nlocal e = <Frame Size={size}/>\n";

    fn position(line: u64, character: u64) -> Value {
        json!({ "line": line, "character": character })
    }

    #[test]
    fn a_position_in_an_expression_goes_down_and_comes_back() {
        let fixture = fixture(SOURCE);
        // `size`, inside the hole.
        let at = position(1, 23);

        let down =
            fixture.remap(Direction::Down).message(&json!({ "position": at })).expect("mapped");

        let up = fixture.remap(Direction::Up).message(&down).expect("mapped back");

        assert_eq!(up["position"], at);
    }

    #[test]
    fn the_uri_swaps_with_the_direction() {
        let fixture = fixture(SOURCE);

        let down = fixture
            .remap(Direction::Down)
            .message(&json!({ "textDocument": { "uri": "file:///src/App.luaux" } }))
            .expect("mapped");
        assert_eq!(down["textDocument"]["uri"], json!("file:///build/App.luau"));

        let up = fixture
            .remap(Direction::Up)
            .message(&json!({ "uri": "file:///build/App.luau" }))
            .expect("mapped");
        assert_eq!(up["uri"], json!("file:///src/App.luaux"));
    }

    #[test]
    fn a_position_in_generated_text_is_refused_rather_than_moved() {
        let fixture = fixture(SOURCE);
        // Column 10 of the output line is inside `create("Frame")(`.
        let mut remap = fixture.remap(Direction::Up);

        assert_eq!(remap.message(&json!({ "position": position(1, 12) })), None);
        assert!(remap.dropped);
    }

    #[test]
    fn a_range_straddling_generated_text_is_refused() {
        let fixture = fixture(SOURCE);
        let mut remap = fixture.remap(Direction::Down);

        // From `local` on line 1 to inside the hole: crosses `create("Frame")({`.
        let straddling = json!({ "range": { "start": position(1, 0), "end": position(1, 26) } });
        assert_eq!(remap.message(&straddling), None);
    }

    #[test]
    fn locations_in_other_files_pass_through_untouched() {
        let fixture = fixture(SOURCE);
        let elsewhere = json!({
            "uri": "file:///other/Thing.luau",
            "range": { "start": position(400, 3), "end": position(400, 9) },
        });

        let mapped = fixture.remap(Direction::Up).message(&elsewhere).expect("passed through");
        assert_eq!(mapped, elsewhere);
    }

    #[test]
    fn an_unmappable_item_in_a_list_is_dropped_and_the_rest_survive() {
        let fixture = fixture(SOURCE);
        let mut remap = fixture.remap(Direction::Up);

        // The generated `create(` has no source; the captured `size` does.
        let line = fixture.output_index.line_start(1);
        let expression = (fixture.output.rfind("size").expect("expression") - line) as u64;

        let list = json!([
            { "range": { "start": position(1, 12), "end": position(1, 18) } },
            { "range": { "start": position(1, expression), "end": position(1, expression + 4) } },
        ]);

        let mapped = remap.message(&list).expect("a list");
        assert_eq!(mapped.as_array().expect("array").len(), 1);
        assert!(remap.dropped, "the caller has to be able to tell");
    }

    #[test]
    fn unknown_fields_are_carried_through() {
        // The whole point of walking generically: luau-lsp may send anything.
        let fixture = fixture(SOURCE);
        let message = json!({ "contents": "hi", "somethingNew": { "nested": [1, 2, 3] } });

        let mapped = fixture.remap(Direction::Up).message(&message).expect("mapped");
        assert_eq!(mapped["somethingNew"]["nested"], json!([1, 2, 3]));
    }

    #[test]
    fn a_line_outside_any_region_maps_identically() {
        let fixture = fixture("local x = 1\nlocal y = 2\n");
        let at = json!({ "position": position(1, 6) });

        let down = fixture.remap(Direction::Down).message(&at).expect("mapped");
        assert_eq!(down, at);
    }
}
