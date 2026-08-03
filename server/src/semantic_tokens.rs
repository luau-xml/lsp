//! Semantic tokens — where the server beats the grammar.
//!
//! TextMate is regex-only. It cannot know whether `<Button>` is a Roblox class
//! or a component defined on line 3, so both get one colour. The resolver does
//! know, and saying so is a small feature with an outsized effect: the file
//! starts communicating structure the grammar cannot see.
//!
//! The grammar stays as the zero-latency fallback for when the server is
//! starting, crashed or absent (decision 5), so these tokens only ever *refine*
//! a file that is already highlighted.

use crate::document::Document;
use crate::line_index::Position;
use crate::project::Project;
use crate::regions;
use crate::tree::{self, Tag};
use luaux::resolve::{blank_luaux_regions, Resolver};
use luaux::roblox;
use serde_json::{json, Value};

/// The legend, in the order the indices below refer to. Reported in
/// `initialize`, and the client is entitled to assume it never changes.
pub const TYPES: &[&str] = &["class", "function"];

const CLASS: u32 = 0;
const COMPONENT: u32 = 1;

pub fn tokens(document: &Document, project: &Project) -> Value {
    let source = &document.text;

    let blanked = blank_luaux_regions(source, &regions::spans(source));
    let resolver = Resolver::new(&blanked, project.config.clone());

    let mut spans: Vec<(usize, usize, u32)> = Vec::new();
    for tag in tree::flatten(&tree::tree(source)) {
        collect(tag, project, &resolver, &mut spans);
    }

    // Both names of an element are emitted, and a nested element's names sit
    // between them, so sorting is not optional — the encoding is a delta chain.
    spans.sort_by_key(|(start, _, _)| *start);

    let mut data: Vec<u32> = Vec::new();
    let mut previous = Position::new(0, 0);

    for (start, end, kind) in spans {
        let at = document.index.position(source, start);
        let length = document.index.position(source, end).character.saturating_sub(at.character);

        if length == 0 {
            continue;
        }

        let line = at.line - previous.line;
        let character = if line == 0 { at.character - previous.character } else { at.character };

        data.extend([line, character, length, kind, 0]);
        previous = at;
    }

    json!({ "data": data })
}

fn collect(tag: &Tag, project: &Project, resolver: &Resolver, out: &mut Vec<(usize, usize, u32)>) {
    // A fragment has no name to colour.
    if tag.name.is_empty() {
        return;
    }

    let kind = classify(&tag.name, project, resolver);

    // A name that resolves to neither is an error the compiler already reports;
    // colouring it as either would be a guess dressed up as knowledge.
    let Some(kind) = kind else { return };

    out.push((tag.open_name.0, tag.open_name.1, kind));
    if let Some((start, end)) = tag.close_name {
        out.push((start, end, kind));
    }
}

fn classify(written: &str, project: &Project, resolver: &Resolver) -> Option<u32> {
    // No Roblox class has a dot, so a dotted name is a component by construction.
    if written.contains('.') {
        return Some(COMPONENT);
    }

    match project.config.resolve_element(written) {
        Ok(Some(_)) => Some(CLASS),
        // Retired by an alias: an error, not a class.
        Err(_) => None,
        Ok(None) if roblox::is_class(written) => Some(CLASS),
        Ok(None) if resolver.bound().contains(written) => Some(COMPONENT),
        Ok(None) => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use luaux::config::Config;
    use std::path::Path;

    fn project(config: &str) -> Project {
        let mut project = Project::discover(Path::new("/nonexistent-luaux-project/a.luaux"));
        project.config = Config::parse(config).expect("config");
        project
    }

    /// Decoded back into (line, character, length, kind).
    fn decoded(source: &str, config: &str) -> Vec<(u32, u32, u32, u32)> {
        let document = Document::new("file:///a.luaux".into(), 1, source.into());
        let value = tokens(&document, &project(config));
        let data: Vec<u32> = serde_json::from_value(value["data"].clone()).expect("data");

        let mut out = Vec::new();
        let (mut line, mut character) = (0u32, 0u32);

        for chunk in data.chunks(5) {
            line += chunk[0];
            character = if chunk[0] == 0 { character + chunk[1] } else { chunk[1] };
            out.push((line, character, chunk[2], chunk[3]));
        }

        out
    }

    #[test]
    fn a_class_and_a_component_are_told_apart() {
        let tokens = decoded("local Row = f()\nlocal e = <Frame><Row/></Frame>", "");
        let kinds: Vec<u32> = tokens.iter().map(|token| token.3).collect();

        // Frame open, Row, Frame close.
        assert_eq!(kinds, [CLASS, COMPONENT, CLASS]);
    }

    #[test]
    fn positions_are_the_names_themselves() {
        let tokens = decoded("local e = <Frame/>", "");
        assert_eq!(tokens, [(0, 11, 5, CLASS)]);
    }

    #[test]
    fn both_halves_of_a_pair_are_coloured() {
        let tokens = decoded("local e = <Frame></Frame>", "");
        assert_eq!(tokens.len(), 2);
        assert_eq!(tokens[1].1, 19);
    }

    #[test]
    fn deltas_are_relative_and_ordered() {
        let tokens = decoded("local e = (\n  <Frame>\n    <UICorner/>\n  </Frame>\n)", "");
        let lines: Vec<u32> = tokens.iter().map(|token| token.0).collect();
        assert_eq!(lines, [1, 2, 3]);
    }

    #[test]
    fn a_dotted_name_is_a_component() {
        let tokens = decoded("local Foo = f()\nlocal e = <Foo.Bar/>", "");
        assert_eq!(tokens.iter().map(|token| token.3).collect::<Vec<_>>(), [COMPONENT]);
    }

    #[test]
    fn an_alias_is_still_a_class() {
        let tokens = decoded("local e = <text/>", "[elements]\nTextLabel = \"text\"\n");
        assert_eq!(tokens.iter().map(|token| token.3).collect::<Vec<_>>(), [CLASS]);
    }

    #[test]
    fn a_name_that_resolves_to_nothing_is_left_alone() {
        // The compiler already reports it; the grammar's colour is the honest
        // fallback rather than a confident wrong one.
        assert!(decoded("local e = <Frmae/>", "").is_empty());
    }

    #[test]
    fn nested_luaux_in_an_expression_is_coloured_too() {
        let tokens =
            decoded("local Row = f()\nlocal e = <Frame>{cond and <Row/> or nil}</Frame>", "");
        assert_eq!(
            tokens.iter().map(|token| token.3).collect::<Vec<_>>(),
            [CLASS, COMPONENT, CLASS]
        );
    }
}
