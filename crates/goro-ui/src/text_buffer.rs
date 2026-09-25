//! Editable multi-line text with a selection: the model behind the commit message editor.
//! Offsets are UTF-8 byte offsets on grapheme boundaries.

use std::ops::Range;

use unicode_segmentation::UnicodeSegmentation;

#[derive(Debug, Default, Clone)]
pub struct TextBuffer {
    text: String,
    selection: Range<usize>,
    /// The cursor is at `selection.start` instead of `selection.end`.
    reversed: bool,
    /// IME composition in progress.
    pub marked: Option<Range<usize>>,
    /// Column (in graphemes) that vertical movement tries to keep.
    goal_column: Option<usize>,
}

impl TextBuffer {
    pub fn text(&self) -> &str {
        &self.text
    }

    pub fn selection(&self) -> Range<usize> {
        self.selection.clone()
    }

    pub fn is_reversed(&self) -> bool {
        self.reversed
    }

    pub fn cursor(&self) -> usize {
        if self.reversed {
            self.selection.start
        } else {
            self.selection.end
        }
    }

    pub fn set_text(&mut self, text: &str) {
        self.text = text.to_string();
        let end = self.text.len();
        self.selection = end..end;
        self.reversed = false;
        self.marked = None;
        self.goal_column = None;
    }

    /// Byte ranges of each line, without the newline.
    pub fn lines(&self) -> Vec<Range<usize>> {
        let mut lines = Vec::new();
        let mut start = 0;
        for (ix, _) in self.text.match_indices('\n') {
            lines.push(start..ix);
            start = ix + 1;
        }
        lines.push(start..self.text.len());
        lines
    }

    /// Index of the line containing `offset`.
    pub fn line_of(&self, offset: usize) -> usize {
        self.text[..offset].matches('\n').count()
    }

    pub fn replace(&mut self, range: Range<usize>, new_text: &str) {
        self.text.replace_range(range.clone(), new_text);
        let cursor = range.start + new_text.len();
        self.selection = cursor..cursor;
        self.reversed = false;
        self.marked = None;
        self.goal_column = None;
    }

    pub fn insert(&mut self, new_text: &str) {
        let range = self.marked.clone().unwrap_or(self.selection.clone());
        self.replace(range, new_text);
    }

    pub fn backspace(&mut self) {
        if self.selection.is_empty() {
            let prev = self.previous_boundary(self.cursor());
            self.selection = prev..self.cursor();
        }
        self.replace(self.selection.clone(), "");
    }

    pub fn delete(&mut self) {
        if self.selection.is_empty() {
            let next = self.next_boundary(self.cursor());
            self.selection = self.cursor()..next;
        }
        self.replace(self.selection.clone(), "");
    }

    pub fn move_to(&mut self, offset: usize) {
        self.selection = offset..offset;
        self.reversed = false;
        self.goal_column = None;
    }

    pub fn select_to(&mut self, offset: usize) {
        if self.reversed {
            self.selection.start = offset;
        } else {
            self.selection.end = offset;
        }
        if self.selection.end < self.selection.start {
            self.reversed = !self.reversed;
            self.selection = self.selection.end..self.selection.start;
        }
        self.goal_column = None;
    }

    pub fn select_all(&mut self) {
        self.selection = 0..self.text.len();
        self.reversed = false;
    }

    pub fn left(&mut self, select: bool) {
        if !select && !self.selection.is_empty() {
            self.move_to(self.selection.start);
        } else {
            let to = self.previous_boundary(self.cursor());
            self.go(to, select);
        }
    }

    pub fn right(&mut self, select: bool) {
        if !select && !self.selection.is_empty() {
            self.move_to(self.selection.end);
        } else {
            let to = self.next_boundary(self.cursor());
            self.go(to, select);
        }
    }

    pub fn up(&mut self, select: bool) {
        self.vertical(-1, select);
    }

    pub fn down(&mut self, select: bool) {
        self.vertical(1, select);
    }

    pub fn line_start(&mut self, select: bool) {
        let line = self.lines()[self.line_of(self.cursor())].clone();
        self.go(line.start, select);
    }

    pub fn line_end(&mut self, select: bool) {
        let line = self.lines()[self.line_of(self.cursor())].clone();
        self.go(line.end, select);
    }

    fn go(&mut self, offset: usize, select: bool) {
        if select {
            self.select_to(offset);
        } else {
            self.move_to(offset);
        }
    }

    fn vertical(&mut self, delta: isize, select: bool) {
        let lines = self.lines();
        let cursor = self.cursor();
        let line = self.line_of(cursor);
        let column = self
            .goal_column
            .unwrap_or_else(|| self.text[lines[line].start..cursor].graphemes(true).count());
        let target = line as isize + delta;
        let offset = if target < 0 {
            0
        } else if target as usize >= lines.len() {
            self.text.len()
        } else {
            let range = lines[target as usize].clone();
            range.start
                + self.text[range.clone()]
                    .grapheme_indices(true)
                    .nth(column)
                    .map_or(range.len(), |(ix, _)| ix)
        };
        self.go(offset, select);
        self.goal_column = Some(column);
    }

    pub fn previous_boundary(&self, offset: usize) -> usize {
        self.text
            .grapheme_indices(true)
            .rev()
            .find_map(|(ix, _)| (ix < offset).then_some(ix))
            .unwrap_or(0)
    }

    pub fn next_boundary(&self, offset: usize) -> usize {
        self.text
            .grapheme_indices(true)
            .find_map(|(ix, _)| (ix > offset).then_some(ix))
            .unwrap_or(self.text.len())
    }

    pub fn offset_to_utf16(&self, offset: usize) -> usize {
        self.text[..offset].encode_utf16().count()
    }

    pub fn offset_from_utf16(&self, utf16: usize) -> usize {
        let mut count = 0;
        for (ix, ch) in self.text.char_indices() {
            if count >= utf16 {
                return ix;
            }
            count += ch.len_utf16();
        }
        self.text.len()
    }

    pub fn range_to_utf16(&self, range: &Range<usize>) -> Range<usize> {
        self.offset_to_utf16(range.start)..self.offset_to_utf16(range.end)
    }

    pub fn range_from_utf16(&self, range: &Range<usize>) -> Range<usize> {
        self.offset_from_utf16(range.start)..self.offset_from_utf16(range.end)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn buffer(text: &str) -> TextBuffer {
        let mut b = TextBuffer::default();
        b.set_text(text);
        b
    }

    #[test]
    fn typing_and_deleting() {
        let mut b = TextBuffer::default();
        b.insert("Fix bug");
        b.insert("\n\nBody");
        assert_eq!(b.text(), "Fix bug\n\nBody");
        b.backspace();
        assert_eq!(b.text(), "Fix bug\n\nBod");
        b.move_to(0);
        b.delete();
        assert_eq!(b.text(), "ix bug\n\nBod");
        b.backspace();
        assert_eq!(b.text(), "ix bug\n\nBod", "backspace at start is a no-op");
    }

    #[test]
    fn graphemes_move_as_one() {
        let mut b = buffer("e\u{301}x👍🏽");
        b.backspace();
        assert_eq!(b.text(), "e\u{301}x", "emoji with skin tone deletes as one");
        b.left(false);
        assert_eq!(b.cursor(), "e\u{301}".len());
        b.left(false);
        assert_eq!(b.cursor(), 0, "combining accent moves with its base");
    }

    #[test]
    fn vertical_movement_keeps_the_goal_column() {
        let mut b = buffer("long first line\nab\nthird line here");
        b.move_to(10);
        b.down(false);
        assert_eq!(
            b.cursor(),
            "long first line\nab".len(),
            "clamped to short line"
        );
        b.down(false);
        assert_eq!(
            b.cursor(),
            "long first line\nab\n".len() + 10,
            "goal column restored"
        );
        b.up(false);
        b.up(false);
        assert_eq!(b.cursor(), 10);
        b.up(false);
        assert_eq!(b.cursor(), 0, "up from the first line goes to the start");
    }

    #[test]
    fn selection_extends_and_reverses() {
        let mut b = buffer("abc\ndef");
        b.move_to(5);
        b.left(true);
        b.left(true);
        assert_eq!(b.selection(), 3..5);
        assert!(b.is_reversed());
        b.up(true);
        assert_eq!(b.selection(), 0..5);
        b.insert("X");
        assert_eq!(b.text(), "Xef");
        b.select_all();
        b.line_end(false);
        assert_eq!(b.cursor(), 3);
    }

    #[test]
    fn line_start_and_end() {
        let mut b = buffer("one\ntwo three");
        b.move_to(6);
        b.line_start(false);
        assert_eq!(b.cursor(), 4);
        b.line_end(true);
        assert_eq!(b.selection(), 4..13);
    }

    #[test]
    fn utf16_round_trip() {
        let b = buffer("a👍b");
        assert_eq!(b.offset_to_utf16("a👍".len()), 3);
        assert_eq!(b.offset_from_utf16(3), "a👍".len());
        assert_eq!(b.range_from_utf16(&(1..3)), 1..5);
    }

    #[test]
    fn ime_marked_text_is_replaced() {
        let mut b = TextBuffer::default();
        b.insert("x");
        b.marked = Some(1..1);
        b.replace(1..1, "に");
        b.marked = Some(1..1 + "に".len());
        b.insert("日本");
        assert_eq!(b.text(), "x日本");
        assert_eq!(b.marked, None);
    }
}
