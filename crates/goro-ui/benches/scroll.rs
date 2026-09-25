//! Frame-time benchmark: jump hunk by hunk through a 1,000-file, 50k-changed-line review
//! and time each frame's layout, text shaping, and scene building (CPU side; GPU
//! submission isn't included because the headless window has no renderer attached).
//!
//! Run with `cargo bench -p goro-ui --bench scroll`. Exits non-zero if p99 exceeds the
//! 16 ms frame budget.

use std::sync::Arc;
use std::time::{Duration, Instant};

use futures::channel::mpsc::unbounded;
use goro_core::diff::{DEFAULT_CONTEXT, FileDiff};
use goro_core::repo::{ChangeStatus, FileChange, Loaded, Section, Source};
use goro_core::review::{Content, FileLoad, highlight_diff};
use goro_core::syntax::Language;
use goro_ui::{Event, GoroView, NextHunk, Startup};
use gpui_kit::{AppContext, Focusable, HeadlessAppContext, px, size};

const FILES: usize = 1_000;
const LINES_PER_FILE: usize = 200;
const CHANGED_PER_FILE: usize = 50;
const FRAMES: usize = 2_000;
const BUDGET: Duration = Duration::from_millis(16);

fn synthetic_file(seed: usize) -> (String, String) {
    let mut old = String::new();
    let mut new = String::new();
    for i in 0..LINES_PER_FILE {
        let line = match i % 7 {
            0 => format!("pub fn handler_{seed}_{i}(input: &str) -> Result<usize, Error> {{\n"),
            1 => format!(
                "    let value = input.parse::<usize>().map_err(|e| Error::Parse(e))?; // {i}\n"
            ),
            2 => format!("    if value > {i} {{ return Ok(value * {seed}); }}\n"),
            3 => "    let message = \"a string literal with some words in it\";\n".to_string(),
            4 => format!("    tracing::debug!(target: \"goro\", value, \"step {i}\");\n"),
            5 => "    Ok(value)\n".to_string(),
            _ => "}\n".to_string(),
        };
        old.push_str(&line);
        // Change CHANGED_PER_FILE lines in five groups of ten, spread over the file.
        if (i / 10) % 4 == 1 {
            new.push_str(&line.replacen('\n', " // changed\n", 1));
        } else {
            new.push_str(&line);
        }
    }
    (old, new)
}

fn main() {
    let t = Instant::now();
    let (tx, rx) = unbounded();
    let changes: Vec<FileChange> = (0..FILES)
        .map(|i| FileChange {
            section: Section::Unstaged,
            status: ChangeStatus::Modified,
            path: format!("src/module_{:02}/file_{i:04}.rs", i / 50).into(),
            old_path: None,
            old: Source::Absent,
            new: Source::Worktree,
        })
        .collect();
    // The review is synthetic; the repository only provides a root for the view.
    let dir = tempfile::tempdir().unwrap();
    let status = std::process::Command::new("git")
        .args(["init", "-q"])
        .current_dir(dir.path())
        .status()
        .unwrap();
    assert!(status.success());
    let repo = std::sync::Arc::new(goro_core::repo::Repo::discover(dir.path()).unwrap());
    tx.unbounded_send(Event::Opened {
        repo,
        changes,
        first: None,
    })
    .unwrap();
    let mut changed_lines = 0;
    for i in 0..FILES {
        let (old, new) = synthetic_file(i);
        let diff = FileDiff::compute(old.into_bytes(), new.into_bytes(), DEFAULT_CONTEXT);
        changed_lines += diff
            .lines
            .iter()
            .filter(|l| l.kind == goro_core::diff::LineKind::Added)
            .count();
        let highlights = highlight_diff(Language::Rust, &diff);
        tx.unbounded_send(Event::Loaded(
            i,
            FileLoad {
                content: Content::Loaded(Loaded::Text(Arc::new(diff))),
                highlights: Some(Arc::new(highlights)),
            },
        ))
        .unwrap();
    }
    drop(tx);
    assert_eq!(changed_lines, FILES * CHANGED_PER_FILE);
    println!(
        "prepared {FILES} files, {changed_lines} changed lines in {:.0?}",
        t.elapsed()
    );

    let platform = gpui_kit::platform::current_platform(true);
    let mut cx = HeadlessAppContext::with_platform(platform.text_system(), Arc::new(()), || None);
    let startup = Startup {
        t0: Instant::now(),
        trace: false,
        bench_exit: false,
    };
    let window = cx
        .open_window(size(px(1280.0), px(820.0)), |window, cx| {
            cx.new(|cx| GoroView::new(startup, Vec::new(), rx, window, cx))
        })
        .unwrap();
    cx.run_until_parked();

    let mut frames = Vec::with_capacity(FRAMES);
    for _ in 0..FRAMES {
        cx.update_window(window.into(), |view, window, cx| {
            let view = view.downcast::<GoroView>().unwrap();
            window.focus(&view.focus_handle(cx), cx);
            window.dispatch_action(Box::new(NextHunk), cx);
            let start = Instant::now();
            let _ = window.draw(cx);
            frames.push(start.elapsed());
        })
        .unwrap();
        cx.run_until_parked();
    }
    let cursor = cx
        .update(|cx| window.read_with(cx, |view, _| view.cursor()))
        .unwrap();
    assert!(cursor > FRAMES * 10, "cursor only reached row {cursor}");
    frames.sort();
    let pct = |p: f64| frames[((frames.len() - 1) as f64 * p) as usize];
    let (p50, p99, max) = (pct(0.50), pct(0.99), frames[frames.len() - 1]);
    println!(
        "frame (next hunk): p50 {p50:.2?}  p99 {p99:.2?}  max {max:.2?}  over {FRAMES} frames"
    );
    if p99 > BUDGET {
        eprintln!("p99 frame time {p99:.2?} exceeds the {BUDGET:?} budget");
        std::process::exit(1);
    }
}
