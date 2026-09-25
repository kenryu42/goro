//! Stage, unstage, discard, undo and commit against real temporary repositories.

use std::path::PathBuf;
use std::process::Command;
use std::sync::Arc;

use goro_core::diff::{FileDiff, LineKind};
use goro_core::git::Git;
use goro_core::ops::{self, CommitOptions, OpError, Selection};
use goro_core::repo::{FileChange, Loaded, Repo, Section};

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
        for (k, v) in [
            ("user.name", "Goro Test"),
            ("user.email", "goro@example.invalid"),
            ("core.autocrlf", "false"),
            ("commit.gpgsign", "false"),
        ] {
            fx.git(&["config", k, v]);
        }
        fx
    }

    fn git(&self, args: &[&str]) -> String {
        let out = Command::new("git")
            .current_dir(&self.root)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .args(args)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8(out.stdout).unwrap()
    }

    fn write(&self, path: &str, contents: impl AsRef<[u8]>) {
        let full = self.root.join(path);
        std::fs::create_dir_all(full.parent().unwrap()).unwrap();
        std::fs::write(full, contents).unwrap();
    }

    fn read(&self, path: &str) -> String {
        std::fs::read_to_string(self.root.join(path)).unwrap()
    }

    fn commit_all(&self) {
        self.git(&["add", "-A"]);
        self.git(&["commit", "-q", "-m", "base"]);
    }

    fn ops_git(&self) -> Git {
        Git::new(&self.root)
            .with_env("GIT_CONFIG_GLOBAL", "/dev/null")
            .with_env("GIT_CONFIG_NOSYSTEM", "1")
    }

    /// The change and its diff for `path` in `section`.
    fn change(&self, section: Section, path: &str) -> (FileChange, Option<Arc<FileDiff>>) {
        let repo = Repo::discover(&self.root).unwrap();
        let change = repo
            .status()
            .unwrap()
            .into_iter()
            .find(|c| c.section == section && c.path == path)
            .unwrap_or_else(|| panic!("no {section:?} change for {path}"));
        let thread = repo.thread_local();
        let diff = match thread.loader().unwrap().load(&change).unwrap() {
            Loaded::Text(diff) => Some(diff),
            _ => None,
        };
        (change, diff)
    }
}

fn lines(n: usize) -> String {
    (1..=n).map(|i| format!("line {i}\n")).collect()
}

/// Changed lines of hunk `hunk`.
fn hunk_lines(diff: &FileDiff, hunk: usize) -> Vec<usize> {
    diff.hunks[hunk]
        .lines
        .clone()
        .filter(|&ix| diff.lines[ix].kind != LineKind::Context)
        .collect()
}

fn two_hunk_fixture() -> Fixture {
    let fx = Fixture::new();
    fx.write("f.txt", lines(40));
    fx.commit_all();
    fx.write(
        "f.txt",
        lines(40)
            .replace("line 3\n", "line three\n")
            .replace("line 30\n", "line thirty\n"),
    );
    fx
}

#[test]
fn stage_a_hunk_then_undo() {
    let fx = two_hunk_fixture();
    let (change, diff) = fx.change(Section::Unstaged, "f.txt");
    let diff = diff.unwrap();
    let undo = ops::stage(
        &fx.ops_git(),
        &change,
        Some(&diff),
        Selection::Lines(&hunk_lines(&diff, 0)),
    )
    .unwrap();
    let staged = fx.git(&["diff", "--cached"]);
    assert!(staged.contains("+line three"), "{staged}");
    assert!(!staged.contains("thirty"), "{staged}");
    assert!(fx.git(&["diff"]).contains("+line thirty"));

    ops::undo(&fx.ops_git(), &undo).unwrap();
    assert_eq!(fx.git(&["diff", "--cached"]), "");
    assert!(
        fx.read("f.txt").contains("line three"),
        "worktree untouched"
    );
}

#[test]
fn unstage_selected_lines() {
    let fx = two_hunk_fixture();
    fx.git(&["add", "f.txt"]);
    let (change, diff) = fx.change(Section::Staged, "f.txt");
    let diff = diff.unwrap();
    ops::unstage(
        &fx.ops_git(),
        &change,
        Some(&diff),
        Selection::Lines(&hunk_lines(&diff, 1)),
    )
    .unwrap();
    let staged = fx.git(&["diff", "--cached"]);
    assert!(staged.contains("+line three"), "{staged}");
    assert!(!staged.contains("thirty"), "{staged}");
}

#[test]
fn discard_a_hunk_keeps_it_recoverable() {
    let fx = two_hunk_fixture();
    let before = fx.read("f.txt");
    let (change, diff) = fx.change(Section::Unstaged, "f.txt");
    let diff = diff.unwrap();
    let undo = ops::discard(
        &fx.ops_git(),
        &change,
        Some(&diff),
        Selection::Lines(&hunk_lines(&diff, 1)),
    )
    .unwrap();
    let after = fx.read("f.txt");
    assert!(
        after.contains("line three") && !after.contains("thirty"),
        "{after}"
    );
    // The discarded content is kept reachable under Goro's private ref.
    let saved = fx.git(&["log", "--format=%s", "refs/goro/undo"]);
    assert!(saved.contains("goro: discard"), "{saved}");

    ops::undo(&fx.ops_git(), &undo).unwrap();
    assert_eq!(fx.read("f.txt"), before);
}

#[test]
fn discard_refuses_when_the_file_changed_since_review() {
    let fx = two_hunk_fixture();
    let (change, diff) = fx.change(Section::Unstaged, "f.txt");
    let diff = diff.unwrap();
    // An agent edits the file after Goro loaded it.
    let edited = fx.read("f.txt").replace("line 20\n", "line twenty\n");
    fx.write("f.txt", &edited);
    let err = ops::discard(
        &fx.ops_git(),
        &change,
        Some(&diff),
        Selection::Lines(&hunk_lines(&diff, 0)),
    )
    .unwrap_err();
    assert!(matches!(err, OpError::Stale(_)), "{err}");
    assert_eq!(fx.read("f.txt"), edited, "file untouched");
}

#[test]
fn undo_refuses_when_the_file_changed_after_the_discard() {
    let fx = two_hunk_fixture();
    let (change, diff) = fx.change(Section::Unstaged, "f.txt");
    let undo = ops::discard(&fx.ops_git(), &change, diff.as_deref(), Selection::File).unwrap();
    assert_eq!(fx.read("f.txt"), lines(40));
    fx.write("f.txt", "agent wrote this\n");
    let err = ops::undo(&fx.ops_git(), &undo).unwrap_err();
    assert!(matches!(err, OpError::Stale(_)), "{err}");
    assert_eq!(fx.read("f.txt"), "agent wrote this\n");
}

#[test]
fn untracked_files_stage_partially_and_discard_with_undo() {
    let fx = Fixture::new();
    fx.write("base.txt", "base\n");
    fx.commit_all();
    fx.write("new.txt", "one\ntwo\nthree\n");
    let (change, diff) = fx.change(Section::Untracked, "new.txt");
    let diff = diff.unwrap();
    let second = diff
        .lines
        .iter()
        .position(|l| diff.line_bytes(l) == b"two")
        .unwrap();
    ops::stage(
        &fx.ops_git(),
        &change,
        Some(&diff),
        Selection::Lines(&[second]),
    )
    .unwrap();
    assert_eq!(fx.git(&["show", ":new.txt"]), "two\n");

    fx.git(&["rm", "-q", "-f", "--cached", "new.txt"]);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(
            fx.root.join("new.txt"),
            std::fs::Permissions::from_mode(0o755),
        )
        .unwrap();
    }
    let (change, diff) = fx.change(Section::Untracked, "new.txt");
    let undo = ops::discard(&fx.ops_git(), &change, diff.as_deref(), Selection::File).unwrap();
    assert!(!fx.root.join("new.txt").exists());
    ops::undo(&fx.ops_git(), &undo).unwrap();
    assert_eq!(fx.read("new.txt"), "one\ntwo\nthree\n");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(fx.root.join("new.txt"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o111, 0o111, "executable bit restored");
    }
}

#[test]
fn selecting_every_line_of_a_deleted_file_stages_the_deletion() {
    let fx = Fixture::new();
    fx.write("gone.txt", "a\nb\n");
    fx.commit_all();
    std::fs::remove_file(fx.root.join("gone.txt")).unwrap();
    let (change, diff) = fx.change(Section::Unstaged, "gone.txt");
    let diff = diff.unwrap();
    let all: Vec<usize> = (0..diff.lines.len()).collect();
    ops::stage(&fx.ops_git(), &change, Some(&diff), Selection::Lines(&all)).unwrap();
    assert_eq!(fx.git(&["status", "--porcelain"]), "D  gone.txt\n");
}

#[test]
fn unstage_a_file_before_the_first_commit() {
    let fx = Fixture::new();
    fx.write("first.txt", "hello\n");
    fx.git(&["add", "first.txt"]);
    let (change, diff) = fx.change(Section::Staged, "first.txt");
    let undo = ops::unstage(&fx.ops_git(), &change, diff.as_deref(), Selection::File).unwrap();
    assert_eq!(fx.git(&["status", "--porcelain"]), "?? first.txt\n");
    ops::undo(&fx.ops_git(), &undo).unwrap();
    assert_eq!(fx.git(&["status", "--porcelain"]), "A  first.txt\n");
}

#[test]
fn staged_changes_cannot_be_discarded() {
    let fx = two_hunk_fixture();
    fx.git(&["add", "f.txt"]);
    let (change, diff) = fx.change(Section::Staged, "f.txt");
    let err = ops::discard(&fx.ops_git(), &change, diff.as_deref(), Selection::File).unwrap_err();
    assert!(matches!(err, OpError::Unsupported(_)), "{err}");
}

#[test]
fn commit_amend_and_signoff() {
    let fx = two_hunk_fixture();
    fx.git(&["add", "f.txt"]);
    let summary = ops::commit(
        &fx.ops_git(),
        "Change two lines\n\nBody text.",
        CommitOptions::default(),
    )
    .unwrap();
    assert!(summary.contains("Change two lines"), "{summary}");
    assert_eq!(
        fx.git(&["log", "-1", "--format=%B"]).trim(),
        "Change two lines\n\nBody text."
    );
    assert_eq!(
        ops::last_commit_message(&fx.ops_git()).unwrap().trim(),
        "Change two lines\n\nBody text."
    );

    ops::commit(
        &fx.ops_git(),
        "Reworded",
        CommitOptions {
            amend: true,
            signoff: true,
        },
    )
    .unwrap();
    let message = fx.git(&["log", "-1", "--format=%B"]);
    assert!(message.starts_with("Reworded"), "{message}");
    assert!(message.contains("Signed-off-by: Goro Test"), "{message}");
    assert_eq!(fx.git(&["rev-list", "--count", "HEAD"]).trim(), "2");
}

#[test]
fn commit_hook_failure_is_reported() {
    let fx = two_hunk_fixture();
    fx.git(&["add", "f.txt"]);
    let hook = fx.root.join(".git/hooks/pre-commit");
    std::fs::write(
        &hook,
        "#!/bin/sh\necho 'lint failed: trailing whitespace' >&2\nexit 1\n",
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    let err = ops::commit(&fx.ops_git(), "msg", CommitOptions::default()).unwrap_err();
    assert!(
        err.to_string().contains("lint failed: trailing whitespace"),
        "{err}"
    );
}

#[test]
fn empty_commit_message_is_rejected_before_running_git() {
    let fx = two_hunk_fixture();
    fx.git(&["add", "f.txt"]);
    let err = ops::commit(&fx.ops_git(), "  \n ", CommitOptions::default()).unwrap_err();
    assert!(matches!(err, OpError::Unsupported(_)), "{err}");
}
