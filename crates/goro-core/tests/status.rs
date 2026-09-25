//! Status and per-file diffs against real temporary repositories, checked against the
//! `git` CLI.

use std::path::PathBuf;
use std::process::Command;

use goro_core::repo::{ChangeStatus, FileChange, Loaded, Repo, Section};

struct Fixture {
    _dir: tempfile::TempDir,
    root: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let fx = Self { _dir: dir, root };
        fx.git(&["init", "-q", "-b", "main"]);
        for (key, value) in [
            ("user.name", "Goro Test"),
            ("user.email", "goro@example.invalid"),
            ("diff.algorithm", "histogram"),
            ("diff.renames", "true"),
            ("status.renames", "true"),
            ("core.autocrlf", "false"),
            ("commit.gpgsign", "false"),
        ] {
            fx.git(&["config", key, value]);
        }
        fx
    }

    fn git(&self, args: &[&str]) -> Vec<u8> {
        let out = self.git_raw(args);
        assert!(
            out.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        out.stdout
    }

    fn git_raw(&self, args: &[&str]) -> std::process::Output {
        Command::new("git")
            .current_dir(&self.root)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .args(args)
            .output()
            .unwrap()
    }

    fn write(&self, path: &str, contents: impl AsRef<[u8]>) {
        let full = self.root.join(path);
        std::fs::create_dir_all(full.parent().unwrap()).unwrap();
        std::fs::write(full, contents).unwrap();
    }

    fn commit_all(&self, message: &str) {
        self.git(&["add", "-A"]);
        self.git(&["commit", "-q", "-m", message]);
    }

    fn repo(&self) -> Repo {
        Repo::discover(&self.root).unwrap()
    }

    /// `git diff` hunks for a change, from the first `@@` on.
    fn git_hunks(&self, change: &FileChange) -> Vec<u8> {
        let path = change.path_lossy().into_owned();
        let out = match change.section {
            Section::Staged => {
                let mut args = vec!["diff", "--cached", "--no-color", "-M", "--"];
                let old = change.old_path.as_ref().map(|p| p.to_string());
                if let Some(old) = &old {
                    args.push(old);
                }
                args.push(&path);
                self.git(&args)
            }
            Section::Unstaged => self.git(&["diff", "--no-color", "--", &path]),
            Section::Untracked => {
                self.git_raw(&["diff", "--no-color", "--no-index", "/dev/null", &path])
                    .stdout
            }
            Section::Snapshot => unreachable!("status never reports snapshot changes"),
        };
        match out.windows(3).position(|w| w == b"\n@@") {
            Some(pos) => out[pos + 1..].to_vec(),
            None => Vec::new(),
        }
    }
}

fn summary(changes: &[FileChange]) -> Vec<(Section, ChangeStatus, String, Option<String>)> {
    changes
        .iter()
        .map(|c| {
            (
                c.section,
                c.status,
                c.path_lossy().into_owned(),
                c.old_path.as_ref().map(|p| p.to_string()),
            )
        })
        .collect()
}

fn lines(n: usize, tag: &str) -> String {
    (1..=n).map(|i| format!("{tag} line {i}\n")).collect()
}

fn build_mixed_fixture() -> Fixture {
    let fx = Fixture::new();
    fx.write("a.txt", lines(30, "a"));
    fx.write("b.txt", lines(30, "b"));
    fx.write("c.txt", lines(5, "c"));
    fx.write("d.txt", lines(5, "d"));
    fx.write("old_name.txt", lines(40, "rename me"));
    fx.write(".gitignore", "*.log\n");
    fx.commit_all("initial");

    fx.write(
        "a.txt",
        lines(30, "a").replace("a line 7\n", "a line seven\n"),
    );
    fx.write(
        "b.txt",
        lines(30, "b").replace("b line 2\n", "b line two\n"),
    );
    fx.git(&["add", "b.txt"]);
    fx.write(
        "b.txt",
        lines(30, "b")
            .replace("b line 2\n", "b line two\n")
            .replace("b line 29\n", "b line twenty-nine\n"),
    );
    std::fs::remove_file(fx.root.join("c.txt")).unwrap();
    fx.git(&["rm", "-q", "d.txt"]);
    fx.git(&["mv", "old_name.txt", "new_name.txt"]);
    fx.write("u.txt", "untracked\n");
    fx.write("sub/dir/x.txt", "nested untracked\n");
    fx.write("debug.log", "ignored\n");
    fx
}

#[test]
fn status_groups_changes_by_section() {
    let fx = build_mixed_fixture();
    let changes = fx.repo().status().unwrap();
    let s = |section, status, path: &str, old: Option<&str>| {
        (section, status, path.to_string(), old.map(str::to_string))
    };
    use ChangeStatus::*;
    use Section::*;
    assert_eq!(
        summary(&changes),
        vec![
            s(Staged, Modified, "b.txt", None),
            s(Staged, Deleted, "d.txt", None),
            s(Staged, Renamed, "new_name.txt", Some("old_name.txt")),
            s(Unstaged, Modified, "a.txt", None),
            s(Unstaged, Modified, "b.txt", None),
            s(Unstaged, Deleted, "c.txt", None),
            s(Untracked, Added, "sub/dir/x.txt", None),
            s(Untracked, Added, "u.txt", None),
        ]
    );
}

#[test]
fn every_diff_matches_git() {
    let fx = build_mixed_fixture();
    let repo = fx.repo();
    let changes = repo.status().unwrap();
    let thread = repo.thread_local();
    let mut loader = thread.loader().unwrap();
    for change in &changes {
        let Loaded::Text(diff) = loader.load(change).unwrap() else {
            panic!("expected text diff for {change:?}");
        };
        assert_eq!(
            String::from_utf8_lossy(&diff.to_unified_hunks()),
            String::from_utf8_lossy(&fx.git_hunks(change)),
            "{:?} {}",
            change.section,
            change.path_lossy()
        );
    }
}

#[test]
fn autocrlf_worktree_is_normalized_like_git() {
    let fx = Fixture::new();
    fx.git(&["config", "core.autocrlf", "true"]);
    fx.write("win.txt", lines(10, "w"));
    fx.commit_all("initial");
    let crlf = lines(10, "w")
        .replace("w line 4\n", "w line four\n")
        .replace('\n', "\r\n");
    fx.write("win.txt", crlf);

    let repo = fx.repo();
    let changes = repo.status().unwrap();
    assert_eq!(changes.len(), 1);
    let thread = repo.thread_local();
    let Loaded::Text(diff) = thread.loader().unwrap().load(&changes[0]).unwrap() else {
        panic!("expected text");
    };
    let changed = diff
        .lines
        .iter()
        .filter(|l| l.kind != goro_core::diff::LineKind::Context)
        .count();
    assert_eq!(changed, 2, "only one line replaced");
    assert_eq!(
        String::from_utf8_lossy(&diff.to_unified_hunks()),
        String::from_utf8_lossy(&fx.git_hunks(&changes[0]))
    );
}

#[test]
fn binary_files_are_not_diffed() {
    let fx = Fixture::new();
    fx.write("img.bin", b"\x89PNG\0\0\x01".as_slice());
    fx.commit_all("initial");
    fx.write("img.bin", b"\x89PNG\0\0\x02\x03".as_slice());
    let repo = fx.repo();
    let changes = repo.status().unwrap();
    let thread = repo.thread_local();
    let loaded = thread.loader().unwrap().load(&changes[0]).unwrap();
    assert!(
        matches!(
            loaded,
            Loaded::Binary {
                old_len: 7,
                new_len: 8
            }
        ),
        "{loaded:?}"
    );
}

#[cfg(unix)]
#[test]
fn symlink_diffs_as_its_target() {
    let fx = Fixture::new();
    std::os::unix::fs::symlink("target-a", fx.root.join("link")).unwrap();
    fx.commit_all("initial");
    std::fs::remove_file(fx.root.join("link")).unwrap();
    std::os::unix::fs::symlink("target-b", fx.root.join("link")).unwrap();
    let repo = fx.repo();
    let changes = repo.status().unwrap();
    let thread = repo.thread_local();
    let Loaded::Text(diff) = thread.loader().unwrap().load(&changes[0]).unwrap() else {
        panic!("expected text");
    };
    assert_eq!(
        String::from_utf8_lossy(&diff.to_unified_hunks()),
        String::from_utf8_lossy(&fx.git_hunks(&changes[0]))
    );
}

#[test]
fn status_never_writes_the_index() {
    let fx = Fixture::new();
    fx.write("a.txt", "one\n");
    fx.commit_all("initial");
    // Touch the file so its stat data no longer matches the index.
    std::thread::sleep(std::time::Duration::from_millis(1100));
    fx.write("a.txt", "one\n");
    let index = fx.root.join(".git/index");
    let before = std::fs::read(&index).unwrap();
    let before_mtime = std::fs::metadata(&index).unwrap().modified().unwrap();
    let changes = fx.repo().status().unwrap();
    assert!(changes.is_empty(), "{changes:?}");
    assert_eq!(std::fs::read(&index).unwrap(), before);
    assert_eq!(
        std::fs::metadata(&index).unwrap().modified().unwrap(),
        before_mtime
    );
}

#[test]
fn discover_from_subdirectory_and_reject_non_repos() {
    let fx = Fixture::new();
    fx.write("deep/er/file.txt", "x\n");
    let repo = Repo::discover(&fx.root.join("deep/er")).unwrap();
    assert_eq!(repo.root().canonicalize().unwrap(), fx.root);
    let outside = tempfile::tempdir().unwrap();
    assert!(Repo::discover(outside.path()).is_err());
}
