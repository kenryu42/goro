//! Goro's window: a file tree and commit box beside one continuous, virtualized diff
//! stream, with stage / unstage / discard / undo / commit.

mod commit_editor;
mod diff_rows;
mod text_buffer;
mod theme;

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use futures::StreamExt;
use futures::channel::mpsc::UnboundedReceiver;
use goro_core::diff::LineKind;
use goro_core::git::Git;
use goro_core::ops::{self, CommitOptions, Selection, Undo};
use goro_core::repo::{FileChange, Repo, Section};
use goro_core::review::{FileLoad, Review, Row, Target, TreeRow, load_files};
use gpui_kit::{
    App, Bounds, ClickEvent, Context, Entity, FocusHandle, Focusable, FontWeight, KeyBinding,
    ListHorizontalSizingBehavior, Menu, MenuItem, ScrollStrategy, SharedString, TitlebarOptions,
    UniformListScrollHandle, Window, WindowAppearance, WindowBounds, WindowOptions, actions, div,
    prelude::*, px, size, uniform_list,
};

use commit_editor::{CommitEditor, EditorColors};
use theme::Theme;

/// Messages from the background loader to the window.
pub enum Event {
    /// The repository was read. `first` is the first file, loaded before first paint.
    Opened {
        repo: Arc<Repo>,
        changes: Vec<FileChange>,
        first: Option<FileLoad>,
    },
    Loaded(usize, FileLoad),
    Failed(String),
}

/// Startup timing. `GORO_TRACE_STARTUP=1` prints each phase to stderr.
#[derive(Clone, Copy)]
pub struct Startup {
    pub t0: Instant,
    pub trace: bool,
    /// Print the first-paint time to stdout and quit (for benchmarks).
    pub bench_exit: bool,
}

impl Startup {
    pub fn mark(&self, phase: &str) {
        if self.trace {
            eprintln!(
                "[goro] {phase}: {:.1}ms",
                self.t0.elapsed().as_secs_f64() * 1000.0
            );
        }
    }
}

actions!(
    goro,
    [
        Quit,
        CloseWindow,
        CursorDown,
        CursorUp,
        SelectDown,
        SelectUp,
        ClearSelection,
        NextHunk,
        PrevHunk,
        NextFile,
        PrevFile,
        /// Stage the target, or unstage it if it's already staged.
        Stage,
        StageFile,
        Discard,
        DiscardFile,
        UndoLast,
        FocusCommit,
        FocusDiff,
        Commit
    ]
);

pub(crate) const ROW_HEIGHT: f32 = 20.0;
const SIDEBAR_WIDTH: f32 = 320.0;
const STATUS_TIMEOUT: Duration = Duration::from_secs(5);

/// Keys that only apply while the diff (not the commit box) has focus.
const DIFF_CONTEXT: &str = "Goro && !CommitEditor";

#[cfg(target_os = "macos")]
const MONO_FONT: &str = "Menlo";
#[cfg(target_os = "windows")]
const MONO_FONT: &str = "Consolas";
#[cfg(not(any(target_os = "macos", target_os = "windows")))]
const MONO_FONT: &str = "DejaVu Sans Mono";

/// How long to hold the window for the review, so the first frame already shows the diff
/// instead of an empty window. Slow repositories open immediately and stream in.
const OPEN_WAIT: Duration = Duration::from_millis(250);

pub fn bind_keys(cx: &mut App) {
    let diff = Some(DIFF_CONTEXT);
    cx.bind_keys([
        KeyBinding::new("cmd-q", Quit, None),
        KeyBinding::new("ctrl-q", Quit, None),
        KeyBinding::new("cmd-w", CloseWindow, None),
        KeyBinding::new("ctrl-w", CloseWindow, None),
        KeyBinding::new("j", CursorDown, diff),
        KeyBinding::new("down", CursorDown, diff),
        KeyBinding::new("k", CursorUp, diff),
        KeyBinding::new("up", CursorUp, diff),
        KeyBinding::new("shift-j", SelectDown, diff),
        KeyBinding::new("shift-down", SelectDown, diff),
        KeyBinding::new("shift-k", SelectUp, diff),
        KeyBinding::new("shift-up", SelectUp, diff),
        KeyBinding::new("escape", ClearSelection, diff),
        KeyBinding::new("n", NextHunk, diff),
        KeyBinding::new("p", PrevHunk, diff),
        KeyBinding::new("]", NextFile, diff),
        KeyBinding::new("[", PrevFile, diff),
        KeyBinding::new("s", Stage, diff),
        KeyBinding::new("shift-s", StageFile, diff),
        KeyBinding::new("x", Discard, diff),
        KeyBinding::new("shift-x", DiscardFile, diff),
        KeyBinding::new("u", UndoLast, diff),
        KeyBinding::new("cmd-z", UndoLast, diff),
        KeyBinding::new("ctrl-z", UndoLast, diff),
        KeyBinding::new("c", FocusCommit, diff),
        KeyBinding::new("escape", FocusDiff, Some(commit_editor::CONTEXT)),
        KeyBinding::new("cmd-enter", Commit, Some("Goro")),
        KeyBinding::new("ctrl-enter", Commit, Some("Goro")),
    ]);
    commit_editor::bind_keys(cx);
}

pub fn run(startup: Startup, mut events: UnboundedReceiver<Event>) {
    gpui_kit::application().run(move |cx: &mut App| {
        startup.mark("platform ready");
        cx.on_action(|_: &Quit, cx| cx.quit());
        bind_keys(cx);
        cx.set_menus([Menu::new("Goro").items([MenuItem::action("Quit Goro", Quit)])]);
        cx.on_window_closed(|cx, _| {
            if cx.windows().is_empty() {
                cx.quit();
            }
        })
        .detach();

        let initial = wait_for_open(&mut events, startup.t0 + OPEN_WAIT);
        startup.mark("review ready for first frame");
        let bounds = Bounds::centered(None, size(px(1280.0), px(820.0)), cx);
        cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                titlebar: Some(TitlebarOptions {
                    title: Some("Goro".into()),
                    ..Default::default()
                }),
                app_id: Some("dev.goro.Goro".into()),
                ..Default::default()
            },
            |window, cx| {
                let view = cx.new(|cx| GoroView::new(startup, initial, events, window, cx));
                window.focus(&view.focus_handle(cx), cx);
                view
            },
        )
        .expect("failed to open window");
        startup.mark("window opened");
        cx.activate(true);
    });
}

/// Collect events until the repository is opened (or failed), or `deadline` passes.
fn wait_for_open(events: &mut UnboundedReceiver<Event>, deadline: Instant) -> Vec<Event> {
    let mut initial = Vec::new();
    loop {
        match events.try_recv() {
            Ok(event) => {
                let done = matches!(event, Event::Opened { .. } | Event::Failed(_));
                initial.push(event);
                if done {
                    // Take whatever else is already loaded, too.
                    while let Ok(event) = events.try_recv() {
                        initial.push(event);
                    }
                    return initial;
                }
            }
            Err(_) if Instant::now() >= deadline => return initial,
            Err(_) => std::thread::sleep(Duration::from_millis(1)),
        }
    }
}

enum State {
    Loading,
    Ready(Review),
    Failed(String),
}

/// A review action on the cursor or selection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Op {
    /// Stage, or unstage if already staged.
    Stage,
    Discard,
}

struct Status {
    text: SharedString,
    is_error: bool,
}

pub struct GoroView {
    state: State,
    repo: Option<Arc<Repo>>,
    cursor: usize,
    /// Start of a line selection; the selection runs from here to the cursor.
    select_anchor: Option<usize>,
    diff_scroll: UniformListScrollHandle,
    tree_scroll: UniformListScrollHandle,
    focus_handle: FocusHandle,
    commit_editor: Entity<CommitEditor>,
    amend: bool,
    signoff: bool,
    undo_stack: Vec<Undo>,
    /// A git action or commit is running.
    busy: bool,
    /// Bumped per reload; stale reload results are dropped.
    reload_generation: u64,
    status: Option<Status>,
    status_generation: u64,
    startup: Startup,
    first_paint_reported: bool,
    /// Forced light/dark; `None` follows the OS.
    appearance: Option<WindowAppearance>,
}

impl GoroView {
    /// `initial` events are applied before the first frame; `events` stream in after.
    pub fn new(
        startup: Startup,
        initial: Vec<Event>,
        mut events: UnboundedReceiver<Event>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        cx.spawn(async move |this, cx| {
            while let Some(event) = events.next().await {
                // Drain everything already queued so a burst of loads costs one rebuild.
                let mut batch = vec![event];
                while let Ok(event) = events.try_recv() {
                    batch.push(event);
                }
                if this
                    .update(cx, |view, cx| {
                        view.apply(batch);
                        cx.notify();
                    })
                    .is_err()
                {
                    break;
                }
            }
        })
        .detach();
        cx.observe_window_appearance(window, |_, _, cx| cx.notify())
            .detach();
        let commit_editor = cx.new(|cx| CommitEditor::new("Commit message", cx));
        let mut view = Self {
            state: State::Loading,
            repo: None,
            cursor: 0,
            select_anchor: None,
            diff_scroll: UniformListScrollHandle::new(),
            tree_scroll: UniformListScrollHandle::new(),
            focus_handle: cx.focus_handle(),
            commit_editor,
            amend: false,
            signoff: false,
            undo_stack: Vec::new(),
            busy: false,
            reload_generation: 0,
            status: None,
            status_generation: 0,
            startup,
            first_paint_reported: false,
            appearance: None,
        };
        view.apply(initial);
        view
    }

    fn apply(&mut self, batch: Vec<Event>) {
        let mut rebuild = false;
        for event in batch {
            match event {
                Event::Opened {
                    repo,
                    changes,
                    first,
                } => {
                    let mut review = Review::new(repo.root().to_path_buf(), changes);
                    if let Some(first) = first {
                        review.set_loaded(0, first);
                    }
                    review.rebuild_rows();
                    self.repo = Some(repo);
                    self.state = State::Ready(review);
                }
                Event::Loaded(file, load) => {
                    if let State::Ready(review) = &mut self.state {
                        review.set_loaded(file, load);
                        rebuild = true;
                    }
                }
                Event::Failed(message) => self.state = State::Failed(message),
            }
        }
        if rebuild {
            self.rebuild_keeping_position(|review| review.rebuild_rows());
        }
    }

    /// Apply a change to the review while keeping the viewport, cursor and selection on
    /// the same content.
    fn rebuild_keeping_position(&mut self, change: impl FnOnce(&mut Review)) {
        let State::Ready(review) = &mut self.state else {
            return;
        };
        let top = top_row(&self.diff_scroll);
        let top_anchor = review.anchor(top);
        let cursor_anchor = review.anchor(self.cursor);
        let select_anchor = self.select_anchor.and_then(|row| review.anchor(row));
        change(review);
        if let Some(anchor) = top_anchor {
            let ix = review.resolve(&anchor);
            if ix != top {
                self.diff_scroll
                    .scroll_to_item_strict(ix, ScrollStrategy::Top);
            }
        }
        self.cursor = cursor_anchor
            .map(|a| review.resolve(&a))
            .unwrap_or(0)
            .min(review.rows().len().saturating_sub(1));
        self.select_anchor = select_anchor.map(|a| review.resolve(&a));
    }

    pub fn review(&self) -> Option<&Review> {
        match &self.state {
            State::Ready(review) => Some(review),
            _ => None,
        }
    }

    pub fn cursor(&self) -> usize {
        self.cursor
    }

    pub fn is_busy(&self) -> bool {
        self.busy
    }

    pub fn status_text(&self) -> Option<(&str, bool)> {
        self.status.as_ref().map(|s| (s.text.as_ref(), s.is_error))
    }

    pub fn commit_editor(&self) -> &Entity<CommitEditor> {
        &self.commit_editor
    }

    /// Force light or dark; `None` follows the OS.
    pub fn set_appearance(&mut self, appearance: Option<WindowAppearance>, cx: &mut Context<Self>) {
        self.appearance = appearance;
        cx.notify();
    }

    fn theme(&self, window: &Window) -> Theme {
        Theme::for_appearance(self.appearance.unwrap_or_else(|| window.appearance()))
    }

    fn git(&self) -> Option<Git> {
        self.repo.as_ref().map(|repo| Git::new(repo.root()))
    }

    fn set_status(
        &mut self,
        text: impl Into<SharedString>,
        is_error: bool,
        cx: &mut Context<Self>,
    ) {
        self.status = Some(Status {
            text: text.into(),
            is_error,
        });
        self.status_generation += 1;
        if !is_error {
            let generation = self.status_generation;
            cx.spawn(async move |this, cx| {
                cx.background_executor().timer(STATUS_TIMEOUT).await;
                let _ = this.update(cx, |view, cx| {
                    if view.status_generation == generation {
                        view.status = None;
                        cx.notify();
                    }
                });
            })
            .detach();
        }
        cx.notify();
    }

    fn move_cursor(
        &mut self,
        to: Option<usize>,
        extend: bool,
        strategy: ScrollStrategy,
        cx: &mut Context<Self>,
    ) {
        let Some(to) = to else {
            return;
        };
        if extend {
            self.select_anchor.get_or_insert(self.cursor);
        } else {
            self.select_anchor = None;
        }
        self.cursor = to;
        self.diff_scroll.scroll_to_item(to, strategy);
        cx.notify();
    }

    fn last_row(&self) -> usize {
        self.review()
            .map_or(0, |r| r.rows().len().saturating_sub(1))
    }

    fn cursor_down(&mut self, _: &CursorDown, _: &mut Window, cx: &mut Context<Self>) {
        let to = (self.cursor + 1).min(self.last_row());
        self.move_cursor(Some(to), false, ScrollStrategy::Nearest, cx);
    }

    fn cursor_up(&mut self, _: &CursorUp, _: &mut Window, cx: &mut Context<Self>) {
        let to = self.cursor.saturating_sub(1);
        self.move_cursor(Some(to), false, ScrollStrategy::Nearest, cx);
    }

    fn select_down(&mut self, _: &SelectDown, _: &mut Window, cx: &mut Context<Self>) {
        let to = (self.cursor + 1).min(self.last_row());
        self.move_cursor(Some(to), true, ScrollStrategy::Nearest, cx);
    }

    fn select_up(&mut self, _: &SelectUp, _: &mut Window, cx: &mut Context<Self>) {
        let to = self.cursor.saturating_sub(1);
        self.move_cursor(Some(to), true, ScrollStrategy::Nearest, cx);
    }

    /// Escape clears the selection, then a lingering error.
    fn clear_selection(&mut self, _: &ClearSelection, _: &mut Window, cx: &mut Context<Self>) {
        if self.select_anchor.take().is_none() {
            self.status = None;
        }
        cx.notify();
    }

    fn next_hunk(&mut self, _: &NextHunk, _: &mut Window, cx: &mut Context<Self>) {
        let to = self.review().and_then(|r| r.next_hunk_row(self.cursor));
        self.move_cursor(to, false, ScrollStrategy::Top, cx);
    }

    fn prev_hunk(&mut self, _: &PrevHunk, _: &mut Window, cx: &mut Context<Self>) {
        let to = self.review().and_then(|r| r.prev_hunk_row(self.cursor));
        self.move_cursor(to, false, ScrollStrategy::Top, cx);
    }

    fn next_file(&mut self, _: &NextFile, _: &mut Window, cx: &mut Context<Self>) {
        let to = self.review().and_then(|r| r.next_file_row(self.cursor));
        self.move_cursor(to, false, ScrollStrategy::Top, cx);
    }

    fn prev_file(&mut self, _: &PrevFile, _: &mut Window, cx: &mut Context<Self>) {
        let to = self.review().and_then(|r| r.prev_file_row(self.cursor));
        self.move_cursor(to, false, ScrollStrategy::Top, cx);
    }

    pub(crate) fn click_row(&mut self, row: usize, extend: bool, cx: &mut Context<Self>) {
        if extend {
            self.select_anchor.get_or_insert(self.cursor);
        } else {
            self.select_anchor = None;
        }
        self.cursor = row;
        cx.notify();
    }

    pub(crate) fn act_on_row(&mut self, row: usize, op: Op, cx: &mut Context<Self>) {
        self.cursor = row;
        self.select_anchor = None;
        self.run_op(op, false, cx);
    }

    fn stage(&mut self, _: &Stage, _: &mut Window, cx: &mut Context<Self>) {
        self.run_op(Op::Stage, false, cx);
    }

    fn stage_file(&mut self, _: &StageFile, _: &mut Window, cx: &mut Context<Self>) {
        self.run_op(Op::Stage, true, cx);
    }

    fn discard(&mut self, _: &Discard, _: &mut Window, cx: &mut Context<Self>) {
        self.run_op(Op::Discard, false, cx);
    }

    fn discard_file(&mut self, _: &DiscardFile, _: &mut Window, cx: &mut Context<Self>) {
        self.run_op(Op::Discard, true, cx);
    }

    /// Stage/unstage/discard the cursor's target (or the whole file) in the background,
    /// then reload.
    fn run_op(&mut self, op: Op, whole_file: bool, cx: &mut Context<Self>) {
        if self.busy {
            return;
        }
        let (Some(git), Some(review)) = (self.git(), self.review()) else {
            return;
        };
        let Some((file, target)) = review.action_target(self.cursor, self.select_anchor) else {
            return;
        };
        let target = if whole_file {
            Target::WholeFile
        } else {
            target
        };
        let entry = review.files[file].clone();
        let staged = entry.change.section == Section::Staged;
        let what = match &target {
            Target::WholeFile => entry.change.path_lossy().into_owned(),
            Target::Lines(lines) => {
                let changed = entry.diff().map_or(0, |d| {
                    lines
                        .iter()
                        .filter(|&&ix| d.lines[ix].kind != LineKind::Context)
                        .count()
                });
                format!(
                    "{changed} line{} of {}",
                    if changed == 1 { "" } else { "s" },
                    entry.change.path_lossy()
                )
            }
        };
        let verb = match (op, staged) {
            (Op::Stage, false) => "Staged",
            (Op::Stage, true) => "Unstaged",
            (Op::Discard, _) => "Discarded",
        };
        self.busy = true;
        cx.notify();
        let task = cx.background_executor().spawn(async move {
            let diff = entry.diff().map(|d| &**d);
            let selection = match &target {
                Target::WholeFile => Selection::File,
                Target::Lines(lines) => Selection::Lines(lines),
            };
            match (op, staged) {
                (Op::Stage, false) => ops::stage(&git, &entry.change, diff, selection),
                (Op::Stage, true) => ops::unstage(&git, &entry.change, diff, selection),
                (Op::Discard, _) => ops::discard(&git, &entry.change, diff, selection),
            }
        });
        cx.spawn(async move |this, cx| {
            let result = task.await;
            let _ = this.update(cx, |view, cx| {
                view.busy = false;
                match result {
                    Ok(undo) => {
                        view.undo_stack.push(undo);
                        view.select_anchor = None;
                        view.set_status(format!("{verb} {what}  (u to undo)"), false, cx);
                        view.reload(cx);
                    }
                    Err(err) => view.set_status(err.to_string(), true, cx),
                }
            });
        })
        .detach();
    }

    fn undo_last(&mut self, _: &UndoLast, _: &mut Window, cx: &mut Context<Self>) {
        if self.busy {
            return;
        }
        let Some(git) = self.git() else {
            return;
        };
        let Some(undo) = self.undo_stack.pop() else {
            self.set_status("Nothing to undo", false, cx);
            return;
        };
        self.busy = true;
        let task = cx.background_executor().spawn({
            let undo = undo.clone();
            async move { ops::undo(&git, &undo) }
        });
        cx.spawn(async move |this, cx| {
            let result = task.await;
            let _ = this.update(cx, |view, cx| {
                view.busy = false;
                match result {
                    Ok(()) => {
                        view.set_status(format!("Undid {}", undo.description), false, cx);
                        view.reload(cx);
                    }
                    Err(err) => view.set_status(err.to_string(), true, cx),
                }
            });
        })
        .detach();
    }

    /// Re-read status and every file, then swap the review in place.
    fn reload(&mut self, cx: &mut Context<Self>) {
        let Some(repo) = self.repo.clone() else {
            return;
        };
        self.reload_generation += 1;
        let generation = self.reload_generation;
        let task = cx.background_executor().spawn(async move {
            let changes = repo.status().map_err(|e| e.to_string())?;
            let loads = Mutex::new(Vec::with_capacity(changes.len()));
            load_files(&repo, &changes, 0..changes.len(), |ix, load| {
                loads.lock().unwrap().push((ix, load));
            });
            Ok::<_, String>((
                repo.root().to_path_buf(),
                changes,
                loads.into_inner().unwrap(),
            ))
        });
        cx.spawn(async move |this, cx| {
            let result = task.await;
            let _ = this.update(cx, |view, cx| {
                if view.reload_generation != generation {
                    return;
                }
                match result {
                    Ok((root, changes, loads)) => view.replace_review(root, changes, loads),
                    Err(err) => view.set_status(err, true, cx),
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn replace_review(
        &mut self,
        root: PathBuf,
        changes: Vec<FileChange>,
        loads: Vec<(usize, FileLoad)>,
    ) {
        self.rebuild_keeping_position(move |review| {
            let mut next = Review::new(root, changes);
            next.carry_view_state(review);
            for (ix, load) in loads {
                next.set_loaded(ix, load);
            }
            next.rebuild_rows();
            *review = next;
        });
    }

    fn focus_commit(&mut self, _: &FocusCommit, window: &mut Window, cx: &mut Context<Self>) {
        window.focus(&self.commit_editor.focus_handle(cx), cx);
    }

    fn focus_diff(&mut self, _: &FocusDiff, window: &mut Window, cx: &mut Context<Self>) {
        window.focus(&self.focus_handle, cx);
    }

    fn toggle_amend(&mut self, cx: &mut Context<Self>) {
        self.amend = !self.amend;
        cx.notify();
        if !self.amend || !self.commit_editor.read(cx).text().is_empty() {
            return;
        }
        let Some(git) = self.git() else {
            return;
        };
        let task = cx
            .background_executor()
            .spawn(async move { ops::last_commit_message(&git) });
        cx.spawn(async move |this, cx| {
            if let Ok(message) = task.await {
                let _ = this.update(cx, |view, cx| {
                    view.commit_editor
                        .update(cx, |editor, cx| editor.set_text(&message, cx));
                });
            }
        })
        .detach();
    }

    fn staged_count(&self) -> usize {
        self.review().map_or(0, |r| {
            r.files
                .iter()
                .filter(|f| f.change.section == Section::Staged)
                .count()
        })
    }

    fn commit(&mut self, _: &Commit, window: &mut Window, cx: &mut Context<Self>) {
        if self.busy {
            return;
        }
        let Some(git) = self.git() else {
            return;
        };
        if !self.amend && self.staged_count() == 0 {
            self.set_status("Nothing staged to commit", true, cx);
            return;
        }
        let message = self.commit_editor.read(cx).text().to_string();
        let options = CommitOptions {
            amend: self.amend,
            signoff: self.signoff,
        };
        self.busy = true;
        self.set_status("Committing…", false, cx);
        let task = cx
            .background_executor()
            .spawn(async move { ops::commit(&git, &message, options) });
        let diff_focus = self.focus_handle.clone();
        cx.spawn_in(window, async move |this, cx| {
            let result = task.await;
            let _ = this.update_in(cx, |view, window, cx| {
                view.busy = false;
                match result {
                    Ok(summary) => {
                        view.commit_editor
                            .update(cx, |editor, cx| editor.set_text("", cx));
                        view.amend = false;
                        // Undo entries refer to the old HEAD.
                        view.undo_stack.clear();
                        view.set_status(format!("Committed {summary}"), false, cx);
                        window.focus(&diff_focus, cx);
                        view.reload(cx);
                    }
                    Err(err) => view.set_status(err.to_string(), true, cx),
                }
            });
        })
        .detach();
    }

    fn close_window(&mut self, _: &CloseWindow, window: &mut Window, _: &mut Context<Self>) {
        window.remove_window();
    }

    /// Called while rendering the first frame that shows the review. Deferred work runs
    /// once that frame has been drawn.
    fn report_first_paint(&mut self, cx: &mut Context<Self>) {
        if self.first_paint_reported {
            return;
        }
        self.first_paint_reported = true;
        let startup = self.startup;
        cx.defer(move |cx| {
            startup.mark("first diff drawn");
            if startup.bench_exit {
                println!(
                    "first_paint_ms={:.1}",
                    startup.t0.elapsed().as_secs_f64() * 1000.0
                );
                cx.quit();
            }
        });
    }

    fn render_sidebar(&self, theme: &Theme, cx: &mut Context<Self>) -> impl IntoElement {
        let review = self.review();
        let title = review
            .and_then(|r| r.root.file_name())
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default();
        let active_file =
            review.and_then(|r| r.rows().get(top_row(&self.diff_scroll)).map(Row::file));
        let count = review.map_or(0, |r| r.tree().len());
        div()
            .flex()
            .flex_col()
            .w(px(SIDEBAR_WIDTH))
            .h_full()
            .flex_none()
            .bg(theme.sidebar_bg)
            .border_r_1()
            .border_color(theme.border)
            .child(
                div()
                    .h(px(ROW_HEIGHT * 2.0))
                    .flex()
                    .items_center()
                    .px_3()
                    .font_weight(FontWeight::BOLD)
                    .border_b_1()
                    .border_color(theme.border)
                    .child(title),
            )
            .child(
                uniform_list(
                    "tree",
                    count,
                    cx.processor(move |this, range: std::ops::Range<usize>, window, cx| {
                        let theme = this.theme(window);
                        let Some(review) = this.review() else {
                            return Vec::new();
                        };
                        range
                            .map(|ix| this.render_tree_row(review, ix, active_file, &theme, cx))
                            .collect()
                    }),
                )
                .track_scroll(&self.tree_scroll)
                .flex_1(),
            )
            .child(self.render_commit_box(theme, cx))
    }

    fn render_commit_box(&self, theme: &Theme, cx: &mut Context<Self>) -> impl IntoElement {
        let staged = self.staged_count();
        let can_commit = !self.busy && (staged > 0 || self.amend);
        let checkbox = |id: &'static str, label: &'static str, on: bool| {
            div()
                .id(id)
                .flex()
                .gap_1()
                .cursor_pointer()
                .text_color(if on { theme.fg } else { theme.muted })
                .child(if on { "☑" } else { "☐" })
                .child(label)
        };
        let label = match (self.amend, staged) {
            (true, _) => "Amend commit".to_string(),
            (false, 0) => "Nothing staged".to_string(),
            (false, 1) => "Commit 1 file".to_string(),
            (false, n) => format!("Commit {n} files"),
        };
        div()
            .flex()
            .flex_col()
            .gap_2()
            .p_2()
            .border_t_1()
            .border_color(theme.border)
            .child(
                div()
                    .h(px(ROW_HEIGHT * 5.0 + 8.0))
                    .p_1()
                    .rounded_sm()
                    .bg(theme.bg)
                    .border_1()
                    .border_color(theme.border)
                    .child(self.commit_editor.clone()),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_3()
                    .child(
                        checkbox("amend", "Amend", self.amend).on_click(
                            cx.listener(|this, _: &ClickEvent, _, cx| this.toggle_amend(cx)),
                        ),
                    )
                    .child(
                        checkbox("signoff", "Sign off", self.signoff).on_click(cx.listener(
                            |this, _: &ClickEvent, _, cx| {
                                this.signoff = !this.signoff;
                                cx.notify();
                            },
                        )),
                    )
                    .child(div().flex_1())
                    .child(
                        div()
                            .id("commit")
                            .px_2()
                            .rounded_sm()
                            .when(can_commit, |el| {
                                el.bg(theme.accent)
                                    .text_color(gpui_kit::white())
                                    .cursor_pointer()
                            })
                            .when(!can_commit, |el| {
                                el.border_1()
                                    .border_color(theme.border)
                                    .text_color(theme.muted)
                            })
                            .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                                this.commit(&Commit, window, cx)
                            }))
                            .child(label),
                    ),
            )
    }

    fn render_tree_row(
        &self,
        review: &Review,
        ix: usize,
        active_file: Option<usize>,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> gpui_kit::AnyElement {
        let row = div()
            .id(("tree-row", ix))
            .w_full()
            .h(px(ROW_HEIGHT))
            .flex()
            .items_center()
            .whitespace_nowrap()
            .overflow_hidden()
            .pr_2();
        match &review.tree()[ix] {
            TreeRow::Section { section, count } => row
                .pl_3()
                .text_color(theme.muted)
                .font_weight(FontWeight::BOLD)
                .child(format!("{} ({count})", section_label(*section)))
                .into_any_element(),
            TreeRow::Dir {
                section,
                depth,
                name,
                path,
                expanded,
            } => {
                let (section, path) = (*section, path.clone());
                row.pl(indent(*depth))
                    .text_color(theme.muted)
                    .cursor_pointer()
                    .hover(|s| s.bg(theme.active_row_bg))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        if let State::Ready(review) = &mut this.state {
                            review.toggle_dir(section, &path);
                            cx.notify();
                        }
                    }))
                    .child(format!("{} {name}/", if *expanded { "▾" } else { "▸" }))
                    .into_any_element()
            }
            TreeRow::File { depth, name, file } => {
                let file = *file;
                let (badge, color) = theme.status(review.files[file].change.status);
                row.pl(indent(*depth))
                    .gap_2()
                    .cursor_pointer()
                    .when(active_file == Some(file), |row| row.bg(theme.active_row_bg))
                    .hover(|s| s.bg(theme.active_row_bg))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        if let Some(row) = this.review().map(|r| r.file_row(file)) {
                            this.cursor = row;
                            this.select_anchor = None;
                            this.diff_scroll
                                .scroll_to_item_strict(row, ScrollStrategy::Top);
                            cx.notify();
                        }
                    }))
                    .child(div().text_color(color).child(badge))
                    .child(name.clone())
                    .into_any_element()
            }
        }
    }

    fn render_diff(&self, theme: &Theme, cx: &mut Context<Self>) -> gpui_kit::AnyElement {
        let review = match &self.state {
            State::Loading => return centered_message("", theme),
            State::Failed(message) => return centered_message(message, theme),
            State::Ready(review) if review.is_empty() => {
                return centered_message("No changes. The working tree is clean.", theme);
            }
            State::Ready(review) => review,
        };
        let top = top_row(&self.diff_scroll);
        let sticky = review
            .rows()
            .get(top)
            .filter(|row| !matches!(row, Row::File { .. }))
            .map(|row| row.file());
        let list = uniform_list(
            "diff",
            review.rows().len(),
            cx.processor(|this, range: std::ops::Range<usize>, window, cx| {
                let theme = this.theme(window);
                let Some(review) = this.review() else {
                    return Vec::new();
                };
                let selection = this
                    .select_anchor
                    .map(|a| a.min(this.cursor)..=a.max(this.cursor));
                range
                    .map(|ix| {
                        let selected = selection.as_ref().is_some_and(|s| s.contains(&ix));
                        diff_rows::render_row(review, ix, ix == this.cursor, selected, &theme, cx)
                    })
                    .collect()
            }),
        )
        .track_scroll(&self.diff_scroll)
        .with_horizontal_sizing_behavior(ListHorizontalSizingBehavior::Unconstrained)
        .with_width_from_item(review.widest_row())
        .size_full();
        div()
            .relative()
            .flex_1()
            .overflow_hidden()
            .child(list)
            .when_some(sticky, |el, file| {
                el.child(
                    div()
                        .absolute()
                        .top_0()
                        .left_0()
                        .right_0()
                        .child(diff_rows::file_header(review, file, None, theme, cx)),
                )
            })
            .into_any_element()
    }

    fn render_status_bar(&self, theme: &Theme) -> impl IntoElement {
        const MAX_ERROR_LINES: usize = 12;
        let error = self.status.as_ref().filter(|s| s.is_error);
        let message = self
            .status
            .as_ref()
            .filter(|s| !s.is_error)
            .map(|s| s.text.clone())
            .unwrap_or_default();
        div()
            .flex()
            .flex_col()
            .border_t_1()
            .border_color(theme.border)
            .bg(theme.sidebar_bg)
            .when_some(error, |el, error| {
                let lines: Vec<&str> = error.text.lines().collect();
                let hidden = lines.len().saturating_sub(MAX_ERROR_LINES);
                el.child(
                    div()
                        .flex()
                        .flex_col()
                        .px_3()
                        .py_1()
                        .border_b_1()
                        .border_color(theme.border)
                        .text_color(theme.error)
                        .children(
                            lines
                                .into_iter()
                                .take(MAX_ERROR_LINES)
                                .map(|line| div().whitespace_nowrap().child(line.to_string())),
                        )
                        .when(hidden > 0, |el| el.child(format!("… {hidden} more lines")))
                        .child(div().text_color(theme.muted).child("esc to dismiss")),
                )
            })
            .child(
                div()
                    .h(px(ROW_HEIGHT + 6.0))
                    .flex()
                    .items_center()
                    .gap_3()
                    .px_3()
                    .whitespace_nowrap()
                    .overflow_hidden()
                    .child(div().flex_1().child(message))
                    .child(
                        div()
                            .text_color(theme.muted)
                            .child("s stage · x discard · ⇧ select · u undo · c commit"),
                    ),
            )
    }
}

impl Focusable for GoroView {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for GoroView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = self.theme(window);
        match &self.state {
            State::Ready(review) => {
                if let Some(name) = review.root.file_name() {
                    window.set_window_title(&format!("{} — Goro", name.to_string_lossy()));
                }
                self.report_first_paint(cx);
            }
            State::Failed(_) => self.report_first_paint(cx),
            State::Loading => {}
        }
        let colors = EditorColors {
            text: theme.fg,
            placeholder: theme.muted,
            cursor: theme.accent,
            selection: theme.selection_bg,
        };
        self.commit_editor
            .update(cx, |editor, _| editor.set_colors(colors));
        div()
            .key_context("Goro")
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(Self::cursor_down))
            .on_action(cx.listener(Self::cursor_up))
            .on_action(cx.listener(Self::select_down))
            .on_action(cx.listener(Self::select_up))
            .on_action(cx.listener(Self::clear_selection))
            .on_action(cx.listener(Self::next_hunk))
            .on_action(cx.listener(Self::prev_hunk))
            .on_action(cx.listener(Self::next_file))
            .on_action(cx.listener(Self::prev_file))
            .on_action(cx.listener(Self::stage))
            .on_action(cx.listener(Self::stage_file))
            .on_action(cx.listener(Self::discard))
            .on_action(cx.listener(Self::discard_file))
            .on_action(cx.listener(Self::undo_last))
            .on_action(cx.listener(Self::focus_commit))
            .on_action(cx.listener(Self::focus_diff))
            .on_action(cx.listener(Self::commit))
            .on_action(cx.listener(Self::close_window))
            .flex()
            .flex_row()
            .size_full()
            .bg(theme.bg)
            .text_color(theme.fg)
            .font_family(MONO_FONT)
            .text_size(px(12.5))
            .line_height(px(ROW_HEIGHT))
            .child(self.render_sidebar(&theme, cx))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .flex_1()
                    .h_full()
                    .child(self.render_diff(&theme, cx))
                    .child(self.render_status_bar(&theme)),
            )
    }
}

/// Index of the topmost visible row (GPUI only exposes this under `test-support`).
fn top_row(handle: &UniformListScrollHandle) -> usize {
    let state = handle.0.borrow();
    state
        .deferred_scroll_to_item
        .as_ref()
        .map(|deferred| deferred.item_index)
        .unwrap_or_else(|| state.base_handle.logical_scroll_top().0)
}

fn indent(depth: usize) -> gpui_kit::Pixels {
    px(12.0 + depth as f32 * 14.0)
}

pub(crate) fn section_label(section: Section) -> &'static str {
    match section {
        Section::Staged => "STAGED",
        Section::Unstaged => "UNSTAGED",
        Section::Untracked => "UNTRACKED",
    }
}

fn centered_message(message: &str, theme: &Theme) -> gpui_kit::AnyElement {
    div()
        .flex_1()
        .flex()
        .items_center()
        .justify_center()
        .text_color(theme.muted)
        .child(SharedString::from(message.to_string()))
        .into_any_element()
}
