//! The review model: every change in a repository, the continuous diff stream shown to the
//! user (as display rows), and the file tree that navigates it.

use std::collections::{HashMap, HashSet};
use std::ops::Range;
use std::path::PathBuf;
use std::sync::Arc;

use gix::bstr::ByteSlice;

use crate::comments::{Comment, Side};
use crate::diff::{FileDiff, LineKind};
use crate::repo::{FileChange, Loaded, Repo, Section, ThreadRepo};
use crate::store::stable_hash;
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

/// Which worktree paths may have changed since a review was loaded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Dirty {
    All,
    Paths(HashSet<gix::bstr::BString>),
}

impl Dirty {
    pub fn merge(&mut self, other: Dirty) {
        match (&mut *self, other) {
            (Dirty::All, _) => {}
            (_, Dirty::All) => *self = Dirty::All,
            (Dirty::Paths(mine), Dirty::Paths(theirs)) => mine.extend(theirs),
        }
    }

    fn contains(&self, path: &gix::bstr::BString) -> bool {
        match self {
            Dirty::All => true,
            Dirty::Paths(paths) => paths.contains(path),
        }
    }
}

/// For each of `changes`, the load from `previous` that is still valid: same section,
/// paths and blob ids, and no worktree side at a dirty path. `None` must be reloaded.
pub fn reuse_loads(
    previous: &[ReviewFile],
    changes: &[FileChange],
    dirty: &Dirty,
) -> Vec<Option<FileLoad>> {
    changes
        .iter()
        .map(|change| {
            let touches_dirty_worktree = [&change.old, &change.new]
                .into_iter()
                .any(|side| *side == crate::repo::Source::Worktree)
                && dirty.contains(&change.path);
            if touches_dirty_worktree {
                return None;
            }
            previous
                .iter()
                .find(|f| f.change == *change && !matches!(f.content, Content::Pending))
                .map(|f| FileLoad {
                    content: f.content.clone(),
                    highlights: f.highlights.clone(),
                })
        })
        .collect()
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
    File {
        file: usize,
    },
    Hunk {
        file: usize,
        hunk: usize,
    },
    Line {
        file: usize,
        line: usize,
    },
    Note {
        file: usize,
        note: Note,
    },
    /// Line `line` of a comment's text (index into [`Review::comments`]).
    Comment {
        file: usize,
        comment: usize,
        line: usize,
    },
}

impl Row {
    pub fn file(&self) -> usize {
        match *self {
            Row::File { file }
            | Row::Hunk { file, .. }
            | Row::Line { file, .. }
            | Row::Note { file, .. }
            | Row::Comment { file, .. } => file,
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
    /// Changed lines at the last look; `None` means nothing counts as new.
    seen: Option<Seen>,
    /// Rows of changed lines not in `seen`, sorted.
    new_rows: Vec<usize>,
    new_per_file: Vec<usize>,
    /// Hashes of reviewed hunks (see [`hunk_hash`]).
    reviewed: HashSet<u64>,
    /// Per file, the hash of each hunk (computed on rebuild).
    hunk_hashes: Vec<Vec<u64>>,
    comments: Vec<Comment>,
}

/// Changed-line hashes per path, as of the last look.
pub type Seen = HashMap<String, HashSet<u64>>;

/// Identity of a changed line for "new since last look": its kind and content, not its
/// position or section, so moving or staging a line doesn't make it new.
pub fn line_hash(kind: LineKind, content: &[u8]) -> u64 {
    let kind = [match kind {
        LineKind::Added => b'+',
        LineKind::Removed => b'-',
        LineKind::Context => b' ',
    }];
    stable_hash(&[&kind, content])
}

/// Identity of a hunk for reviewed marks: its file and changed lines. Any edit to the
/// hunk's changes makes it a different hunk.
pub fn hunk_hash(path: &[u8], diff: &FileDiff, hunk: usize) -> u64 {
    let mut parts: Vec<&[u8]> = vec![path];
    for line in &diff.lines[diff.hunks[hunk].lines.clone()] {
        match line.kind {
            LineKind::Added => parts.extend([b"+".as_slice(), diff.line_bytes(line)]),
            LineKind::Removed => parts.extend([b"-".as_slice(), diff.line_bytes(line)]),
            LineKind::Context => {}
        }
    }
    stable_hash(&parts)
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
            seen: None,
            new_rows: Vec::new(),
            new_per_file: Vec::new(),
            reviewed: HashSet::new(),
            hunk_hashes: Vec::new(),
            comments: Vec::new(),
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
        if matches!(self.rows[cursor], Row::Comment { .. }) {
            return None;
        }
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
                Row::Comment { .. } => return None,
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
        self.new_rows.clear();
        self.new_per_file = vec![0; self.files.len()];
        self.hunk_hashes = self
            .files
            .iter()
            .map(|entry| match entry.diff() {
                Some(diff) => (0..diff.hunks.len())
                    .map(|h| hunk_hash(&entry.change.path, diff, h))
                    .collect(),
                None => Vec::new(),
            })
            .collect();
        let mut placed = HashSet::new();
        for (file, entry) in self.files.iter().enumerate() {
            let seen = self
                .seen
                .as_ref()
                .map(|seen| seen.get(entry.change.path.to_str_lossy().as_ref()));
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
                        if self.reviewed.contains(&self.hunk_hashes[file][hunk_ix]) {
                            continue;
                        }
                        for line in hunk.lines.clone() {
                            let l = &diff.lines[line];
                            let is_new = l.kind != LineKind::Context
                                && seen.is_some_and(|seen| {
                                    !seen.is_some_and(|s| {
                                        s.contains(&line_hash(l.kind, diff.line_bytes(l)))
                                    })
                                });
                            if is_new {
                                self.new_rows.push(self.rows.len());
                                self.new_per_file[file] += 1;
                            }
                            self.rows.push(Row::Line { file, line });
                            let anchor = match l.kind {
                                LineKind::Removed => (Side::Old, l.old_no),
                                _ => (Side::New, l.new_no),
                            };
                            if let (side, Some(number)) = anchor {
                                for (ix, comment) in self.comments.iter().enumerate() {
                                    if comment.side == side
                                        && comment.end == number
                                        && comment.path.as_bytes() == entry.change.path.as_slice()
                                        && placed.insert(ix)
                                    {
                                        push_comment_rows(&mut self.rows, file, ix, comment);
                                    }
                                }
                            }
                        }
                    }
                    None
                }
            };
            if let Some(note) = note {
                self.rows.push(Row::Note { file, note });
            }
            // Comments whose line is no longer in the diff go at the end of their file,
            // under the file's last section.
            let last_of_path = !self.files[file + 1..]
                .iter()
                .any(|f| f.change.path == entry.change.path);
            if last_of_path {
                for (ix, comment) in self.comments.iter().enumerate() {
                    if comment.path.as_bytes() == entry.change.path.as_slice() && placed.insert(ix)
                    {
                        push_comment_rows(&mut self.rows, file, ix, comment);
                    }
                }
            }
        }
    }

    /// Set review comments. Call [`Review::rebuild_rows`] after.
    pub fn set_comments(&mut self, comments: Vec<Comment>) {
        self.comments = comments;
    }

    pub fn comments(&self) -> &[Comment] {
        &self.comments
    }

    /// Set what the user saw at their last look. Call [`Review::rebuild_rows`] after.
    pub fn set_seen(&mut self, seen: Option<Seen>) {
        self.seen = seen;
    }

    /// Every changed line now: what "seen" becomes when the user looks.
    pub fn snapshot_seen(&self) -> Seen {
        let mut seen = Seen::new();
        for entry in &self.files {
            let Some(diff) = entry.diff() else {
                continue;
            };
            let lines = seen
                .entry(entry.change.path.to_str_lossy().into_owned())
                .or_default();
            lines.extend(
                diff.lines
                    .iter()
                    .filter(|l| l.kind != LineKind::Context)
                    .map(|l| line_hash(l.kind, diff.line_bytes(l))),
            );
        }
        seen
    }

    pub fn seen(&self) -> Option<&Seen> {
        self.seen.as_ref()
    }

    pub fn is_new(&self, row: usize) -> bool {
        self.new_rows.binary_search(&row).is_ok()
    }

    pub fn new_count(&self) -> usize {
        self.new_rows.len()
    }

    pub fn file_new_count(&self, file: usize) -> usize {
        self.new_per_file.get(file).copied().unwrap_or(0)
    }

    pub fn next_new_row(&self, row: usize) -> Option<usize> {
        let ix = self.new_rows.partition_point(|&r| r <= row);
        self.new_rows.get(ix).copied()
    }

    pub fn prev_new_row(&self, row: usize) -> Option<usize> {
        let ix = self.new_rows.partition_point(|&r| r < row);
        ix.checked_sub(1).map(|ix| self.new_rows[ix])
    }

    /// Set reviewed hunk hashes. Call [`Review::rebuild_rows`] after.
    pub fn set_reviewed(&mut self, reviewed: HashSet<u64>) {
        self.reviewed = reviewed;
    }

    pub fn hunk_is_reviewed(&self, file: usize, hunk: usize) -> bool {
        self.hunk_hashes
            .get(file)
            .and_then(|h| h.get(hunk))
            .is_some_and(|hash| self.reviewed.contains(hash))
    }

    /// Every hunk of the file is reviewed (and it has at least one).
    pub fn file_is_reviewed(&self, file: usize) -> bool {
        self.hunk_hashes
            .get(file)
            .is_some_and(|h| !h.is_empty() && h.iter().all(|hash| self.reviewed.contains(hash)))
    }

    /// Toggle reviewed for the hunk at `row` (a hunk header or one of its lines), or for
    /// the whole file on its header. Rebuilds rows.
    pub fn toggle_reviewed(&mut self, row: usize) {
        let Some(&row) = self.rows.get(row) else {
            return;
        };
        let file = row.file();
        let hunks: Vec<usize> = match row {
            Row::Hunk { hunk, .. } => vec![hunk],
            Row::Line { line, .. } => self.files[file]
                .diff()
                .and_then(|d| d.hunks.iter().position(|h| h.lines.contains(&line)))
                .into_iter()
                .collect(),
            Row::File { .. } | Row::Note { .. } | Row::Comment { .. } => {
                (0..self.hunk_hashes[file].len()).collect()
            }
        };
        let mark = !hunks.iter().all(|&h| self.hunk_is_reviewed(file, h));
        for h in hunks {
            let hash = self.hunk_hashes[file][h];
            if mark {
                self.reviewed.insert(hash);
            } else {
                self.reviewed.remove(&hash);
            }
        }
        self.rebuild_rows();
    }

    /// Reviewed marks for hunks that still exist (stale marks are dropped).
    pub fn reviewed_for_save(&self) -> HashSet<u64> {
        self.hunk_hashes
            .iter()
            .flatten()
            .filter(|hash| self.reviewed.contains(hash))
            .copied()
            .collect()
    }

    /// Keep view state (collapsed directories) from the review this one replaces.
    pub fn carry_view_state(&mut self, previous: &Review) {
        self.collapsed = previous.collapsed.clone();
        self.comments = previous.comments.clone();
        self.seen = previous.seen.clone();
        self.reviewed = previous.reviewed.clone();
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

fn push_comment_rows(rows: &mut Vec<Row>, file: usize, comment: usize, c: &Comment) {
    let lines = c.text.lines().count().max(1);
    rows.extend((0..lines).map(|line| Row::Comment {
        file,
        comment,
        line,
    }));
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
    use crate::review::{Dirty, Target, reuse_loads};

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
    fn unchanged_files_are_reused_on_reload() {
        let blob = |n: u8| Source::Blob {
            id: gix::ObjectId::from_bytes_or_panic(&[n; 20]),
            kind: gix::objs::tree::EntryKind::Blob,
        };
        let staged = FileChange {
            section: Section::Staged,
            status: ChangeStatus::Modified,
            path: "s.rs".into(),
            old_path: None,
            old: blob(1),
            new: blob(2),
        };
        let mut review = Review::new(
            PathBuf::from("/r"),
            vec![
                staged.clone(),
                change(Section::Unstaged, "a.rs"),
                change(Section::Unstaged, "b.rs"),
            ],
        );
        for file in 0..3 {
            review.set_loaded(file, text_load("x\n", "y\n"));
        }
        let restaged = FileChange {
            new: blob(3),
            ..staged.clone()
        };
        let next = vec![
            staged.clone(),
            restaged,
            change(Section::Unstaged, "a.rs"),
            change(Section::Unstaged, "b.rs"),
            change(Section::Untracked, "new.rs"),
        ];
        let dirty = Dirty::Paths(["b.rs".into()].into_iter().collect());
        let reused: Vec<bool> = reuse_loads(&review.files, &next, &dirty)
            .iter()
            .map(Option::is_some)
            .collect();
        // Same blobs: reused. New blob id: reload. Clean worktree path: reused.
        // Dirty worktree path: reload. New file: load.
        assert_eq!(reused, [true, false, true, false, false]);
        assert!(
            reuse_loads(&review.files, &next, &Dirty::All)
                .iter()
                .enumerate()
                .all(|(ix, l)| l.is_some() == (ix == 0)),
            "everything touching the worktree reloads when all paths are dirty"
        );
    }

    fn loaded_review(files: &[(Section, &str, &str, &str)]) -> Review {
        let mut review = Review::new(
            PathBuf::from("/r"),
            files.iter().map(|(s, p, _, _)| change(*s, p)).collect(),
        );
        for (ix, (_, _, old, new)) in files.iter().enumerate() {
            review.set_loaded(ix, text_load(old, new));
        }
        review.rebuild_rows();
        review
    }

    fn new_line_texts(review: &Review) -> Vec<String> {
        (0..review.rows().len())
            .filter(|&r| review.is_new(r))
            .map(|r| match review.rows()[r] {
                Row::Line { file, line } => {
                    let diff = review.files[file].diff().unwrap();
                    String::from_utf8_lossy(diff.line_bytes(&diff.lines[line])).into_owned()
                }
                other => panic!("only lines are new: {other:?}"),
            })
            .collect()
    }

    #[test]
    fn lines_changed_after_the_last_look_are_new() {
        let old = "a\nb\nc\nd\ne\nf\ng\nh\n";
        let first = loaded_review(&[(Section::Unstaged, "f.rs", old, &old.replace("b\n", "B\n"))]);
        assert!(
            new_line_texts(&first).is_empty(),
            "no seen state yet: nothing is new"
        );
        let seen = first.snapshot_seen();

        // The agent changes g; b's change was already seen, and it has since been staged.
        let mut next = loaded_review(&[
            (Section::Staged, "f.rs", old, &old.replace("b\n", "B\n")),
            (
                Section::Unstaged,
                "f.rs",
                &old.replace("b\n", "B\n"),
                &old.replace("b\n", "B\n").replace("g\n", "G\n"),
            ),
        ]);
        next.set_seen(Some(seen));
        next.rebuild_rows();
        assert_eq!(new_line_texts(&next), ["g", "G"]);
        let first_new = next.next_new_row(0).unwrap();
        assert_eq!(next.next_new_row(first_new), Some(first_new + 1));
        assert_eq!(next.prev_new_row(first_new + 1), Some(first_new));
        assert_eq!(next.file_new_count(0), 0);
        assert_eq!(next.file_new_count(1), 2);
    }

    #[test]
    fn reviewed_hunks_collapse_and_reset_when_their_content_changes() {
        let old: String = (1..=30).map(|i| format!("line {i}\n")).collect();
        let new = old
            .replace("line 3\n", "line three\n")
            .replace("line 25\n", "line twenty-five\n");
        let mut review = loaded_review(&[(Section::Unstaged, "f.rs", &old, &new)]);
        let rows_before = review.rows().len();
        let second_hunk = review
            .next_hunk_row(review.next_hunk_row(0).unwrap())
            .unwrap();
        review.toggle_reviewed(second_hunk);
        assert!(review.hunk_is_reviewed(0, 1));
        assert!(!review.file_is_reviewed(0));
        assert_eq!(review.rows().len(), rows_before - 8, "hunk lines hidden");
        // Reviewing from the file header reviews the rest of the file.
        review.toggle_reviewed(0);
        assert!(review.file_is_reviewed(0));
        let saved = review.reviewed_for_save();
        assert_eq!(saved.len(), 2);

        // The agent edits the second hunk again: it is no longer reviewed.
        let mut edited = loaded_review(&[(
            Section::Unstaged,
            "f.rs",
            &old,
            &new.replace("twenty-five", "25!"),
        )]);
        edited.set_reviewed(saved);
        edited.rebuild_rows();
        assert!(edited.hunk_is_reviewed(0, 0));
        assert!(!edited.hunk_is_reviewed(0, 1));
        assert_eq!(
            edited.reviewed_for_save().len(),
            1,
            "stale marks are pruned"
        );
    }

    #[test]
    fn dirty_sets_merge() {
        let paths = |ps: &[&str]| Dirty::Paths(ps.iter().map(|p| (*p).into()).collect());
        let mut d = paths(&["a"]);
        d.merge(paths(&["b"]));
        assert_eq!(d, paths(&["a", "b"]));
        d.merge(Dirty::All);
        assert_eq!(d, Dirty::All);
        d.merge(paths(&["c"]));
        assert_eq!(d, Dirty::All);
    }

    #[test]
    fn comment_rows_follow_their_line() {
        use crate::comments::{Comment, Side};
        let old: String = (1..=10).map(|i| format!("line {i}\n")).collect();
        let new = old.replace("line 5\n", "line five\n");
        let mut review = loaded_review(&[(Section::Unstaged, "f.rs", &old, &new)]);
        let comment = |id, side, start, end, text: &str| Comment {
            id,
            path: "f.rs".into(),
            side,
            start,
            end,
            excerpt: Vec::new(),
            text: text.into(),
        };
        review.set_comments(vec![
            comment(1, Side::New, 5, 5, "first line\nsecond line"),
            comment(2, Side::Old, 5, 5, "on the removal"),
            comment(3, Side::New, 99, 99, "outdated"),
        ]);
        review.rebuild_rows();
        let rows = review.rows();
        let find = |pred: &dyn Fn(&Row) -> bool| rows.iter().position(pred).unwrap();
        let added = find(
            &|r| matches!(r, Row::Line { line, .. } if review.files[0].diff().unwrap().lines[*line].kind == LineKind::Added),
        );
        let removed = find(
            &|r| matches!(r, Row::Line { line, .. } if review.files[0].diff().unwrap().lines[*line].kind == LineKind::Removed),
        );
        assert_eq!(
            rows[removed + 1],
            Row::Comment {
                file: 0,
                comment: 1,
                line: 0
            }
        );
        assert_eq!(
            rows[added + 1],
            Row::Comment {
                file: 0,
                comment: 0,
                line: 0
            }
        );
        assert_eq!(
            rows[added + 2],
            Row::Comment {
                file: 0,
                comment: 0,
                line: 1
            },
            "one row per text line"
        );
        assert_eq!(
            rows[rows.len() - 1],
            Row::Comment {
                file: 0,
                comment: 2,
                line: 0
            },
            "unanchored at file end"
        );
        assert_eq!(
            review.action_target(added + 1, None),
            None,
            "actions skip comment rows"
        );
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
