//! Building a [`SourceMap`] from a `.luaux` source and the `.luau` the compiler
//! produced from it.
//!
//! The compiler does not hand out a map, so this recovers one. Two properties
//! make that possible without guessing:
//!
//! 1. **Lines are preserved**, so region *N* of the source occupies the same
//!    output lines, which pins every region's output range without searching.
//! 2. **Expressions are captured verbatim**, so each one appears byte for byte in
//!    the output, immediately after generated text whose exact shape is known —
//!    `Size = `, `Text = `, `__luaux_read(`.
//!
//! Everything located here is then checked by [`SourceMap::push`], which refuses
//! any run whose two sides do not actually agree. So a shape this builder does
//! not recognise costs *coverage* — that position simply stops being forwarded —
//! and can never produce a position pointing at the wrong code (decision 6).

use crate::line_index::LineIndex;
use crate::regions::{self, braced};
use crate::sourcemap::{Run, SourceMap};
use luaux::backend::Helpers;
use luaux::config::Config;
use luaux::markup::{Attribute, AttributeValue, Child, Element, Node, Span};
use luaux::resolve::{blank_luaux_regions, Resolution, Resolver};
use luaux::roblox;

/// A searched run has to be at least this long. Short needles match coincidences
/// — a lone `x` occurs inside `create("TextBox")` — and a coincidental match is
/// the one failure mode [`SourceMap::push`] cannot catch, since the text really
/// is identical.
const MIN_SEARCH: usize = 3;

pub fn build(source: &str, output: &str, config: &Config) -> SourceMap {
    let mut map = SourceMap::default();
    let found = regions::regions(source);

    let spans: Vec<(usize, usize)> = found.iter().map(|r| (r.start, r.end)).collect();
    let blanked = blank_luaux_regions(source, &spans);
    let resolver = Resolver::new(&blanked, config.clone());

    let source_lines = LineIndex::new(source);
    let output_lines = LineIndex::new(output);

    // Line preservation is the load-bearing assumption. If it does not hold, the
    // whole approach is invalid, so produce nothing rather than something wrong.
    if source_lines.line_count() != output_lines.line_count() {
        return map;
    }

    let mut builder = Builder {
        source,
        output,
        resolver: &resolver,
        map: &mut map,
        cursor: 0,
        limit: output.len(),
    };

    let split = preamble(source, output, &resolver, config);
    let mut source_cursor = 0usize;
    let mut output_cursor = 0usize;
    let mut index = 0usize;

    while index < found.len() {
        // Regions sharing a line have to be mapped as a group: only the last one
        // on the line can have its output end pinned by the line's suffix.
        let mut last = index;
        while last + 1 < found.len()
            && source_lines.line_of(found[last].end) == source_lines.line_of(found[last + 1].start)
        {
            last += 1;
        }

        let Some(group_end) =
            region_output_end(source, output, &source_lines, &output_lines, found[last].end)
        else {
            // Without a bound the rest of the file cannot be placed. Stop, and
            // keep what is already known to be right.
            return map;
        };

        builder.identity(&mut source_cursor, &mut output_cursor, found[index].start, split);

        builder.cursor = output_cursor;
        builder.limit = group_end;

        for step in index..=last {
            builder.node(&found[step].node);

            if step < last {
                let between = found[step].end;
                let next = found[step + 1].start;
                builder.verbatim(between, next - between);
            }
        }

        output_cursor = group_end;
        source_cursor = found[last].end;
        index = last + 1;
    }

    builder.identity(&mut source_cursor, &mut output_cursor, source.len(), split);
    map
}

/// Where the generated region ending at `source_end` stops in the output.
///
/// Everything after a region on its final line is copied verbatim, so the output
/// line's own end minus that suffix is the answer — no search, no guess.
fn region_output_end(
    source: &str,
    output: &str,
    source_lines: &LineIndex,
    output_lines: &LineIndex,
    source_end: usize,
) -> Option<usize> {
    let line = source_lines.line_of(source_end);
    let (_, source_line_end) = source_lines.line_range(source, line);
    let (output_line_start, output_line_end) = output_lines.line_range(output, line);

    let suffix = source_line_end.checked_sub(source_end)?;
    let end = output_line_end.checked_sub(suffix)?;

    // The suffix must really be there; if it is not, the line does not line up
    // and nothing about this region can be trusted.
    (end >= output_line_start
        && output.get(end..output_line_end) == source.get(source_end..source_line_end))
    .then_some(end)
}

/// Where the injected helper preamble goes, and how long it is.
///
/// `luaux::imports::inject` puts helpers on the line of the first statement, so
/// everything from there on shifts. Calling `inject` itself rather than
/// reconstructing the text keeps this correct if the helpers ever change.
fn preamble(
    source: &str,
    output: &str,
    resolver: &Resolver,
    config: &Config,
) -> Option<(usize, usize)> {
    let bound = resolver.bound();

    let helpers = Helpers {
        create: false,
        merge_props: output.contains(&format!("local function {}(", luaux::imports::MERGE_HELPER))
            && !bound.contains(luaux::imports::MERGE_HELPER),
        read: output.contains(&format!("local function {}(", luaux::imports::READ_HELPER))
            && !bound.contains(luaux::imports::READ_HELPER),
    };

    if !helpers.merge_props && !helpers.read {
        return None;
    }

    let offset = regions::first_statement_offset(source)?;
    // A one-byte stand-in for the file, so what comes back is preamble + "x".
    let injected = luaux::imports::inject("x", helpers, bound, config).ok()?;

    Some((offset, injected.len().checked_sub(1)?))
}

struct Builder<'a> {
    source: &'a str,
    output: &'a str,
    resolver: &'a Resolver,
    map: &'a mut SourceMap,
    /// Output offset reached so far. Monotone: the compiler emits in source
    /// order, so nothing is ever located behind it.
    cursor: usize,
    /// End of the region currently being mapped. Searches never cross it.
    limit: usize,
}

impl Builder<'_> {
    /// Records the untouched stretch from `source_cursor` up to `until`.
    ///
    /// Split at the injection point, since the helper preamble sits between the
    /// two halves and belongs to neither.
    fn identity(
        &mut self,
        source_cursor: &mut usize,
        output_cursor: &mut usize,
        until: usize,
        split: Option<(usize, usize)>,
    ) {
        let mut start = *source_cursor;

        if let Some((at, length)) = split {
            if start <= at && at < until {
                if at > start {
                    self.map.push(
                        self.source,
                        self.output,
                        Run { source: start, output: *output_cursor, length: at - start },
                    );
                    *output_cursor += at - start;
                }
                *output_cursor += length;
                start = at;
            }
        }

        if until > start {
            self.map.push(
                self.source,
                self.output,
                Run { source: start, output: *output_cursor, length: until - start },
            );
            *output_cursor += until - start;
        }

        *source_cursor = until;
        self.cursor = *output_cursor;
    }

    /// Locates generated text and steps over it, recording nothing.
    ///
    /// Anchors like `Size = ` and `__luaux_read(` exist only in the output, so
    /// they carry no run — their value is that they put the cursor exactly on
    /// the verbatim text that follows.
    fn anchor(&mut self, needle: &str) -> bool {
        let Some(window) = self.output.get(self.cursor..self.limit) else {
            return false;
        };
        let Some(at) = window.find(needle) else {
            return false;
        };

        self.cursor += at + needle.len();
        true
    }

    /// Records `length` source bytes as a run, wherever they landed.
    ///
    /// Sitting at the cursor is the ordinary case and needs no search at all.
    /// The fallback exists for text that follows a nested region, whose emitted
    /// length is unknown — and it insists on a single unambiguous occurrence,
    /// because picking between two is guessing.
    fn verbatim(&mut self, source_start: usize, length: usize) -> bool {
        let Some(text) = self.source.get(source_start..source_start + length) else {
            return false;
        };
        if text.is_empty() {
            return false;
        }

        let Some(window) = self.output.get(self.cursor..self.limit) else {
            return false;
        };

        let at = if window.starts_with(text) {
            0
        } else if text.len() >= MIN_SEARCH {
            match window.find(text) {
                Some(at) if !window[at + 1..].contains(text) => at,
                _ => return false,
            }
        } else {
            return false;
        };

        let output = self.cursor + at;
        if !self.map.push(self.source, self.output, Run { source: source_start, output, length }) {
            return false;
        }

        self.cursor = output + length;
        true
    }

    /// Records a component's tag name, which is emitted as the call it becomes.
    ///
    /// Searched as `Row(` rather than as `Row`, because a bare name is far too
    /// easy to find twice and an ambiguous search records nothing at all:
    /// `<TextLabel><Label/></TextLabel>` has one inside the string
    /// `"TextLabel"`, which costs the whole feature for that element.
    ///
    /// Uniqueness is still required even so. Emission is in source order, so the
    /// *first* match is nearly always the right one — and "nearly always" is
    /// exactly what decision 6 refuses. An attribute whose value happens to
    /// contain `Row(`, on an element whose own name did not map, would put the
    /// run on text the author never wrote.
    fn component_name(&mut self, source_start: usize, name: &str) -> bool {
        if name.is_empty() || self.source.get(source_start..source_start + name.len()) != Some(name)
        {
            return false;
        }

        let needle = format!("{name}(");
        let Some(window) = self.output.get(self.cursor..self.limit) else { return false };

        let at = if window.starts_with(&needle) {
            0
        } else {
            match window.find(&needle) {
                Some(at) if !window[at + 1..].contains(&needle) => at,
                _ => return false,
            }
        };

        let output = self.cursor + at;
        let run = Run { source: source_start, output, length: name.len() };

        if !self.map.push(self.source, self.output, run) {
            return false;
        }

        self.cursor = output + name.len();
        true
    }

    /// Steps over `Key = `, recording the key itself when the author wrote it.
    ///
    /// An attribute name is generated in the sense that the compiler decides
    /// where to put it — but when no alias renamed it, the text is the author's,
    /// character for character. Recording it is what lets luau-lsp answer about
    /// `Activated` at all: hover, go-to-definition, references and highlight on
    /// an attribute name have nowhere to go without a run here.
    ///
    /// An alias breaks that: `bgColor` in the source is `BackgroundColor3` in
    /// the output, so there is no shared text and no run — and the position
    /// falls back to what this server can say for itself.
    fn attribute_key(&mut self, written: &str, canonical: &str, source_start: usize) -> bool {
        let anchor = format!("{canonical} = ");

        let Some(window) = self.output.get(self.cursor..self.limit) else { return false };
        let Some(at) = window.find(&anchor) else { return false };

        let output = self.cursor + at;

        if written == canonical {
            self.map.push(
                self.source,
                self.output,
                Run { source: source_start, output, length: written.len() },
            );
        }

        self.cursor = output + anchor.len();
        true
    }

    fn node(&mut self, node: &Node) {
        match node {
            Node::Element(element) => self.element(element),
            Node::Fragment(fragment) => {
                let plan = TextPlan::default();
                self.children(&fragment.children, &plan);
            }
        }
    }

    fn element(&mut self, element: &Element) {
        // Mirrors the compiler's recovery: a name that resolves to neither is
        // emitted as written, with its attributes left alone. The output for
        // such an element exists now, so refusing to map it would take hover and
        // completion away from the whole rest of the file over one bad tag —
        // which is the cost the recovery was for.
        let (intrinsic, resolved) = match self.resolver.resolve(&element.name, element.span.start) {
            Ok(Resolution::Intrinsic(class)) => (Some(class), true),
            Ok(Resolution::Component) => (None, true),
            Ok(Resolution::Unresolved(written)) => (Some(written), false),
            Err(_) => (Some(element.name.as_written()), false),
        };

        // A component's tag name is a *reference*: `<Row/>` emits `Row(`, the
        // same identifier the author wrote and bound. Recording it is what lets
        // luau-lsp answer about it at all — its inferred type, and the doc
        // comment above its binding — none of which this server can work out.
        //
        // An intrinsic's name is not a reference. `<Frame/>` emits
        // `create("Frame")`, where the text matches but means a string literal,
        // so a run there would point hover at a `string`. That is worse than
        // saying nothing, and worse than the answer we give ourselves.
        if intrinsic.is_none() {
            let written = element.name.as_written();
            let (start, _) = crate::tree::open_name(self.source, element.span.start, &written);
            self.component_name(start, &written);
        }

        // Unresolved elements get no text plan, exactly as in the backend: with
        // no class there is nothing to decide what its text children become.
        let plan = match resolved {
            true => TextPlan::of(element, intrinsic.as_deref()),
            false => TextPlan::default(),
        };

        for attribute in &element.attributes {
            match attribute {
                Attribute::Spread { span, .. } => {
                    if let Some((start, end)) = braced(self.source, span.start) {
                        self.expression(start, end);
                    }
                }
                Attribute::Named { name, value, span } => {
                    // Rule 5: text between the tags replaces a `Text` attribute,
                    // so the attribute is never emitted.
                    if plan.emits_text && name == "Text" {
                        continue;
                    }

                    let key = match (&intrinsic, resolved) {
                        (Some(class), true) => {
                            match self.resolver.resolve_attribute(class, name, span.start) {
                                Ok(key) => key,
                                Err(_) => continue,
                            }
                        }
                        _ => name.clone(),
                    };

                    match value {
                        AttributeValue::Expression(_) => {
                            if !self.attribute_key(name, &key, span.start) {
                                continue;
                            }
                            if let Some((start, end)) = braced(self.source, span.start) {
                                self.expression(start, end);
                            }
                        }
                        AttributeValue::StringLiteral(literal) => {
                            if !self.attribute_key(name, &key, span.start) {
                                continue;
                            }
                            // The span ends just past the closing quote, and the
                            // literal is stored with its quotes.
                            let start = span.end.saturating_sub(literal.len());
                            self.verbatim(start, literal.len());
                        }
                        AttributeValue::Boolean if self.attribute_key(name, &key, span.start) => {
                            // `Visible` becomes `Visible = true`; the `true` is
                            // ours, so it maps to nothing.
                            self.anchor("true");
                        }
                        // Renamed by an alias, so the written name is nowhere in
                        // the output. Step over the pair and record nothing.
                        AttributeValue::Boolean => {
                            self.anchor(&format!("{key} = true"));
                        }
                    }
                }
            }
        }

        self.text(&plan);
        self.children(&element.children, &plan);
    }

    /// The `Text` property that literal and expression children lower into.
    fn text(&mut self, plan: &TextPlan) {
        if !plan.emits_text || !self.anchor("Text = ") {
            return;
        }

        match plan.parts.as_slice() {
            // A lone expression is emitted bare — Vide treats a function on a
            // property key as a source either way.
            [TextPart::Expression(span)] => {
                if let Some((start, end)) = braced(self.source, span.start) {
                    self.expression(start, end);
                }
            }
            parts => {
                for part in parts {
                    let TextPart::Expression(span) = part else { continue };
                    if !self.anchor(&format!("{}(", luaux::imports::READ_HELPER)) {
                        continue;
                    }
                    if let Some((start, end)) = braced(self.source, span.start) {
                        self.expression(start, end);
                    }
                }
            }
        }
    }

    fn children(&mut self, children: &[Child], plan: &TextPlan) {
        for child in children {
            match child {
                Child::Node(node) => self.node(node),
                Child::Expression { span, .. } if !plan.consumes_expressions => {
                    if let Some((start, end)) = braced(self.source, span.start) {
                        self.expression(start, end);
                    }
                }
                _ => {}
            }
        }
    }

    /// A captured expression, which is verbatim except where LuauX is nested in
    /// it — those parts were compiled and have to be walked, not matched.
    fn expression(&mut self, start: usize, end: usize) {
        let Some(text) = self.source.get(start..end) else { return };
        let nested = regions::regions(text);

        if nested.is_empty() {
            self.verbatim(start, end - start);
            return;
        }

        let mut cursor = 0usize;

        for region in &nested {
            if region.start > cursor {
                self.verbatim(start + cursor, region.start - cursor);
            }
            self.region_at(start + region.start);
            cursor = region.end;
        }

        if cursor < text.len() {
            self.verbatim(start + cursor, text.len() - cursor);
        }
    }

    /// Re-parses a nested region against the outer source so its spans are in
    /// file coordinates, then walks it like any other.
    fn region_at(&mut self, offset: usize) {
        if let Ok((node, _)) = luaux::markup::parse_node(self.source, offset) {
            self.node(&node);
        }
    }
}

/// Which children lower into the `Text` property. Mirrors the Vide backend's own
/// rules closely enough to know what it emitted, and stays silent when it cannot
/// tell — an unrecognised shape costs coverage, never correctness.
#[derive(Default)]
struct TextPlan {
    emits_text: bool,
    consumes_expressions: bool,
    parts: Vec<TextPart>,
}

enum TextPart {
    Literal,
    Expression(Span),
}

impl TextPlan {
    fn of(element: &Element, intrinsic: Option<&str>) -> Self {
        let literals = element.children.iter().any(|child| matches!(child, Child::Text { .. }));
        let expressions =
            element.children.iter().any(|child| matches!(child, Child::Expression { .. }));
        let nodes = element.children.iter().any(|child| matches!(child, Child::Node(_)));

        // A component takes children, not text; a class with no `Text` property
        // takes expressions as ordinary children. Both compile without a plan.
        let Some(class) = intrinsic else { return Self::default() };

        if (!literals && !expressions)
            || !roblox::has_text_property(class)
            // Ambiguous, and the compiler refuses rather than guessing.
            || (expressions && nodes)
        {
            return Self::default();
        }

        let parts = element
            .children
            .iter()
            .filter_map(|child| match child {
                Child::Text { .. } => Some(TextPart::Literal),
                Child::Expression { span, .. } => Some(TextPart::Expression(*span)),
                _ => None,
            })
            .collect();

        Self { emits_text: true, consumes_expressions: expressions, parts }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use luaux::backend::Vide;

    /// Compiles like the CLI does, then maps the pair.
    fn compiled(source: &str) -> (String, SourceMap, Config) {
        let config = Config::with_create("create");
        let (output, _) = luaux::compile::compile_configured(source, &Vide, config.clone())
            .unwrap_or_else(|error| panic!("compile {source:?}: {error}"));
        let map = build(source, &output, &config);
        (output, map, config)
    }

    /// `to_source(to_output(n)) == n` over every run — the invariant every
    /// forwarded feature depends on. And that a run really is the same text on
    /// both sides, which is what makes the position mean anything once it gets
    /// there.
    fn assert_round_trips(source: &str, output: &str, map: &SourceMap) {
        for run in map.runs() {
            for step in 0..run.length {
                let offset = run.source + step;
                let to = map
                    .to_output(offset)
                    .unwrap_or_else(|| panic!("{offset} in a run did not map"));
                assert_eq!(map.to_source(to), Some(offset), "round trip at {offset}");
            }

            assert_eq!(
                &source[run.source..run.source + run.length],
                &output[run.output..run.output + run.length],
                "run at {} is not verbatim",
                run.source,
            );
        }
    }

    /// Offset of `needle` in the source, mapped, then read back out of the output.
    fn maps_to<'a>(
        source: &str,
        output: &'a str,
        map: &SourceMap,
        needle: &str,
    ) -> Option<&'a str> {
        let at = source.find(needle)?;
        let to = map.to_output(at)?;
        output.get(to..to + needle.len())
    }

    const FIXTURES: &[&str] = &[
        "local create = f()\nlocal e = <Frame/>\n",
        "local create = f()\nlocal e = <Frame Size={size} Name='a' Visible />\n",
        "local create = f()\nlocal e = <TextLabel>Clicked {count} times</TextLabel>\n",
        "local create = f()\nlocal e = <TextButton>{label}</TextButton>\n",
        "local create = f()\nlocal e = <Frame>{cond and child or nil}</Frame>\n",
        "local create = f()\nlocal e = <Frame {props} Name={n} />\n",
        "local create = f()\nlocal e = (\n  <Frame\n    Size={size}\n    Visible\n  >\n    <TextLabel>Hi</TextLabel>\n  </Frame>\n)\n",
        "local create = f()\nlocal Row = f()\nlocal e = <Frame>{cond and <Row Name={n}/> or nil}</Frame>\n",
        "local create = f()\nlocal e = (\n  <TextButton\n    Activated={function()\n      count(count() + 1)\n    end}\n  />\n)\n",
        "--!strict\nlocal create = f()\nlocal p = {}\nlocal e = <Frame {p} Name={n} />\n",
    ];

    #[test]
    fn every_run_round_trips() {
        for fixture in FIXTURES {
            let (output, map, _) = compiled(fixture);
            assert_round_trips(fixture, &output, &map);
        }
    }

    #[test]
    fn untouched_source_maps_identically() {
        let source = "local x = 1\nlocal y = 2\n";
        let (output, map, _) = compiled(source);
        assert_eq!(output, source);
        assert_eq!(map.to_output(0), Some(0));
        assert_eq!(map.to_output(source.len() - 1), Some(source.len() - 1));
    }

    #[test]
    fn captured_expressions_are_mapped() {
        let source = "local create = f()\nlocal e = <Frame Size={size}/>\n";
        let (output, map, _) = compiled(source);
        assert_eq!(maps_to(source, &output, &map, "size"), Some("size"));
    }

    #[test]
    fn code_after_a_region_maps_back() {
        let source = "local create = f()\nlocal e = <Frame/>\nlocal after = 1\n";
        let (output, map, _) = compiled(source);
        assert_eq!(maps_to(source, &output, &map, "after"), Some("after"));
    }

    #[test]
    fn generated_text_maps_to_nothing_in_either_direction() {
        let source = "local create = f()\nlocal e = <Frame Size={size}/>\n";
        let (output, map, _) = compiled(source);

        // `create("Frame")({ Size = ` is ours, not the author's.
        let generated = output.find("create(\"Frame\")").expect("generated");
        assert_eq!(map.to_source(generated), None);
        // And the tag itself has no counterpart in the output.
        assert_eq!(map.to_output(source.find("<Frame").expect("tag")), None);
    }

    #[test]
    fn the_injected_preamble_shifts_its_line_and_no_other() {
        // A spread pulls in the merge helper, which lands on the first statement.
        let source =
            "local create = f()\nlocal p = {}\nlocal e = <Frame {p} Name={n} />\nlocal tail = 1\n";
        let (output, map, _) = compiled(source);
        assert!(output.contains("__luaux_merge"), "{output}");

        // Line 0 shifted right by the preamble...
        let first = map.to_output(0).expect("start of file maps");
        assert!(first > 0, "the preamble should have shifted line 0");
        // ...and a later line did not.
        let tail = source.find("tail").expect("tail");
        assert_eq!(maps_to(source, &output, &map, "tail"), Some("tail"));
        assert_eq!(map.to_output(tail), output.find("tail"));
    }

    #[test]
    fn interpolated_text_maps_each_expression() {
        let source = "local create = f()\nlocal e = <TextLabel>Clicked {count} times</TextLabel>\n";
        let (output, map, _) = compiled(source);
        assert_eq!(maps_to(source, &output, &map, "count"), Some("count"));
    }

    #[test]
    fn luaux_nested_in_an_expression_maps_its_own_attributes() {
        let source =
            "local create = f()\nlocal Row = f()\nlocal e = <Frame>{cond and <Row Name={n}/> or nil}</Frame>\n";
        let (output, map, _) = compiled(source);

        assert_eq!(maps_to(source, &output, &map, "cond and "), Some("cond and "));
        assert_eq!(maps_to(source, &output, &map, " or nil"), Some(" or nil"));
        // The nested element's own attribute expression, inside a compiled region.
        let n = source.find("{n}").expect("attribute") + 1;
        let to = map.to_output(n).expect("attribute expression maps");
        assert_eq!(&output[to..to + 1], "n");
    }

    #[test]
    fn multi_line_expressions_map_across_lines() {
        let source = "local create = f()\nlocal e = (\n  <TextButton\n    Activated={function()\n      count(count() + 1)\n    end}\n  />\n)\n";
        let (output, map, _) = compiled(source);
        assert_eq!(
            maps_to(source, &output, &map, "count(count() + 1)"),
            Some("count(count() + 1)")
        );
    }

    /// A whole realistic file — several regions, a fragment, markup and hole
    /// comments, a component, interpolated text, and LuauX nested two deep
    /// inside a captured function.
    ///
    /// Coverage is what this is about: the unit fixtures each check one shape,
    /// and a real file is where the shapes interact.
    const REALISTIC: &str = r#"local vide = require("@pkg/vide")
local create, source = vide.create, vide.source

local function Button(props)
  return (
    <TextButton
      Size={UDim2.fromOffset(160, 40)}
      Activated={props.OnClick}
      Text={props.Label}
    >
      <UICorner CornerRadius={UDim.new(0, 8)} />
    </TextButton>
  )
end

return function()
  local count = source(0)

  return (
    <Frame Size={UDim2.fromScale(1, 1)} BackgroundTransparency={1}>
      <!-- Interpolated text is reactive. -->
      <TextLabel>Clicked {count} times</TextLabel>
      {--[[ a comment in a hole ]]}

      <Button Label="Click me" OnClick={function() count(count() + 1) end} />

      {function()
        return count() > 4 and (<TextLabel>that is a lot</TextLabel>) or nil
      end}
    </Frame>
  )
end
"#;

    #[test]
    fn a_realistic_file_maps_every_captured_expression() {
        let (output, map, _) = compiled(REALISTIC);
        assert_round_trips(REALISTIC, &output, &map);

        for expression in [
            "UDim2.fromOffset(160, 40)",
            "props.OnClick",
            "props.Label",
            "UDim.new(0, 8)",
            "UDim2.fromScale(1, 1)",
            "\"Click me\"",
            "function() count(count() + 1) end",
            // Inside the conditional child, which is itself inside a hole.
            "count() > 4",
        ] {
            let at =
                REALISTIC.find(expression).unwrap_or_else(|| panic!("{expression} in fixture"));
            let to = map.to_output(at).unwrap_or_else(|| panic!("{expression} did not map"));

            assert_eq!(&output[to..to + expression.len()], expression);
        }
    }

    #[test]
    fn an_attribute_name_maps_when_no_alias_renamed_it() {
        let source = "local create = f()\nlocal e = <Frame Visible={v} Name='a'/>\n";
        let (output, map, _) = compiled(source);
        assert_round_trips(source, &output, &map);

        // Without this, hover, references and go-to-definition on an attribute
        // name have nowhere to go.
        for name in ["Visible", "Name"] {
            let at = source.find(name).expect("attribute");
            let to = map.to_output(at).unwrap_or_else(|| panic!("{name} did not map"));
            assert_eq!(&output[to..to + name.len()], name);
        }
    }

    #[test]
    fn an_aliased_attribute_name_maps_to_nothing() {
        // `bgColor` is nowhere in the output — it compiled to
        // `BackgroundColor3` — so there is no position to forward to, and
        // pointing at the canonical name would be pointing at text the author
        // never wrote.
        let source = "local create = f()\nlocal e = <Frame bgColor={c}/>\n";
        let config =
            Config::parse("[factory]\ncreate = \"create\"\n\n[properties.Frame]\nBackgroundColor3 = \"bgColor\"\n")
                .expect("config");
        let (output, _) =
            luaux::compile::compile_configured(source, &Vide, config.clone()).expect("compile");
        let map = build(source, &output, &config);

        assert_round_trips(source, &output, &map);
        assert_eq!(map.to_output(source.find("bgColor").expect("attribute")), None);
        // The value beside it still maps, so the expression is not lost with it.
        let value = source.find("{c}").expect("value") + 1;
        assert!(map.to_output(value).is_some());
    }

    /// A component tag is emitted as the call it becomes, so its name maps and
    /// luau-lsp can be asked about it. An intrinsic's is not: `<Frame/>` emits
    /// `create("Frame")`, where the same text means a string literal.
    #[test]
    fn a_component_tag_name_maps_and_a_class_tag_name_does_not() {
        let source = "local create = f()\nlocal Row = f()\nlocal e = <Row/>\n";
        let (output, map, _) = compiled(source);
        assert_round_trips(source, &output, &map);

        let at = source.rfind("Row").expect("tag");
        let to = map.to_output(at).expect("the component name maps");
        assert_eq!(&output[to..to + 3], "Row");

        // The class name appears in the output only inside a string.
        let class = "local create = f()\nlocal e = <Frame/>\n";
        let (_, map, _) = compiled(class);
        assert_eq!(map.to_output(class.find("Frame").expect("tag")), None);
    }

    /// The name is searched for as `Row(`, so one that also occurs inside the
    /// enclosing class's own string literal is still found.
    #[test]
    fn a_component_named_inside_its_parents_class_still_maps() {
        let source =
            "local create = f()\nlocal Label = f()\nlocal e = <TextLabel><Label/></TextLabel>\n";
        let (output, map, _) = compiled(source);
        assert_round_trips(source, &output, &map);

        let at = source.find("<Label/>").expect("tag") + 1;
        let to = map.to_output(at).expect("the component name maps");
        assert_eq!(&output[to..to + 5], "Label");
    }

    /// Two of the same component in one region cannot be told apart by a search,
    /// and the answer to that is *no run* — coverage, never a wrong position
    /// (decision 6). Recorded so the limit is a decision rather than a surprise.
    #[test]
    fn two_sibling_components_of_one_name_map_to_nothing() {
        let source = "local create = f()\nlocal Row = f()\nlocal e = <Frame><Row/><Row/></Frame>\n";
        let (output, map, _) = compiled(source);
        assert_round_trips(source, &output, &map);

        let first = source.find("<Row/>").expect("tag") + 1;
        assert_eq!(map.to_output(first), None);
    }

    /// `< Row/>` parses, so the name is not at `span.start + 1`. The run has to
    /// land on the name the author wrote or not exist at all.
    #[test]
    fn whitespace_after_the_open_angle_does_not_shift_the_run() {
        let source = "local create = f()\nlocal Row = f()\nlocal e = < Row/>\n";
        let (output, map, _) = compiled(source);
        assert_round_trips(source, &output, &map);

        let at = source.find("Row/>").expect("tag");
        let to = map.to_output(at).expect("the component name maps");
        assert_eq!(&output[to..to + 3], "Row");
    }

    #[test]
    fn a_mismatched_line_count_yields_an_empty_map() {
        // Nothing here claims these two correspond, and the builder must not
        // pretend otherwise.
        let map = build("local a = 1\n", "local a = 1\nlocal b = 2\n", &Config::default());
        assert!(map.is_empty());
    }

    #[test]
    fn two_regions_on_one_line_both_map() {
        let source = "local create = f()\nlocal a, b = <Frame Name={x}/>, <Frame Name={y}/>\n";
        let (output, map, _) = compiled(source);
        assert_round_trips(source, &output, &map);

        for name in ["{x}", "{y}"] {
            let at = source.find(name).expect("attribute") + 1;
            let to = map.to_output(at).expect("attribute expression maps");
            assert_eq!(&output[to..to + 1], &name[1..2]);
        }
    }
}
