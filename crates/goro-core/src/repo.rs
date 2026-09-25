//! Repository access: what changed, and the before/after contents of each change.
//!
//! Reads only. Goro never writes the index during status (no stat refresh), so it is
//! safe to run while an agent is working in the same repository.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use gix::bstr::{BStr, BString, ByteSlice};
use gix::objs::tree::EntryKind;
use gix::status::index_worktree::Item as WorktreeItem;
use gix::status::plumbing::index_as_worktree::{Change as WorktreeChange, EntryStatus};

use crate::diff::{DEFAULT_CONTEXT, FileDiff, is_binary};

/// Files larger than this are listed but not diffed.
pub const MAX_DIFF_BYTES: u64 = 16 * 1024 * 1024;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("not a git repository: {0}")]
    NotARepository(PathBuf),
    #[error("repository has no working tree: {0}")]
    Bare(PathBuf),
    #[error(transparent)]
    Git(Box<dyn std::error::Error + Send + Sync>),
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

fn git_err(err: impl std::error::Error + Send + Sync + 'static) -> Error {
    Error::Git(Box::new(err))
}

/// Where a change lives relative to git's three trees.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Section {
    /// HEAD → index (`git diff --cached`).
    Staged,
    /// Index → worktree (`git diff`).
    Unstaged,
    /// Not tracked by git and not ignored.
    Untracked,
    /// Between two snapshots (an agent turn); read-only.
    Snapshot,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChangeStatus {
    Added,
    Modified,
    Deleted,
    Renamed,
    Copied,
    TypeChanged,
    Conflicted,
}

/// One side of a change.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Source {
    Absent,
    Blob { id: gix::ObjectId, kind: EntryKind },
    Worktree,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileChange {
    pub section: Section,
    pub status: ChangeStatus,
    /// Repository-relative path, `/`-separated.
    pub path: BString,
    /// Source path of a rename or copy.
    pub old_path: Option<BString>,
    pub old: Source,
    pub new: Source,
}

impl FileChange {
    pub fn path_lossy(&self) -> std::borrow::Cow<'_, str> {
        self.path.to_str_lossy()
    }
}

/// What a change looks like once its contents are loaded.
#[derive(Debug, Clone)]
pub enum Loaded {
    Text(Arc<FileDiff>),
    Binary {
        old_len: u64,
        new_len: u64,
    },
    TooLarge {
        len: u64,
    },
    Submodule,
    Conflict,
    /// An untracked directory that is itself a repository, or a non-file entry.
    NotAFile,
    /// An image, shown before and after instead of as a diff.
    Image {
        kind: ImageKind,
        old: Option<Arc<[u8]>>,
        new: Option<Arc<[u8]>>,
    },
}

/// Raster image formats shown as images (SVG stays a text diff).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImageKind {
    Png,
    Jpeg,
    Gif,
    Webp,
    Bmp,
    Tiff,
    Ico,
}

impl ImageKind {
    pub fn for_path(path: &BStr) -> Option<Self> {
        let ext = path.rsplit_str(".").next()?.to_ascii_lowercase();
        Some(match ext.as_slice() {
            b"png" => Self::Png,
            b"jpg" | b"jpeg" => Self::Jpeg,
            b"gif" => Self::Gif,
            b"webp" => Self::Webp,
            b"bmp" => Self::Bmp,
            b"tif" | b"tiff" => Self::Tiff,
            b"ico" => Self::Ico,
            _ => return None,
        })
    }
}

pub struct Repo {
    inner: gix::ThreadSafeRepository,
    root: PathBuf,
}

impl Repo {
    pub fn discover(path: &Path) -> Result<Self, Error> {
        let inner = gix::ThreadSafeRepository::discover(path)
            .map_err(|_| Error::NotARepository(path.to_path_buf()))?;
        let root = inner
            .work_dir()
            .ok_or_else(|| Error::Bare(path.to_path_buf()))?
            .to_path_buf();
        Ok(Self { inner, root })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// All changes, sorted by section and then path.
    pub fn status(&self) -> Result<Vec<FileChange>, Error> {
        let repo = self.inner.to_thread_local();
        let iter = repo
            .status(gix::progress::Discard)
            .map_err(git_err)?
            .untracked_files(gix::status::UntrackedFiles::Files)
            .index_worktree_rewrites(None)
            .index_worktree_submodules(gix::status::Submodule::Given {
                ignore: gix::submodule::config::Ignore::Dirty,
                check_dirty: false,
            })
            .into_iter(None)
            .map_err(git_err)?;

        let mut changes = Vec::new();
        for item in iter {
            let item = item.map_err(git_err)?;
            if let Some(change) = convert_item(item) {
                changes.push(change);
            }
        }
        changes.sort_by(|a, b| (a.section, &a.path).cmp(&(b.section, &b.path)));
        changes.dedup_by(|a, b| a.section == b.section && a.path == b.path);
        Ok(changes)
    }

    /// A handle for the current thread; content loading needs one per worker thread.
    pub fn thread_local(&self) -> ThreadRepo {
        ThreadRepo {
            repo: self.inner.to_thread_local(),
            root: self.root.clone(),
        }
    }
}

pub struct ThreadRepo {
    repo: gix::Repository,
    root: PathBuf,
}

impl ThreadRepo {
    pub fn loader(&self) -> Result<Loader<'_>, Error> {
        let (pipeline, index) = self.repo.filter_pipeline(None).map_err(git_err)?;
        Ok(Loader {
            repo: &self.repo,
            pipeline,
            index,
            root: &self.root,
        })
    }
}

fn convert_item(item: gix::status::Item) -> Option<FileChange> {
    match item {
        gix::status::Item::TreeIndex(change) => Some(convert_tree_index(change)),
        gix::status::Item::IndexWorktree(item) => convert_index_worktree(item),
    }
}

fn blob(id: &gix::oid, mode: gix::index::entry::Mode) -> Source {
    Source::Blob {
        id: id.to_owned(),
        kind: mode_to_kind(mode),
    }
}

fn mode_to_kind(mode: gix::index::entry::Mode) -> EntryKind {
    mode.to_tree_entry_mode()
        .map(|m| m.kind())
        .unwrap_or(EntryKind::Blob)
}

fn convert_tree_index(change: gix::diff::index::Change) -> FileChange {
    use gix::diff::index::ChangeRef;
    let (status, path, old_path, old, new) = match change {
        ChangeRef::Addition {
            location,
            entry_mode,
            id,
            ..
        } => (
            ChangeStatus::Added,
            location.into_owned(),
            None,
            Source::Absent,
            blob(&id, entry_mode),
        ),
        ChangeRef::Deletion {
            location,
            entry_mode,
            id,
            ..
        } => (
            ChangeStatus::Deleted,
            location.into_owned(),
            None,
            blob(&id, entry_mode),
            Source::Absent,
        ),
        ChangeRef::Modification {
            location,
            previous_entry_mode,
            previous_id,
            entry_mode,
            id,
            ..
        } => {
            let status = if mode_to_kind(previous_entry_mode) == mode_to_kind(entry_mode)
                || !is_type_change(previous_entry_mode, entry_mode)
            {
                ChangeStatus::Modified
            } else {
                ChangeStatus::TypeChanged
            };
            (
                status,
                location.into_owned(),
                None,
                blob(&previous_id, previous_entry_mode),
                blob(&id, entry_mode),
            )
        }
        ChangeRef::Rewrite {
            source_location,
            source_entry_mode,
            source_id,
            location,
            entry_mode,
            id,
            copy,
            ..
        } => (
            if copy {
                ChangeStatus::Copied
            } else {
                ChangeStatus::Renamed
            },
            location.into_owned(),
            Some(source_location.into_owned()),
            blob(&source_id, source_entry_mode),
            blob(&id, entry_mode),
        ),
    };
    FileChange {
        section: Section::Staged,
        status,
        path,
        old_path,
        old,
        new,
    }
}

/// Executable-bit flips are modifications; file ↔ symlink ↔ submodule are type changes.
fn is_type_change(a: gix::index::entry::Mode, b: gix::index::entry::Mode) -> bool {
    let norm = |kind: EntryKind| match kind {
        EntryKind::BlobExecutable => EntryKind::Blob,
        other => other,
    };
    norm(mode_to_kind(a)) != norm(mode_to_kind(b))
}

fn convert_index_worktree(item: WorktreeItem) -> Option<FileChange> {
    match item {
        WorktreeItem::Modification {
            entry,
            rela_path,
            status,
            ..
        } => {
            let old = blob(&entry.id, entry.mode);
            let (status, new) = match status {
                EntryStatus::Conflict { .. } => (ChangeStatus::Conflicted, Source::Worktree),
                EntryStatus::Change(WorktreeChange::Removed) => {
                    (ChangeStatus::Deleted, Source::Absent)
                }
                EntryStatus::Change(WorktreeChange::Type { .. }) => {
                    (ChangeStatus::TypeChanged, Source::Worktree)
                }
                EntryStatus::Change(WorktreeChange::Modification { .. })
                | EntryStatus::Change(WorktreeChange::SubmoduleModification(_)) => {
                    (ChangeStatus::Modified, Source::Worktree)
                }
                EntryStatus::IntentToAdd => {
                    return Some(FileChange {
                        section: Section::Unstaged,
                        status: ChangeStatus::Added,
                        path: rela_path,
                        old_path: None,
                        old: Source::Absent,
                        new: Source::Worktree,
                    });
                }
                EntryStatus::NeedsUpdate(_) => return None,
            };
            Some(FileChange {
                section: Section::Unstaged,
                status,
                path: rela_path,
                old_path: None,
                old,
                new,
            })
        }
        WorktreeItem::DirectoryContents { entry, .. } => {
            (entry.status == gix::dir::entry::Status::Untracked).then_some(FileChange {
                section: Section::Untracked,
                status: ChangeStatus::Added,
                path: entry.rela_path,
                old_path: None,
                old: Source::Absent,
                new: Source::Worktree,
            })
        }
        // Worktree rename tracking is disabled; `git diff` doesn't show these either.
        WorktreeItem::Rewrite { .. } => None,
    }
}

/// Loads file contents for diffing, applying git's clean filters (e.g. CRLF) to worktree
/// files so the diff matches `git diff`.
pub struct Loader<'r> {
    repo: &'r gix::Repository,
    pipeline: gix::filter::Pipeline<'r>,
    index: gix::worktree::IndexPersistedOrInMemory,
    root: &'r Path,
}

enum Side {
    Absent,
    Bytes(Vec<u8>),
    TooLarge(u64),
    Submodule,
    NotAFile,
}

impl Loader<'_> {
    pub fn load(&mut self, change: &FileChange) -> Result<Loaded, Error> {
        if change.status == ChangeStatus::Conflicted {
            return Ok(Loaded::Conflict);
        }
        let old = self.side(
            &change.old,
            change.old_path.as_ref().unwrap_or(&change.path).as_ref(),
        )?;
        let new = self.side(&change.new, change.path.as_ref())?;
        Ok(match (old, new) {
            (Side::Submodule, _) | (_, Side::Submodule) => Loaded::Submodule,
            (Side::NotAFile, _) | (_, Side::NotAFile) => Loaded::NotAFile,
            (Side::TooLarge(len), _) | (_, Side::TooLarge(len)) => Loaded::TooLarge { len },
            (old, new) if ImageKind::for_path(change.path.as_ref()).is_some() => Loaded::Image {
                kind: ImageKind::for_path(change.path.as_ref()).expect("checked above"),
                old: old.into_image_bytes(),
                new: new.into_image_bytes(),
            },
            (old, new) => {
                let old = old.into_bytes();
                let new = new.into_bytes();
                if is_binary(&old) || is_binary(&new) {
                    Loaded::Binary {
                        old_len: old.len() as u64,
                        new_len: new.len() as u64,
                    }
                } else {
                    Loaded::Text(Arc::new(FileDiff::compute(old, new, DEFAULT_CONTEXT)))
                }
            }
        })
    }

    fn side(&mut self, source: &Source, path: &BStr) -> Result<Side, Error> {
        match source {
            Source::Absent => Ok(Side::Absent),
            Source::Blob {
                kind: EntryKind::Commit,
                ..
            } => Ok(Side::Submodule),
            Source::Blob { id, .. } => {
                let header = self.repo.find_header(*id).map_err(git_err)?;
                if header.size() > MAX_DIFF_BYTES {
                    return Ok(Side::TooLarge(header.size()));
                }
                let object = self.repo.find_object(*id).map_err(git_err)?;
                Ok(Side::Bytes(object.detach().data))
            }
            Source::Worktree => self.worktree_side(path),
        }
    }

    fn worktree_side(&mut self, rela_path: &BStr) -> Result<Side, Error> {
        let rela = gix::path::from_bstr(rela_path);
        let full = self.root.join(&rela);
        let meta = std::fs::symlink_metadata(&full)?;
        if meta.file_type().is_symlink() {
            // git diffs a symlink as its target path.
            let target = std::fs::read_link(&full)?;
            return Ok(Side::Bytes(
                gix::path::into_bstr(target).into_owned().into(),
            ));
        }
        if meta.is_dir() {
            return Ok(if full.join(".git").exists() {
                Side::Submodule
            } else {
                Side::NotAFile
            });
        }
        if !meta.is_file() {
            return Ok(Side::NotAFile);
        }
        if meta.len() > MAX_DIFF_BYTES {
            return Ok(Side::TooLarge(meta.len()));
        }
        let file = std::fs::File::open(&full)?;
        let mut out = Vec::with_capacity(meta.len() as usize);
        match self
            .pipeline
            .convert_to_git(file, &rela, &self.index)
            .map_err(git_err)?
        {
            gix::filter::plumbing::pipeline::convert::ToGitOutcome::Unchanged(mut file) => {
                file.read_to_end(&mut out)?;
            }
            gix::filter::plumbing::pipeline::convert::ToGitOutcome::Process(mut read) => {
                read.read_to_end(&mut out)?;
            }
            gix::filter::plumbing::pipeline::convert::ToGitOutcome::Buffer(buf) => {
                out.extend_from_slice(buf);
            }
        }
        Ok(Side::Bytes(out))
    }
}

impl Side {
    fn into_image_bytes(self) -> Option<Arc<[u8]>> {
        match self {
            Side::Bytes(bytes) => Some(bytes.into()),
            _ => None,
        }
    }

    fn into_bytes(self) -> Vec<u8> {
        match self {
            Side::Bytes(bytes) => bytes,
            _ => Vec::new(),
        }
    }
}
