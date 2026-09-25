//! Agent turns: snapshots of the working tree taken by agent hooks at the start and end
//! of each turn, so a turn's changes can be reviewed on their own.
//!
//! A snapshot is a commit (tree = the working tree, including untracked files git doesn't
//! ignore) built through a temporary index, so the user's index is never touched. It lives
//! at `refs/goro/turns/<session>/<unix ms>-<start|end>`, with its metadata as JSON in the
//! commit message.

use std::ffi::OsString;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use gix::bstr::{BString, ByteSlice};
use gix::objs::tree::EntryKind;

use crate::git::Git;
use crate::ops::OpError;
use crate::repo::{ChangeStatus, FileChange, Section, Source};
use crate::store::stable_hash;

pub const TURNS_REF: &str = "refs/goro/turns";
/// Snapshots kept per repository.
pub const KEEP_SNAPSHOTS: usize = 400;
const MESSAGE_PREFIX: &str = "goro turn snapshot\n\n";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TurnEvent {
    Start,
    End,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SnapshotMeta {
    /// `claude` or `codex`.
    pub agent: String,
    pub event: TurnEvent,
    pub session_id: String,
    /// Set by agents that have one (Codex); Claude Code turns pair by order.
    #[serde(default)]
    pub turn_id: Option<String>,
    /// The user's prompt, on turn start.
    #[serde(default)]
    pub prompt: Option<String>,
    pub at_ms: u64,
}

impl SnapshotMeta {
    pub fn now_ms() -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_millis() as u64)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Snapshot {
    pub commit: String,
    pub tree: String,
    pub meta: SnapshotMeta,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Turn {
    pub prompt: Option<String>,
    pub start: Option<Snapshot>,
    /// `None` while the turn is still running.
    pub end: Option<Snapshot>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Session {
    pub id: String,
    pub agent: String,
    /// In order.
    pub turns: Vec<Turn>,
    /// Time of the latest snapshot.
    pub last_at_ms: u64,
}

/// Snapshot the working tree and record it for `meta`.
pub fn snapshot(git: &Git, meta: &SnapshotMeta) -> Result<Snapshot, OpError> {
    let tree = worktree_tree(git)?;
    let message = format!(
        "{MESSAGE_PREFIX}{}",
        serde_json::to_string(meta).expect("metadata serializes")
    );
    let commit = git.run_internal(["commit-tree", tree.as_str(), "-m", message.as_str()], None)?;
    let commit = String::from_utf8_lossy(&commit.stdout).trim().to_string();
    let event = match meta.event {
        TurnEvent::Start => "start",
        TurnEvent::End => "end",
    };
    let name = format!(
        "{TURNS_REF}/{}/{}-{event}",
        ref_component(&meta.session_id),
        meta.at_ms
    );
    git.run_internal(["update-ref", name.as_str(), commit.as_str()], None)?;
    Ok(Snapshot {
        commit,
        tree,
        meta: meta.clone(),
    })
}

/// A tree of the current working tree (tracked and untracked, ignoring what git
/// ignores), written through a temporary copy of the index.
pub fn worktree_tree(git: &Git) -> Result<String, OpError> {
    let out = git.run(["rev-parse", "--absolute-git-dir"], None)?;
    let git_dir = std::path::PathBuf::from(String::from_utf8_lossy(&out.stdout).trim());
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    let temp_index = git_dir.join(format!("goro-index-{}-{nanos}", std::process::id()));
    let index = git_dir.join("index");
    if index.exists() {
        // Starting from the real index keeps its stat cache, so unchanged files aren't
        // re-hashed.
        std::fs::copy(&index, &temp_index)?;
    }
    let temp_git = git
        .clone()
        .with_env("GIT_INDEX_FILE", OsString::from(&temp_index));
    let result = (|| {
        temp_git.run(["add", "-A"], None)?;
        let tree = temp_git.run(["write-tree"], None)?;
        Ok(String::from_utf8_lossy(&tree.stdout).trim().to_string())
    })();
    let _ = std::fs::remove_file(&temp_index);
    result
}

/// All recorded sessions, most recently active first.
pub fn list(git: &Git) -> Result<Vec<Session>, OpError> {
    let mut snapshots = read_snapshots(git)?;
    snapshots.sort_by_key(|s| s.meta.at_ms);
    let mut sessions: Vec<Session> = Vec::new();
    for snapshot in snapshots {
        let session = match sessions
            .iter_mut()
            .find(|s| s.id == snapshot.meta.session_id)
        {
            Some(session) => session,
            None => {
                sessions.push(Session {
                    id: snapshot.meta.session_id.clone(),
                    agent: snapshot.meta.agent.clone(),
                    turns: Vec::new(),
                    last_at_ms: 0,
                });
                sessions.last_mut().unwrap()
            }
        };
        session.last_at_ms = snapshot.meta.at_ms;
        match snapshot.meta.event {
            TurnEvent::Start => session.turns.push(Turn {
                prompt: snapshot.meta.prompt.clone(),
                start: Some(snapshot),
                end: None,
            }),
            TurnEvent::End => {
                // The open turn with the same id, else the latest open turn.
                let open = session.turns.iter_mut().rev().find(|t| {
                    t.end.is_none()
                        && match (&snapshot.meta.turn_id, t.start.as_ref()) {
                            (Some(id), Some(start)) => start.meta.turn_id.as_ref() == Some(id),
                            _ => true,
                        }
                });
                match open {
                    Some(turn) => turn.end = Some(snapshot),
                    None => session.turns.push(Turn {
                        prompt: None,
                        start: None,
                        end: Some(snapshot),
                    }),
                }
            }
        }
    }
    sessions.sort_by_key(|s| std::cmp::Reverse(s.last_at_ms));
    Ok(sessions)
}

/// The changes from `old_tree` to `new_tree` (rename detection on), sorted by path.
pub fn changes_between(
    git: &Git,
    old_tree: &str,
    new_tree: &str,
) -> Result<Vec<FileChange>, OpError> {
    let out = git.run(
        ["diff-tree", "-r", "-z", "--raw", "-M", old_tree, new_tree],
        None,
    )?;
    let mut fields = out.stdout.split(|b| *b == 0).filter(|f| !f.is_empty());
    let mut changes = Vec::new();
    while let Some(header) = fields.next() {
        // `:<old mode> <new mode> <old id> <new id> <status>`
        let header = header.strip_prefix(b":").unwrap_or(header);
        let parts: Vec<&[u8]> = header.split(|b| *b == b' ').collect();
        let [old_mode, new_mode, old_id, new_id, status] = parts[..] else {
            break;
        };
        let Some(path) = fields.next() else {
            break;
        };
        let (status, old_path, path) = match status.first() {
            Some(b'R') | Some(b'C') => {
                let Some(new_path) = fields.next() else {
                    break;
                };
                let status = if status[0] == b'R' {
                    ChangeStatus::Renamed
                } else {
                    ChangeStatus::Copied
                };
                (status, Some(BString::from(path)), BString::from(new_path))
            }
            Some(b'A') => (ChangeStatus::Added, None, BString::from(path)),
            Some(b'D') => (ChangeStatus::Deleted, None, BString::from(path)),
            Some(b'T') => (ChangeStatus::TypeChanged, None, BString::from(path)),
            _ => (ChangeStatus::Modified, None, BString::from(path)),
        };
        changes.push(FileChange {
            section: Section::Snapshot,
            status,
            path,
            old_path,
            old: side(old_mode, old_id),
            new: side(new_mode, new_id),
        });
    }
    changes.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(changes)
}

fn side(mode: &[u8], id: &[u8]) -> Source {
    if id.iter().all(|b| *b == b'0') {
        return Source::Absent;
    }
    let kind = match mode {
        b"100755" => EntryKind::BlobExecutable,
        b"120000" => EntryKind::Link,
        b"160000" => EntryKind::Commit,
        _ => EntryKind::Blob,
    };
    match gix::ObjectId::from_hex(id.trim()) {
        Ok(id) => Source::Blob { id, kind },
        Err(_) => Source::Absent,
    }
}

/// Delete all but the newest `keep` snapshots.
pub fn prune(git: &Git, keep: usize) -> Result<(), OpError> {
    let out = git.run(["for-each-ref", "--format=%(refname)", TURNS_REF], None)?;
    let mut refs: Vec<(u64, String)> = String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|name| {
            let leaf = name.rsplit('/').next()?;
            let at: u64 = leaf.split('-').next()?.parse().ok()?;
            Some((at, name.to_string()))
        })
        .collect();
    if refs.len() <= keep {
        return Ok(());
    }
    refs.sort();
    let stale = refs.len() - keep;
    let commands: String = refs[..stale]
        .iter()
        .map(|(_, name)| format!("delete {name}\n"))
        .collect();
    git.run_internal(["update-ref", "--stdin"], Some(commands.as_bytes()))?;
    Ok(())
}

fn read_snapshots(git: &Git) -> Result<Vec<Snapshot>, OpError> {
    // Records separated by 0x1e, fields by 0x1f.
    let out = git.run(
        [
            "for-each-ref",
            "--format=%(objectname)%1f%(tree)%1f%(contents)%1e",
            TURNS_REF,
        ],
        None,
    )?;
    Ok(String::from_utf8_lossy(&out.stdout)
        .split('\u{1e}')
        .filter_map(|record| {
            let mut fields = record.trim_start_matches('\n').splitn(3, '\u{1f}');
            let (commit, tree, message) = (fields.next()?, fields.next()?, fields.next()?);
            let json = message.strip_prefix(MESSAGE_PREFIX)?;
            let meta: SnapshotMeta = serde_json::from_str(json.trim()).ok()?;
            Some(Snapshot {
                commit: commit.to_string(),
                tree: tree.to_string(),
                meta,
            })
        })
        .collect())
}

/// A ref-safe path component for `id`: kept as is when simple, else a stable hash.
fn ref_component(id: &str) -> String {
    let simple = !id.is_empty()
        && id.len() <= 100
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
        && !id.starts_with('-');
    if simple {
        id.to_string()
    } else {
        format!("h{:016x}", stable_hash(&[id.as_bytes()]))
    }
}
