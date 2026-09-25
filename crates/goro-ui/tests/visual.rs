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
use goro_ui::{Event, GoroView, NextFile, NextHunk, Startup};
use gpui_kit::{
    AppContext, Focusable, HeadlessAppContext, WindowAppearance, WindowHandle, px, size,
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

fn open(
    appearance: WindowAppearance,
) -> (
    HeadlessAppContext,
    WindowHandle<GoroView>,
    tempfile::TempDir,
) {
    let (dir, root) = fixture();
    let repo = Repo::discover(&root).unwrap();
    let changes = repo.status().unwrap();
    let thread = repo.thread_local();
    let (tx, rx) = unbounded();
    tx.unbounded_send(Event::Opened {
        root: repo.root().to_path_buf(),
        changes: changes.clone(),
        first: Some(load_file(&thread, &changes[0])),
    })
    .unwrap();
    for (ix, change) in changes.iter().enumerate().skip(1) {
        tx.unbounded_send(Event::Loaded(ix, load_file(&thread, change)))
            .unwrap();
    }
    drop(tx);

    let platform = gpui_kit::platform::current_platform(true);
    let mut cx = HeadlessAppContext::with_platform(
        platform.text_system(),
        Arc::new(()),
        gpui_kit::platform::current_headless_renderer,
    );
    let startup = Startup {
        t0: Instant::now(),
        trace: false,
        bench_exit: false,
    };
    let window = cx
        .open_window(size(px(1100.0), px(640.0)), |window, cx| {
            cx.new(|cx| {
                let mut view = GoroView::new(startup, Vec::new(), rx, window, cx);
                view.set_appearance(Some(appearance), cx);
                view
            })
        })
        .unwrap();
    cx.run_until_parked();
    (cx, window, dir)
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

fn main() {
    let tests: [(&str, fn()); 3] = [
        (
            "renders_every_file_in_one_stream",
            renders_every_file_in_one_stream,
        ),
        ("renders_in_light_mode", renders_in_light_mode),
        (
            "keyboard_moves_between_hunks_and_files",
            keyboard_moves_between_hunks_and_files,
        ),
    ];
    for (name, test) in tests {
        test();
        println!("test {name} ... ok");
    }
}
