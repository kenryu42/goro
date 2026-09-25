//! Line diffs with git-compatible hunks.
//!
//! Diffs are computed over raw bytes (lines include their terminator, as git does), so a
//! change in line endings or a missing final newline is a real change, and the output can
//! be turned back into a patch that `git apply` accepts.

use std::ops::Range;
use std::sync::Arc;

use imara_diff::{Algorithm, Diff, InternedInput, sources::byte_lines};

/// Lines of context around each change, matching git's default `-U3`.
pub const DEFAULT_CONTEXT: u32 = 3;

/// git treats a file as binary if its first 8000 bytes contain a NUL.
const BINARY_SNIFF_LEN: usize = 8000;

/// Maximum length of a hunk header's function context, as in git.
const FUNC_CONTEXT_MAX: usize = 80;

pub fn is_binary(bytes: &[u8]) -> bool {
    bytes[..bytes.len().min(BINARY_SNIFF_LEN)].contains(&0)
}

/// File contents with a line index. Lines are byte ranges that include their terminator.
#[derive(Debug, Clone)]
pub struct Text {
    bytes: Arc<[u8]>,
    line_starts: Vec<usize>,
}

impl Text {
    pub fn new(bytes: impl Into<Arc<[u8]>>) -> Self {
        let bytes = bytes.into();
        let mut line_starts = vec![0];
        line_starts.extend(
            bytes
                .iter()
                .enumerate()
                .filter(|(_, b)| **b == b'\n')
                .map(|(ix, _)| ix + 1),
        );
        if line_starts.last() == Some(&bytes.len()) {
            line_starts.pop();
        }
        Self { bytes, line_starts }
    }

    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }

    pub fn line_count(&self) -> usize {
        self.line_starts.len()
    }

    /// Byte range of line `ix` (0-based), including its terminator.
    pub fn line_range(&self, ix: usize) -> Range<usize> {
        let start = self.line_starts[ix];
        let end = self
            .line_starts
            .get(ix + 1)
            .copied()
            .unwrap_or(self.bytes.len());
        start..end
    }

    fn line(&self, ix: usize) -> &[u8] {
        &self.bytes[self.line_range(ix)]
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LineKind {
    Context,
    Added,
    Removed,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiffLine {
    pub kind: LineKind,
    /// 1-based line number in the old text (context and removed lines).
    pub old_no: Option<u32>,
    /// 1-based line number in the new text (context and added lines).
    pub new_no: Option<u32>,
    /// Byte range of the line's content without its terminator, in the old text for
    /// removed lines and in the new text otherwise.
    pub content: Range<usize>,
    /// The line is the last line of its file and has no terminating newline.
    pub no_newline: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hunk {
    pub old_start: u32,
    pub old_len: u32,
    pub new_start: u32,
    pub new_len: u32,
    /// Byte range in the old text of the nearest preceding "function" line, shown after
    /// the `@@` header the way git does.
    pub func_context: Option<Range<usize>>,
    /// Range into [`FileDiff::lines`].
    pub lines: Range<usize>,
}

#[derive(Debug, Clone)]
pub struct FileDiff {
    pub old: Text,
    pub new: Text,
    pub hunks: Vec<Hunk>,
    pub lines: Vec<DiffLine>,
}

impl FileDiff {
    pub fn compute(old: impl Into<Arc<[u8]>>, new: impl Into<Arc<[u8]>>, context: u32) -> Self {
        let old = Text::new(old);
        let new = Text::new(new);
        let input = InternedInput::new(byte_lines(old.bytes()), byte_lines(new.bytes()));
        let mut diff = Diff::compute(Algorithm::Histogram, &input);
        diff.postprocess_lines(&input);
        let changes: Vec<imara_diff::Hunk> = diff.hunks().collect();

        let mut hunks = Vec::new();
        let mut lines = Vec::new();
        let mut func_line: Option<usize> = None;
        let mut func_search_limit: isize = -1;
        let ctx = context as usize;

        let mut group_start = 0;
        while group_start < changes.len() {
            // Group changes whose context would touch or overlap (git: gap <= 2 * context).
            let mut group_end = group_start + 1;
            while group_end < changes.len()
                && (changes[group_end].before.start - changes[group_end - 1].before.end) as usize
                    <= 2 * ctx
            {
                group_end += 1;
            }
            let group = &changes[group_start..group_end];
            let first = &group[0];
            let last = &group[group.len() - 1];

            let lead = (first.before.start as usize).min(ctx);
            let old_from = first.before.start as usize - lead;
            let new_from = first.after.start as usize - lead;
            let trail = (old.line_count() - last.before.end as usize).min(ctx);
            let old_to = last.before.end as usize + trail;
            let new_to = last.after.end as usize + trail;

            // git searches backwards from the line before the hunk, stopping at the
            // previous hunk's search start, and keeps the last match if none is found.
            let search_from = old_from as isize - 1;
            let mut l = search_from;
            while l > func_search_limit {
                if is_func_line(old.line(l as usize)) {
                    func_line = Some(l as usize);
                    break;
                }
                l -= 1;
            }
            func_search_limit = search_from;

            let lines_start = lines.len();
            let (mut o, mut n) = (old_from, new_from);
            let push_context = |lines: &mut Vec<DiffLine>, o: usize, n: usize| {
                lines.push(DiffLine {
                    kind: LineKind::Context,
                    old_no: Some(o as u32 + 1),
                    new_no: Some(n as u32 + 1),
                    content: content_range(&new, n),
                    no_newline: !ends_with_newline(new.line(n)),
                });
            };
            for change in group {
                while o < change.before.start as usize {
                    push_context(&mut lines, o, n);
                    o += 1;
                    n += 1;
                }
                for ix in change.before.clone() {
                    let ix = ix as usize;
                    lines.push(DiffLine {
                        kind: LineKind::Removed,
                        old_no: Some(ix as u32 + 1),
                        new_no: None,
                        content: content_range(&old, ix),
                        no_newline: !ends_with_newline(old.line(ix)),
                    });
                }
                for ix in change.after.clone() {
                    let ix = ix as usize;
                    lines.push(DiffLine {
                        kind: LineKind::Added,
                        old_no: None,
                        new_no: Some(ix as u32 + 1),
                        content: content_range(&new, ix),
                        no_newline: !ends_with_newline(new.line(ix)),
                    });
                }
                o = change.before.end as usize;
                n = change.after.end as usize;
            }
            while o < old_to {
                push_context(&mut lines, o, n);
                o += 1;
                n += 1;
            }

            let old_len = (old_to - old_from) as u32;
            let new_len = (new_to - new_from) as u32;
            hunks.push(Hunk {
                old_start: header_start(old_from, old_len),
                old_len,
                new_start: header_start(new_from, new_len),
                new_len,
                func_context: func_line.map(|l| func_context_range(&old, l)),
                lines: lines_start..lines.len(),
            });
            group_start = group_end;
        }

        Self {
            old,
            new,
            hunks,
            lines,
        }
    }

    /// Text of a line's content (no terminator).
    pub fn line_bytes(&self, line: &DiffLine) -> &[u8] {
        let text = match line.kind {
            LineKind::Removed => &self.old,
            LineKind::Context | LineKind::Added => &self.new,
        };
        &text.bytes()[line.content.clone()]
    }

    pub fn func_context_bytes(&self, hunk: &Hunk) -> Option<&[u8]> {
        hunk.func_context
            .clone()
            .map(|range| &self.old.bytes()[range])
    }

    /// The hunks in git's unified format, starting at the first `@@` line.
    pub fn to_unified_hunks(&self) -> Vec<u8> {
        let mut out = Vec::new();
        for hunk in &self.hunks {
            out.extend_from_slice(b"@@ -");
            push_range(&mut out, hunk.old_start, hunk.old_len);
            out.extend_from_slice(b" +");
            push_range(&mut out, hunk.new_start, hunk.new_len);
            out.extend_from_slice(b" @@");
            if let Some(func) = self.func_context_bytes(hunk) {
                out.push(b' ');
                out.extend_from_slice(func);
            }
            out.push(b'\n');
            for line in &self.lines[hunk.lines.clone()] {
                out.push(match line.kind {
                    LineKind::Context => b' ',
                    LineKind::Added => b'+',
                    LineKind::Removed => b'-',
                });
                let text = match line.kind {
                    LineKind::Removed => &self.old,
                    _ => &self.new,
                };
                let content = &text.bytes()[line.content.clone()];
                out.extend_from_slice(content);
                // Keep the original terminator (`\n` or `\r\n`).
                let terminator_end = text.line_starts_after(line.content.end);
                out.extend_from_slice(&text.bytes()[line.content.end..terminator_end]);
                if line.no_newline {
                    out.extend_from_slice(b"\n\\ No newline at end of file\n");
                }
            }
        }
        out
    }
}

impl Text {
    fn line_starts_after(&self, content_end: usize) -> usize {
        let bytes = self.bytes();
        if bytes.get(content_end) == Some(&b'\r') && bytes.get(content_end + 1) == Some(&b'\n') {
            content_end + 2
        } else if bytes.get(content_end) == Some(&b'\n') {
            content_end + 1
        } else {
            content_end
        }
    }
}

fn ends_with_newline(line: &[u8]) -> bool {
    line.last() == Some(&b'\n')
}

fn content_range(text: &Text, ix: usize) -> Range<usize> {
    let range = text.line_range(ix);
    let line = &text.bytes()[range.clone()];
    let trim = if line.ends_with(b"\r\n") {
        2
    } else if line.ends_with(b"\n") {
        1
    } else {
        0
    };
    range.start..range.end - trim
}

/// git prints the line before the hunk when the hunk is empty on that side.
fn header_start(from: usize, len: u32) -> u32 {
    if len == 0 {
        from as u32
    } else {
        from as u32 + 1
    }
}

fn push_range(out: &mut Vec<u8>, start: u32, len: u32) {
    out.extend_from_slice(start.to_string().as_bytes());
    if len != 1 {
        out.push(b',');
        out.extend_from_slice(len.to_string().as_bytes());
    }
}

/// git's default function-name heuristic: a line starting with a letter, `_`, or `$`.
fn is_func_line(line: &[u8]) -> bool {
    matches!(line.first(), Some(b) if b.is_ascii_alphabetic() || *b == b'_' || *b == b'$')
}

fn func_context_range(text: &Text, ix: usize) -> Range<usize> {
    let range = text.line_range(ix);
    let mut len = range.len().min(FUNC_CONTEXT_MAX);
    let bytes = text.bytes();
    while len > 0 && bytes[range.start + len - 1].is_ascii_whitespace() {
        len -= 1;
    }
    range.start..range.start + len
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_lines_include_terminators() {
        let text = Text::new(b"a\r\nb\nc".as_slice());
        assert_eq!(text.line_count(), 3);
        assert_eq!(text.line(0), b"a\r\n");
        assert_eq!(text.line(2), b"c");
        assert_eq!(Text::new(b"".as_slice()).line_count(), 0);
        assert_eq!(Text::new(b"x\n".as_slice()).line_count(), 1);
    }

    #[test]
    fn line_numbers_and_kinds() {
        let diff = FileDiff::compute(b"a\nb\nc\n".as_slice(), b"a\nB\nc\n".as_slice(), 3);
        let kinds: Vec<_> = diff
            .lines
            .iter()
            .map(|l| (l.kind, l.old_no, l.new_no, diff.line_bytes(l).to_vec()))
            .collect();
        assert_eq!(
            kinds,
            vec![
                (LineKind::Context, Some(1), Some(1), b"a".to_vec()),
                (LineKind::Removed, Some(2), None, b"b".to_vec()),
                (LineKind::Added, None, Some(2), b"B".to_vec()),
                (LineKind::Context, Some(3), Some(3), b"c".to_vec()),
            ]
        );
    }

    #[test]
    fn binary_sniffing() {
        assert!(is_binary(b"abc\0def"));
        assert!(!is_binary(b"plain text"));
    }
}
