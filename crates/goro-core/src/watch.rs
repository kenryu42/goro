//! Live updates: watch the worktree and git's own state, debounce, drop ignored paths, and
//! report which worktree paths may have changed.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use notify::{RecursiveMode, Watcher as _};

use crate::review::Dirty;

/// Quiet period after the last event before reporting.
const DEBOUNCE: Duration = Duration::from_millis(25);
/// Report at least this often while events keep arriving.
const MAX_DELAY: Duration = Duration::from_millis(100);

#[derive(Debug, thiserror::Error)]
pub enum WatchError {
    #[error("live updates unavailable: {0}")]
    Notify(#[from] notify::Error),
    #[error("live updates unavailable: {0}")]
    Repo(String),
}

/// Keeps the watch alive; dropping it stops watching.
pub struct RepoWatcher {
    _watcher: notify::RecommendedWatcher,
}

/// Watch `root` and call `on_change` (on a background thread) after relevant changes.
/// Changes inside `.git` other than the index, HEAD and refs, and paths git ignores, are
/// not reported. Index/HEAD/ref changes report no dirty worktree paths.
pub fn watch(
    root: &Path,
    on_change: impl Fn(Dirty) + Send + 'static,
) -> Result<RepoWatcher, WatchError> {
    let repo = gix::open(root).map_err(|e| WatchError::Repo(e.to_string()))?;
    let root = repo
        .workdir()
        .ok_or_else(|| WatchError::Repo("no working tree".into()))?
        .canonicalize()
        .map_err(|e| WatchError::Repo(e.to_string()))?;
    let git_dir = repo
        .git_dir()
        .canonicalize()
        .map_err(|e| WatchError::Repo(e.to_string()))?;
    let thread_safe = repo.into_sync();

    let (tx, rx) = mpsc::channel::<PathBuf>();
    let mut watcher = notify::recommended_watcher(move |event: notify::Result<notify::Event>| {
        if let Ok(event) = event
            && is_change(&event.kind)
        {
            for path in event.paths {
                let _ = tx.send(path);
            }
        }
    })?;
    watcher.watch(&root, RecursiveMode::Recursive)?;
    if !git_dir.starts_with(&root) {
        watcher.watch(&git_dir, RecursiveMode::Recursive)?;
    }

    std::thread::Builder::new()
        .name("goro-watch".into())
        .spawn(move || {
            let repo = thread_safe.to_thread_local();
            while let Ok(first) = rx.recv() {
                let started = Instant::now();
                let mut paths = vec![first];
                loop {
                    let remaining = MAX_DELAY.saturating_sub(started.elapsed()).min(DEBOUNCE);
                    match rx.recv_timeout(remaining) {
                        Ok(path) => paths.push(path),
                        Err(mpsc::RecvTimeoutError::Timeout) => break,
                        Err(mpsc::RecvTimeoutError::Disconnected) => return,
                    }
                    if started.elapsed() >= MAX_DELAY {
                        break;
                    }
                }
                if let Some(dirty) = classify(&repo, &root, &git_dir, paths) {
                    on_change(dirty);
                }
            }
        })
        .map_err(|e| WatchError::Repo(e.to_string()))?;
    Ok(RepoWatcher { _watcher: watcher })
}

/// Turn raw event paths into a report, or `None` if nothing relevant changed.
fn classify(
    repo: &gix::Repository,
    root: &Path,
    git_dir: &Path,
    paths: Vec<PathBuf>,
) -> Option<Dirty> {
    let mut git_state_changed = false;
    let mut dirty = HashSet::new();
    let index = repo.index_or_empty().ok();
    let mut excludes = index.as_ref().and_then(|index| {
        repo.excludes(
            index,
            None,
            gix::worktree::stack::state::ignore::Source::WorktreeThenIdMappingIfNotSkipped,
        )
        .ok()
    });
    for path in paths {
        if let Ok(rela) = path.strip_prefix(git_dir) {
            if is_git_state(rela) {
                git_state_changed = true;
            }
            continue;
        }
        let Ok(rela) = path.strip_prefix(root) else {
            continue;
        };
        if rela.as_os_str().is_empty() {
            continue;
        }
        if let Some(excludes) = excludes.as_mut()
            && is_ignored(excludes, rela)
        {
            continue;
        }
        if path.is_dir() {
            // A directory appeared, vanished or was renamed: anything below may differ.
            return Some(Dirty::All);
        }
        dirty.insert(
            gix::path::to_unix_separators_on_windows(gix::path::into_bstr(rela)).into_owned(),
        );
    }
    if dirty.is_empty() && !git_state_changed {
        None
    } else {
        Some(Dirty::Paths(dirty))
    }
}

/// Whether an event is a write. Reads must not count: Linux reports opens and reads, and
/// reloading reads files, which would trigger itself forever.
pub fn is_change(kind: &notify::EventKind) -> bool {
    use notify::EventKind;
    use notify::event::{MetadataKind, ModifyKind};
    match kind {
        EventKind::Access(_) => false,
        EventKind::Modify(ModifyKind::Metadata(MetadataKind::AccessTime)) => false,
        EventKind::Create(_) | EventKind::Modify(_) | EventKind::Remove(_) | EventKind::Any => true,
        EventKind::Other => false,
    }
}

/// The parts of `.git` whose changes affect status.
fn is_git_state(rela: &Path) -> bool {
    let mut components = rela.components();
    let Some(first) = components.next() else {
        return false;
    };
    let first = first.as_os_str();
    first == "index" || first == "HEAD" || first == "packed-refs" || first == "refs"
}

/// Like git: a path is ignored if it or any parent directory is.
fn is_ignored(excludes: &mut gix::AttributeStack<'_>, rela: &Path) -> bool {
    let mut prefix = PathBuf::new();
    let components: Vec<_> = rela.components().collect();
    for (ix, component) in components.iter().enumerate() {
        prefix.push(component);
        let is_last = ix + 1 == components.len();
        let mode = (!is_last).then_some(gix::index::entry::Mode::DIR);
        if excludes
            .at_path(&prefix, mode)
            .is_ok_and(|platform| platform.is_excluded())
        {
            return true;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;
    use std::sync::{Arc, Mutex};

    fn git(root: &Path, args: &[&str]) {
        let out = Command::new("git")
            .current_dir(root)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .args(args)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    /// Wait until a report arrives (or time out), return all reports so far.
    fn wait(reports: &Arc<Mutex<Vec<Dirty>>>, timeout: Duration) -> Vec<Dirty> {
        let start = Instant::now();
        while start.elapsed() < timeout {
            if !reports.lock().unwrap().is_empty() {
                // Let a trailing batch land too.
                std::thread::sleep(Duration::from_millis(150));
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        std::mem::take(&mut *reports.lock().unwrap())
    }

    fn paths(reports: &[Dirty]) -> Vec<String> {
        let mut out: Vec<String> = reports
            .iter()
            .flat_map(|d| match d {
                Dirty::All => vec!["<all>".to_string()],
                Dirty::Paths(p) => p.iter().map(|p| p.to_string()).collect(),
            })
            .collect();
        out.sort();
        out.dedup();
        out
    }

    #[test]
    fn reports_worktree_and_index_changes_but_not_ignored_paths() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        git(&root, &["init", "-q"]);
        std::fs::write(root.join(".gitignore"), "target/\n*.log\n").unwrap();
        std::fs::create_dir_all(root.join("target/debug")).unwrap();
        std::fs::write(root.join("a.txt"), "a\n").unwrap();
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("src/b.txt"), "b\n").unwrap();

        let reports = Arc::new(Mutex::new(Vec::new()));
        let sink = reports.clone();
        let _watcher = watch(&root, move |d| sink.lock().unwrap().push(d)).unwrap();
        // FSEvents may deliver events from before the watch started; drain them.
        std::thread::sleep(Duration::from_millis(300));
        reports.lock().unwrap().clear();

        std::fs::write(root.join("target/debug/out.o"), "x").unwrap();
        std::fs::write(root.join("debug.log"), "x").unwrap();
        assert_eq!(
            wait(&reports, Duration::from_millis(600)),
            [] as [Dirty; 0],
            "ignored"
        );

        std::fs::write(root.join("a.txt"), "changed\n").unwrap();
        assert_eq!(paths(&wait(&reports, Duration::from_secs(3))), ["a.txt"]);

        // Reported with `/`, as git and the review name paths, on every platform.
        std::fs::write(root.join("src/b.txt"), "changed\n").unwrap();
        assert_eq!(
            paths(&wait(&reports, Duration::from_secs(3))),
            ["src/b.txt"]
        );

        git(&root, &["add", "a.txt"]);
        let reports_after_add = wait(&reports, Duration::from_secs(3));
        assert!(
            reports_after_add
                .iter()
                .any(|d| matches!(d, Dirty::Paths(p) if p.is_empty())),
            "index change reported without dirty paths: {reports_after_add:?}"
        );
    }

    #[test]
    fn only_writes_count_as_changes() {
        use notify::EventKind;
        use notify::event::{AccessKind, CreateKind, MetadataKind, ModifyKind, RemoveKind};
        // Reading a file (Linux reports opens and reads) must not trigger a reload: status
        // itself reads files, which would loop forever.
        assert!(!is_change(&EventKind::Access(AccessKind::Any)));
        assert!(!is_change(&EventKind::Modify(ModifyKind::Metadata(
            MetadataKind::AccessTime
        ))));
        assert!(is_change(&EventKind::Create(CreateKind::File)));
        assert!(is_change(&EventKind::Modify(ModifyKind::Any)));
        assert!(is_change(&EventKind::Modify(ModifyKind::Metadata(
            MetadataKind::Permissions
        ))));
        assert!(is_change(&EventKind::Remove(RemoveKind::Any)));
    }

    #[test]
    fn git_state_paths() {
        assert!(is_git_state(Path::new("index")));
        assert!(is_git_state(Path::new("refs/heads/main")));
        assert!(!is_git_state(Path::new("objects/ab/cdef")));
        assert!(!is_git_state(Path::new("index.lock")));
    }
}
