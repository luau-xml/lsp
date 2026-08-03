//! Locating the LuauX regions in a `.luaux` file.
//!
//! Mirrors what `luaux::compile` does internally: drive the lexer until the
//! scanner says a `<` opens LuauX, hand off to the LuauX parser, then resume past
//! the region. The compiler keeps its version private, so this one exists — but
//! it uses the same public `Lexer`, `Scanner` and `parse_node`, so the two agree
//! by construction rather than by coincidence.

use luaux::lexer::Lexer;
use luaux::markup::{self, Node};
use luaux::markup_scan::Scanner;

/// One outermost LuauX region: its byte span and its parsed tree.
pub struct Region {
    pub start: usize,
    pub end: usize,
    pub node: Node,
}

/// Every outermost LuauX region in `source`.
///
/// Stops at the first thing that will not lex or parse, returning what it found
/// so far. A half-typed file is the normal state, so a partial answer is the
/// useful one — callers that need to know whether the file is whole ask the
/// compiler, which reports the error properly.
pub fn regions(source: &str) -> Vec<Region> {
    let mut lexer = Lexer::new(source);
    let mut scanner = Scanner::new(source);
    let mut found = Vec::new();

    while let Some(token) = lexer.next_token() {
        let Ok(token) = token else { break };
        let lookahead = Lexer::at(source, token.end);

        if !scanner.feed(token, &lookahead) {
            continue;
        }

        let Ok((node, end)) = markup::parse_node(source, token.start) else {
            break;
        };

        found.push(Region { start: token.start, end, node });
        lexer.seek(end);
        scanner.note_luaux_region();
    }

    found
}

/// Byte spans only, for callers that just need to know where LuauX is.
pub fn spans(source: &str) -> Vec<(usize, usize)> {
    regions(source).into_iter().map(|region| (region.start, region.end)).collect()
}

/// Byte range of the trimmed expression inside the `{ … }` at or after `from`.
///
/// The LuauX parser trims what it captures, so it is the trimmed slice that
/// corresponds to emitted text — and the trimmed slice that a nested region's
/// offsets are relative to.
pub fn braced(source: &str, from: usize) -> Option<(usize, usize)> {
    let open = from + source.get(from..)?.find('{')?;
    let close = luaux::lexer::find_matching_brace(source, open).ok()?;

    let inner = source.get(open + 1..close)?;
    let start = open + 1 + (inner.len() - inner.trim_start().len());
    let end = start + inner.trim().len();

    (end > start).then_some((start, end))
}

/// Byte offset of the first non-trivia token — where `luaux::imports` injects
/// its helpers, and so the one place a shift can appear outside a LuauX region.
pub fn first_statement_offset(source: &str) -> Option<usize> {
    let mut lexer = Lexer::new(source);

    while let Some(token) = lexer.next_token() {
        let token = token.ok()?;
        if !token.is_trivia() {
            return Some(token.start);
        }
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_each_outermost_region() {
        let source = "local a = <Frame/>\nlocal b = <TextLabel/>";
        let spans = spans(source);
        assert_eq!(spans.len(), 2);
        assert_eq!(&source[spans[0].0..spans[0].1], "<Frame/>");
        assert_eq!(&source[spans[1].0..spans[1].1], "<TextLabel/>");
    }

    #[test]
    fn nested_luaux_is_inside_its_outermost_region() {
        let source = "local e = <Frame>{cond and <Row/> or nil}</Frame>";
        let spans = spans(source);
        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0].1, source.len());
    }

    #[test]
    fn comparisons_are_not_regions() {
        assert!(spans("local ok = a < b").is_empty());
        assert!(spans("local m: Map<string, number> = f()").is_empty());
    }

    #[test]
    fn a_half_typed_tag_yields_what_came_before_it() {
        // `<Fra` does not parse. The earlier region is still reported.
        let source = "local a = <Frame/>\nlocal b = <Fra";
        assert_eq!(spans(source).len(), 1);
    }

    #[test]
    fn the_injection_point_follows_leading_directives() {
        let source = "--!strict\n-- note\nlocal x = 1";
        assert_eq!(first_statement_offset(source), Some(source.find("local").unwrap()));
        assert_eq!(first_statement_offset("-- only a comment\n"), None);
    }
}
