//! The element tree, flattened into the tag occurrences features actually ask
//! about.
//!
//! Document symbols want the nesting, semantic tokens and hover want individual
//! tag names, and rename wants an open/close *pair*. All three come off the same
//! walk, so they cannot disagree about what an element is.
//!
//! Nested LuauX inside a captured expression is walked too. The expression is
//! held verbatim by the parser, so the region inside it is still source text at
//! this point and re-parsing it in place is what makes `{cond and <Row/> or nil}`
//! appear in the outline like anything else.

use crate::regions;
use luaux::markup::{Attribute, AttributeValue, Child, Element, ElementName, Node};

/// One element, with the spans of the names as written.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tag {
    pub name: String,
    /// Whole element, `<` to `>`.
    pub start: usize,
    pub end: usize,
    /// The name in the opening tag.
    pub open_name: (usize, usize),
    /// The name in the closing tag, absent when self-closing or a fragment.
    pub close_name: Option<(usize, usize)>,
    /// A `Name="…"` attribute, for the outline.
    pub label: Option<String>,
    pub children: Vec<Tag>,
}

impl Tag {
    /// Whether `offset` is on either of this element's names.
    pub fn name_at(&self, offset: usize) -> Option<(usize, usize)> {
        [Some(self.open_name), self.close_name]
            .into_iter()
            .flatten()
            .find(|(start, end)| *start <= offset && offset <= *end)
    }
}

/// Every outermost element in `source`, with its descendants.
pub fn tree(source: &str) -> Vec<Tag> {
    regions::regions(source).iter().filter_map(|region| tag(source, &region.node)).collect()
}

/// Depth-first, outermost first.
pub fn flatten(tags: &[Tag]) -> Vec<&Tag> {
    let mut out = Vec::new();

    for tag in tags {
        out.push(tag);
        out.extend(flatten(&tag.children));
    }

    out
}

/// The innermost element whose span holds `offset`.
pub fn innermost_at(tags: &[Tag], offset: usize) -> Option<&Tag> {
    flatten(tags).into_iter().rfind(|tag| tag.start <= offset && offset <= tag.end)
}

fn tag(source: &str, node: &Node) -> Option<Tag> {
    match node {
        Node::Element(element) => Some(element_tag(source, element)),
        Node::Fragment(fragment) => Some(Tag {
            name: String::new(),
            start: fragment.span.start,
            end: fragment.span.end,
            // `<>` has a name of length zero, which is still a position.
            open_name: (fragment.span.start + 1, fragment.span.start + 1),
            close_name: None,
            label: None,
            children: children(source, &fragment.children),
        }),
    }
}

fn element_tag(source: &str, element: &Element) -> Tag {
    let written = element.name.as_written();
    let open_name = open_name(source, element.span.start, &written);

    let mut nested = children(source, &element.children);
    nested.extend(attribute_regions(source, element));
    nested.sort_by_key(|tag| tag.start);

    Tag {
        name: written.clone(),
        start: element.span.start,
        end: element.span.end,
        open_name,
        close_name: close_name(source, element, &written),
        label: label(element),
        children: nested,
    }
}

/// Where the name sits inside `< Frame …>`, given the offset of the `<`.
///
/// Found rather than computed, for the same reason as [`close_name`]: the parser
/// skips whitespace after the `<`, so `< Frame/>` is a legal element whose name
/// does **not** begin at `span.start + 1`. Computing it there underlines the
/// wrong bytes on hover, and rename works from these same spans — so it would
/// rewrite `" Fr"` and leave the rest, which is data loss.
///
/// Shared with [`crate::map_builder`], which needs the same offset to record the
/// run that lets luau-lsp answer about a component tag.
pub fn open_name(source: &str, open_angle: usize, written: &str) -> (usize, usize) {
    let mut start = open_angle + 1;
    while source.as_bytes().get(start).is_some_and(u8::is_ascii_whitespace) {
        start += 1;
    }

    (start, start + written.len())
}

/// Where the name sits inside `</Frame >`.
///
/// Found rather than computed, because whitespace is allowed on both sides of
/// the name and an off-by-one here would rename the wrong bytes.
fn close_name(source: &str, element: &Element, written: &str) -> Option<(usize, usize)> {
    let span = &source.get(element.span.start..element.span.end)?;
    let at = span.rfind("</")?;

    let mut start = element.span.start + at + 2;
    while source.as_bytes().get(start).is_some_and(u8::is_ascii_whitespace) {
        start += 1;
    }

    (source.get(start..start + written.len()) == Some(written))
        .then_some((start, start + written.len()))
}

/// `Name="Header"` — the one attribute worth showing in an outline.
fn label(element: &Element) -> Option<String> {
    element.attributes.iter().find_map(|attribute| match attribute {
        Attribute::Named { name, value: AttributeValue::StringLiteral(literal), .. }
            if name == "Name" =>
        {
            Some(literal.trim_matches(['"', '\'']).to_string())
        }
        _ => None,
    })
}

fn children(source: &str, children: &[Child]) -> Vec<Tag> {
    let mut out = Vec::new();

    for child in children {
        match child {
            Child::Node(node) => out.extend(tag(source, node)),
            // LuauX nested in a captured expression, re-parsed where it sits.
            Child::Expression { span, .. } => out.extend(embedded(source, span.start)),
            _ => {}
        }
    }

    out
}

fn attribute_regions(source: &str, element: &Element) -> Vec<Tag> {
    element
        .attributes
        .iter()
        .flat_map(|attribute| match attribute {
            Attribute::Spread { span, .. } => embedded(source, span.start),
            Attribute::Named { value: AttributeValue::Expression(_), span, .. } => {
                embedded(source, span.start)
            }
            _ => Vec::new(),
        })
        .collect()
}

/// Elements inside the `{ … }` starting at or after `from`.
fn embedded(source: &str, from: usize) -> Vec<Tag> {
    let Some((start, end)) = regions::braced(source, from) else {
        return Vec::new();
    };
    let Some(text) = source.get(start..end) else {
        return Vec::new();
    };

    // Scanned in the slice, re-parsed against the whole file so every span is in
    // file coordinates.
    regions::regions(text)
        .iter()
        .filter_map(|region| {
            let (node, _) = luaux::markup::parse_node(source, start + region.start).ok()?;
            tag(source, &node)
        })
        .collect()
}

/// Whether a name is written with a dot, which makes it a component whatever
/// else is true (no Roblox class has one).
pub fn is_dotted(name: &ElementName) -> bool {
    matches!(name, ElementName::Member(_))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(source: &str) -> Vec<String> {
        flatten(&tree(source)).into_iter().map(|tag| tag.name.clone()).collect()
    }

    #[test]
    fn walks_nested_elements() {
        assert_eq!(
            names("local e = <Frame><TextLabel/><UICorner/></Frame>"),
            ["Frame", "TextLabel", "UICorner"]
        );
    }

    #[test]
    fn walks_into_captured_expressions() {
        assert_eq!(names("local e = <Frame>{cond and <Row/> or nil}</Frame>"), ["Frame", "Row"]);
        assert_eq!(names("local e = <Frame Size={f(<TextLabel/>)} />"), ["Frame", "TextLabel"]);
    }

    #[test]
    fn finds_both_halves_of_a_pair() {
        let source = "local e = <Frame></Frame>";
        let tags = tree(source);
        let frame = &tags[0];

        assert_eq!(&source[frame.open_name.0..frame.open_name.1], "Frame");
        let close = frame.close_name.expect("a closing name");
        assert_eq!(&source[close.0..close.1], "Frame");
        assert!(close.0 > frame.open_name.0);
    }

    #[test]
    fn a_self_closing_element_has_no_closing_name() {
        assert_eq!(tree("local e = <Frame/>")[0].close_name, None);
    }

    /// `< Frame/>` is legal: the parser skips whitespace after the `<`. Hover
    /// underlines these bytes and rename *rewrites* them, so computing the name
    /// as `span.start + 1` turns a rename into `" Fr"` plus leftovers.
    #[test]
    fn whitespace_after_the_open_angle_does_not_shift_the_name() {
        let source = "local e = < Frame />";
        let open = tree(source)[0].open_name;
        assert_eq!(&source[open.0..open.1], "Frame");
    }

    #[test]
    fn whitespace_in_a_closing_tag_does_not_shift_the_name() {
        let source = "local e = <Frame></ Frame >";
        let close = tree(source)[0].close_name.expect("a closing name");
        assert_eq!(&source[close.0..close.1], "Frame");
    }

    #[test]
    fn the_name_attribute_labels_the_outline() {
        assert_eq!(tree("local e = <Frame Name=\"Header\"/>")[0].label.as_deref(), Some("Header"));
        assert_eq!(tree("local e = <Frame/>")[0].label, None);
    }

    #[test]
    fn the_innermost_element_wins() {
        let source = "local e = <Frame><TextLabel/></Frame>";
        let tags = tree(source);
        let at = source.find("TextLabel").expect("inner");

        assert_eq!(innermost_at(&tags, at).map(|tag| tag.name.as_str()), Some("TextLabel"));
        assert_eq!(innermost_at(&tags, 0).map(|tag| tag.name.as_str()), None);
    }

    #[test]
    fn a_position_on_either_name_is_on_the_element() {
        let source = "local e = <Frame></Frame>";
        let frame = &tree(source)[0];

        assert!(frame.name_at(source.find("Frame").expect("open")).is_some());
        assert!(frame.name_at(source.rfind("Frame").expect("close")).is_some());
        assert!(frame.name_at(0).is_none());
    }

    #[test]
    fn fragments_appear_with_their_children() {
        assert_eq!(names("local e = (<><Frame/></>)"), ["", "Frame"]);
    }
}
