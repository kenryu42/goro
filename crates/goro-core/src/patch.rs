//! Exact partial patches for `git apply`: a subset of a diff's changed lines, turned into
//! a patch that applies with zero fuzz.
//!
//! A *forward* patch applies to the diff's old side (staging worktree changes into the
//! index): unselected removals stay as context and unselected additions are dropped. A
//! *reverse* patch is applied with `-R` to the diff's new side (unstaging from the index,
//! discarding from the worktree): unselected additions stay as context and unselected
//! removals are dropped. Either way, the side the patch is matched against is exactly the
//! content on disk.

use gix::bstr::{BStr, ByteSlice};

use crate::diff::{DiffLine, FileDiff, LineKind};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    /// Applied as-is to the old side.
    Forward,
    /// Applied with `git apply -R` to the new side.
    Reverse,
}

/// The file-level header of the patch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Header<'a> {
    /// The file exists on the side the patch applies to.
    Modify,
    /// The file doesn't exist yet (staging part of an untracked file).
    NewFile { mode: &'a str },
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum PatchError {
    #[error(
        "this selection splits an end-of-file newline change; include the last line of the file"
    )]
    SplitsFinalNewline,
}

enum Out<'a> {
    Context(&'a DiffLine),
    Added(&'a DiffLine),
    Removed(&'a DiffLine),
}

/// Build a patch for the selected changed lines (indices into `diff.lines`). Context
/// lines in the selection are ignored. Returns `None` if nothing changed is selected.
pub fn build(
    path: &BStr,
    diff: &FileDiff,
    selected: impl Fn(usize) -> bool,
    direction: Direction,
    header: Header,
) -> Result<Option<Vec<u8>>, PatchError> {
    let mut body = Vec::new();
    // Line-count delta (other side minus base side) of the hunks emitted so far.
    let mut offset: i64 = 0;
    for hunk in &diff.hunks {
        let range = hunk.lines.clone();
        if !range
            .clone()
            .any(|ix| diff.lines[ix].kind != LineKind::Context && selected(ix))
        {
            continue;
        }
        let mut out = Vec::new();
        for ix in range {
            let line = &diff.lines[ix];
            let is_selected = selected(ix);
            out.push(match (line.kind, direction, is_selected) {
                (LineKind::Context, _, _) => Out::Context(line),
                (LineKind::Added, _, true) => Out::Added(line),
                (LineKind::Removed, _, true) => Out::Removed(line),
                (LineKind::Removed, Direction::Forward, false) => Out::Context(line),
                (LineKind::Added, Direction::Reverse, false) => Out::Context(line),
                (LineKind::Added, Direction::Forward, false)
                | (LineKind::Removed, Direction::Reverse, false) => continue,
            });
        }
        // A line without a newline must be the last on its side.
        for (ix, o) in out.iter().enumerate() {
            let (line, old_side, new_side) = match o {
                Out::Context(l) => (*l, true, true),
                Out::Added(l) => (*l, false, true),
                Out::Removed(l) => (*l, true, false),
            };
            if line.no_newline {
                let later = |side_old: bool| {
                    out[ix + 1..].iter().any(|o| match o {
                        Out::Context(_) => true,
                        Out::Added(_) => !side_old,
                        Out::Removed(_) => side_old,
                    })
                };
                if (old_side && later(true)) || (new_side && later(false)) {
                    return Err(PatchError::SplitsFinalNewline);
                }
            }
        }

        let old_len = out.iter().filter(|o| !matches!(o, Out::Added(_))).count() as i64;
        let new_len = out.iter().filter(|o| !matches!(o, Out::Removed(_))).count() as i64;
        // The side that exists on disk keeps the original hunk's position.
        let (old_start, new_start) = match direction {
            Direction::Forward => {
                let base = first_line(hunk.old_start, hunk.old_len);
                (base, base + offset)
            }
            Direction::Reverse => {
                let base = first_line(hunk.new_start, hunk.new_len);
                (base - offset, base)
            }
        };
        offset += match direction {
            Direction::Forward => new_len - old_len,
            Direction::Reverse => old_len - new_len,
        };
        body.extend_from_slice(b"@@ -");
        push_range(&mut body, old_start, old_len);
        body.extend_from_slice(b" +");
        push_range(&mut body, new_start, new_len);
        body.extend_from_slice(b" @@\n");
        for o in &out {
            let (prefix, line) = match o {
                Out::Context(l) => (b' ', *l),
                Out::Added(l) => (b'+', *l),
                Out::Removed(l) => (b'-', *l),
            };
            body.push(prefix);
            body.extend_from_slice(diff.line_bytes(line));
            body.extend_from_slice(terminator(diff, line));
            if line.no_newline {
                body.extend_from_slice(b"\n\\ No newline at end of file\n");
            }
        }
    }
    if body.is_empty() {
        return Ok(None);
    }

    let a = quote_path(b"a/", path);
    let b = quote_path(b"b/", path);
    let mut patch = Vec::new();
    patch.extend_from_slice(b"diff --git ");
    patch.extend_from_slice(&a);
    patch.push(b' ');
    patch.extend_from_slice(&b);
    patch.push(b'\n');
    match header {
        Header::Modify => {
            patch.extend_from_slice(b"--- ");
            patch.extend_from_slice(&a);
        }
        Header::NewFile { mode } => {
            patch.extend_from_slice(b"new file mode ");
            patch.extend_from_slice(mode.as_bytes());
            patch.extend_from_slice(b"\n--- /dev/null");
        }
    }
    patch.extend_from_slice(b"\n+++ ");
    patch.extend_from_slice(&b);
    patch.push(b'\n');
    patch.extend_from_slice(&body);
    Ok(Some(patch))
}

/// 1-based first line of a hunk side, as a signed number for offset math. For an empty
/// side, git's header names the line *before* the hunk.
fn first_line(start: u32, len: u32) -> i64 {
    if len == 0 {
        start as i64 + 1
    } else {
        start as i64
    }
}

fn push_range(out: &mut Vec<u8>, first: i64, len: i64) {
    let start = if len == 0 { first - 1 } else { first };
    out.extend_from_slice(start.to_string().as_bytes());
    if len != 1 {
        out.push(b',');
        out.extend_from_slice(len.to_string().as_bytes());
    }
}

fn terminator<'a>(diff: &'a FileDiff, line: &DiffLine) -> &'a [u8] {
    let text = match line.kind {
        LineKind::Removed => &diff.old,
        _ => &diff.new,
    };
    let bytes = text.bytes();
    let end = line.content.end;
    if bytes[end..].starts_with(b"\r\n") {
        &bytes[end..end + 2]
    } else if bytes[end..].starts_with(b"\n") {
        &bytes[end..end + 1]
    } else {
        &[]
    }
}

/// Quote a path the way git does when it contains special characters.
fn quote_path(prefix: &[u8], path: &BStr) -> Vec<u8> {
    let needs_quoting = path
        .iter()
        .any(|&b| b == b'"' || b == b'\\' || !(0x20..0x7f).contains(&b));
    let mut out = Vec::with_capacity(path.len() + prefix.len() + 2);
    if !needs_quoting {
        out.extend_from_slice(prefix);
        out.extend_from_slice(path.as_bytes());
        return out;
    }
    out.push(b'"');
    out.extend_from_slice(prefix);
    for &b in path.iter() {
        match b {
            b'"' => out.extend_from_slice(b"\\\""),
            b'\\' => out.extend_from_slice(b"\\\\"),
            b'\t' => out.extend_from_slice(b"\\t"),
            b'\n' => out.extend_from_slice(b"\\n"),
            b if !(0x20..0x7f).contains(&b) => {
                out.extend_from_slice(format!("\\{b:03o}").as_bytes());
            }
            b => out.push(b),
        }
    }
    out.push(b'"');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quotes_special_paths_like_git() {
        assert_eq!(quote_path(b"a/", b"src/x.rs".as_bstr()), b"a/src/x.rs");
        assert_eq!(quote_path(b"a/", b"my file".as_bstr()), b"a/my file");
        assert_eq!(
            quote_path(b"b/", "caf\u{e9}\".txt".as_bytes().as_bstr()),
            b"\"b/caf\\303\\251\\\".txt\""
        );
    }

    #[test]
    fn nothing_selected_is_no_patch() {
        let diff = FileDiff::compute(b"a\n".as_slice(), b"b\n".as_slice(), 3);
        let patch = build(
            b"f".as_bstr(),
            &diff,
            |_| false,
            Direction::Forward,
            Header::Modify,
        );
        assert_eq!(patch, Ok(None));
    }
}
