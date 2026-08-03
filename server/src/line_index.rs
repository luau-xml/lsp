//! Byte offsets ↔ LSP positions.
//!
//! LSP counts characters in UTF-16 code units and Rust counts bytes, so every
//! position crossing the protocol boundary passes through here. luau-lsp
//! negotiates `utf-16` too, so the same conversion serves both directions of the
//! proxy.

/// Line starts, for one revision of one document.
#[derive(Debug, Clone, Default)]
pub struct LineIndex {
    /// Byte offset of the start of each line. Always begins with 0.
    starts: Vec<usize>,
    length: usize,
}

/// A zero-based LSP position.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Position {
    pub line: u32,
    /// UTF-16 code units from the start of the line.
    pub character: u32,
}

impl Position {
    pub fn new(line: u32, character: u32) -> Self {
        Self { line, character }
    }
}

impl LineIndex {
    pub fn new(text: &str) -> Self {
        let mut starts = vec![0];
        starts.extend(
            text.bytes().enumerate().filter(|(_, byte)| *byte == b'\n').map(|(index, _)| index + 1),
        );

        Self { starts, length: text.len() }
    }

    pub fn line_count(&self) -> usize {
        self.starts.len()
    }

    /// Byte offset where `line` starts, or the end of the file if it is past the
    /// last line.
    pub fn line_start(&self, line: usize) -> usize {
        self.starts.get(line).copied().unwrap_or(self.length)
    }

    /// Byte range of `line`, excluding its terminator.
    pub fn line_range(&self, text: &str, line: usize) -> (usize, usize) {
        let start = self.line_start(line);
        let end =
            self.starts.get(line + 1).map(|next| next.saturating_sub(1)).unwrap_or(self.length);
        // A `\r\n` terminator leaves the `\r` behind.
        let end = if text[start..end].ends_with('\r') { end - 1 } else { end };
        (start, end)
    }

    /// Zero-based line containing `offset`.
    pub fn line_of(&self, offset: usize) -> usize {
        match self.starts.binary_search(&offset) {
            Ok(line) => line,
            Err(next) => next - 1,
        }
    }

    /// Byte offset → position.
    ///
    /// Total, deliberately. Offsets past the end clamp to the end, and one that
    /// lands *inside* a character snaps to its start. Both are what a caller
    /// asking about a stale revision, or about a length it computed in bytes,
    /// should get — and a language server that panics on a position takes every
    /// feature down with it over one non-breaking space.
    pub fn position(&self, text: &str, offset: usize) -> Position {
        let offset = floor_boundary(text, offset);
        let line = self.line_of(offset);
        let start = self.line_start(line);
        let character = text[start..offset].chars().map(char::len_utf16).sum::<usize>();

        Position::new(line as u32, character as u32)
    }

    /// Position → byte offset.
    ///
    /// Returns `None` for a line past the end of the document. A character past
    /// the end of its line clamps to the line's end, because editors legitimately
    /// send `character: u32::MAX` to mean "end of line".
    pub fn offset(&self, text: &str, position: Position) -> Option<usize> {
        let line = position.line as usize;
        if line >= self.starts.len() {
            return None;
        }

        let (start, end) = self.line_range(text, line);
        let mut units = 0usize;

        for (index, character) in text[start..end].char_indices() {
            if units >= position.character as usize {
                return Some(start + index);
            }
            units += character.len_utf16();
        }

        Some(end)
    }
}

/// The greatest character boundary at or before `offset`, clamped to the text.
pub fn floor_boundary(text: &str, offset: usize) -> usize {
    let mut offset = offset.min(text.len());
    while !text.is_char_boundary(offset) {
        offset -= 1;
    }
    offset
}

/// The least character boundary at or after `offset`, clamped to the text.
///
/// What "one character wide" means when a length was computed in bytes — a
/// diagnostic pointing at `\u{a0}` is two bytes, not one.
pub fn ceil_boundary(text: &str, offset: usize) -> usize {
    let mut offset = offset.min(text.len());
    while !text.is_char_boundary(offset) {
        offset += 1;
    }
    offset
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_offsets_to_lines() {
        let text = "a\nbb\n\nccc";
        let index = LineIndex::new(text);

        assert_eq!(index.position(text, 0), Position::new(0, 0));
        assert_eq!(index.position(text, 2), Position::new(1, 0));
        assert_eq!(index.position(text, 4), Position::new(1, 2));
        assert_eq!(index.position(text, 5), Position::new(2, 0));
        assert_eq!(index.position(text, 6), Position::new(3, 0));
    }

    #[test]
    fn round_trips_every_boundary() {
        let text = "local e = <Frame/>\n  <TextLabel>Hi</TextLabel>\nend\n";
        let index = LineIndex::new(text);

        for offset in 0..=text.len() {
            if !text.is_char_boundary(offset) {
                continue;
            }
            let position = index.position(text, offset);
            assert_eq!(index.offset(text, position), Some(offset), "at {offset}");
        }
    }

    #[test]
    fn counts_utf16_code_units() {
        // An emoji is one char, two UTF-16 units, four bytes — all three differ,
        // which is exactly the case that goes wrong when they are conflated.
        let text = "a😀b";
        let index = LineIndex::new(text);

        assert_eq!(index.position(text, 5), Position::new(0, 3));
        assert_eq!(index.offset(text, Position::new(0, 3)), Some(5));
        // Landing inside the surrogate pair snaps to the character it belongs to.
        assert_eq!(index.offset(text, Position::new(0, 2)), Some(5));
    }

    #[test]
    fn a_character_past_the_line_clamps_to_its_end() {
        let text = "ab\ncd\n";
        let index = LineIndex::new(text);
        assert_eq!(index.offset(text, Position::new(0, 99)), Some(2));
        assert_eq!(index.offset(text, Position::new(9, 0)), None);
    }

    #[test]
    fn an_offset_inside_a_character_snaps_to_its_start() {
        // A non-breaking space — Option+Space on a Mac — is two bytes. A length
        // computed in bytes lands in the middle of one, and slicing there used
        // to panic and take the whole server with it.
        let text = "a\u{a0}b";
        let index = LineIndex::new(text);

        assert_eq!(index.position(text, 2), index.position(text, 1));
        assert_eq!(index.position(text, 3), Position::new(0, 2));
        // Past the end is still the end.
        assert_eq!(index.position(text, 999), Position::new(0, 3));
    }

    #[test]
    fn boundaries_round_outwards() {
        let text = "a\u{a0}b";
        assert_eq!((floor_boundary(text, 2), ceil_boundary(text, 2)), (1, 3));
        // Already on one: unchanged in both directions.
        assert_eq!((floor_boundary(text, 1), ceil_boundary(text, 1)), (1, 1));
        assert_eq!(ceil_boundary(text, 999), text.len());
    }

    #[test]
    fn crlf_terminators_are_not_part_of_the_line() {
        let text = "ab\r\ncd";
        let index = LineIndex::new(text);
        assert_eq!(index.line_range(text, 0), (0, 2));
        assert_eq!(index.offset(text, Position::new(0, 99)), Some(2));
        assert_eq!(index.position(text, 4), Position::new(1, 0));
    }
}
