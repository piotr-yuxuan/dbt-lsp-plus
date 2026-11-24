// SPDX-License-Identifier: EUPL-1.2 OR GPL-3.0-or-later
//! Positions in a text: LSP's line and character, and byte offsets.  LSP's
//! characters count UTF-16 units, the unit the server announces by
//! announcing none.

use serde_json::{Value, json};
use std::fs;
use std::path::Path;

/// The text of the file at PATH, as `decoded` gives it; none when it cannot
/// be read or is not UTF-8.
pub fn read(path: &Path) -> Option<String> {
    decoded(fs::read(path).ok()?)
}

/// BYTES as text, without the byte order mark that editors leave out of a
/// document's text and so of its positions; none when they are not UTF-8.
pub fn decoded(bytes: Vec<u8>) -> Option<String> {
    let mut text = String::from_utf8(bytes).ok()?;
    if text.starts_with('\u{feff}') {
        text.drain(..'\u{feff}'.len_utf8());
    }
    Some(text)
}

/// The unit in which `LineStarts::offset` counts characters: UTF-16, LSP's,
/// or code points, as dbt, the YAML parser and a shell's user count them.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Encoding {
    Utf16,
    Utf32,
}

impl Encoding {
    fn units(self, c: char) -> usize {
        match self {
            Encoding::Utf16 => c.len_utf16(),
            Encoding::Utf32 => 1,
        }
    }
}

/// The start of each line of a text.
pub struct LineStarts<'a> {
    text: &'a str,
    starts: Vec<usize>,
}

impl<'a> LineStarts<'a> {
    /// A line ends at `\n`, `\r\n` or a lone `\r`, as LSP says.
    pub fn new(text: &'a str) -> Self {
        let bytes = text.as_bytes();
        let ends = |&(at, &byte): &(usize, &u8)| byte == b'\n' || (byte == b'\r' && bytes.get(at + 1) != Some(&b'\n'));
        let breaks = bytes.iter().enumerate().filter(ends).map(|(at, _)| at + 1);
        LineStarts { text, starts: std::iter::once(0).chain(breaks).collect() }
    }

    /// The line, from 0, of a byte OFFSET.
    pub fn line(&self, offset: usize) -> usize {
        self.starts.partition_point(|&start| start <= offset) - 1
    }

    /// The byte offset at which LINE starts; the end of the text past the last line.
    pub fn line_start(&self, line: usize) -> usize {
        self.starts.get(line).copied().unwrap_or(self.text.len())
    }

    /// The LSP position of a byte OFFSET.
    pub fn position(&self, offset: usize) -> Value {
        // An offset inside a character counts from that character's start, never panics.
        let mut offset = offset.min(self.text.len());
        while !self.text.is_char_boundary(offset) {
            offset -= 1;
        }
        let line = self.line(offset);
        let start = self.starts[line];
        let character: usize = self.text[start..offset].chars().map(char::len_utf16).sum();
        json!({"line": line, "character": character})
    }

    /// The LSP range from START to END, two byte offsets.
    pub fn range(&self, start: usize, end: usize) -> Value {
        json!({"start": self.position(start), "end": self.position(end)})
    }

    /// The byte offset of LINE and CHARACTER; past the end of a line, the end
    /// of that line, and past the last line, the end of the text.
    pub fn offset(&self, line: u64, character: u64, encoding: Encoding) -> usize {
        let Some(&start) = usize::try_from(line).ok().and_then(|line| self.starts.get(line)) else {
            return self.text.len();
        };
        let mut units = 0;
        for (at, c) in self.text[start..].char_indices() {
            if units >= character as usize || c == '\n' || c == '\r' {
                return start + at;
            }
            units += encoding.units(c);
        }
        self.text.len()
    }

    /// The byte offset of an LSP position object, if it is one.
    pub fn offset_of(&self, position: &Value) -> Option<usize> {
        let line = position.get("line")?.as_u64()?;
        let character = position.get("character")?.as_u64()?;
        Some(self.offset(line, character, Encoding::Utf16))
    }
}

/// Applies one `TextDocumentContentChangeEvent` to TEXT.  A change without
/// text is ignored: the document keeps its last known content.
pub fn apply_change(text: &mut String, change: &Value) {
    let Some(new_text) = change.get("text").and_then(Value::as_str) else {
        return;
    };
    let Some(range) = change.get("range") else {
        *text = new_text.to_owned();
        return;
    };
    let starts = LineStarts::new(text);
    let start = range.get("start").and_then(|p| starts.offset_of(p));
    let end = range.get("end").and_then(|p| starts.offset_of(p));
    if let (Some(start), Some(end)) = (start, end) {
        text.replace_range(start..end.max(start), new_text);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // "é" is one UTF-16 unit and two UTF-8 bytes; "𝄞" is two UTF-16 units.
    const TEXT: &str = "select 'é𝄞' as x\r\nfrom t\n";

    #[test]
    fn counts_characters_in_the_announced_unit() {
        let starts = LineStarts::new(TEXT);
        let x = TEXT.find(" as").unwrap();
        assert_eq!(starts.position(x), json!({"line": 0, "character": 12}));
        assert_eq!(starts.offset(0, 12, Encoding::Utf16), x);
        assert_eq!(starts.offset(0, 11, Encoding::Utf32), x);
    }

    #[test]
    fn clamps_positions_past_a_line_or_the_text() {
        let starts = LineStarts::new(TEXT);
        assert_eq!(starts.offset(0, 99, Encoding::Utf16), TEXT.find('\r').unwrap());
        assert_eq!(starts.offset(1, 4, Encoding::Utf16), TEXT.find(" t").unwrap());
        assert_eq!(starts.offset(9, 0, Encoding::Utf16), TEXT.len());
        assert_eq!(starts.position(TEXT.len()), json!({"line": 2, "character": 0}));
    }

    #[test]
    fn a_lone_carriage_return_ends_a_line() {
        let text = "a\rb\r\nc\nd";
        let starts = LineStarts::new(text);
        assert_eq!(starts.position(2), json!({"line": 1, "character": 0}));
        assert_eq!(starts.position(5), json!({"line": 2, "character": 0}));
        assert_eq!(starts.offset(3, 0, Encoding::Utf16), 7);
    }

    #[test]
    fn applies_incremental_and_full_changes() {
        let mut text = TEXT.to_owned();
        let change = json!({"range": {"start": {"line": 1, "character": 5},
                                      "end": {"line": 1, "character": 6}},
                            "text": "orders"});
        apply_change(&mut text, &change);
        assert_eq!(text, "select 'é𝄞' as x\r\nfrom orders\n");
        // Character 10 falls inside the surrogate pair of "𝄞": it snaps to the
        // next character boundary rather than splitting the character.
        let mid_surrogate = json!({"range": {"start": {"line": 0, "character": 10},
                                             "end": {"line": 0, "character": 11}},
                                   "text": "-"});
        apply_change(&mut text, &mid_surrogate);
        assert_eq!(text, "select 'é𝄞-' as x\r\nfrom orders\n");
        let no_text = json!({"range": {"start": {"line": 0, "character": 0},
                                       "end": {"line": 0, "character": 3}}});
        apply_change(&mut text, &no_text);
        assert!(text.starts_with("select"));
        apply_change(&mut text, &json!({"text": "new"}));
        assert_eq!(text, "new");
    }
}
