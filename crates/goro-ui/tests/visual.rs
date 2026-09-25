//! Renders the real Goro window headlessly against a fixture repository.
//!
//! Set `GORO_SCREENSHOT_DIR` to also save PNG screenshots (macOS only; other platforms
//! have no headless GPU renderer yet).
//!
//! Runs without the libtest harness because the macOS platform must be created on the
//! main thread.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::time::Instant;

use futures::channel::mpsc::unbounded;
use goro_core::repo::Repo;
use goro_core::review::{Row, load_file};
use goro_core::store::Store;
use goro_ui::{Event, GoroView, NextFile, NextHunk, Startup};
use gpui_kit::{
    AppContext, Focusable, HeadlessAppContext, Keystroke, WindowAppearance, WindowHandle, px, size,
};

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

fn write(root: &Path, path: &str, contents: impl AsRef<[u8]>) {
    let full = root.join(path);
    std::fs::create_dir_all(full.parent().unwrap()).unwrap();
    std::fs::write(full, contents).unwrap();
}

const RUST_BEFORE: &str = r#"use std::collections::HashMap;

/// Counts words in a text.
pub fn count_words(text: &str) -> HashMap<String, usize> {
    let mut counts = HashMap::new();
    for word in text.split_whitespace() {
	*counts.entry(word.to_string()).or_insert(0) += 1;
    }
    counts
}

pub fn longest(words: &[&str]) -> Option<&str> {
    words.iter().copied().max_by_key(|w| w.len())
}
"#;

const RUST_AFTER: &str = r#"use std::collections::HashMap;

/// Counts words in a text, case-insensitively.
pub fn count_words(text: &str) -> HashMap<String, usize> {
    let mut counts = HashMap::new();
    for word in text.split_whitespace() {
	*counts.entry(word.to_lowercase()).or_insert(0) += 1;
    }
    counts
}

pub fn longest(words: &[&str]) -> Option<&str> {
    // Ties go to the first word.
    words.iter().copied().rev().max_by_key(|w| w.len())
}
"#;

fn fixture() -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    git(&root, &["init", "-q", "-b", "main"]);
    git(&root, &["config", "user.name", "Goro Test"]);
    git(&root, &["config", "user.email", "goro@example.invalid"]);
    write(&root, "src/words.rs", RUST_BEFORE);
    write(&root, "README.md", "# Words\n\nCounts words.\n");
    write(&root, "old.txt", "remove me\n");
    write(&root, "logo.bin", b"\x89PNG\0\x01".as_slice());
    git(&root, &["add", "-A"]);
    git(&root, &["commit", "-q", "-m", "initial"]);

    write(
        &root,
        "README.md",
        "# Words\n\nCounts words, ignoring case.\n",
    );
    git(&root, &["add", "README.md"]);
    write(&root, "src/words.rs", RUST_AFTER);
    std::fs::remove_file(root.join("old.txt")).unwrap();
    write(&root, "logo.bin", b"\x89PNG\0\x02\x03".as_slice());
    write(
        &root,
        "web/app.ts",
        "export function greet(name: string): string {\n  return `Hello, ${name}!`;\n}\n",
    );
    (dir, root)
}

/// Temporary directories that must outlive the window.
struct Dirs {
    _repo: tempfile::TempDir,
    store: tempfile::TempDir,
}

fn open(appearance: WindowAppearance) -> (HeadlessAppContext, WindowHandle<GoroView>, Dirs) {
    let (repo, root) = fixture();
    let store = tempfile::tempdir().unwrap();
    let (cx, window) = open_repo(&root, appearance, Store::at(store.path()));
    (cx, window, Dirs { _repo: repo, store })
}

fn open_repo(
    root: &Path,
    appearance: WindowAppearance,
    store: Store,
) -> (HeadlessAppContext, WindowHandle<GoroView>) {
    let repo = Repo::discover(root).unwrap();
    let changes = repo.status().unwrap();
    let thread = repo.thread_local();
    let (tx, rx) = unbounded();
    let loads: Vec<_> = changes.iter().map(|c| load_file(&thread, c)).collect();
    drop(thread);
    let mut loads = loads.into_iter();
    tx.unbounded_send(Event::Opened {
        state: store.repo_state(repo.root()),
        repo: Arc::new(repo),
        changes: changes.clone(),
        first: loads.next(),
    })
    .unwrap();
    for (ix, load) in loads.enumerate() {
        tx.unbounded_send(Event::Loaded(ix + 1, load)).unwrap();
    }
    drop(tx);

    let platform = gpui_kit::platform::current_platform(true);
    let mut cx = HeadlessAppContext::with_platform(
        platform.text_system(),
        Arc::new(()),
        gpui_kit::platform::current_headless_renderer,
    );
    cx.update(goro_ui::bind_keys);
    let startup = Startup {
        t0: Instant::now(),
        trace: false,
        bench_exit: false,
    };
    let window = cx
        .open_window(size(px(1100.0), px(640.0)), |window, cx| {
            cx.new(|cx| {
                let mut view = GoroView::new(startup, Vec::new(), rx, Some(store), window, cx);
                view.set_appearance(Some(appearance), cx);
                view
            })
        })
        .unwrap();
    cx.run_until_parked();
    (cx, window)
}

/// Let real threads (the file watcher, git) and the app's tasks run until `condition`
/// holds, or panic after `timeout`.
fn wait_until(
    cx: &mut HeadlessAppContext,
    window: WindowHandle<GoroView>,
    timeout: std::time::Duration,
    what: &str,
    condition: impl Fn(&GoroView, &gpui_kit::App) -> bool,
) {
    let start = Instant::now();
    loop {
        cx.run_until_parked();
        if read(cx, window, &condition) {
            return;
        }
        assert!(start.elapsed() < timeout, "timed out waiting for {what}");
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
}

fn save_screenshot(cx: &mut HeadlessAppContext, window: WindowHandle<GoroView>, name: &str) {
    let Some(dir) = std::env::var_os("GORO_SCREENSHOT_DIR") else {
        return;
    };
    if gpui_kit::platform::current_headless_renderer().is_none() {
        return;
    }
    let image = cx.capture_screenshot(window.into()).unwrap();
    image.save(Path::new(&dir).join(name)).unwrap();
}

fn renders_every_file_in_one_stream() {
    let (mut cx, window, _dir) = open(WindowAppearance::Dark);
    let paths: Vec<String> = cx
        .update(|cx| {
            window.read_with(cx, |view, _| {
                let review = view.review().expect("review loaded");
                review
                    .rows()
                    .iter()
                    .filter_map(|row| match row {
                        Row::File { file } => {
                            Some(review.files[*file].change.path_lossy().into_owned())
                        }
                        _ => None,
                    })
                    .collect()
            })
        })
        .unwrap();
    assert_eq!(
        paths,
        [
            "README.md",
            "logo.bin",
            "old.txt",
            "src/words.rs",
            "web/app.ts"
        ]
    );
    save_screenshot(&mut cx, window, "review-dark.png");
}

fn renders_in_light_mode() {
    let (mut cx, window, _dir) = open(WindowAppearance::Light);
    save_screenshot(&mut cx, window, "review-light.png");
}

fn keyboard_moves_between_hunks_and_files() {
    let (mut cx, window, _dir) = open(WindowAppearance::Dark);
    let row_at_cursor = |cx: &mut HeadlessAppContext| {
        cx.update(|cx| window.read_with(cx, |view, _| view.review().unwrap().rows()[view.cursor()]))
            .unwrap()
    };
    let dispatch = |cx: &mut HeadlessAppContext, action: Box<dyn gpui_kit::Action>| {
        cx.update_window(window.into(), |view, window, cx| {
            let view = view.downcast::<GoroView>().unwrap();
            window.focus(&view.focus_handle(cx), cx);
            window.dispatch_action(action, cx);
        })
        .unwrap();
        cx.run_until_parked();
    };

    dispatch(&mut cx, Box::new(NextHunk));
    assert!(matches!(
        row_at_cursor(&mut cx),
        Row::Hunk { file: 0, hunk: 0 }
    ));
    dispatch(&mut cx, Box::new(NextFile));
    assert!(matches!(row_at_cursor(&mut cx), Row::File { file: 1 }));
    dispatch(&mut cx, Box::new(NextHunk));
    // logo.bin and old.txt... the next hunk after file 1 is in old.txt (file 2).
    assert!(matches!(
        row_at_cursor(&mut cx),
        Row::Hunk { file: 2, hunk: 0 }
    ));
}

fn git_out(root: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .current_dir(root)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .args(args)
        .output()
        .unwrap();
    String::from_utf8(out.stdout).unwrap()
}

/// Press each space-separated key.
fn press(cx: &mut HeadlessAppContext, window: WindowHandle<GoroView>, keys: &str) {
    cx.update_window(window.into(), |view, window, cx| {
        let view = view.downcast::<GoroView>().unwrap();
        // Keep focus where the app put it (commit box, switcher); start on the diff.
        if window.focused(cx).is_none() {
            window.focus(&view.focus_handle(cx), cx);
        }
    })
    .unwrap();
    for key in keys.split(' ') {
        cx.update_window(window.into(), |_, window, cx| {
            window.dispatch_keystroke(Keystroke::parse(key).unwrap(), cx);
        })
        .unwrap();
        cx.run_until_parked();
    }
}

fn read<R>(
    cx: &mut HeadlessAppContext,
    window: WindowHandle<GoroView>,
    f: impl FnOnce(&GoroView, &gpui_kit::App) -> R,
) -> R {
    cx.update(|cx| window.read_with(cx, |view, cx| f(view, cx)))
        .unwrap()
}

fn root_of(cx: &mut HeadlessAppContext, window: WindowHandle<GoroView>) -> PathBuf {
    read(cx, window, |view, _| view.review().unwrap().root.clone())
}

fn stage_selected_lines_then_undo() {
    let (mut cx, window, _dir) = open(WindowAppearance::Dark);
    let root = root_of(&mut cx, window);
    // src/words.rs is the fourth file; its first changed lines are the doc comment.
    press(&mut cx, window, "] ] ] n j j j shift-j s");
    let staged = git_out(&root, &["diff", "--cached", "--", "src/words.rs"]);
    assert!(
        staged.contains("+/// Counts words in a text, case-insensitively."),
        "{staged}"
    );
    assert!(
        !staged.contains("to_lowercase"),
        "only the selected lines: {staged}"
    );
    let status = read(&mut cx, window, |v, _| {
        v.status_text().map(|(t, e)| (t.to_string(), e))
    });
    assert_eq!(
        status,
        Some((
            "Staged 2 lines of src/words.rs  (u to undo)".to_string(),
            false
        ))
    );
    let staged_files = read(&mut cx, window, |v, _| {
        v.review()
            .unwrap()
            .files
            .iter()
            .filter(|f| f.change.section == goro_core::repo::Section::Staged)
            .map(|f| f.change.path_lossy().into_owned())
            .collect::<Vec<_>>()
    });
    assert_eq!(
        staged_files,
        ["README.md", "src/words.rs"],
        "review reloaded"
    );
    save_screenshot(&mut cx, window, "review-staged.png");

    press(&mut cx, window, "u");
    assert_eq!(
        git_out(&root, &["diff", "--cached", "--", "src/words.rs"]),
        ""
    );
}

fn discard_file_then_undo() {
    let (mut cx, window, _dir) = open(WindowAppearance::Dark);
    let root = root_of(&mut cx, window);
    // old.txt (deleted in the worktree) is the third file.
    press(&mut cx, window, "] ] x");
    assert_eq!(
        std::fs::read_to_string(root.join("old.txt")).unwrap(),
        "remove me\n"
    );
    press(&mut cx, window, "u");
    assert!(!root.join("old.txt").exists(), "undo deletes it again");
}

fn commit_box_takes_typing_and_commits() {
    let (mut cx, window, _dir) = open(WindowAppearance::Dark);
    let root = root_of(&mut cx, window);
    let cursor_before = read(&mut cx, window, |v, _| v.cursor());
    press(&mut cx, window, "c");
    // Letters that are shortcuts in the diff must type into the editor.
    press(&mut cx, window, "shift-f i x space j s x");
    let text = read(&mut cx, window, |v, cx| {
        v.commit_editor().read(cx).text().to_string()
    });
    assert_eq!(text, "Fix jsx");
    assert_eq!(read(&mut cx, window, |v, _| v.cursor()), cursor_before);
    save_screenshot(&mut cx, window, "review-commit-box.png");

    press(&mut cx, window, "ctrl-enter");
    assert_eq!(
        git_out(&root, &["log", "-1", "--format=%s"]).trim(),
        "Fix jsx"
    );
    assert_eq!(git_out(&root, &["diff", "--cached", "--name-only"]), "");
    let text = read(&mut cx, window, |v, cx| {
        v.commit_editor().read(cx).text().to_string()
    });
    assert_eq!(text, "", "editor cleared after commit");
}

fn failing_hook_output_is_shown_in_full() {
    let (mut cx, window, _dir) = open(WindowAppearance::Dark);
    let root = root_of(&mut cx, window);
    let hook = root.join(".git/hooks/pre-commit");
    std::fs::create_dir_all(hook.parent().unwrap()).unwrap();
    std::fs::write(
        &hook,
        "#!/bin/sh\necho 'lint: README.md:3 trailing whitespace' >&2\necho 'lint: 1 problem' >&2\nexit 1\n",
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    press(&mut cx, window, "c");
    press(&mut cx, window, "w i p ctrl-enter");
    let (text, is_error) = read(&mut cx, window, |v, _| {
        v.status_text().map(|(t, e)| (t.to_string(), e)).unwrap()
    });
    assert!(is_error);
    assert!(
        text.contains("README.md:3 trailing whitespace") && text.contains("1 problem"),
        "{text}"
    );
    assert!(text.starts_with("Commit failed:\n"), "{text}");
    assert_eq!(
        git_out(&root, &["log", "--oneline"]).lines().count(),
        1,
        "nothing committed"
    );
    save_screenshot(&mut cx, window, "review-hook-failure.png");
    // The first escape leaves the commit box; the next dismisses the error.
    press(&mut cx, window, "escape");
    assert!(read(&mut cx, window, |v, _| v.status_text().is_some()));
    press(&mut cx, window, "escape");
    assert!(
        read(&mut cx, window, |v, _| v.status_text().is_none()),
        "esc dismisses"
    );
}

fn row_text(view: &GoroView, row: usize) -> String {
    let review = view.review().unwrap();
    match review.rows()[row] {
        Row::Line { file, line } => {
            let diff = review.files[file].diff().unwrap();
            String::from_utf8_lossy(diff.line_bytes(&diff.lines[line])).into_owned()
        }
        other => format!("{other:?}"),
    }
}

fn live_updates_mark_new_lines() {
    let (mut cx, window, _dirs) = open(WindowAppearance::Dark);
    let root = root_of(&mut cx, window);
    let new_count =
        |cx: &mut HeadlessAppContext| read(cx, window, |v, _| v.review().unwrap().new_count());
    assert_eq!(new_count(&mut cx), 0, "the first look is taken on open");

    // An agent edits a file while Goro is open.
    std::fs::write(
        root.join("web/app.ts"),
        "export function greet(name: string): string {\n  return `Hi, ${name}!`;\n}\n",
    )
    .unwrap();
    wait_until(
        &mut cx,
        window,
        std::time::Duration::from_secs(5),
        "the edit",
        |v, _| v.review().unwrap().new_count() > 0,
    );
    assert_eq!(new_count(&mut cx), 1, "only the changed line is new");
    press(&mut cx, window, "tab");
    let text = read(&mut cx, window, |v, _| row_text(v, v.cursor()));
    assert_eq!(text, "  return `Hi, ${name}!`;");
    save_screenshot(&mut cx, window, "review-new-lines.png");

    press(&mut cx, window, "m");
    assert_eq!(new_count(&mut cx), 0);
}

fn reviewed_marks_collapse_and_persist() {
    let (mut cx, window, dirs) = open(WindowAppearance::Dark);
    let root = root_of(&mut cx, window);
    let rows =
        |cx: &mut HeadlessAppContext| read(cx, window, |v, _| v.review().unwrap().rows().len());
    let before = rows(&mut cx);
    press(&mut cx, window, "n r");
    assert!(read(&mut cx, window, |v, _| v
        .review()
        .unwrap()
        .hunk_is_reviewed(0, 0)));
    assert!(rows(&mut cx) < before, "reviewed hunk collapses");
    save_screenshot(&mut cx, window, "review-reviewed.png");
    drop(cx);

    let (mut cx, window) = open_repo(&root, WindowAppearance::Dark, Store::at(dirs.store.path()));
    assert!(
        read(&mut cx, window, |v, _| v
            .review()
            .unwrap()
            .hunk_is_reviewed(0, 0)),
        "reviewed marks survive a restart"
    );
}

fn switcher_opens_and_dismisses() {
    let (mut cx, window, dirs) = open(WindowAppearance::Dark);
    let root = root_of(&mut cx, window);
    Store::at(dirs.store.path()).touch_recent(&root).unwrap();
    press(&mut cx, window, "ctrl-p");
    assert!(read(&mut cx, window, |v, _| v.picker_open()));
    // Typing goes to the switcher's filter, not to the diff's shortcuts.
    let cursor = read(&mut cx, window, |v, _| v.cursor());
    press(&mut cx, window, "j j");
    assert_eq!(read(&mut cx, window, |v, _| v.cursor()), cursor);
    press(&mut cx, window, "escape");
    assert!(!read(&mut cx, window, |v, _| v.picker_open()));
    press(&mut cx, window, "j");
    assert_eq!(
        read(&mut cx, window, |v, _| v.cursor()),
        cursor + 1,
        "focus back on the diff"
    );

    // Choosing the repository that's already open just returns to its window.
    press(&mut cx, window, "ctrl-p");
    save_screenshot(&mut cx, window, "review-switcher.png");
    press(&mut cx, window, "enter");
    assert!(!read(&mut cx, window, |v, _| v.picker_open()));
    assert_eq!(
        cx.update(|cx| cx.windows().len()),
        1,
        "no second window for the same repo"
    );
}

fn comments_are_added_edited_deleted_and_persisted() {
    let (mut cx, window, dirs) = open(WindowAppearance::Dark);
    let root = root_of(&mut cx, window);
    let comments = |cx: &mut HeadlessAppContext| {
        read(cx, window, |v, _| {
            v.review()
                .unwrap()
                .comments()
                .iter()
                .map(|c| (c.location(), c.text.clone()))
                .collect::<Vec<_>>()
        })
    };
    // src/words.rs: the doc comment's added line.
    press(&mut cx, window, "] ] ] n j j j j a");
    press(
        &mut cx,
        window,
        "shift-s a y space c a s e space i s space w r o n g ctrl-enter",
    );
    assert_eq!(
        comments(&mut cx),
        [(
            "src/words.rs:3".to_string(),
            "Say case is wrong".to_string()
        )]
    );
    press(&mut cx, window, "j");
    let on_comment = read(&mut cx, window, |v, _| {
        matches!(v.review().unwrap().rows()[v.cursor()], Row::Comment { .. })
    });
    assert!(on_comment, "the comment row follows its line");
    save_screenshot(&mut cx, window, "review-comment.png");

    // Edit it (appending), then check the markdown on the clipboard.
    press(&mut cx, window, "enter");
    press(&mut cx, window, "space x ctrl-enter");
    assert_eq!(comments(&mut cx)[0].1, "Say case is wrong x");
    press(&mut cx, window, "y");
    let clipboard = cx
        .update(|cx| cx.read_from_clipboard())
        .and_then(|item| item.text())
        .unwrap_or_default();
    assert!(
        clipboard.contains("## src/words.rs:3") && clipboard.contains("Say case is wrong x"),
        "{clipboard}"
    );

    // Comments survive reopening.
    drop(cx);
    let (mut cx, window) = open_repo(&root, WindowAppearance::Dark, Store::at(dirs.store.path()));
    assert_eq!(comments(&mut cx).len(), 1);
    // x on a comment row deletes the comment, not the code.
    let row = read(&mut cx, window, |v, _| {
        v.review()
            .unwrap()
            .rows()
            .iter()
            .position(|r| matches!(r, Row::Comment { .. }))
            .unwrap()
    });
    cx.update_window(window.into(), |view, _, cx| {
        view.downcast::<GoroView>()
            .unwrap()
            .update(cx, |v, cx| v.set_cursor(row, cx));
    })
    .unwrap();
    press(&mut cx, window, "x");
    assert!(comments(&mut cx).is_empty());
    assert!(root.join("src/words.rs").exists());
}

fn sending_a_review_answers_the_waiting_agent() {
    let (mut cx, window, _dirs) = open(WindowAppearance::Dark);
    let (tx, mut rx) = futures::channel::oneshot::channel();
    cx.update_window(window.into(), |view, _, cx| {
        view.downcast::<GoroView>()
            .unwrap()
            .update(cx, |v, cx| v.add_waiter(tx, cx));
    })
    .unwrap();
    press(&mut cx, window, "n j j a");
    press(&mut cx, window, "f i x ctrl-enter");
    save_screenshot(&mut cx, window, "review-waiting.png");
    press(&mut cx, window, "ctrl-shift-enter");
    let markdown = rx.try_recv().unwrap().expect("review sent");
    assert!(
        markdown.contains("# Review comments (1)") && markdown.contains("\nfix\n"),
        "{markdown}"
    );
    assert!(
        read(&mut cx, window, |v, _| v
            .review()
            .unwrap()
            .comments()
            .is_empty()),
        "sent comments are cleared"
    );
}

fn turns_are_reviewed_on_their_own() {
    use goro_core::git::Git;
    use goro_core::turns::{self, SnapshotMeta, TurnEvent};
    let (repo_dir, root) = fixture();
    let git = Git::new(&root);
    let meta = |event, at_ms| SnapshotMeta {
        agent: "claude".into(),
        event,
        session_id: "session-1".into(),
        turn_id: None,
        prompt: Some("Handle empty input".into()),
        at_ms,
    };
    // A turn that only touches web/app.ts, on top of the fixture's other changes.
    let now = SnapshotMeta::now_ms();
    turns::snapshot(&git, &meta(TurnEvent::Start, now - 60_000)).unwrap();
    std::fs::write(root.join("web/app.ts"), "export function greet(name: string): string {\n  return name ? `Hello, ${name}!` : \"Hello!\";\n}\n").unwrap();
    turns::snapshot(&git, &meta(TurnEvent::End, now - 30_000)).unwrap();

    let store = tempfile::tempdir().unwrap();
    let (mut cx, window) = open_repo(&root, WindowAppearance::Dark, Store::at(store.path()));
    wait_until(
        &mut cx,
        window,
        std::time::Duration::from_secs(5),
        "sessions",
        |v, _| v.session_count() == 1,
    );
    press(&mut cx, window, "<");
    wait_until(
        &mut cx,
        window,
        std::time::Duration::from_secs(5),
        "the turn's diff",
        |v, _| {
            v.review()
                .unwrap()
                .files
                .iter()
                .all(|f| f.change.section == goro_core::repo::Section::Snapshot)
                && !v.review().unwrap().files.is_empty()
        },
    );
    let files = read(&mut cx, window, |v, _| {
        v.review()
            .unwrap()
            .files
            .iter()
            .map(|f| f.change.path_lossy().into_owned())
            .collect::<Vec<_>>()
    });
    assert_eq!(files, ["web/app.ts"], "only what the turn changed");
    save_screenshot(&mut cx, window, "review-turn.png");

    press(&mut cx, window, "n s");
    let status = read(&mut cx, window, |v, _| {
        v.status_text().map(|(t, _)| t.to_string())
    });
    assert!(
        status.unwrap_or_default().contains("read-only"),
        "turn changes can't be staged"
    );

    press(&mut cx, window, "w");
    wait_until(
        &mut cx,
        window,
        std::time::Duration::from_secs(5),
        "the working tree",
        |v, _| v.review().unwrap().files.len() == 5,
    );
    drop(repo_dir);
}

/// A `size`×`size` PNG of one color (stored deflate blocks, so no compression library).
fn png(size: u32, rgb: [u8; 3]) -> Vec<u8> {
    fn crc32(bytes: &[u8]) -> u32 {
        let mut crc = !0u32;
        for &b in bytes {
            crc ^= b as u32;
            for _ in 0..8 {
                crc = if crc & 1 != 0 {
                    0xedb8_8320 ^ (crc >> 1)
                } else {
                    crc >> 1
                };
            }
        }
        !crc
    }
    fn chunk(out: &mut Vec<u8>, kind: &[u8; 4], data: &[u8]) {
        out.extend((data.len() as u32).to_be_bytes());
        let mut body = kind.to_vec();
        body.extend(data);
        out.extend(&body);
        out.extend(crc32(&body).to_be_bytes());
    }
    // Each scanline: filter type 0, then the pixels.
    let scanline: Vec<u8> = std::iter::once(0)
        .chain(std::iter::repeat_n(rgb, size as usize).flatten())
        .collect();
    let raw = scanline.repeat(size as usize);
    let mut zlib = vec![0x78, 0x01];
    for (ix, block) in raw.chunks(65_535).enumerate() {
        let last = (ix + 1) * 65_535 >= raw.len();
        zlib.push(last as u8);
        zlib.extend((block.len() as u16).to_le_bytes());
        zlib.extend((!(block.len() as u16)).to_le_bytes());
        zlib.extend(block);
    }
    let (mut a, mut b) = (1u32, 0u32);
    for &byte in &raw {
        a = (a + byte as u32) % 65_521;
        b = (b + a) % 65_521;
    }
    zlib.extend(((b << 16) | a).to_be_bytes());
    let mut out = b"\x89PNG\r\n\x1a\n".to_vec();
    let mut ihdr = size.to_be_bytes().to_vec();
    ihdr.extend(size.to_be_bytes());
    ihdr.extend([8, 2, 0, 0, 0]);
    chunk(&mut out, b"IHDR", &ihdr);
    chunk(&mut out, b"IDAT", &zlib);
    chunk(&mut out, b"IEND", &[]);
    out
}

fn split_view_and_image_diffs() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    git(&root, &["init", "-q"]);
    write(&root, "src/words.rs", RUST_BEFORE);
    write(&root, "icon.png", png(32, [200, 60, 60]));
    git(&root, &["add", "-A"]);
    git(&root, &["commit", "-q", "-m", "base"]);
    write(&root, "src/words.rs", RUST_AFTER);
    write(&root, "icon.png", png(32, [60, 120, 220]));
    let store = tempfile::tempdir().unwrap();
    let (mut cx, window) = open_repo(&root, WindowAppearance::Dark, Store::at(store.path()));
    let image_rows = read(&mut cx, window, |v, _| {
        v.review()
            .unwrap()
            .rows()
            .iter()
            .filter(|r| matches!(r, Row::Image { .. }))
            .count()
    });
    assert_eq!(image_rows, goro_core::review::IMAGE_ROWS);
    press(&mut cx, window, "v");
    let pairs = read(&mut cx, window, |v, _| {
        v.review()
            .unwrap()
            .rows()
            .iter()
            .filter(|r| matches!(r, Row::Pair { .. }))
            .count()
    });
    assert!(pairs > 0, "v switches to side by side");
    save_screenshot(&mut cx, window, "review-split-image.png");
    press(&mut cx, window, "v");
    let pairs = read(&mut cx, window, |v, _| {
        v.review()
            .unwrap()
            .rows()
            .iter()
            .filter(|r| matches!(r, Row::Pair { .. }))
            .count()
    });
    assert_eq!(pairs, 0);
}

fn settings_keybindings_apply() {
    let config = PathBuf::from(std::env::var_os("GORO_CONFIG_DIR").unwrap());
    std::fs::create_dir_all(&config).unwrap();
    std::fs::write(
        config.join("settings.json"),
        r#"{ "global_hotkey": null, "keybindings": { "ctrl-j": "goro::NextHunk", "n": "" } }"#,
    )
    .unwrap();
    let (mut cx, window, _dirs) = open(WindowAppearance::Dark);
    cx.update(goro_ui::init_settings);
    press(&mut cx, window, "n");
    assert_eq!(read(&mut cx, window, |v, _| v.cursor()), 0, "n was unbound");
    press(&mut cx, window, "ctrl-j");
    let row = read(&mut cx, window, |v, _| {
        v.review().unwrap().rows()[v.cursor()]
    });
    assert!(
        matches!(row, Row::Hunk { .. }),
        "ctrl-j runs NextHunk: {row:?}"
    );
}

fn main() {
    // Isolate every git process (including Goro's own) from the user's configuration.
    // SAFETY: no other threads exist yet.
    let isolated = tempfile::tempdir().unwrap();
    unsafe {
        std::env::set_var("GIT_CONFIG_GLOBAL", "/dev/null");
        std::env::set_var("GIT_CONFIG_NOSYSTEM", "1");
        // Never read the user's agent logs or Goro state.
        std::env::set_var("CLAUDE_CONFIG_DIR", isolated.path().join("claude"));
        std::env::set_var("CODEX_HOME", isolated.path().join("codex"));
        std::env::set_var("GORO_DATA_DIR", isolated.path().join("goro"));
        std::env::set_var("GORO_CONFIG_DIR", isolated.path().join("config"));
    }
    let tests: [(&str, fn()); 15] = [
        (
            "renders_every_file_in_one_stream",
            renders_every_file_in_one_stream,
        ),
        ("renders_in_light_mode", renders_in_light_mode),
        (
            "keyboard_moves_between_hunks_and_files",
            keyboard_moves_between_hunks_and_files,
        ),
        (
            "stage_selected_lines_then_undo",
            stage_selected_lines_then_undo,
        ),
        ("discard_file_then_undo", discard_file_then_undo),
        (
            "commit_box_takes_typing_and_commits",
            commit_box_takes_typing_and_commits,
        ),
        (
            "failing_hook_output_is_shown_in_full",
            failing_hook_output_is_shown_in_full,
        ),
        ("live_updates_mark_new_lines", live_updates_mark_new_lines),
        (
            "reviewed_marks_collapse_and_persist",
            reviewed_marks_collapse_and_persist,
        ),
        ("switcher_opens_and_dismisses", switcher_opens_and_dismisses),
        (
            "comments_are_added_edited_deleted_and_persisted",
            comments_are_added_edited_deleted_and_persisted,
        ),
        (
            "sending_a_review_answers_the_waiting_agent",
            sending_a_review_answers_the_waiting_agent,
        ),
        (
            "turns_are_reviewed_on_their_own",
            turns_are_reviewed_on_their_own,
        ),
        ("split_view_and_image_diffs", split_view_and_image_diffs),
        ("settings_keybindings_apply", settings_keybindings_apply),
    ];
    for (name, test) in tests {
        test();
        println!("test {name} ... ok");
    }
}
