//! What is at the cursor, on a file that does not parse.
//!
//! `<Fra` and `{cou` are not valid LuauX, and **that is the normal state of a
//! file being typed**. Every feature people notice most — tag, attribute and
//! close-tag completion — therefore has to work without the AST, so this is a
//! tolerant lexical scan: it never fails, and it never refuses to answer
//! because something later in the file is incomplete.
//!
//! Only the *entry* decision reuses the compiler: whether a given `<` opens
//! LuauX or is a comparison is [`luaux::markup_scan::Scanner`]'s job, and
//! guessing differently here would put the cursor in the wrong world. When the
//! Luau lexer itself gives up — an unterminated string a few lines above is
//! enough — the scan falls back to the same heuristic the TextMate grammar uses
//! and carries on.

use luaux::lexer::{Lexer, TokenKind};
use luaux::markup_scan::Scanner;

/// An element whose closing tag has not been seen yet.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpenTag {
    pub name: String,
    /// Offset of the `<`.
    pub start: usize,
}

/// Where the cursor is, in LuauX terms.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Context {
    /// Ordinary Luau, outside any LuauX region.
    Luau,
    /// Inside a captured `{ … }`. Verbatim Luau, so it is forwarded.
    Expression { start: usize, end: usize },
    /// Typing an element name: `<Fra`, or `</Fra` when `closing`.
    TagName { start: usize, prefix: String, closing: bool },
    /// Inside an open tag, on or before an attribute name: `<Frame Te`.
    AttributeName { tag: String, start: usize, prefix: String },
    /// Inside a quoted attribute value: `<Frame Name="…`.
    AttributeValue { tag: String, name: String },
    /// Text between tags.
    Text { tag: Option<String> },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Scan {
    pub context: Context,
    /// Enclosing elements, innermost last.
    pub open: Vec<OpenTag>,
}

impl Scan {
    /// The element the cursor is inside, if any.
    pub fn enclosing(&self) -> Option<&OpenTag> {
        self.open.last()
    }
}

/// Where the component's own name begins inside a tag name.
///
/// A tag name may be a member expression — `<App.Header/>` — because this scan
/// takes `.` as part of one, and the two ends of such a name answer different
/// questions. The last segment is the component; everything before it is the
/// table it came out of.
pub fn member_offset(name: &str) -> usize {
    name.rfind('.').map(|dot| dot + 1).unwrap_or(0)
}

/// Classifies `cursor` — a byte offset — against `source`.
pub fn scan(source: &str, cursor: usize) -> Scan {
    let mut state = State { source, cursor, found: None, open: Vec::new(), at_cursor: Vec::new() };

    state.luau(0, source.len());

    Scan { context: state.found.unwrap_or(Context::Luau), open: state.at_cursor }
}

struct State<'a> {
    source: &'a str,
    cursor: usize,
    found: Option<Context>,
    open: Vec<OpenTag>,
    at_cursor: Vec<OpenTag>,
}

impl<'a> State<'a> {
    /// Whether `[start, end]` contains the cursor. Inclusive at both ends: the
    /// cursor sits *between* bytes, and both edges of a construct are inside it
    /// as far as completion is concerned.
    fn holds(&self, start: usize, end: usize) -> bool {
        start <= self.cursor && self.cursor <= end
    }

    /// Records a construct that definitely contains the cursor. Later wins,
    /// because the walk goes outside-in: a tag inside a hole is the better
    /// answer than the hole.
    fn record(&mut self, context: Context) {
        self.at_cursor = self.open.clone();
        self.found = Some(context);
    }

    /// Records only if nothing has claimed the cursor yet.
    ///
    /// For the two places that describe *absence* — an unclosed tag, and the end
    /// of the text — where something more specific may have already answered.
    fn record_fallback(&mut self, context: Context) {
        if self.found.is_none() {
            self.record(context);
        }
    }

    /// Scans Luau text, handing off at each `<` that opens LuauX.
    fn luau(&mut self, from: usize, to: usize) {
        let text = &self.source[..to];
        let mut lexer = Lexer::at(text, from);
        let mut scanner = Scanner::new(text);

        loop {
            let Some(token) = lexer.next_token() else { return };

            let token = match token {
                Ok(token) => token,
                // A half-typed string a few lines up is enough to stop the Luau
                // lexer. Falling back keeps the cursor's own line answerable,
                // which is the whole point of scanning tolerantly.
                Err(error) => return self.heuristic(error.offset, to),
            };

            let opens = scanner.feed(token, &Lexer::at(text, token.end));

            if !opens {
                continue;
            }

            let end = self.markup(token.start, to);
            lexer.seek(end);
            scanner.note_luaux_region();
        }
    }

    /// Last-resort entry detection: `<` immediately followed by a name, `/` or
    /// `>`. The same rule the grammar uses, and wrong in the same cosmetic way —
    /// `a <b` reads as a tag. Only reached once the Luau lexer has already failed.
    fn heuristic(&mut self, from: usize, to: usize) {
        let bytes = self.source.as_bytes();
        let mut at = from;

        while at < to {
            if bytes[at] == b'<' && opens_tag(self.source, at) {
                at = self.markup(at, to);
                continue;
            }
            at += 1;
        }
    }

    /// Scans one LuauX region from its `<`, returning where it ended.
    ///
    /// "Ended" is best-effort: an unclosed element runs to `to`, which is
    /// correct — everything after it really is still inside the element as far
    /// as the person typing is concerned.
    fn markup(&mut self, from: usize, to: usize) -> usize {
        let depth = self.open.len();
        let mut at = from;

        while at < to {
            // Sitting exactly on a construct's `<` or `{` means sitting in the
            // text before it. Anything that really does contain the cursor
            // records over this.
            if self.cursor == at {
                let tag = self.open.last().map(|open| open.name.clone());
                self.record_fallback(Context::Text { tag });
            }

            let rest = &self.source[at..to];

            if rest.starts_with("<!--") {
                at = match rest.find("-->") {
                    Some(end) => at + end + 3,
                    None => to,
                };
                continue;
            }

            if rest.starts_with("</") {
                at = self.close_tag(at, to);
                if self.open.len() <= depth {
                    return at;
                }
                continue;
            }

            if rest.starts_with('<') {
                at = self.open_tag(at, to);
                // A fragment or element that self-closed at the outermost level
                // ends the region.
                if self.open.len() <= depth {
                    return at;
                }
                continue;
            }

            if rest.starts_with('{') {
                at = self.hole(at, to);
                continue;
            }

            // Text, up to the next construct.
            let end = rest.find(['<', '{']).map(|offset| at + offset).unwrap_or(to);

            if self.holds(at, end) {
                let tag = self.open.last().map(|open| open.name.clone());
                self.record(Context::Text { tag });
            }

            at = end.max(at + 1);
        }

        // The end of an unclosed element is still inside it.
        if self.holds(at, to) {
            let tag = self.open.last().map(|open| open.name.clone());
            self.record_fallback(Context::Text { tag });
        }

        to
    }

    /// `<Frame …>` / `<Frame …/>` / `<>`.
    fn open_tag(&mut self, from: usize, to: usize) -> usize {
        let name_start = from + 1;
        let name_end = identifier_end(self.source, name_start, to);
        let name = self.source[name_start..name_end].to_string();

        // `<` and `<Fra` are both a name being typed.
        if self.holds(name_start, name_end) {
            self.record(Context::TagName {
                start: name_start,
                prefix: self.source[name_start..self.cursor.min(name_end)].to_string(),
                closing: false,
            });
        }

        let mut at = name_end;

        // A fragment: `<>` has no name and no attributes.
        if name.is_empty() {
            if at < to && self.source.as_bytes()[at] == b'>' {
                self.open.push(OpenTag { name: String::new(), start: from });
                return at + 1;
            }
            return at.max(from + 1);
        }

        loop {
            if at >= to {
                // Unclosed. Everything after the name is still inside this tag —
                // unless the cursor is on the name itself, which is someone
                // typing the element, not an attribute.
                if self.cursor > name_end && self.holds(name_end, to) {
                    self.record_fallback(Context::AttributeName {
                        tag: name.clone(),
                        start: to,
                        prefix: String::new(),
                    });
                }
                return to;
            }

            let rest = &self.source[at..to];

            if rest.starts_with("/>") {
                return at + 2;
            }

            if rest.starts_with('>') {
                self.open.push(OpenTag { name, start: from });
                return at + 1;
            }

            let byte = self.source.as_bytes()[at];

            if byte.is_ascii_whitespace() {
                // Whitespace inside a tag is where a new attribute goes — but
                // only *after* the space. Before it, the cursor is still on
                // whatever precedes, usually the element name being typed.
                if self.cursor == at + 1 {
                    self.record(Context::AttributeName {
                        tag: name.clone(),
                        start: self.cursor,
                        prefix: String::new(),
                    });
                }
                at += 1;
                continue;
            }

            if byte == b'{' {
                at = self.hole(at, to);
                continue;
            }

            if byte == b'_' || byte.is_ascii_alphabetic() {
                let end = identifier_end(self.source, at, to);
                let attribute = self.source[at..end].to_string();

                if self.holds(at, end) {
                    self.record(Context::AttributeName {
                        tag: name.clone(),
                        start: at,
                        prefix: self.source[at..self.cursor.min(end)].to_string(),
                    });
                }

                at = self.attribute_value(&name, &attribute, end, to);
                continue;
            }

            // Something that belongs in none of the above — a stray character,
            // or a non-breaking space, which the LuauX parser does not count as
            // whitespace either. Stepping over it by one *byte* would land
            // inside it and the next slice would panic.
            at = step(self.source, at);
        }
    }

    /// Whatever follows an attribute name: `=` and a value, or nothing.
    fn attribute_value(&mut self, tag: &str, name: &str, from: usize, to: usize) -> usize {
        let mut at = from;

        while at < to && self.source.as_bytes()[at] == b' ' {
            at += 1;
        }

        if at >= to || self.source.as_bytes()[at] != b'=' {
            return from;
        }

        at += 1;
        while at < to && self.source.as_bytes()[at] == b' ' {
            at += 1;
        }

        if at >= to {
            return at;
        }

        match self.source.as_bytes()[at] {
            b'{' => self.hole(at, to),
            quote @ (b'"' | b'\'') => {
                let end = string_end(self.source, at, to, quote);
                if self.holds(at + 1, end.min(to)) {
                    self.record(Context::AttributeValue {
                        tag: tag.to_string(),
                        name: name.to_string(),
                    });
                }
                (end + 1).min(to)
            }
            _ => at,
        }
    }

    /// `</Frame>`, closing the innermost element it matches.
    fn close_tag(&mut self, from: usize, to: usize) -> usize {
        let name_start = from + 2;
        let name_end = identifier_end(self.source, name_start, to);

        if self.holds(name_start, name_end) {
            self.record(Context::TagName {
                start: name_start,
                prefix: self.source[name_start..self.cursor.min(name_end)].to_string(),
                closing: true,
            });
        }

        self.open.pop();

        match self.source[name_end..to].find('>') {
            Some(offset) => name_end + offset + 1,
            None => to,
        }
    }

    /// A `{ … }` hole. Its contents are Luau, and may hold LuauX in turn.
    fn hole(&mut self, from: usize, to: usize) -> usize {
        let end = match luaux::lexer::find_matching_brace(self.source, from) {
            Ok(end) if end < to => end,
            // Unclosed — the hole owns the rest of what we are scanning, which
            // is exactly right while it is being typed.
            _ => to,
        };

        if self.holds(from + 1, end) {
            self.record(Context::Expression { start: from + 1, end });
        }

        // Nested LuauX inside the hole is scanned in its own right, and wins
        // over the enclosing Expression because it records later.
        self.luau(from + 1, end);

        (end + 1).min(to)
    }
}

/// The element a `>` just typed at `offset` has left open, if any.
///
/// Backs auto-closing: type `<Frame>` and `</Frame>` should appear after the
/// cursor, as it does in every editor's HTML and JSX support. Deciding it here
/// rather than in the extension is what makes it right — whether a `<` opened a
/// tag at all is [`Scanner`]'s question, and the answer differs from what a
/// regex would say.
///
/// An empty name is a fragment, which closes with `</>`.
pub fn closing_tag(source: &str, offset: usize) -> Option<String> {
    let before = source.get(..offset)?;

    // Only ever just after a `>`, and not the `/>` that already closed one.
    if !before.ends_with('>') || before.ends_with("/>") {
        return None;
    }

    // `</Frame>` also ends in `>`, and closes rather than opens. Without this,
    // finishing a closing tag would offer to close whatever encloses it.
    let opened = before.rfind('<')?;
    if before[opened..].starts_with("</") {
        return None;
    }

    let scan = scan(source, offset);

    // Between the tags is where the cursor lands, and where the text would go.
    if !matches!(scan.context, Context::Text { .. }) {
        return None;
    }

    let open = scan.open.last()?;

    // Already closed by hand, or by a previous round of this.
    let rest = source.get(offset..)?.trim_start();
    if rest.starts_with(&format!("</{}>", open.name)) {
        return None;
    }

    Some(open.name.clone())
}

/// Whether the `<` at `at` looks like a tag rather than a comparison.
fn opens_tag(source: &str, at: usize) -> bool {
    match source.as_bytes().get(at + 1) {
        Some(b'/' | b'>') => true,
        Some(byte) => *byte == b'_' || byte.is_ascii_alphabetic(),
        None => false,
    }
}

fn identifier_end(source: &str, from: usize, to: usize) -> usize {
    let bytes = source.as_bytes();
    let mut at = from;

    while at < to {
        let byte = bytes[at];
        // Dots belong to the name: `<Foo.Bar/>` is one element.
        if byte == b'_' || byte == b'.' || byte.is_ascii_alphanumeric() {
            at += 1;
        } else {
            break;
        }
    }

    at
}

/// Offset of the closing quote, or `to` if the string never closes.
fn string_end(source: &str, open: usize, to: usize, quote: u8) -> usize {
    let bytes = source.as_bytes();
    let mut at = open + 1;

    while at < to {
        match bytes[at] {
            // The escape, and then the whole character it escapes — `\é` is
            // three bytes, not two.
            b'\\' => at = step(source, step(source, at)),
            b'\n' => return at,
            byte if byte == quote => return at,
            _ => at = step(source, at),
        }
    }

    to
}

/// The next character boundary after `at`.
///
/// Everything in this scan slices `source` at the offset it has reached, so an
/// offset that lands inside a character is not a slightly-wrong answer — it is a
/// panic, and a panic here takes every feature down over one keystroke.
fn step(source: &str, at: usize) -> usize {
    crate::line_index::ceil_boundary(source, at + 1)
}

/// The token kind a name has, so callers can tell a keyword from an identifier
/// without re-lexing.
pub fn is_name(source: &str) -> bool {
    matches!(
        luaux::lexer::tokenize(source).as_deref(),
        Ok([token]) if token.kind == TokenKind::Name
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Scans with `|` marking the cursor.
    fn at(marked: &str) -> Scan {
        let cursor = marked.find('|').expect("a cursor marker");
        let source = marked.replace('|', "");
        scan(&source, cursor)
    }

    #[test]
    fn plain_luau_is_plain_luau() {
        assert_eq!(at("local x = 1|").context, Context::Luau);
        assert_eq!(at("local ok = a <| b").context, Context::Luau);
        assert_eq!(at("local m: Map<string|, number> = f()").context, Context::Luau);
    }

    #[test]
    fn a_half_typed_tag_is_a_tag_name() {
        let scan = at("local e = <Fra|");
        assert!(
            matches!(&scan.context, Context::TagName { prefix, closing: false, .. } if prefix == "Fra"),
            "{:?}",
            scan.context
        );
    }

    #[test]
    fn a_bare_open_angle_is_a_tag_name_too() {
        // The moment someone types `<`, the class list is what they want.
        let scan = at("local e = <|");
        assert!(
            matches!(&scan.context, Context::TagName { prefix, .. } if prefix.is_empty()),
            "{:?}",
            scan.context
        );
    }

    #[test]
    fn a_closing_tag_is_marked_as_one() {
        let scan = at("local e = <Frame></Fra|");
        assert!(
            matches!(&scan.context, Context::TagName { closing: true, prefix, .. } if prefix == "Fra"),
            "{:?}",
            scan.context
        );
    }

    #[test]
    fn attribute_position_knows_its_element() {
        let scan = at("local e = <TextLabel Te|");
        assert!(
            matches!(&scan.context, Context::AttributeName { tag, prefix, .. }
                if tag == "TextLabel" && prefix == "Te"),
            "{:?}",
            scan.context
        );
    }

    #[test]
    fn whitespace_inside_a_tag_is_attribute_position() {
        let scan = at("local e = <TextLabel |/>");
        assert!(
            matches!(&scan.context, Context::AttributeName { tag, prefix, .. }
                if tag == "TextLabel" && prefix.is_empty()),
            "{:?}",
            scan.context
        );
    }

    #[test]
    fn an_attribute_string_is_its_own_context() {
        let scan = at("local e = <Frame Name=\"a|\"/>");
        assert!(
            matches!(&scan.context, Context::AttributeValue { tag, name }
                if tag == "Frame" && name == "Name"),
            "{:?}",
            scan.context
        );
    }

    #[test]
    fn a_hole_is_an_expression() {
        let scan = at("local e = <Frame Size={siz|}/>");
        assert!(matches!(scan.context, Context::Expression { .. }), "{:?}", scan.context);
    }

    #[test]
    fn an_unclosed_hole_still_reads_as_an_expression() {
        // `{cou` is the normal state of a hole being typed.
        let scan = at("local e = <Frame Size={cou|");
        assert!(matches!(scan.context, Context::Expression { .. }), "{:?}", scan.context);
    }

    #[test]
    fn text_between_tags_is_text() {
        let scan = at("local e = <TextLabel>Hel|lo</TextLabel>");
        assert!(
            matches!(&scan.context, Context::Text { tag: Some(tag) } if tag == "TextLabel"),
            "{:?}",
            scan.context
        );
    }

    #[test]
    fn nested_luaux_inside_a_hole_wins_over_the_hole() {
        let scan = at("local e = <Frame>{cond and <Ro| or nil}</Frame>");
        assert!(
            matches!(&scan.context, Context::TagName { prefix, .. } if prefix == "Ro"),
            "{:?}",
            scan.context
        );
    }

    #[test]
    fn the_open_tag_stack_is_reported_innermost_last() {
        let scan = at("local e = <Frame><ScrollingFrame><TextLabel>|");
        let names: Vec<&str> = scan.open.iter().map(|open| open.name.as_str()).collect();
        assert_eq!(names, ["Frame", "ScrollingFrame", "TextLabel"]);
    }

    #[test]
    fn a_closed_element_leaves_the_stack() {
        let scan = at("local e = <Frame><TextLabel/>|</Frame>");
        let names: Vec<&str> = scan.open.iter().map(|open| open.name.as_str()).collect();
        assert_eq!(names, ["Frame"]);
    }

    #[test]
    fn scanning_survives_a_broken_string_earlier_in_the_file() {
        // The Luau lexer cannot get past line 1; the cursor's own line must
        // still answer, or completion dies exactly when a file is mid-edit.
        let scan = at("local s = \"unterminated\nlocal e = <Fra|");
        assert!(
            matches!(&scan.context, Context::TagName { prefix, .. } if prefix == "Fra"),
            "{:?}",
            scan.context
        );
    }

    #[test]
    fn a_fragment_nests_like_an_element() {
        let scan = at("local e = <><Frame>|</Frame></>");
        let names: Vec<&str> = scan.open.iter().map(|open| open.name.as_str()).collect();
        assert_eq!(names, ["", "Frame"]);
    }

    #[test]
    fn member_names_are_one_name() {
        let scan = at("local e = <Foo.Ba|r/>");
        assert!(
            matches!(&scan.context, Context::TagName { prefix, .. } if prefix == "Foo.Ba"),
            "{:?}",
            scan.context
        );
    }

    /// Every offset of a file full of multi-byte characters, in every position
    /// the scanner can be asked about. Nothing here may panic — a language
    /// server that dies on a keystroke is worse than one that answers vaguely.
    #[test]
    fn scanning_never_panics_on_multi_byte_text() {
        for source in [
            // Option+Space, which the LuauX parser does not count as whitespace.
            "local e = <TextButton\u{a0}Text='x'/>",
            "local e = <Frame Name='ü\u{a0}😀'/>",
            "local e = <TextLabel>héllo — 😀</TextLabel>",
            "local e = <Frame Name='a\\é'/>",
            "local e = <Frame Size={ü + 1}/>",
            "local e = <Frame Name='unterminated\u{a0}",
            "local s = \"broken\nlocal e = <Fra\u{a0}",
        ] {
            for cursor in 0..=source.len() {
                if source.is_char_boundary(cursor) {
                    let _ = scan(source, cursor);
                }
            }
        }
    }

    #[test]
    fn a_non_breaking_space_is_not_an_attribute_name() {
        // It is not whitespace to the parser, so it separates nothing — but the
        // element name before it is still what is being typed.
        let scan = at("local e = <TextButton\u{a0}Te|xt='x'/>");
        assert!(
            matches!(&scan.context, Context::AttributeName { tag, .. } if tag == "TextButton"),
            "{:?}",
            scan.context
        );
    }

    /// Where `|` is, having just typed the character before it.
    fn closes(marked: &str) -> Option<String> {
        let cursor = marked.find('|').expect("a cursor marker");
        closing_tag(&marked.replace('|', ""), cursor)
    }

    #[test]
    fn an_opened_tag_wants_closing() {
        assert_eq!(closes("local e = <Frame>|"), Some("Frame".into()));
        assert_eq!(closes("local e = <Frame><TextLabel>|"), Some("TextLabel".into()));
        // A fragment closes with `</>`.
        assert_eq!(closes("local e = <>|"), Some(String::new()));
        assert_eq!(closes("local e = <Frame Name='a'>|"), Some("Frame".into()));
    }

    #[test]
    fn nothing_that_is_already_closed_wants_closing_again() {
        assert_eq!(closes("local e = <Frame/>|"), None);
        assert_eq!(closes("local e = <Frame>|</Frame>"), None);
        // Finishing a closing tag must not offer to close what encloses it.
        assert_eq!(closes("local e = <Panel><Frame></Frame>|"), None);
    }

    #[test]
    fn a_comparison_is_not_a_tag_to_close() {
        assert_eq!(closes("local ok = a > b|"), None);
        assert_eq!(closes("local m: Map<string, number>|"), None);
        assert_eq!(closes("local x = 1|"), None);
    }

    #[test]
    fn closing_is_only_offered_right_after_the_bracket() {
        // A `>` further back was dealt with when it was typed.
        assert_eq!(closes("local e = <Frame>a|"), None);
    }

    #[test]
    fn code_after_a_region_is_luau_again() {
        assert_eq!(at("local e = <Frame/>\nlocal after = 1|").context, Context::Luau);
        assert_eq!(at("local e = <Frame>a</Frame>\nlocal after = 1|").context, Context::Luau);
    }
}
