//! One window's view: the file tree, commit box, and diff stream for a repository, kept
//! live as the working tree changes.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use futures::StreamExt;
use futures::channel::mpsc::{UnboundedReceiver, unbounded};
use goro_core::diff::LineKind;
use goro_core::git::Git;
use goro_core::ops::{self, CommitOptions, Selection, Undo};
use goro_core::repo::{FileChange, Repo, Section};
use goro_core::review::{Dirty, FileLoad, Review, Row, Target, TreeRow, load_files, reuse_loads};
use goro_core::store::{RepoState, Store};
use goro_core::watch::{RepoWatcher, watch};
use gpui_kit::{
    App, ClickEvent, Context, Entity, FocusHandle, Focusable, FontWeight,
    ListHorizontalSizingBehavior, ScrollStrategy, SharedString, UniformListScrollHandle, Window,
    WindowAppearance, div, prelude::*, px, uniform_list,
};

use crate::diff_rows;
use crate::switcher::Switcher;
use crate::text_editor::{EditorColors, TextEditor};
use crate::theme::Theme;
use crate::*;

pub(crate) const ROW_HEIGHT: f32 = 20.0;
const SIDEBAR_WIDTH: f32 = 320.0;
const STATUS_TIMEOUT: Duration = Duration::from_secs(5);
/// Leaving the window after at least this long counts as a look.
const LOOK_MIN: Duration = Duration::from_secs(1);

#[cfg(target_os = "macos")]
const MONO_FONT: &str = "Menlo";
#[cfg(target_os = "windows")]
const MONO_FONT: &str = "Consolas";
#[cfg(not(any(target_os = "macos", target_os = "windows")))]
const MONO_FONT: &str = "DejaVu Sans Mono";

enum State {
    Loading,
    /// Nothing given and nothing detected; the switcher opens a repository.
    NoRepository,
    Ready(Box<Review>),
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
    commit_editor: Entity<TextEditor>,
    amend: bool,
    signoff: bool,
    undo_stack: Vec<Undo>,
    /// A git action or commit is running.
    busy: bool,
    store: Option<Store>,
    watcher: Option<RepoWatcher>,
    /// Changes waiting for the next reload; merged while one is running.
    pending_reload: Option<Dirty>,
    reload_in_flight: bool,
    /// No look recorded yet: take one as soon as everything is loaded.
    baseline_pending: bool,
    /// When the window last became active (for "last look").
    active_since: Option<Instant>,
    switcher: Option<Entity<Switcher>>,
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
        store: Option<Store>,
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
                        view.apply(batch, cx);
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
        cx.observe_window_activation(window, |this, window, cx| {
            this.activation_changed(window.is_window_active(), cx);
        })
        .detach();
        let commit_editor = cx.new(|cx| TextEditor::new("Commit message", cx));
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
            store,
            watcher: None,
            pending_reload: None,
            reload_in_flight: false,
            baseline_pending: false,
            active_since: None,
            switcher: None,
            status: None,
            status_generation: 0,
            startup,
            first_paint_reported: false,
            appearance: None,
        };
        view.apply(initial, cx);
        view
    }

    fn apply(&mut self, batch: Vec<Event>, cx: &mut Context<Self>) {
        let mut rebuild = false;
        for event in batch {
            match event {
                Event::Opened {
                    repo,
                    changes,
                    first,
                    state,
                } => {
                    let mut review = Review::new(repo.root().to_path_buf(), changes);
                    if let Some(first) = first {
                        review.set_loaded(0, first);
                    }
                    self.baseline_pending = state.seen.is_none();
                    review.set_seen(state.seen);
                    review.set_reviewed(state.reviewed);
                    review.rebuild_rows();
                    self.repo = Some(repo);
                    self.state = State::Ready(Box::new(review));
                    self.start_watching(cx);
                }
                Event::NoRepository => self.state = State::NoRepository,
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
        self.take_baseline_when_loaded(cx);
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

    pub fn commit_editor(&self) -> &Entity<TextEditor> {
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
        let dirty = match op {
            Op::Discard => Dirty::Paths([entry.change.path.clone()].into_iter().collect()),
            Op::Stage => Dirty::Paths(Default::default()),
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
                        view.request_reload(dirty, cx);
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
                        view.request_reload(Dirty::All, cx);
                    }
                    Err(err) => view.set_status(err.to_string(), true, cx),
                }
            });
        })
        .detach();
    }

    /// Re-read status and every file, then swap the review in place.
    /// Reload after `dirty` changed. One reload runs at a time; changes arriving
    /// meanwhile are merged and trigger exactly one more, so a busy agent can't starve it.
    pub(crate) fn request_reload(&mut self, dirty: Dirty, cx: &mut Context<Self>) {
        match &mut self.pending_reload {
            Some(pending) => pending.merge(dirty),
            None => self.pending_reload = Some(dirty),
        }
        if !self.reload_in_flight {
            self.start_reload(cx);
        }
    }

    fn start_reload(&mut self, cx: &mut Context<Self>) {
        let (Some(repo), State::Ready(review)) = (self.repo.clone(), &self.state) else {
            return;
        };
        let Some(dirty) = self.pending_reload.take() else {
            return;
        };
        let previous = review.files.clone();
        self.reload_in_flight = true;
        let task = cx.background_executor().spawn(async move {
            let changes = repo.status().map_err(|e| e.to_string())?;
            let mut loads = reuse_loads(&previous, &changes, &dirty);
            let missing: Vec<usize> = (0..changes.len())
                .filter(|&ix| loads[ix].is_none())
                .collect();
            let loaded = Mutex::new(Vec::with_capacity(missing.len()));
            load_files(&repo, &changes, missing, |ix, load| {
                loaded.lock().unwrap().push((ix, load));
            });
            for (ix, load) in loaded.into_inner().unwrap() {
                loads[ix] = Some(load);
            }
            let loads: Vec<FileLoad> = loads
                .into_iter()
                .map(|l| l.expect("every file loaded"))
                .collect();
            Ok::<_, String>((repo.root().to_path_buf(), changes, loads))
        });
        cx.spawn(async move |this, cx| {
            let result = task.await;
            let _ = this.update(cx, |view, cx| {
                view.reload_in_flight = false;
                match result {
                    Ok((root, changes, loads)) => view.replace_review(root, changes, loads),
                    Err(err) => view.set_status(err, true, cx),
                }
                if view.pending_reload.is_some() {
                    view.start_reload(cx);
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn replace_review(&mut self, root: PathBuf, changes: Vec<FileChange>, loads: Vec<FileLoad>) {
        self.rebuild_keeping_position(move |review| {
            let mut next = Review::new(root, changes);
            next.carry_view_state(review);
            for (ix, load) in loads.into_iter().enumerate() {
                next.set_loaded(ix, load);
            }
            next.rebuild_rows();
            *review = next;
        });
    }

    fn start_watching(&mut self, cx: &mut Context<Self>) {
        let Some(repo) = &self.repo else {
            return;
        };
        if self.watcher.is_some() {
            return;
        }
        let (tx, mut rx) = unbounded::<Dirty>();
        match watch(repo.root(), move |dirty| {
            let _ = tx.unbounded_send(dirty);
        }) {
            Ok(watcher) => {
                self.watcher = Some(watcher);
                cx.spawn(async move |this, cx| {
                    while let Some(mut dirty) = rx.next().await {
                        while let Ok(more) = rx.try_recv() {
                            dirty.merge(more);
                        }
                        if this
                            .update(cx, |view, cx| view.request_reload(dirty, cx))
                            .is_err()
                        {
                            break;
                        }
                    }
                })
                .detach();
            }
            Err(err) => self.set_status(err.to_string(), true, cx),
        }
    }

    fn activation_changed(&mut self, active: bool, cx: &mut Context<Self>) {
        if active {
            self.active_since = Some(Instant::now());
        } else if self
            .active_since
            .take()
            .is_some_and(|since| since.elapsed() >= LOOK_MIN)
        {
            self.mark_seen(cx);
        }
    }

    /// Everything shown now counts as seen.
    fn mark_seen(&mut self, cx: &mut Context<Self>) {
        if let State::Ready(review) = &mut self.state {
            let seen = review.snapshot_seen();
            review.set_seen(Some(seen));
        }
        self.rebuild_keeping_position(|review| review.rebuild_rows());
        self.save_state(cx);
        cx.notify();
    }

    fn take_baseline_when_loaded(&mut self, cx: &mut Context<Self>) {
        let loaded = self.review().is_some_and(|r| {
            r.files
                .iter()
                .all(|f| !matches!(f.content, goro_core::review::Content::Pending))
        });
        if self.baseline_pending && loaded {
            self.baseline_pending = false;
            self.mark_seen(cx);
        }
    }

    fn save_state(&self, cx: &mut Context<Self>) {
        let (Some(store), Some(review)) = (self.store.clone(), self.review()) else {
            return;
        };
        let root = review.root.clone();
        let state = RepoState {
            seen: review.seen().cloned(),
            reviewed: review.reviewed_for_save(),
        };
        cx.background_executor()
            .spawn(async move {
                let _ = store.save_repo_state(&root, &state);
            })
            .detach();
    }

    fn next_new(&mut self, _: &NextNew, _: &mut Window, cx: &mut Context<Self>) {
        let to = self.review().and_then(|r| r.next_new_row(self.cursor));
        self.move_cursor(to, false, ScrollStrategy::Center, cx);
    }

    fn prev_new(&mut self, _: &PrevNew, _: &mut Window, cx: &mut Context<Self>) {
        let to = self.review().and_then(|r| r.prev_new_row(self.cursor));
        self.move_cursor(to, false, ScrollStrategy::Center, cx);
    }

    fn mark_seen_action(&mut self, _: &MarkSeen, _: &mut Window, cx: &mut Context<Self>) {
        self.mark_seen(cx);
        self.set_status("Marked everything as seen", false, cx);
    }

    fn toggle_reviewed(&mut self, _: &ToggleReviewed, _: &mut Window, cx: &mut Context<Self>) {
        let Some(review) = self.review() else {
            return;
        };
        // Keep the cursor on the hunk header: toggling collapses the hunk's lines.
        let rows = review.rows();
        if let Some(Row::Line { .. }) = rows.get(self.cursor) {
            let header = (0..=self.cursor)
                .rev()
                .find(|&r| matches!(rows[r], Row::Hunk { .. }));
            if let Some(header) = header {
                self.cursor = header;
            }
        }
        self.select_anchor = None;
        let cursor = self.cursor;
        self.rebuild_keeping_position(|review| review.toggle_reviewed(cursor));
        self.save_state(cx);
        cx.notify();
    }

    fn open_switcher(&mut self, _: &OpenSwitcher, window: &mut Window, cx: &mut Context<Self>) {
        let recent = self.store.as_ref().map(Store::recent).unwrap_or_default();
        let current = self.root().map(Path::to_path_buf);
        let appearance = self.appearance;
        let switcher = cx.new(|cx| Switcher::new(recent, current, appearance, window, cx));
        cx.subscribe_in(
            &switcher,
            window,
            |this, _, event: &crate::switcher::SwitcherEvent, window, cx| {
                this.switcher = None;
                window.focus(&this.focus_handle, cx);
                if let crate::switcher::SwitcherEvent::Open(path) = event {
                    // After this update: `open_repo` reads every window, including this one.
                    let path = path.clone();
                    cx.defer(move |cx| crate::open_repo(Some(path), cx));
                }
                cx.notify();
            },
        )
        .detach();
        window.focus(&switcher.focus_handle(cx), cx);
        self.switcher = Some(switcher);
        cx.notify();
    }

    pub fn switcher_open(&self) -> bool {
        self.switcher.is_some()
    }

    pub fn root(&self) -> Option<&Path> {
        self.repo.as_ref().map(|r| r.root())
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
                        view.request_reload(Dirty::Paths(Default::default()), cx);
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
                    .key_context("CommitBox")
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
                    .child(
                        div()
                            .when(review.file_is_reviewed(file), |el| {
                                el.text_color(theme.muted)
                            })
                            .child(name.clone()),
                    )
                    .when(review.file_is_reviewed(file), |el| {
                        el.child(div().text_color(theme.muted).child("✓"))
                    })
                    .when(review.file_new_count(file) > 0, |el| {
                        el.child(
                            div()
                                .text_color(theme.new_marker)
                                .child(format!("● {}", review.file_new_count(file))),
                        )
                    })
                    .into_any_element()
            }
        }
    }

    fn render_diff(&self, theme: &Theme, cx: &mut Context<Self>) -> gpui_kit::AnyElement {
        let review = match &self.state {
            State::Loading => return centered_message("", theme),
            State::NoRepository => {
                return centered_message(
                    "No repository found. Press ⌘P (Ctrl+P) to open one.",
                    theme,
                );
            }
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
                        let flags = diff_rows::RowFlags {
                            is_cursor: ix == this.cursor,
                            is_selected: selected,
                            is_new: review.is_new(ix),
                        };
                        diff_rows::render_row(review, ix, flags, &theme, cx)
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
        let new_count = self.review().map_or(0, Review::new_count);
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
                    .when(new_count > 0, |el| {
                        el.child(
                            div()
                                .text_color(theme.new_marker)
                                .child(format!("● {new_count} new since last look (tab)")),
                        )
                    })
                    .child(
                        div().text_color(theme.muted).child(
                            "s stage · x discard · r reviewed · u undo · c commit · ⌘P repos",
                        ),
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
            State::Failed(_) | State::NoRepository => self.report_first_paint(cx),
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
            .on_action(cx.listener(Self::next_new))
            .on_action(cx.listener(Self::prev_new))
            .on_action(cx.listener(Self::mark_seen_action))
            .on_action(cx.listener(Self::toggle_reviewed))
            .on_action(cx.listener(Self::open_switcher))
            .relative()
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
            .when_some(self.switcher.clone(), |el, switcher| {
                el.child(
                    div()
                        .absolute()
                        .inset_0()
                        .flex()
                        .justify_center()
                        .items_start()
                        .pt(px(80.0))
                        .child(switcher),
                )
            })
    }
}

/// Index of the topmost visible row (GPUI only exposes this under `test-support`).
fn top_row(handle: &UniformListScrollHandle) -> usize {
    let state = handle.0.borrow();
    // A pending scroll only predicts the top row when it scrolls that row to the top.
    state
        .deferred_scroll_to_item
        .as_ref()
        .filter(|deferred| matches!(deferred.strategy, ScrollStrategy::Top))
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
