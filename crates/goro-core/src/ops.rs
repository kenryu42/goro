//! Review actions: stage, unstage and discard (whole files, hunks or lines), undo, and
//! commit. Everything goes through the `git` CLI.
//!
//! Safety with a live agent:
//! - A discard only applies if the file on disk still matches the diff the user reviewed.
//! - Discarded content is saved as git objects, reachable from `refs/goro/undo`, before
//!   anything is removed.
//! - Undo only applies if the file (or index entry) is still exactly as the action left
//!   it; otherwise it refuses rather than overwrite newer work.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use gix::bstr::{BStr, BString, ByteSlice};

use crate::diff::{FileDiff, LineKind};
use crate::git::{Git, GitError};
use crate::patch::{self, Direction, Header, PatchError};
use crate::repo::{ChangeStatus, FileChange, Section, Source};

pub const UNDO_REF: &str = "refs/goro/undo";

const READ_ONLY_SNAPSHOT: OpError = OpError::Unsupported(
    "a turn's changes are read-only; switch to the working tree (w) to act on them",
);
/// The previous generation of undo history (see [`keep_reachable`]).
pub const UNDO_PREVIOUS_REF: &str = "refs/goro/undo-previous";
/// Discards per generation of undo history.
const UNDO_HISTORY: usize = 200;

#[derive(Debug, thiserror::Error)]
pub enum OpError {
    #[error(transparent)]
    Git(#[from] GitError),
    #[error(transparent)]
    Patch(#[from] PatchError),
    /// `git commit` failed, usually a hook; the message is its output.
    #[error("Commit failed:\n{0}")]
    CommitFailed(String),
    /// The file or index changed since the user saw it.
    #[error("{0}")]
    Stale(String),
    #[error("{0}")]
    Unsupported(&'static str),
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

/// What an action applies to.
#[derive(Debug, Clone, Copy)]
pub enum Selection<'a> {
    File,
    /// Indices into the file's [`FileDiff::lines`]; context lines are ignored.
    Lines(&'a [usize]),
}

#[derive(Debug, Clone, Copy, Default)]
pub struct CommitOptions {
    pub amend: bool,
    pub signoff: bool,
}

/// Everything needed to reverse one action.
#[derive(Debug, Clone)]
pub struct Undo {
    pub description: String,
    index: Vec<IndexState>,
    worktree: Vec<WorktreeState>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct IndexEntry {
    mode: String,
    oid: String,
}

#[derive(Debug, Clone)]
struct IndexState {
    path: BString,
    before: Option<IndexEntry>,
    after: Option<IndexEntry>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FileKind {
    Regular,
    Executable,
    Symlink,
}

#[derive(Debug, Clone)]
struct SavedFile {
    /// Blob of the raw (unfiltered) bytes.
    oid: String,
    kind: FileKind,
}

#[derive(Debug, Clone)]
struct WorktreeState {
    path: BString,
    before: Option<SavedFile>,
    /// Raw blob id after the action, `None` if the file was removed.
    after: Option<String>,
}

enum Effective<'a> {
    File,
    Lines(&'a [usize]),
}

/// A line selection covering every changed line is a whole-file action (so new, deleted
/// and mode-changed files behave as expected).
fn effective<'a>(
    selection: Selection<'a>,
    diff: Option<&FileDiff>,
) -> Result<Effective<'a>, OpError> {
    let Selection::Lines(lines) = selection else {
        return Ok(Effective::File);
    };
    let diff = diff.ok_or(OpError::Unsupported(
        "only whole-file actions are possible for this file",
    ))?;
    let changed = |ix: &usize| {
        diff.lines
            .get(*ix)
            .is_some_and(|l| l.kind != LineKind::Context)
    };
    let selected = lines.iter().filter(|ix| changed(ix)).count();
    if selected == 0 {
        return Err(OpError::Unsupported("the selection has no changed lines"));
    }
    let total = diff
        .lines
        .iter()
        .filter(|l| l.kind != LineKind::Context)
        .count();
    Ok(if selected == total {
        Effective::File
    } else {
        Effective::Lines(lines)
    })
}

fn selected_in(lines: &[usize]) -> impl Fn(usize) -> bool + '_ {
    move |ix| lines.contains(&ix)
}

fn path_arg(path: &BStr) -> OsString {
    gix::path::from_bstr(path).into_owned().into_os_string()
}

fn paths(change: &FileChange) -> Vec<BString> {
    let mut paths = vec![change.path.clone()];
    paths.extend(change.old_path.clone());
    paths
}

pub fn stage(
    git: &Git,
    change: &FileChange,
    diff: Option<&FileDiff>,
    selection: Selection,
) -> Result<Undo, OpError> {
    match change.section {
        Section::Staged => return Err(OpError::Unsupported("already staged")),
        Section::Snapshot => return Err(READ_ONLY_SNAPSHOT),
        Section::Unstaged | Section::Untracked => {}
    }
    if change.status == ChangeStatus::Conflicted {
        return Err(OpError::Unsupported("resolve the conflict first"));
    }
    let before = snapshot_index(git, &paths(change))?;
    match effective(selection, diff)? {
        Effective::File => {
            git.run(
                [
                    OsString::from("add"),
                    "-A".into(),
                    "--".into(),
                    path_arg(change.path.as_ref()),
                ],
                None,
            )?;
        }
        Effective::Lines(lines) => {
            let diff = diff.expect("line selections have a diff");
            let header = if change.old == Source::Absent {
                match file_kind(&git.root().join(gix::path::from_bstr(change.path.as_bstr())))? {
                    Some(FileKind::Executable) => Header::NewFile { mode: "100755" },
                    Some(FileKind::Regular) => Header::NewFile { mode: "100644" },
                    _ => return Err(OpError::Unsupported("stage this file as a whole")),
                }
            } else {
                Header::Modify
            };
            let patch = patch::build(
                change.path.as_ref(),
                diff,
                selected_in(lines),
                Direction::Forward,
                header,
            )?
            .expect("selection has changed lines");
            git.run(
                ["apply", "--cached", "--whitespace=nowarn", "-"],
                Some(&patch),
            )?;
        }
    }
    let after = snapshot_index(git, &paths(change))?;
    Ok(Undo {
        description: format!("stage {}", change.path),
        index: zip_index(before, after),
        worktree: Vec::new(),
    })
}

pub fn unstage(
    git: &Git,
    change: &FileChange,
    diff: Option<&FileDiff>,
    selection: Selection,
) -> Result<Undo, OpError> {
    if change.section != Section::Staged {
        return Err(OpError::Unsupported("not staged"));
    }
    let before = snapshot_index(git, &paths(change))?;
    match effective(selection, diff)? {
        Effective::File => {
            let mut args: Vec<OsString> = if head_exists(git) {
                vec!["restore".into(), "--staged".into(), "--".into()]
            } else {
                vec!["rm".into(), "-q".into(), "--cached".into(), "--".into()]
            };
            args.extend(paths(change).iter().map(|p| path_arg(p.as_ref())));
            git.run(args, None)?;
        }
        Effective::Lines(lines) => {
            let diff = diff.expect("line selections have a diff");
            let patch = patch::build(
                change.path.as_ref(),
                diff,
                selected_in(lines),
                Direction::Reverse,
                Header::Modify,
            )?
            .expect("selection has changed lines");
            git.run(
                ["apply", "--cached", "-R", "--whitespace=nowarn", "-"],
                Some(&patch),
            )?;
        }
    }
    let after = snapshot_index(git, &paths(change))?;
    Ok(Undo {
        description: format!("unstage {}", change.path),
        index: zip_index(before, after),
        worktree: Vec::new(),
    })
}

pub fn discard(
    git: &Git,
    change: &FileChange,
    diff: Option<&FileDiff>,
    selection: Selection,
) -> Result<Undo, OpError> {
    match change.section {
        Section::Staged => {
            return Err(OpError::Unsupported(
                "unstage these changes before discarding them",
            ));
        }
        Section::Snapshot => return Err(READ_ONLY_SNAPSHOT),
        Section::Unstaged | Section::Untracked => {}
    }
    if change.status == ChangeStatus::Conflicted {
        return Err(OpError::Unsupported("resolve the conflict first"));
    }
    let root = git.root().to_path_buf();
    let path = change.path.clone();
    let full = root.join(gix::path::from_bstr(path.as_bstr()));

    // Compare-and-swap: the file must still be what the user reviewed.
    if let Some(diff) = diff {
        let current = clean_hash(git, &full, path.as_ref())?;
        let reviewed = match change.new {
            Source::Absent => None,
            _ => Some(hash_raw(git, diff.new.bytes(), false)?),
        };
        if current != reviewed {
            return Err(OpError::Stale(format!(
                "{path} changed on disk since it was loaded; review the new version first"
            )));
        }
    }

    let before = save_worktree(git, &full)?;
    if let Some(saved) = &before {
        keep_reachable(git, &[&saved.oid], &format!("goro: discard {path}"))?;
    }
    match effective(selection, diff)? {
        Effective::File => match change.section {
            Section::Untracked => {
                if full.is_dir() && !full.is_symlink() {
                    return Err(OpError::Unsupported("can't discard a directory"));
                }
                std::fs::remove_file(&full)?;
            }
            _ => {
                git.run(
                    [
                        OsString::from("restore"),
                        "--worktree".into(),
                        "--".into(),
                        path_arg(path.as_ref()),
                    ],
                    None,
                )?;
            }
        },
        Effective::Lines(lines) => {
            let diff = diff.expect("line selections have a diff");
            let patch = patch::build(
                path.as_ref(),
                diff,
                selected_in(lines),
                Direction::Reverse,
                Header::Modify,
            )?
            .expect("selection has changed lines");
            git.run(["apply", "-R", "--whitespace=nowarn", "-"], Some(&patch))?;
        }
    }
    let after = raw_hash(git, &full)?;
    Ok(Undo {
        description: format!("discard {path}"),
        index: Vec::new(),
        worktree: vec![WorktreeState {
            path,
            before,
            after,
        }],
    })
}

/// Reverse an action, if nothing has changed since it was applied.
pub fn undo(git: &Git, undo: &Undo) -> Result<(), OpError> {
    for state in &undo.index {
        if index_entry(git, state.path.as_ref())? != state.after {
            return Err(OpError::Stale(format!(
                "the index entry for {} changed since; undo would overwrite it",
                state.path
            )));
        }
    }
    for state in &undo.worktree {
        let full = git.root().join(gix::path::from_bstr(state.path.as_bstr()));
        if raw_hash(git, &full)? != state.after {
            return Err(OpError::Stale(format!(
                "{} changed since; undo would overwrite it",
                state.path
            )));
        }
    }
    for state in &undo.index {
        match &state.before {
            Some(entry) => {
                git.run(
                    [
                        OsString::from("update-index"),
                        "--add".into(),
                        "--cacheinfo".into(),
                        {
                            let mut arg = OsString::from(format!("{},{},", entry.mode, entry.oid));
                            arg.push(path_arg(state.path.as_ref()));
                            arg
                        },
                    ],
                    None,
                )?;
            }
            None => {
                git.run(
                    [
                        OsString::from("update-index"),
                        "--force-remove".into(),
                        "--".into(),
                        path_arg(state.path.as_ref()),
                    ],
                    None,
                )?;
            }
        }
    }
    for state in &undo.worktree {
        let full = git.root().join(gix::path::from_bstr(state.path.as_bstr()));
        restore_worktree(git, &full, state.before.as_ref())?;
    }
    Ok(())
}

pub fn commit(git: &Git, message: &str, options: CommitOptions) -> Result<String, OpError> {
    if message.trim().is_empty() {
        return Err(OpError::Unsupported("write a commit message first"));
    }
    let mut args = vec!["commit", "-F", "-"];
    if options.amend {
        args.push("--amend");
    }
    if options.signoff {
        args.push("--signoff");
    }
    let out = git
        .run(args, Some(message.as_bytes()))
        .map_err(|err| match err {
            GitError::Failed { message, .. } => OpError::CommitFailed(message),
            other => OpError::Git(other),
        })?;
    Ok(String::from_utf8_lossy(&out.stdout)
        .lines()
        .next()
        .unwrap_or_default()
        .to_string())
}

/// HEAD's message, for amending. Empty before the first commit.
pub fn last_commit_message(git: &Git) -> Result<String, OpError> {
    if !head_exists(git) {
        return Ok(String::new());
    }
    let out = git.run(["log", "-1", "--format=%B"], None)?;
    Ok(String::from_utf8_lossy(&out.stdout).trim_end().to_string())
}

fn head_exists(git: &Git) -> bool {
    git.run(["rev-parse", "-q", "--verify", "HEAD"], None)
        .is_ok()
}

fn snapshot_index(
    git: &Git,
    paths: &[BString],
) -> Result<Vec<(BString, Option<IndexEntry>)>, OpError> {
    paths
        .iter()
        .map(|p| Ok((p.clone(), index_entry(git, p.as_ref())?)))
        .collect()
}

fn zip_index(
    before: Vec<(BString, Option<IndexEntry>)>,
    after: Vec<(BString, Option<IndexEntry>)>,
) -> Vec<IndexState> {
    before
        .into_iter()
        .zip(after)
        .map(|((path, before), (_, after))| IndexState {
            path,
            before,
            after,
        })
        .collect()
}

/// The stage-0 index entry for `path`.
fn index_entry(git: &Git, path: &BStr) -> Result<Option<IndexEntry>, OpError> {
    let out = git.run(
        [
            OsString::from("ls-files"),
            "-s".into(),
            "-z".into(),
            "--".into(),
            path_arg(path),
        ],
        None,
    )?;
    for record in out.stdout.split(|b| *b == 0) {
        let Some(tab) = record.find_byte(b'\t') else {
            continue;
        };
        let (meta, entry_path) = (&record[..tab], &record[tab + 1..]);
        let mut fields = meta.split_str(" ");
        let (Some(mode), Some(oid), Some(stage)) = (fields.next(), fields.next(), fields.next())
        else {
            continue;
        };
        if entry_path == path.as_bytes() && stage == b"0" {
            return Ok(Some(IndexEntry {
                mode: mode.to_str_lossy().into_owned(),
                oid: oid.to_str_lossy().into_owned(),
            }));
        }
    }
    Ok(None)
}

fn file_kind(full: &Path) -> Result<Option<FileKind>, OpError> {
    let meta = match std::fs::symlink_metadata(full) {
        Ok(meta) => meta,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(err.into()),
    };
    if meta.file_type().is_symlink() {
        return Ok(Some(FileKind::Symlink));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if meta.permissions().mode() & 0o111 != 0 {
            return Ok(Some(FileKind::Executable));
        }
    }
    Ok(Some(FileKind::Regular))
}

/// Raw bytes of a worktree entry: file contents, or a symlink's target.
fn read_raw(full: &Path) -> Result<Option<(Vec<u8>, FileKind)>, OpError> {
    let Some(kind) = file_kind(full)? else {
        return Ok(None);
    };
    let bytes = match kind {
        FileKind::Symlink => gix::path::into_bstr(std::fs::read_link(full)?)
            .into_owned()
            .into(),
        _ => std::fs::read(full)?,
    };
    Ok(Some((bytes, kind)))
}

fn hash_raw(git: &Git, bytes: &[u8], write: bool) -> Result<String, OpError> {
    let mut args = vec!["hash-object", "--no-filters", "--stdin"];
    if write {
        args.push("-w");
    }
    let out = git.run(args, Some(bytes))?;
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

fn raw_hash(git: &Git, full: &Path) -> Result<Option<String>, OpError> {
    read_raw(full)?
        .map(|(bytes, _)| hash_raw(git, &bytes, false))
        .transpose()
}

/// Hash of the worktree file after git's clean filters, comparable with a diff side.
fn clean_hash(git: &Git, full: &Path, path: &BStr) -> Result<Option<String>, OpError> {
    let Some((bytes, kind)) = read_raw(full)? else {
        return Ok(None);
    };
    if kind == FileKind::Symlink {
        return hash_raw(git, &bytes, false).map(Some);
    }
    let mut path_flag = OsString::from("--path=");
    path_flag.push(path_arg(path));
    let out = git.run(
        [OsString::from("hash-object"), path_flag, "--stdin".into()],
        Some(&bytes),
    )?;
    Ok(Some(
        String::from_utf8_lossy(&out.stdout).trim().to_string(),
    ))
}

fn save_worktree(git: &Git, full: &Path) -> Result<Option<SavedFile>, OpError> {
    read_raw(full)?
        .map(|(bytes, kind)| {
            Ok(SavedFile {
                oid: hash_raw(git, &bytes, true)?,
                kind,
            })
        })
        .transpose()
}

/// Record blobs in a commit on [`UNDO_REF`] so `git gc` keeps them.
fn keep_reachable(git: &Git, oids: &[&str], message: &str) -> Result<(), OpError> {
    keep_reachable_within(git, oids, message, UNDO_HISTORY)
}

/// Keeps two generations of at most `limit` entries: when the current chain is full it
/// becomes [`UNDO_PREVIOUS_REF`] (dropping the generation before it) and a new chain
/// starts, so history stays bounded without rewriting commits.
fn keep_reachable_within(
    git: &Git,
    oids: &[&str],
    message: &str,
    limit: usize,
) -> Result<(), OpError> {
    let listing: String = oids
        .iter()
        .enumerate()
        .map(|(ix, oid)| format!("100644 blob {oid}\t{ix}\n"))
        .collect();
    let tree = git.run_internal(["mktree"], Some(listing.as_bytes()))?;
    let tree = String::from_utf8_lossy(&tree.stdout).trim().to_string();
    let mut parent = git
        .run(["rev-parse", "-q", "--verify", UNDO_REF], None)
        .ok()
        .map(|out| String::from_utf8_lossy(&out.stdout).trim().to_string());
    if let Some(current) = &parent {
        let out = git.run(["rev-list", "--count", current.as_str()], None)?;
        let length: usize = String::from_utf8_lossy(&out.stdout)
            .trim()
            .parse()
            .unwrap_or(0);
        if length >= limit {
            git.run_internal(["update-ref", UNDO_PREVIOUS_REF, current.as_str()], None)?;
            parent = None;
        }
    }
    let mut args = vec!["commit-tree", tree.as_str(), "-m", message];
    if let Some(parent) = &parent {
        args.extend(["-p", parent.as_str()]);
    }
    let commit = git.run_internal(args, None)?;
    let commit = String::from_utf8_lossy(&commit.stdout).trim().to_string();
    git.run_internal(["update-ref", UNDO_REF, commit.as_str()], None)?;
    Ok(())
}

fn restore_worktree(git: &Git, full: &Path, saved: Option<&SavedFile>) -> Result<(), OpError> {
    if file_kind(full)?.is_some() {
        std::fs::remove_file(full)?;
    }
    let Some(saved) = saved else {
        return Ok(());
    };
    let bytes = git
        .run(["cat-file", "blob", saved.oid.as_str()], None)?
        .stdout;
    if let Some(parent) = full.parent() {
        std::fs::create_dir_all(parent)?;
    }
    match saved.kind {
        #[cfg(unix)]
        FileKind::Symlink => {
            let target: PathBuf = gix::path::from_bstr(bytes.as_bstr()).into_owned();
            std::os::unix::fs::symlink(target, full)?;
        }
        _ => {
            std::fs::write(full, &bytes)?;
            #[cfg(unix)]
            if saved.kind == FileKind::Executable {
                use std::os::unix::fs::PermissionsExt;
                let mut perms = std::fs::metadata(full)?.permissions();
                perms.set_mode(perms.mode() | 0o111);
                std::fs::set_permissions(full, perms)?;
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn git_in(dir: &Path) -> Git {
        let git = Git::new(dir)
            .with_env("GIT_CONFIG_GLOBAL", "/dev/null")
            .with_env("GIT_CONFIG_NOSYSTEM", "1");
        git.run(["init", "-q"], None).unwrap();
        git
    }

    fn count(git: &Git, reference: &str) -> usize {
        git.run(["rev-list", "--count", reference], None)
            .map(|out| String::from_utf8_lossy(&out.stdout).trim().parse().unwrap())
            .unwrap_or(0)
    }

    #[test]
    fn undo_history_keeps_two_generations() {
        let dir = tempfile::tempdir().unwrap();
        let git = git_in(dir.path());
        let blobs: Vec<String> = (0..5)
            .map(|i| hash_raw(&git, format!("saved {i}\n").as_bytes(), true).unwrap())
            .collect();
        for blob in &blobs {
            keep_reachable_within(&git, &[blob], "goro: discard", 2).unwrap();
        }
        assert_eq!(count(&git, UNDO_REF), 1);
        assert_eq!(count(&git, UNDO_PREVIOUS_REF), 2);
        let reachable = git
            .run(["rev-list", "--objects", "--all"], None)
            .unwrap()
            .stdout;
        let reachable = String::from_utf8_lossy(&reachable);
        for (ix, blob) in blobs.iter().enumerate() {
            assert_eq!(reachable.contains(blob.as_str()), ix >= 2, "blob {ix}");
        }
    }
}
