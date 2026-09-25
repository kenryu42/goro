//! Turn snapshots: taken through a temporary index (the user's index is untouched),
//! stored under `refs/goro/turns/`, paired into turns, and pruned.

use std::path::PathBuf;
use std::process::Command;

use goro_core::git::Git;
use goro_core::turns::{self, SnapshotMeta, TurnEvent};

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
        fx.git(&["config", "user.name", "Goro Test"]);
        fx.git(&["config", "user.email", "goro@example.invalid"]);
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

    fn write(&self, path: &str, contents: &str) {
        let full = self.root.join(path);
        std::fs::create_dir_all(full.parent().unwrap()).unwrap();
        std::fs::write(full, contents).unwrap();
    }

    fn ops_git(&self) -> Git {
        Git::new(&self.root)
            .with_env("GIT_CONFIG_GLOBAL", "/dev/null")
            .with_env("GIT_CONFIG_NOSYSTEM", "1")
    }
}

fn meta(event: TurnEvent, session: &str, turn: Option<&str>, at_ms: u64) -> SnapshotMeta {
    SnapshotMeta {
        agent: "claude".into(),
        event,
        session_id: session.into(),
        turn_id: turn.map(Into::into),
        prompt: (event == TurnEvent::Start).then(|| format!("prompt at {at_ms}")),
        at_ms,
    }
}

#[test]
fn snapshots_capture_the_worktree_without_touching_the_index() {
    let fx = Fixture::new();
    fx.write("a.txt", "one\n");
    fx.write(".gitignore", "*.log\n");
    fx.git(&["add", "-A"]);
    fx.git(&["commit", "-q", "-m", "base"]);
    fx.write("a.txt", "two\n");
    fx.write("new/untracked.txt", "fresh\n");
    fx.write("debug.log", "ignored\n");
    fx.git(&["add", "a.txt"]);
    fx.write("a.txt", "three\n");
    let index_before = std::fs::read(fx.root.join(".git/index")).unwrap();

    let snapshot =
        turns::snapshot(&fx.ops_git(), &meta(TurnEvent::Start, "s1", None, 1_000)).unwrap();

    assert_eq!(
        std::fs::read(fx.root.join(".git/index")).unwrap(),
        index_before,
        "index untouched"
    );
    let files = fx.git(&["ls-tree", "-r", "--name-only", &snapshot.tree]);
    assert_eq!(
        files.lines().collect::<Vec<_>>(),
        [".gitignore", "a.txt", "new/untracked.txt"]
    );
    assert_eq!(
        fx.git(&["show", &format!("{}:a.txt", snapshot.tree)]),
        "three\n",
        "worktree, not index"
    );
    let refs = fx.git(&["for-each-ref", "--format=%(refname)", "refs/goro/turns"]);
    assert_eq!(refs.trim(), "refs/goro/turns/s1/1000-start");
}

#[test]
fn snapshots_pair_into_turns_per_session() {
    let fx = Fixture::new();
    fx.write("a.txt", "one\n");
    fx.git(&["add", "-A"]);
    fx.git(&["commit", "-q", "-m", "base"]);
    let git = fx.ops_git();
    turns::snapshot(&git, &meta(TurnEvent::Start, "s1", None, 1_000)).unwrap();
    fx.write("a.txt", "two\n");
    turns::snapshot(&git, &meta(TurnEvent::End, "s1", None, 2_000)).unwrap();
    turns::snapshot(&git, &meta(TurnEvent::Start, "s1", None, 3_000)).unwrap();
    fx.write("a.txt", "three\n");
    // A second session interleaves; Codex-style turn ids pair explicitly.
    turns::snapshot(&git, &meta(TurnEvent::Start, "codex-9", Some("t1"), 3_500)).unwrap();
    turns::snapshot(&git, &meta(TurnEvent::End, "codex-9", Some("t1"), 3_600)).unwrap();

    let sessions = turns::list(&git).unwrap();
    assert_eq!(sessions.len(), 2);
    // Most recent activity first.
    assert_eq!(sessions[0].id, "codex-9");
    let s1 = &sessions[1];
    assert_eq!(s1.id, "s1");
    assert_eq!(s1.turns.len(), 2);
    assert_eq!(s1.turns[0].prompt.as_deref(), Some("prompt at 1000"));
    assert!(s1.turns[0].start.is_some() && s1.turns[0].end.is_some());
    assert!(
        s1.turns[1].end.is_none(),
        "the second turn is still running"
    );
    let first = &s1.turns[0];
    let diff = fx.git(&[
        "diff",
        &first.start.as_ref().unwrap().tree,
        &first.end.as_ref().unwrap().tree,
    ]);
    assert!(diff.contains("-one") && diff.contains("+two"), "{diff}");
}

#[test]
fn pruning_keeps_the_newest_snapshots() {
    let fx = Fixture::new();
    fx.write("a.txt", "one\n");
    fx.git(&["add", "-A"]);
    fx.git(&["commit", "-q", "-m", "base"]);
    let git = fx.ops_git();
    for at in 1..=5 {
        turns::snapshot(&git, &meta(TurnEvent::Start, "s", None, at * 1_000)).unwrap();
    }
    turns::prune(&git, 3).unwrap();
    let refs = fx.git(&["for-each-ref", "--format=%(refname)", "refs/goro/turns"]);
    assert_eq!(
        refs.lines().collect::<Vec<_>>(),
        [
            "refs/goro/turns/s/3000-start",
            "refs/goro/turns/s/4000-start",
            "refs/goro/turns/s/5000-start"
        ]
    );
}

#[test]
fn session_ids_are_made_safe_for_ref_names() {
    let fx = Fixture::new();
    fx.write("a.txt", "one\n");
    fx.git(&["add", "-A"]);
    fx.git(&["commit", "-q", "-m", "base"]);
    let git = fx.ops_git();
    turns::snapshot(&git, &meta(TurnEvent::Start, "../weird id:*?", None, 1)).unwrap();
    let sessions = turns::list(&git).unwrap();
    assert_eq!(
        sessions[0].id, "../weird id:*?",
        "the original id survives in metadata"
    );
}

#[test]
fn changes_between_snapshots_match_git_diff() {
    use goro_core::repo::{ChangeStatus, Loaded, Repo, Section};
    let fx = Fixture::new();
    fx.write("keep.txt", "same\n");
    fx.write("edit.txt", "one\ntwo\nthree\n");
    fx.write("gone.txt", "bye\n");
    fx.write(
        "move-me.txt",
        &"a long enough line to be recognized as a rename\n".repeat(5),
    );
    fx.git(&["add", "-A"]);
    fx.git(&["commit", "-q", "-m", "base"]);
    let git = fx.ops_git();
    let before = turns::snapshot(&git, &meta(TurnEvent::Start, "s", None, 1)).unwrap();
    fx.write("edit.txt", "one\nTWO\nthree\n");
    std::fs::remove_file(fx.root.join("gone.txt")).unwrap();
    fx.write("new.txt", "hello\n");
    std::fs::rename(fx.root.join("move-me.txt"), fx.root.join("moved.txt")).unwrap();
    let after = turns::snapshot(&git, &meta(TurnEvent::End, "s", None, 2)).unwrap();

    let changes = turns::changes_between(&git, &before.tree, &after.tree).unwrap();
    let summary: Vec<(ChangeStatus, String, Option<String>)> = changes
        .iter()
        .map(|c| {
            (
                c.status,
                c.path.to_string(),
                c.old_path.as_ref().map(|p| p.to_string()),
            )
        })
        .collect();
    assert_eq!(
        summary,
        [
            (ChangeStatus::Modified, "edit.txt".into(), None),
            (ChangeStatus::Deleted, "gone.txt".into(), None),
            (
                ChangeStatus::Renamed,
                "moved.txt".into(),
                Some("move-me.txt".into())
            ),
            (ChangeStatus::Added, "new.txt".into(), None),
        ]
    );
    assert!(changes.iter().all(|c| c.section == Section::Snapshot));

    let repo = Repo::discover(&fx.root).unwrap();
    let thread = repo.thread_local();
    let mut loader = thread.loader().unwrap();
    for change in &changes {
        let Loaded::Text(diff) = loader.load(change).unwrap() else {
            panic!("text expected for {change:?}");
        };
        let mut args = vec![
            "diff",
            "-M",
            before.tree.as_str(),
            after.tree.as_str(),
            "--",
        ];
        let old = change.old_path.as_ref().map(|p| p.to_string());
        if let Some(old) = &old {
            args.push(old);
        }
        let path = change.path.to_string();
        args.push(&path);
        let expected = fx.git(&args);
        let expected = expected
            .find("\n@@")
            .map(|pos| expected[pos + 1..].to_string())
            .unwrap_or_default();
        assert_eq!(
            String::from_utf8_lossy(&diff.to_unified_hunks()),
            expected,
            "{path}"
        );
    }
}
