//! Where a name was bound.
//!
//! The compiler's resolver answers *whether* a name is bound, which is all it
//! needs to decide intrinsic-versus-component. Hover and go-to-definition want
//! the position as well, and that is a lexical question: find the `local`,
//! `local function` or `function` that introduces it.
//!
//! Deliberately not a scope analysis. A tag resolves against the whole file, so
//! the first binding of a name is the one that made `<Row/>` legal, and a
//! shadowed local of the same name is not a different component.

use luaux::lexer::{tokenize, Token, TokenKind};
use luaux::resolve::blank_luaux_regions;

/// Byte range of the defining occurrence of `name`, if there is one.
pub fn find(source: &str, name: &str) -> Option<(usize, usize)> {
    // `.luaux` is not lexable as Luau, so the regions go first.
    let blanked = blank_luaux_regions(source, &crate::regions::spans(source));
    let tokens = tokenize(&blanked).ok()?;

    let significant: Vec<&Token> = tokens.iter().filter(|token| !token.is_trivia()).collect();

    for (index, token) in significant.iter().enumerate() {
        if token.kind != TokenKind::Name || token.text(&blanked) != name {
            continue;
        }

        let previous = index.checked_sub(1).map(|at| significant[at]);
        let keyword = previous.map(|token| token.text(&blanked));

        let introduced = match keyword {
            // `local Row`, `local a, Row`, `function Row`
            Some("local" | "function") => true,
            Some(",") => {
                index.checked_sub(2).is_some_and(|at| is_local_list(&blanked, &significant, at))
            }
            _ => false,
        };

        if introduced {
            return Some((token.start, token.end));
        }
    }

    None
}

/// Walks back over `a, b, c` to see whether the list started with `local`.
fn is_local_list(source: &str, tokens: &[&Token], mut at: usize) -> bool {
    loop {
        match tokens[at].text(source) {
            "local" => return true,
            "," => {}
            text if tokens[at].kind == TokenKind::Name && !text.is_empty() => {}
            _ => return false,
        }

        match at.checked_sub(1) {
            Some(previous) => at = previous,
            None => return false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn found(source: &str, name: &str) -> Option<String> {
        find(source, name).map(|(start, end)| source[start..end].to_string())
    }

    #[test]
    fn finds_the_usual_shapes() {
        for source in [
            "local Row = require('./Row')",
            "local function Row() end",
            "function Row() end",
            "local Row",
        ] {
            assert_eq!(found(source, "Row").as_deref(), Some("Row"), "{source}");
        }
    }

    #[test]
    fn finds_a_name_later_in_a_local_list() {
        assert_eq!(found("local a, Row = f()", "Row").as_deref(), Some("Row"));
        assert_eq!(found("local a, b, Row = f()", "Row").as_deref(), Some("Row"));
    }

    #[test]
    fn a_use_is_not_a_binding() {
        assert_eq!(found("Row()", "Row"), None);
        assert_eq!(found("local x = Row", "Row"), None);
        // `t.Row = 1` assigns a field, not a name.
        assert_eq!(found("local t = {}\nt.Row = 1", "Row"), None);
    }

    #[test]
    fn the_binding_is_found_past_luaux() {
        // The region is blanked first, or the file does not lex at all.
        let source = "local e = <Frame/>\nlocal Row = f()";
        let at = find(source, "Row").expect("a binding");
        assert_eq!(&source[at.0..at.1], "Row");
        assert!(at.0 > source.find("Frame").expect("region"));
    }

    #[test]
    fn an_unknown_name_is_not_invented() {
        assert_eq!(found("local x = 1", "Row"), None);
    }
}
