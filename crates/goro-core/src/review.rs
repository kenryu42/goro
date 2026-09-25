//! The review model: every change in a repository, the continuous diff stream shown to the
//! user (as display rows), and the file tree that navigates it.

use std::collections::HashSet;
use std::ops::Range;
use std::path::PathBuf;
use std::sync::Arc;

use gix::bstr::ByteSlice;

use crate::diff::{FileDiff, LineKind};
use crate::repo::{FileChange, Loaded, Repo, Section, ThreadRepo};
use crate::syntax::{self, Language, Span, Style};

/// Syntax spans for both sides of a text diff.
#[derive(Debug, Default)]
pub struct Highlights {
    pub old: Vec<Span>,
    pub new: Vec<Span>,
}

#[derive(Debug, Clone)]
pub enum Content {
    Pending,
    Loaded(Loaded),
    Failed(Arc<str>),
}

#[derive(Debug, Clone)]
pub struct ReviewFile {
    pub change: FileChange,
    pub content: Content,
    pub highlights: Option<Arc<Highlights>>,
}

impl ReviewFile {
    pub fn diff(&self) -> Option<&Arc<FileDiff>> {
        match &self.content {
            Content::Loaded(Loaded::Text(diff)) => Some(diff),
            _ => None,
        }
    }
}

/// A fully loaded file, produced off the UI thread.
#[derive(Debug)]
pub struct FileLoad {
    pub content: Content,
    pub highlights: Option<Arc<Highlights>>,
}

/// Load and highlight one change.
pub fn load_file(thread: &ThreadRepo, change: &FileChange) -> FileLoad {
    let loaded = thread.loader().and_then(|mut loader| loader.load(change));
    match loaded {
        Ok(loaded) => {
            let highlights = match (&loaded, Language::for_path(&change.path_lossy())) {
                (Loaded::Text(diff), Some(language)) => {
                    Some(Arc::new(highlight_diff(language, diff)))
                }
                _ => None,
            };
            FileLoad {
                content: Content::Loaded(loaded),
                highlights,
            }
        }
        Err(err) => FileLoad {
            content: Content::Failed(err.to_string().into()),
            highlights: None,
        },
    }
}

/// Highlight both sides of a diff. Whole files are parsed (highlighting depends on
/// context), but only spans on lines the diff shows are kept, which keeps memory
/// proportional to the diff rather than to file size.
pub fn highlight_diff(language: Language, diff: &FileDiff) -> Highlights {
    let shown = |removed: bool| {
        diff.lines
            .iter()
            .filter(|l| (l.kind == LineKind::Removed) == removed)
            .map(|l| l.content.clone())
            .collect::<Vec<_>>()
    };
    Highlights {
        old: retain_on_lines(syntax::highlight(language, diff.old.bytes()), &shown(true)),
        new: retain_on_lines(syntax::highlight(language, diff.new.bytes()), &shown(false)),
    }
}

/// Keep spans that overlap any of `lines` (sorted, non-overlapping byte ranges).
fn retain_on_lines(mut spans: Vec<Span>, lines: &[Range<usize>]) -> Vec<Span> {
    let mut line = 0;
    spans.retain(|span| {
        while line < lines.len() && lines[line].end <= span.range.start {
            line += 1;
        }
        // A span may start before a line and still overlap it (multi-line comments).
        line < lines.len() && lines[line].start < span.range.end
    });
    spans.shrink_to_fit();
    spans
}

/// Load `indices` of `changes` in parallel, calling `sink` as each file finishes.
pub fn load_files(
    repo: &Repo,
    changes: &[FileChange],
    indices: impl IntoIterator<Item = usize>,
    sink: impl Fn(usize, FileLoad) + Send + Sync,
) {
    use rayon::prelude::*;
    let indices: Vec<usize> = indices.into_iter().collect();
    indices.into_par_iter().for_each_init(
        || repo.thread_local(),
        |thread, ix| sink(ix, load_file(thread, &changes[ix])),
    );
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Note {
    Loading,
    Binary {
        old_len: u64,
        new_len: u64,
    },
    TooLarge {
        len: u64,
    },
    Submodule,
    Conflict,
    NotAFile,
    /// Loaded, but no content change (mode or rename only).
    NoContentChange,
    Failed,
}

/// One row of the diff stream. Rows are uniform height so the list can be virtualized.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Row {
    File { file: usize },
    Hunk { file: usize, hunk: usize },
    Line { file: usize, line: usize },
    Note { file: usize, note: Note },
}

impl Row {
    pub fn file(&self) -> usize {
        match *self {
            Row::File { file }
            | Row::Hunk { file, .. }
            | Row::Line { file, .. }
            | Row::Note { file, .. } => file,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TreeRow {
    Section {
        section: Section,
        count: usize,
    },
    Dir {
        section: Section,
        depth: usize,
        /// Display name; single-child directory chains are compacted (`a/b/c`).
        name: String,
        /// Full directory path, the key for expand/collapse.
        path: String,
        expanded: bool,
    },
    File {
        depth: usize,
        name: String,
        file: usize,
    },
}

/// What a stage/unstage/discard applies to within one file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    WholeFile,
    /// Indices into the file's diff lines.
    Lines(Vec<usize>),
}

/// A position in the stream that survives a reload: a file (by section and path) and a
/// row offset within it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Anchor {
    pub section: Section,
    pub path: gix::bstr::BString,
    pub offset: usize,
}

pub struct Review {
    pub root: PathBuf,
    pub files: Vec<ReviewFile>,
    rows: Vec<Row>,
    file_rows: Vec<usize>,
    hunk_rows: Vec<usize>,
    tree: Vec<TreeRow>,
    collapsed: HashSet<(Section, String)>,
}

impl Review {
    pub fn new(root: PathBuf, changes: Vec<FileChange>) -> Self {
        let files = changes
            .into_iter()
            .map(|change| ReviewFile {
                change,
                content: Content::Pending,
                highlights: None,
            })
            .collect();
        let mut review = Self {
            root,
            files,
            rows: Vec::new(),
            file_rows: Vec::new(),
            hunk_rows: Vec::new(),
            tree: Vec::new(),
            collapsed: HashSet::new(),
        };
        review.rebuild_rows();
        review.rebuild_tree();
        review
    }

    pub fn rows(&self) -> &[Row] {
        &self.rows
    }

    pub fn tree(&self) -> &[TreeRow] {
        &self.tree
    }

    pub fn is_empty(&self) -> bool {
        self.files.is_empty()
    }

    /// Apply loaded files. Call [`Review::rebuild_rows`] once after a batch.
    pub fn set_loaded(&mut self, file: usize, load: FileLoad) {
        let entry = &mut self.files[file];
        entry.content = load.content;
        entry.highlights = load.highlights;
    }

    pub fn file_row(&self, file: usize) -> usize {
        self.file_rows[file]
    }

    /// First hunk row after `row`, if any.
    pub fn next_hunk_row(&self, row: usize) -> Option<usize> {
        let ix = self.hunk_rows.partition_point(|&r| r <= row);
        self.hunk_rows.get(ix).copied()
    }

    /// Last hunk row before `row`, if any.
    pub fn prev_hunk_row(&self, row: usize) -> Option<usize> {
        let ix = self.hunk_rows.partition_point(|&r| r < row);
        ix.checked_sub(1).map(|ix| self.hunk_rows[ix])
    }

    pub fn next_file_row(&self, row: usize) -> Option<usize> {
        let ix = self.file_rows.partition_point(|&r| r <= row);
        self.file_rows.get(ix).copied()
    }

    pub fn prev_file_row(&self, row: usize) -> Option<usize> {
        let ix = self.file_rows.partition_point(|&r| r < row);
        ix.checked_sub(1).map(|ix| self.file_rows[ix])
    }

    /// The file and part of it an action applies to. With a selection (`anchor` to
    /// `cursor`), the selected lines of the cursor's file; a selection that includes the
    /// file header is the whole file. Without one: the hunk under the cursor, or the whole
    /// file on its header or a note.
    pub fn action_target(&self, cursor: usize, anchor: Option<usize>) -> Option<(usize, Target)> {
        let file = self.rows.get(cursor)?.file();
        let hunk_lines = |hunk: usize| -> Vec<usize> {
            self.files[file]
                .diff()
                .map(|d| d.hunks[hunk].lines.clone().collect())
                .unwrap_or_default()
        };
        let target = match anchor {
            Some(anchor) => {
                let range = anchor.min(cursor)..=anchor.max(cursor);
                let mut lines = Vec::new();
                for row in &self.rows[range] {
                    match *row {
                        Row::File { file: f } if f == file => {
                            return Some((file, Target::WholeFile));
                        }
                        Row::Hunk { file: f, hunk } if f == file => lines.extend(hunk_lines(hunk)),
                        Row::Line { file: f, line } if f == file => lines.push(line),
                        _ => {}
                    }
                }
                lines.sort_unstable();
                lines.dedup();
                Target::Lines(lines)
            }
            None => match self.rows[cursor] {
                Row::File { .. } | Row::Note { .. } => Target::WholeFile,
                Row::Hunk { hunk, .. } => Target::Lines(hunk_lines(hunk)),
                Row::Line { line, .. } => {
                    let diff = self.files[file].diff()?;
                    let hunk = diff.hunks.iter().position(|h| h.lines.contains(&line))?;
                    Target::Lines(hunk_lines(hunk))
                }
            },
        };
        Some((file, target))
    }

    pub fn anchor(&self, row: usize) -> Option<Anchor> {
        let file = self.rows.get(row)?.file();
        let change = &self.files[file].change;
        Some(Anchor {
            section: change.section,
            path: change.path.clone(),
            offset: row - self.file_rows[file],
        })
    }

    /// The row for `anchor`: the same offset in the same file (clamped), or the start of
    /// the file that now sorts after it.
    pub fn resolve(&self, anchor: &Anchor) -> usize {
        let key = (anchor.section, &anchor.path);
        let ix = self
            .files
            .partition_point(|f| (f.change.section, &f.change.path) < key);
        match self.files.get(ix) {
            Some(f) if (f.change.section, &f.change.path) == key => {
                let end = self
                    .file_rows
                    .get(ix + 1)
                    .copied()
                    .unwrap_or(self.rows.len());
                (self.file_rows[ix] + anchor.offset).min(end - 1)
            }
            Some(_) => self.file_rows[ix],
            None => self.rows.len().saturating_sub(1),
        }
    }

    /// Index of the longest row, for sizing horizontal scroll.
    pub fn widest_row(&self) -> Option<usize> {
        self.rows
            .iter()
            .enumerate()
            .max_by_key(|(_, row)| match **row {
                Row::Line { file, line } => self.files[file]
                    .diff()
                    .map_or(0, |d| d.lines[line].content.len()),
                _ => 0,
            })
            .map(|(ix, _)| ix)
    }

    pub fn rebuild_rows(&mut self) {
        self.rows.clear();
        self.file_rows.clear();
        self.hunk_rows.clear();
        for (file, entry) in self.files.iter().enumerate() {
            self.file_rows.push(self.rows.len());
            self.rows.push(Row::File { file });
            let note = match &entry.content {
                Content::Pending => Some(Note::Loading),
                Content::Failed(_) => Some(Note::Failed),
                Content::Loaded(Loaded::Binary { old_len, new_len }) => Some(Note::Binary {
                    old_len: *old_len,
                    new_len: *new_len,
                }),
                Content::Loaded(Loaded::TooLarge { len }) => Some(Note::TooLarge { len: *len }),
                Content::Loaded(Loaded::Submodule) => Some(Note::Submodule),
                Content::Loaded(Loaded::Conflict) => Some(Note::Conflict),
                Content::Loaded(Loaded::NotAFile) => Some(Note::NotAFile),
                Content::Loaded(Loaded::Text(diff)) if diff.hunks.is_empty() => {
                    Some(Note::NoContentChange)
                }
                Content::Loaded(Loaded::Text(diff)) => {
                    for (hunk_ix, hunk) in diff.hunks.iter().enumerate() {
                        self.hunk_rows.push(self.rows.len());
                        self.rows.push(Row::Hunk {
                            file,
                            hunk: hunk_ix,
                        });
                        self.rows
                            .extend(hunk.lines.clone().map(|line| Row::Line { file, line }));
                    }
                    None
                }
            };
            if let Some(note) = note {
                self.rows.push(Row::Note { file, note });
            }
        }
    }

    /// Keep view state (collapsed directories) from the review this one replaces.
    pub fn carry_view_state(&mut self, previous: &Review) {
        self.collapsed = previous.collapsed.clone();
        self.rebuild_tree();
    }

    pub fn toggle_dir(&mut self, section: Section, path: &str) {
        let key = (section, path.to_string());
        if !self.collapsed.remove(&key) {
            self.collapsed.insert(key);
        }
        self.rebuild_tree();
    }

    fn rebuild_tree(&mut self) {
        self.tree.clear();
        let mut start = 0;
        while start < self.files.len() {
            let section = self.files[start].change.section;
            let end = start
                + self.files[start..]
                    .iter()
                    .take_while(|f| f.change.section == section)
                    .count();
            self.tree.push(TreeRow::Section {
                section,
                count: end - start,
            });
            let paths: Vec<(usize, String)> = (start..end)
                .map(|ix| (ix, self.files[ix].change.path_lossy().into_owned()))
                .collect();
            build_tree_level(&mut self.tree, &self.collapsed, section, &paths, "", 0);
            start = end;
        }
    }
}

/// Emit tree rows for `paths` (all under `prefix`, sorted) at `depth`.
fn build_tree_level(
    out: &mut Vec<TreeRow>,
    collapsed: &HashSet<(Section, String)>,
    section: Section,
    paths: &[(usize, String)],
    prefix: &str,
    depth: usize,
) {
    let mut ix = 0;
    while ix < paths.len() {
        let (file, path) = &paths[ix];
        let rest = &path[prefix.len()..];
        match rest.split_once('/') {
            None => {
                out.push(TreeRow::File {
                    depth,
                    name: rest.to_string(),
                    file: *file,
                });
                ix += 1;
            }
            Some((first, _)) => {
                let dir_prefix = format!("{prefix}{first}/");
                let group_len = paths[ix..]
                    .iter()
                    .take_while(|(_, p)| p.starts_with(&dir_prefix))
                    .count();
                let group = &paths[ix..ix + group_len];
                // Compact single-child directory chains: `a/` containing only `a/b/`.
                let mut dir = dir_prefix;
                loop {
                    let first_rest = &group[0].1[dir.len()..];
                    let Some((next, _)) = first_rest.split_once('/') else {
                        break;
                    };
                    let candidate = format!("{dir}{next}/");
                    if group.iter().all(|(_, p)| p.starts_with(&candidate)) {
                        dir = candidate;
                    } else {
                        break;
                    }
                }
                let path = dir.trim_end_matches('/').to_string();
                let expanded = !collapsed.contains(&(section, path.clone()));
                out.push(TreeRow::Dir {
                    section,
                    depth,
                    name: path[prefix.len()..].to_string(),
                    path,
                    expanded,
                });
                if expanded {
                    build_tree_level(out, collapsed, section, group, &dir, depth + 1);
                }
                ix += group_len;
            }
        }
    }
}

/// Tab stop width used when rendering code.
pub const TAB_WIDTH: usize = 4;

/// A line of code ready to render: tabs expanded, invalid UTF-8 replaced, and syntax
/// spans remapped onto the display text. Spans are dropped if the line isn't valid UTF-8.
pub fn display_line(
    bytes: &[u8],
    spans: impl Iterator<Item = Span>,
) -> (String, Vec<(Range<usize>, Style)>) {
    let Ok(text) = std::str::from_utf8(bytes) else {
        return (bytes.to_str_lossy().replace('\t', "    "), Vec::new());
    };
    if !text.contains('\t') {
        return (
            text.to_string(),
            spans.map(|s| (s.range, s.style)).collect(),
        );
    }
    // Map each source byte offset to its display offset.
    let mut out = String::with_capacity(text.len() + 16);
    let mut offsets = Vec::with_capacity(text.len() + 1);
    let mut column = 0;
    for (ix, ch) in text.char_indices() {
        while offsets.len() <= ix {
            offsets.push(out.len());
        }
        if ch == '\t' {
            let width = TAB_WIDTH - column % TAB_WIDTH;
            out.extend(std::iter::repeat_n(' ', width));
            column += width;
        } else {
            out.push(ch);
            column += 1;
        }
    }
    while offsets.len() <= text.len() {
        offsets.push(out.len());
    }
    let spans = spans
        .map(|s| (offsets[s.range.start]..offsets[s.range.end], s.style))
        .collect();
    (out, spans)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::diff::{FileDiff, LineKind};
    use crate::repo::{ChangeStatus, Source};
    use crate::review::Target;

    fn change(section: Section, path: &str) -> FileChange {
        FileChange {
            section,
            status: ChangeStatus::Modified,
            path: path.into(),
            old_path: None,
            old: Source::Absent,
            new: Source::Worktree,
        }
    }

    fn text_load(old: &str, new: &str) -> FileLoad {
        FileLoad {
            content: Content::Loaded(Loaded::Text(Arc::new(FileDiff::compute(
                old.as_bytes().to_vec(),
                new.as_bytes().to_vec(),
                3,
            )))),
            highlights: None,
        }
    }

    #[test]
    fn rows_stream_files_hunks_and_lines() {
        let mut review = Review::new(
            PathBuf::from("/r"),
            vec![
                change(Section::Staged, "a.rs"),
                change(Section::Unstaged, "b.rs"),
            ],
        );
        assert_eq!(
            review.rows(),
            &[
                Row::File { file: 0 },
                Row::Note {
                    file: 0,
                    note: Note::Loading
                },
                Row::File { file: 1 },
                Row::Note {
                    file: 1,
                    note: Note::Loading
                },
            ]
        );
        review.set_loaded(0, text_load("a\nb\n", "a\nB\n"));
        review.rebuild_rows();
        assert_eq!(
            &review.rows()[..5],
            &[
                Row::File { file: 0 },
                Row::Hunk { file: 0, hunk: 0 },
                Row::Line { file: 0, line: 0 },
                Row::Line { file: 0, line: 1 },
                Row::Line { file: 0, line: 2 },
            ]
        );
        assert_eq!(review.file_row(1), 5);
        assert_eq!(review.next_hunk_row(0), Some(1));
        assert_eq!(review.next_hunk_row(1), None);
        assert_eq!(review.prev_hunk_row(4), Some(1));
        assert_eq!(review.next_file_row(0), Some(5));
        assert_eq!(review.prev_file_row(5), Some(0));
    }

    #[test]
    fn anchors_survive_a_reload() {
        let mut before = Review::new(
            PathBuf::from("/r"),
            vec![
                change(Section::Unstaged, "a.rs"),
                change(Section::Unstaged, "b.rs"),
                change(Section::Unstaged, "c.rs"),
            ],
        );
        for file in 0..3 {
            before.set_loaded(file, text_load("x\ny\nz\n", "x\nY\nz\n"));
        }
        before.rebuild_rows();
        // Row 2 of b.rs (hunk header is 1, first line 2).
        let row = before.file_row(1) + 2;
        let anchor = before.anchor(row).unwrap();

        // b.rs got staged: it now lives in the Staged section; a.rs and c.rs stay.
        let mut after = Review::new(
            PathBuf::from("/r"),
            vec![
                change(Section::Staged, "b.rs"),
                change(Section::Unstaged, "a.rs"),
                change(Section::Unstaged, "c.rs"),
            ],
        );
        for file in 0..3 {
            after.set_loaded(file, text_load("x\ny\nz\n", "x\nY\nz\n"));
        }
        after.rebuild_rows();
        // Gone from Unstaged: land on the next Unstaged file, c.rs.
        assert_eq!(after.resolve(&anchor), after.file_row(2));
        // Still present: same offset within the file.
        let a_anchor = before.anchor(before.file_row(0) + 3).unwrap();
        assert_eq!(after.resolve(&a_anchor), after.file_row(1) + 3);
        // Offsets past the end of a shorter file clamp to its last row.
        let mut far = a_anchor.clone();
        far.offset = 999;
        assert_eq!(after.resolve(&far), after.file_row(2) - 1);
    }

    #[test]
    fn action_targets_follow_cursor_and_selection() {
        let mut review = Review::new(PathBuf::from("/r"), vec![change(Section::Unstaged, "a.rs")]);
        let old: String = (1..=20).map(|i| format!("{i}\n")).collect();
        let new = old.replace("2\n", "two\n").replace("15\n", "fifteen\n");
        review.set_loaded(0, text_load(&old, &new));
        review.rebuild_rows();
        let rows = review.rows().to_vec();
        let hunk_rows: Vec<usize> = (0..rows.len())
            .filter(|&r| matches!(rows[r], Row::Hunk { .. }))
            .collect();
        let lines_of = |hunk: usize| -> Vec<usize> {
            let diff = review.files[0].diff().unwrap();
            diff.hunks[hunk].lines.clone().collect()
        };
        use Target::*;
        // File header: the whole file.
        assert_eq!(review.action_target(0, None), Some((0, WholeFile)));
        // Hunk header, or any line inside a hunk: that hunk.
        assert_eq!(
            review.action_target(hunk_rows[1], None),
            Some((0, Lines(lines_of(1))))
        );
        assert_eq!(
            review.action_target(hunk_rows[0] + 2, None),
            Some((0, Lines(lines_of(0))))
        );
        // A selection: exactly the selected lines, in either direction.
        let (a, b) = (hunk_rows[0] + 2, hunk_rows[0] + 3);
        let expected = |r: usize| match rows[r] {
            Row::Line { line, .. } => line,
            _ => unreachable!(),
        };
        assert_eq!(
            review.action_target(b, Some(a)),
            Some((0, Lines(vec![expected(a), expected(b)])))
        );
        // A selection including the file header is the whole file.
        assert_eq!(review.action_target(a, Some(0)), Some((0, WholeFile)));
    }

    #[test]
    fn tree_groups_sections_and_compacts_directories() {
        let review = Review::new(
            PathBuf::from("/r"),
            vec![
                change(Section::Staged, "README.md"),
                change(Section::Unstaged, "src/core/a.rs"),
                change(Section::Unstaged, "src/core/b.rs"),
                change(Section::Unstaged, "src/ui/deep/c.rs"),
                change(Section::Unstaged, "z.txt"),
            ],
        );
        let rendered: Vec<String> = review
            .tree()
            .iter()
            .map(|row| match row {
                TreeRow::Section { section, count } => format!("{section:?} ({count})"),
                TreeRow::Dir { depth, name, .. } => format!("{}{name}/", "  ".repeat(*depth)),
                TreeRow::File { depth, name, .. } => format!("{}{name}", "  ".repeat(*depth)),
            })
            .collect();
        assert_eq!(
            rendered,
            vec![
                "Staged (1)",
                "README.md",
                "Unstaged (4)",
                "src/",
                "  core/",
                "    a.rs",
                "    b.rs",
                "  ui/deep/",
                "    c.rs",
                "z.txt",
            ]
        );
    }

    #[test]
    fn collapsing_a_directory_hides_its_children() {
        let mut review = Review::new(
            PathBuf::from("/r"),
            vec![
                change(Section::Unstaged, "src/a.rs"),
                change(Section::Unstaged, "src/b.rs"),
                change(Section::Unstaged, "top.rs"),
            ],
        );
        review.toggle_dir(Section::Unstaged, "src");
        assert_eq!(review.tree().len(), 3, "{:?}", review.tree());
        let mut reloaded = Review::new(
            PathBuf::from("/r"),
            review.files.iter().map(|f| f.change.clone()).collect(),
        );
        reloaded.carry_view_state(&review);
        assert_eq!(
            reloaded.tree(),
            review.tree(),
            "collapsed state survives a reload"
        );
        review.toggle_dir(Section::Unstaged, "src");
        assert_eq!(review.tree().len(), 5);
    }

    #[test]
    fn highlights_keep_only_spans_on_diff_lines() {
        let old: String = (0..1000)
            .map(|i| format!("let x{i} = \"s\"; // c\n"))
            .collect();
        let new = old.replace("let x500 = ", "let changed = ");
        let diff = FileDiff::compute(old.into_bytes(), new.into_bytes(), 3);
        let highlights = highlight_diff(Language::Rust, &diff);
        let shown = |side: LineKind| -> Vec<std::ops::Range<usize>> {
            diff.lines
                .iter()
                .filter(|l| match side {
                    LineKind::Removed => l.kind == LineKind::Removed,
                    _ => l.kind != LineKind::Removed,
                })
                .map(|l| l.content.clone())
                .collect()
        };
        for (spans, lines) in [
            (&highlights.old, shown(LineKind::Removed)),
            (&highlights.new, shown(LineKind::Added)),
        ] {
            assert!(!spans.is_empty());
            for span in spans {
                assert!(
                    lines
                        .iter()
                        .any(|l| span.range.start < l.end && l.start < span.range.end),
                    "span {span:?} is not on a diff line"
                );
            }
        }
        // 1 removed line and 7 context/added lines, a handful of spans each.
        assert!(highlights.old.len() < 10, "{}", highlights.old.len());
        assert!(highlights.new.len() < 60, "{}", highlights.new.len());
    }

    #[test]
    fn display_line_expands_tabs_and_remaps_spans() {
        let spans = vec![Span {
            range: 1..3,
            style: Style::Keyword,
        }];
        let (text, mapped) = display_line(b"\tfn x", spans.into_iter());
        assert_eq!(text, "    fn x");
        assert_eq!(mapped, vec![(4..6, Style::Keyword)]);

        let (text, mapped) = display_line(b"ab\tc", std::iter::empty());
        assert_eq!(text, "ab  c");
        assert!(mapped.is_empty());

        let (text, mapped) = display_line(
            b"\xff\tx",
            [Span {
                range: 0..1,
                style: Style::String,
            }]
            .into_iter(),
        );
        assert_eq!(text, "\u{fffd}    x");
        assert!(mapped.is_empty());
    }
}
