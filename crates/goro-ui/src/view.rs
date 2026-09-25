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
use crate::picker::{Picker, PickerEvent, PickerItem};
use crate::text_editor::{EditorColors, TextEditor};
use crate::theme::Theme;
use crate::*;
use futures::channel::oneshot;
use goro_core::comments::{Comment, to_markdown};
use goro_core::turns::{self, Session};

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

/// What the diff compares.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Mode {
    /// HEAD, index and worktree: staged, unstaged and untracked changes.
    WorkingTree,
    /// One agent turn: its start snapshot to its end (or the worktree while it runs).
    Turn { session: String, turn: usize },
    /// A whole session: its first snapshot to the worktree now.
    Session { session: String },
}

enum PickerPurpose {
    Repos(Vec<PathBuf>),
    Turns(Vec<Mode>),
}

struct Draft {
    editor: Entity<TextEditor>,
    /// The comment being edited, or a new one on these lines of `file`.
    editing: Option<u64>,
    file: usize,
    lines: Vec<usize>,
    location: String,
}

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
    picker: Option<(Entity<Picker>, PickerPurpose)>,
    /// What the diff compares.
    mode: Mode,
    /// Recorded agent sessions (from turn snapshots), most recent first.
    sessions: Vec<Session>,
    /// A comment being written or edited.
    draft: Option<Draft>,
    /// `goro --wait` callers waiting for this review.
    waiters: Vec<oneshot::Sender<String>>,
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
            picker: None,
            mode: Mode::WorkingTree,
            sessions: Vec::new(),
            draft: None,
            waiters: Vec::new(),
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
                    review.set_comments(state.comments);
                    review.rebuild_rows();
                    self.repo = Some(repo);
                    self.state = State::Ready(Box::new(review));
                    self.start_watching(cx);
                    self.load_sessions(cx);
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
        if !self.delete_comment_at_cursor(cx) {
            self.run_op(Op::Discard, false, cx);
        }
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
        let trees = match self.comparison_trees() {
            Ok(trees) => trees,
            Err(err) => {
                self.set_status(err, true, cx);
                return;
            }
        };
        self.reload_in_flight = true;
        let task = cx.background_executor().spawn(async move {
            let changes = match trees {
                None => repo.status().map_err(|e| e.to_string())?,
                Some((old, new)) => {
                    let git = Git::new(repo.root());
                    let new = match new {
                        Some(tree) => tree,
                        None => turns::worktree_tree(&git).map_err(|e| e.to_string())?,
                    };
                    turns::changes_between(&git, &old, &new).map_err(|e| e.to_string())?
                }
            };
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

    /// For turn and session modes: the tree to compare from, and the tree to compare to
    /// (`None`: the worktree now). `None` for the working tree.
    fn comparison_trees(&self) -> Result<Option<(String, Option<String>)>, String> {
        let session_of = |id: &str| {
            self.sessions
                .iter()
                .find(|s| s.id == id)
                .ok_or_else(|| "that session's snapshots are gone".to_string())
        };
        match &self.mode {
            Mode::WorkingTree => Ok(None),
            Mode::Turn { session, turn } => {
                let turn = session_of(session)?
                    .turns
                    .get(*turn)
                    .ok_or("that turn's snapshots are gone")?;
                let old = turn
                    .start
                    .as_ref()
                    .or(turn.end.as_ref())
                    .ok_or("that turn has no snapshots")?;
                Ok(Some((
                    old.tree.clone(),
                    turn.end.as_ref().map(|e| e.tree.clone()),
                )))
            }
            Mode::Session { session } => {
                let first = session_of(session)?
                    .turns
                    .iter()
                    .find_map(|t| t.start.as_ref().or(t.end.as_ref()))
                    .ok_or("that session has no snapshots")?;
                Ok(Some((first.tree.clone(), None)))
            }
        }
    }

    /// Re-read recorded agent sessions (after a hook records a turn).
    pub fn load_sessions(&mut self, cx: &mut Context<Self>) {
        let Some(repo) = self.repo.clone() else {
            return;
        };
        let task = cx
            .background_executor()
            .spawn(async move { turns::list(&Git::new(repo.root())) });
        cx.spawn(async move |this, cx| {
            if let Ok(sessions) = task.await {
                let _ = this.update(cx, |view, cx| {
                    view.sessions = sessions;
                    // A running turn or session view compares against a new snapshot.
                    if view.mode != Mode::WorkingTree {
                        view.request_reload(Dirty::All, cx);
                    }
                    cx.notify();
                });
            }
        })
        .detach();
    }

    pub fn mode(&self) -> &Mode {
        &self.mode
    }

    fn set_mode(&mut self, mode: Mode, cx: &mut Context<Self>) {
        if self.mode == mode {
            return;
        }
        self.mode = mode;
        self.cursor = 0;
        self.select_anchor = None;
        self.diff_scroll
            .scroll_to_item_strict(0, ScrollStrategy::Top);
        self.request_reload(Dirty::All, cx);
        cx.notify();
    }

    fn show_working_tree(&mut self, _: &ShowWorkingTree, _: &mut Window, cx: &mut Context<Self>) {
        self.set_mode(Mode::WorkingTree, cx);
    }

    /// Turns in timeline order (oldest first) across the current session.
    fn step_turn(&mut self, delta: isize, cx: &mut Context<Self>) {
        let (session, turn) = match &self.mode {
            Mode::Turn { session, turn } => (session.clone(), *turn as isize + delta),
            // From the working tree or a session view, `<` goes to the latest turn.
            _ => match self.sessions.first() {
                Some(s) if !s.turns.is_empty() => (s.id.clone(), s.turns.len() as isize - 1),
                _ => {
                    self.set_status(
                        "No agent turns recorded yet (goro hooks install)",
                        false,
                        cx,
                    );
                    return;
                }
            },
        };
        let count = self
            .sessions
            .iter()
            .find(|s| s.id == session)
            .map_or(0, |s| s.turns.len()) as isize;
        if (0..count).contains(&turn) {
            self.set_mode(
                Mode::Turn {
                    session,
                    turn: turn as usize,
                },
                cx,
            );
        }
    }

    fn prev_turn(&mut self, _: &PrevTurn, _: &mut Window, cx: &mut Context<Self>) {
        self.step_turn(-1, cx);
    }

    fn next_turn(&mut self, _: &NextTurn, _: &mut Window, cx: &mut Context<Self>) {
        self.step_turn(1, cx);
    }

    fn open_timeline(&mut self, _: &OpenTimeline, window: &mut Window, cx: &mut Context<Self>) {
        let mut items = vec![PickerItem {
            title: "Working tree".into(),
            detail: "staged, unstaged and untracked changes".into(),
            ..Default::default()
        }];
        let mut modes = vec![Mode::WorkingTree];
        for session in &self.sessions {
            let short: String = session.id.chars().take(8).collect();
            items.push(PickerItem {
                title: format!("Whole session ({} turns)", session.turns.len()),
                detail: format!("{} · session {short}", session.agent),
                tag: None,
                note: Some(time_label(session.last_at_ms)),
            });
            modes.push(Mode::Session {
                session: session.id.clone(),
            });
            for (ix, turn) in session.turns.iter().enumerate().rev() {
                let at = turn
                    .start
                    .as_ref()
                    .or(turn.end.as_ref())
                    .map_or(0, |s| s.meta.at_ms);
                items.push(PickerItem {
                    title: format!("  Turn {}", ix + 1),
                    detail: turn
                        .prompt
                        .as_deref()
                        .map(first_line)
                        .unwrap_or_else(|| "(no prompt recorded)".into()),
                    tag: turn.end.is_none().then(|| "running".into()),
                    note: Some(time_label(at)),
                });
                modes.push(Mode::Turn {
                    session: session.id.clone(),
                    turn: ix,
                });
            }
        }
        if self.sessions.is_empty() {
            self.set_status(
                "No agent turns recorded yet: run `goro hooks install`, then let an agent work",
                false,
                cx,
            );
        }
        self.show_picker(
            "Review a turn…",
            items,
            PickerPurpose::Turns(modes),
            window,
            cx,
        );
    }

    fn show_picker(
        &mut self,
        placeholder: &str,
        items: Vec<PickerItem>,
        purpose: PickerPurpose,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Entity<Picker> {
        let appearance = self.appearance;
        let picker = cx.new(|cx| Picker::new(placeholder, items, appearance, cx));
        cx.subscribe_in(
            &picker,
            window,
            |this, _, event: &PickerEvent, window, cx| {
                let Some((_, purpose)) = this.picker.take() else {
                    return;
                };
                window.focus(&this.focus_handle, cx);
                match (event, purpose) {
                    (PickerEvent::Pick(ix), PickerPurpose::Repos(roots)) => {
                        if let Some(root) = roots.get(*ix).cloned() {
                            // After this update: `open_repo` reads every window, including this one.
                            cx.defer(move |cx| {
                                crate::open_repo(Some(root), cx);
                            });
                        }
                    }
                    (PickerEvent::Typed(text), PickerPurpose::Repos(_)) => {
                        let path = PathBuf::from(text);
                        if path.is_dir() {
                            cx.defer(move |cx| {
                                crate::open_repo(Some(path), cx);
                            });
                        }
                    }
                    (PickerEvent::Pick(ix), PickerPurpose::Turns(modes)) => {
                        if let Some(mode) = modes.get(*ix).cloned() {
                            this.set_mode(mode, cx);
                        }
                    }
                    _ => {}
                }
                cx.notify();
            },
        )
        .detach();
        window.focus(&picker.focus_handle(cx), cx);
        self.picker = Some((picker.clone(), purpose));
        cx.notify();
        picker
    }

    /// Lines a new comment covers: the selection, else the line (or hunk) at the cursor.
    fn comment_target(&self) -> Option<(usize, Vec<usize>)> {
        let review = self.review()?;
        if self.select_anchor.is_none()
            && let Some(Row::Line { file, line }) = review.rows().get(self.cursor)
        {
            return Some((*file, vec![*line]));
        }
        match review.action_target(self.cursor, self.select_anchor)? {
            (file, Target::Lines(lines)) => Some((file, lines)),
            (_, Target::WholeFile) => None,
        }
    }

    fn add_comment(&mut self, _: &AddComment, window: &mut Window, cx: &mut Context<Self>) {
        let Some((file, lines)) = self.comment_target() else {
            self.set_status(
                "Put the cursor on a line (or select lines) to comment",
                false,
                cx,
            );
            return;
        };
        let review = self.review().expect("a target implies a review");
        let path = review.files[file].change.path_lossy().into_owned();
        let Some(diff) = review.files[file].diff() else {
            return;
        };
        let location = Comment::on_lines(0, &path, diff, &lines, String::new())
            .map(|c| c.location())
            .unwrap_or(path);
        self.open_draft(None, file, lines, location, "", window, cx);
    }

    fn edit_comment(&mut self, _: &EditComment, window: &mut Window, cx: &mut Context<Self>) {
        let Some(review) = self.review() else {
            return;
        };
        let Some(Row::Comment { file, comment, .. }) = review.rows().get(self.cursor).copied()
        else {
            return;
        };
        let comment = review.comments()[comment].clone();
        self.open_draft(
            Some(comment.id),
            file,
            Vec::new(),
            comment.location(),
            &comment.text,
            window,
            cx,
        );
    }

    #[allow(clippy::too_many_arguments)]
    fn open_draft(
        &mut self,
        editing: Option<u64>,
        file: usize,
        lines: Vec<usize>,
        location: String,
        text: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let editor = cx.new(|cx| {
            let mut editor = TextEditor::new("What should change here?", cx);
            editor.set_text(text, cx);
            editor
        });
        window.focus(&editor.focus_handle(cx), cx);
        self.draft = Some(Draft {
            editor,
            editing,
            file,
            lines,
            location,
        });
        cx.notify();
    }

    fn save_comment(&mut self, _: &SaveComment, window: &mut Window, cx: &mut Context<Self>) {
        let Some(draft) = self.draft.take() else {
            return;
        };
        window.focus(&self.focus_handle, cx);
        let text = draft.editor.read(cx).text().trim().to_string();
        let State::Ready(review) = &mut self.state else {
            return;
        };
        let mut comments = review.comments().to_vec();
        match draft.editing {
            Some(id) if text.is_empty() => comments.retain(|c| c.id != id),
            Some(id) => {
                if let Some(c) = comments.iter_mut().find(|c| c.id == id) {
                    c.text = text;
                }
            }
            None if text.is_empty() => {}
            None => {
                let id = comments.iter().map(|c| c.id).max().unwrap_or(0) + 1;
                let entry = &review.files[draft.file];
                if let Some(diff) = entry.diff()
                    && let Some(comment) =
                        Comment::on_lines(id, &entry.change.path_lossy(), diff, &draft.lines, text)
                {
                    comments.push(comment);
                }
            }
        }
        self.rebuild_keeping_position(move |review| {
            review.set_comments(comments);
            review.rebuild_rows();
        });
        self.save_state(cx);
        cx.notify();
    }

    fn cancel_comment(&mut self, _: &CancelComment, window: &mut Window, cx: &mut Context<Self>) {
        self.draft = None;
        window.focus(&self.focus_handle, cx);
        cx.notify();
    }

    /// Delete the comment under the cursor. Returns whether there was one.
    fn delete_comment_at_cursor(&mut self, cx: &mut Context<Self>) -> bool {
        let Some(Row::Comment { comment, .. }) = self
            .review()
            .and_then(|r| r.rows().get(self.cursor).copied())
        else {
            return false;
        };
        let State::Ready(review) = &mut self.state else {
            return false;
        };
        let mut comments = review.comments().to_vec();
        comments.remove(comment);
        self.rebuild_keeping_position(move |review| {
            review.set_comments(comments);
            review.rebuild_rows();
        });
        self.save_state(cx);
        self.set_status("Comment deleted", false, cx);
        true
    }

    fn copy_comments(&mut self, _: &CopyComments, _: &mut Window, cx: &mut Context<Self>) {
        let Some(review) = self.review() else {
            return;
        };
        let count = review.comments().len();
        cx.write_to_clipboard(gpui_kit::ClipboardItem::new_string(to_markdown(
            review.comments(),
        )));
        self.set_status(format!("Copied {count} comment(s) as markdown"), false, cx);
    }

    pub fn add_waiter(&mut self, reply: oneshot::Sender<String>, cx: &mut Context<Self>) {
        self.waiters.push(reply);
        self.set_status(
            "An agent is waiting for your review: ⌘⇧↵ sends your comments",
            false,
            cx,
        );
        cx.notify();
    }

    pub fn is_waited_on(&self) -> bool {
        self.waiters.iter().any(|w| !w.is_canceled())
    }

    fn send_review(&mut self, _: &SendReview, _: &mut Window, cx: &mut Context<Self>) {
        self.waiters.retain(|w| !w.is_canceled());
        if self.waiters.is_empty() {
            self.set_status(
                "No agent is waiting (`goro --wait`); press y to copy comments",
                false,
                cx,
            );
            return;
        }
        let Some(review) = self.review() else {
            return;
        };
        let markdown = to_markdown(review.comments());
        let count = review.comments().len();
        for waiter in self.waiters.drain(..) {
            let _ = waiter.send(markdown.clone());
        }
        self.rebuild_keeping_position(|review| {
            review.set_comments(Vec::new());
            review.rebuild_rows();
        });
        self.save_state(cx);
        self.set_status(format!("Sent {count} comment(s) to the agent"), false, cx);
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
        // A look at a turn's snapshots isn't a look at the working tree.
        if self.mode != Mode::WorkingTree {
            return;
        }
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
            comments: review.comments().to_vec(),
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
        let recent: Vec<PathBuf> = self
            .store
            .as_ref()
            .map(Store::recent)
            .unwrap_or_default()
            .into_iter()
            .map(|r| r.root)
            .filter(|root| root.is_dir())
            .collect();
        let current = self.root().map(Path::to_path_buf);
        let item = move |root: &PathBuf, agent: bool| PickerItem {
            title: root
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| root.to_string_lossy().into_owned()),
            detail: root.to_string_lossy().into_owned(),
            tag: agent.then(|| "agent".into()),
            note: (current.as_ref() == Some(root)).then(|| "open".into()),
        };
        let items = recent.iter().map(|r| item(r, false)).collect();
        let picker = self.show_picker(
            "Open repository…",
            items,
            PickerPurpose::Repos(recent.clone()),
            window,
            cx,
        );
        // Agent-active repositories and change counts arrive from a background scan.
        let task = cx.background_executor().spawn(async move {
            let mut roots: Vec<(PathBuf, bool)> = Vec::new();
            for activity in goro_core::detect::agent_activity(
                &goro_core::detect::AgentLogs::default_locations(),
            ) {
                let Some(dir) = activity.cwd.ancestors().find(|d| d.is_dir()) else {
                    continue;
                };
                if let Ok(repo) = Repo::discover(dir) {
                    let root = repo.root().to_path_buf();
                    if !roots.iter().any(|(r, _)| *r == root) {
                        roots.push((root, true));
                    }
                }
            }
            for root in recent {
                if !roots.iter().any(|(r, _)| *r == root) {
                    roots.push((root, false));
                }
            }
            roots
                .into_iter()
                .map(|(root, agent)| {
                    let count = Repo::discover(&root)
                        .and_then(|r| r.status())
                        .ok()
                        .map(|c| c.len());
                    (root, agent, count)
                })
                .collect::<Vec<_>>()
        });
        cx.spawn(async move |this, cx| {
            let scanned = task.await;
            let _ = this.update(cx, |view, cx| {
                let Some((current_picker, PickerPurpose::Repos(roots))) = &mut view.picker else {
                    return;
                };
                if *current_picker != picker {
                    return;
                }
                *roots = scanned.iter().map(|(r, _, _)| r.clone()).collect();
                let items = scanned
                    .iter()
                    .map(|(root, agent, count)| {
                        let mut i = item(root, *agent);
                        if i.note.is_none() {
                            i.note = count.map(|n| match n {
                                0 => "clean".to_string(),
                                1 => "1 change".to_string(),
                                n => format!("{n} changes"),
                            });
                        }
                        i
                    })
                    .collect();
                picker.update(cx, |p, cx| p.set_items(items, cx));
            });
        })
        .detach();
    }

    pub fn session_count(&self) -> usize {
        self.sessions.len()
    }

    pub fn set_cursor(&mut self, row: usize, cx: &mut Context<Self>) {
        self.cursor = row;
        self.select_anchor = None;
        cx.notify();
    }

    pub fn picker_open(&self) -> bool {
        self.picker.is_some()
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
        let show_new = self.mode == Mode::WorkingTree;
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
                    .when(show_new && review.file_new_count(file) > 0, |el| {
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
                            is_new: this.mode == Mode::WorkingTree && review.is_new(ix),
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

    fn render_wait_banner(
        &self,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> Option<gpui_kit::AnyElement> {
        if !self.is_waited_on() {
            return None;
        }
        let count = self.review().map_or(0, |r| r.comments().len());
        Some(
            div()
                .flex()
                .items_center()
                .gap_3()
                .px_3()
                .py_1()
                .bg(theme.hunk_bg)
                .border_b_1()
                .border_color(theme.border)
                .child(div().flex_1().child(format!(
                    "An agent is waiting for your review · {count} comment(s)"
                )))
                .child(diff_rows::button(
                    "send-review",
                    "Send review ⌘⇧↵",
                    theme,
                    cx.listener(|this, _: &ClickEvent, window, cx| {
                        this.send_review(&SendReview, window, cx)
                    }),
                ))
                .into_any_element(),
        )
    }

    fn render_mode_bar(&self, theme: &Theme) -> Option<gpui_kit::AnyElement> {
        let (title, detail) = match &self.mode {
            Mode::WorkingTree => return None,
            Mode::Turn { session, turn } => {
                let s = self.sessions.iter().find(|s| &s.id == session)?;
                let t = s.turns.get(*turn)?;
                let at = t
                    .start
                    .as_ref()
                    .or(t.end.as_ref())
                    .map_or(0, |x| x.meta.at_ms);
                (
                    format!("Turn {} of {} · {}", turn + 1, s.turns.len(), s.agent),
                    format!(
                        "“{}” · {}{}",
                        t.prompt.as_deref().map(first_line).unwrap_or_default(),
                        time_label(at),
                        if t.end.is_none() { " · running" } else { "" }
                    ),
                )
            }
            Mode::Session { session } => {
                let s = self.sessions.iter().find(|s| &s.id == session)?;
                (
                    format!("Whole session · {} turns · {}", s.turns.len(), s.agent),
                    format!("start of session → now · {}", time_label(s.last_at_ms)),
                )
            }
        };
        Some(
            div()
                .flex()
                .items_center()
                .gap_3()
                .px_3()
                .py_1()
                .bg(theme.file_header_bg)
                .border_b_1()
                .border_color(theme.border)
                .whitespace_nowrap()
                .overflow_hidden()
                .child(div().font_weight(FontWeight::BOLD).child(title))
                .child(div().flex_1().text_color(theme.muted).child(detail))
                .child(
                    div()
                        .text_color(theme.muted)
                        .child("< > turns · t timeline · w working tree"),
                )
                .into_any_element(),
        )
    }

    fn render_draft(&self, theme: &Theme) -> Option<gpui_kit::AnyElement> {
        let draft = self.draft.as_ref()?;
        let editor = draft.editor.clone();
        Some(
            div()
                .key_context("CommentBox")
                .flex()
                .flex_col()
                .gap_1()
                .p_2()
                .border_t_1()
                .border_color(theme.border)
                .bg(theme.sidebar_bg)
                .child(
                    div()
                        .flex()
                        .gap_2()
                        .child(div().font_weight(FontWeight::BOLD).child(format!(
                            "{} comment on {}",
                            if draft.editing.is_some() {
                                "Edit"
                            } else {
                                "New"
                            },
                            draft.location
                        )))
                        .child(
                            div()
                                .text_color(theme.muted)
                                .child("⌘↵ save · esc cancel · empty deletes"),
                        ),
                )
                .child(
                    div()
                        .h(px(ROW_HEIGHT * 4.0 + 8.0))
                        .p_1()
                        .rounded_sm()
                        .bg(theme.bg)
                        .border_1()
                        .border_color(theme.border)
                        .child(editor),
                )
                .into_any_element(),
        )
    }

    fn render_status_bar(&self, theme: &Theme) -> impl IntoElement {
        const MAX_ERROR_LINES: usize = 12;
        // "New since last look" is about the working tree, not a turn's snapshots.
        let new_count = match self.mode {
            Mode::WorkingTree => self.review().map_or(0, Review::new_count),
            _ => 0,
        };
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
        if let Some(draft) = &self.draft {
            draft
                .editor
                .update(cx, |editor, _| editor.set_colors(colors));
        }
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
            .on_action(cx.listener(Self::show_working_tree))
            .on_action(cx.listener(Self::open_timeline))
            .on_action(cx.listener(Self::prev_turn))
            .on_action(cx.listener(Self::next_turn))
            .on_action(cx.listener(Self::add_comment))
            .on_action(cx.listener(Self::edit_comment))
            .on_action(cx.listener(Self::save_comment))
            .on_action(cx.listener(Self::cancel_comment))
            .on_action(cx.listener(Self::copy_comments))
            .on_action(cx.listener(Self::send_review))
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
                    // Don't grow to the widest diff line; the list scrolls horizontally.
                    .min_w(px(0.0))
                    .h_full()
                    .when_some(self.render_wait_banner(&theme, cx), |el, banner| {
                        el.child(banner)
                    })
                    .when_some(self.render_mode_bar(&theme), |el, bar| el.child(bar))
                    .child(self.render_diff(&theme, cx))
                    .when_some(self.render_draft(&theme), |el, draft| el.child(draft))
                    .child(self.render_status_bar(&theme)),
            )
            .when_some(
                self.picker.as_ref().map(|(p, _)| p.clone()),
                |el, switcher| {
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
                },
            )
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
        Section::Snapshot => "CHANGES",
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

/// "3m ago"-style label for a Unix time in milliseconds.
pub(crate) fn time_label(at_ms: u64) -> String {
    let now = goro_core::turns::SnapshotMeta::now_ms();
    let secs = now.saturating_sub(at_ms) / 1000;
    match secs {
        0..60 => "just now".into(),
        60..3600 => format!("{}m ago", secs / 60),
        3600..86_400 => format!("{}h ago", secs / 3600),
        _ => format!("{}d ago", secs / 86_400),
    }
}

fn first_line(text: &str) -> String {
    let line = text.lines().next().unwrap_or_default().trim();
    if line.chars().count() > 80 {
        format!("{}…", line.chars().take(80).collect::<String>())
    } else {
        line.to_string()
    }
}
