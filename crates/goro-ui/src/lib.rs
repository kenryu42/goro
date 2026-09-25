//! Goro's window: a file tree beside one continuous, virtualized diff stream.

mod theme;

use std::path::PathBuf;
use std::time::{Duration, Instant};

use futures::StreamExt;
use futures::channel::mpsc::UnboundedReceiver;
use goro_core::diff::LineKind;
use goro_core::repo::{FileChange, Section};
use goro_core::review::{FileLoad, Note, Review, Row, TreeRow, display_line};
use goro_core::syntax::spans_in;
use gpui_kit::{
    App, Bounds, Context, FocusHandle, Focusable, FontWeight, HighlightStyle, KeyBinding,
    ListHorizontalSizingBehavior, Menu, MenuItem, ScrollStrategy, SharedString, StyledText,
    TitlebarOptions, UniformListScrollHandle, Window, WindowAppearance, WindowBounds,
    WindowOptions, actions, div, prelude::*, px, size, uniform_list,
};

use theme::Theme;

/// Messages from the background loader to the window.
pub enum Event {
    /// The repository was read. `first` is the first file, loaded before first paint.
    Opened {
        root: PathBuf,
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
        NextHunk,
        PrevHunk,
        NextFile,
        PrevFile
    ]
);

const ROW_HEIGHT: f32 = 20.0;
const SIDEBAR_WIDTH: f32 = 300.0;
const GUTTER_DIGITS: usize = 5;

#[cfg(target_os = "macos")]
const MONO_FONT: &str = "Menlo";
#[cfg(target_os = "windows")]
const MONO_FONT: &str = "Consolas";
#[cfg(not(any(target_os = "macos", target_os = "windows")))]
const MONO_FONT: &str = "DejaVu Sans Mono";

/// How long to hold the window for the review, so the first frame already shows the diff
/// instead of an empty window. Slow repositories open immediately and stream in.
const OPEN_WAIT: Duration = Duration::from_millis(250);

pub fn run(startup: Startup, mut events: UnboundedReceiver<Event>) {
    gpui_kit::application().run(move |cx: &mut App| {
        startup.mark("platform ready");
        cx.on_action(|_: &Quit, cx| cx.quit());
        cx.bind_keys([
            KeyBinding::new("cmd-q", Quit, None),
            KeyBinding::new("ctrl-q", Quit, None),
            KeyBinding::new("cmd-w", CloseWindow, None),
            KeyBinding::new("ctrl-w", CloseWindow, None),
            KeyBinding::new("j", CursorDown, Some("Goro")),
            KeyBinding::new("down", CursorDown, Some("Goro")),
            KeyBinding::new("k", CursorUp, Some("Goro")),
            KeyBinding::new("up", CursorUp, Some("Goro")),
            KeyBinding::new("n", NextHunk, Some("Goro")),
            KeyBinding::new("p", PrevHunk, Some("Goro")),
            KeyBinding::new("]", NextFile, Some("Goro")),
            KeyBinding::new("[", PrevFile, Some("Goro")),
        ]);
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

pub struct GoroView {
    state: State,
    cursor: usize,
    diff_scroll: UniformListScrollHandle,
    tree_scroll: UniformListScrollHandle,
    focus_handle: FocusHandle,
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
        let mut view = Self {
            state: State::Loading,
            cursor: 0,
            diff_scroll: UniformListScrollHandle::new(),
            tree_scroll: UniformListScrollHandle::new(),
            focus_handle: cx.focus_handle(),
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
                    root,
                    changes,
                    first,
                } => {
                    let mut review = Review::new(root, changes);
                    if let Some(first) = first {
                        review.set_loaded(0, first);
                    }
                    review.rebuild_rows();
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
        if rebuild && let State::Ready(review) = &mut self.state {
            // Keep the row at the top of the viewport stable while rows are inserted.
            let top = top_row(&self.diff_scroll);
            let anchor = review.rows().get(top).copied();
            let cursor = review.rows().get(self.cursor).copied();
            review.rebuild_rows();
            if let Some(ix) = anchor.and_then(|row| find_row(review, row))
                && ix != top
            {
                self.diff_scroll
                    .scroll_to_item_strict(ix, ScrollStrategy::Top);
            }
            self.cursor = cursor
                .and_then(|row| find_row(review, row))
                .unwrap_or(self.cursor)
                .min(review.rows().len().saturating_sub(1));
        }
    }

    fn move_cursor(&mut self, to: Option<usize>, strategy: ScrollStrategy, cx: &mut Context<Self>) {
        if let Some(to) = to {
            self.cursor = to;
            self.diff_scroll.scroll_to_item(to, strategy);
            cx.notify();
        }
    }

    pub fn review(&self) -> Option<&Review> {
        match &self.state {
            State::Ready(review) => Some(review),
            _ => None,
        }
    }

    fn cursor_down(&mut self, _: &CursorDown, _: &mut Window, cx: &mut Context<Self>) {
        let to = self
            .review()
            .map(|r| (self.cursor + 1).min(r.rows().len().saturating_sub(1)));
        self.move_cursor(to, ScrollStrategy::Nearest, cx);
    }

    fn cursor_up(&mut self, _: &CursorUp, _: &mut Window, cx: &mut Context<Self>) {
        let to = self.review().map(|_| self.cursor.saturating_sub(1));
        self.move_cursor(to, ScrollStrategy::Nearest, cx);
    }

    fn next_hunk(&mut self, _: &NextHunk, _: &mut Window, cx: &mut Context<Self>) {
        let to = self.review().and_then(|r| r.next_hunk_row(self.cursor));
        self.move_cursor(to, ScrollStrategy::Top, cx);
    }

    fn prev_hunk(&mut self, _: &PrevHunk, _: &mut Window, cx: &mut Context<Self>) {
        let to = self.review().and_then(|r| r.prev_hunk_row(self.cursor));
        self.move_cursor(to, ScrollStrategy::Top, cx);
    }

    fn next_file(&mut self, _: &NextFile, _: &mut Window, cx: &mut Context<Self>) {
        let to = self.review().and_then(|r| r.next_file_row(self.cursor));
        self.move_cursor(to, ScrollStrategy::Top, cx);
    }

    fn prev_file(&mut self, _: &PrevFile, _: &mut Window, cx: &mut Context<Self>) {
        let to = self.review().and_then(|r| r.prev_file_row(self.cursor));
        self.move_cursor(to, ScrollStrategy::Top, cx);
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

    fn render_diff(
        &self,
        theme: &Theme,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> gpui_kit::AnyElement {
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
            cx.processor(|this, range: std::ops::Range<usize>, window, _cx| {
                let theme = this.theme(window);
                let Some(review) = this.review() else {
                    return Vec::new();
                };
                range
                    .map(|ix| render_diff_row(review, ix, ix == this.cursor, &theme))
                    .collect()
            }),
        )
        .track_scroll(&self.diff_scroll)
        .with_horizontal_sizing_behavior(ListHorizontalSizingBehavior::Unconstrained)
        .with_width_from_item(review.widest_row())
        .size_full();
        let _ = window;
        div()
            .relative()
            .flex_1()
            .h_full()
            .overflow_hidden()
            .child(list)
            .when_some(sticky, |el, file| {
                el.child(
                    div()
                        .absolute()
                        .top_0()
                        .left_0()
                        .right_0()
                        .child(file_header(review, file, theme)),
                )
            })
            .into_any_element()
    }
}

impl GoroView {
    pub fn cursor(&self) -> usize {
        self.cursor
    }

    /// Force light or dark; `None` follows the OS.
    pub fn set_appearance(&mut self, appearance: Option<WindowAppearance>, cx: &mut Context<Self>) {
        self.appearance = appearance;
        cx.notify();
    }

    fn theme(&self, window: &Window) -> Theme {
        Theme::for_appearance(self.appearance.unwrap_or_else(|| window.appearance()))
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
        if let State::Ready(review) = &self.state {
            if let Some(name) = review.root.file_name() {
                window.set_window_title(&format!("{} — Goro", name.to_string_lossy()));
            }
            self.report_first_paint(cx);
        } else if matches!(self.state, State::Failed(_)) {
            self.report_first_paint(cx);
        }
        div()
            .key_context("Goro")
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(Self::cursor_down))
            .on_action(cx.listener(Self::cursor_up))
            .on_action(cx.listener(Self::next_hunk))
            .on_action(cx.listener(Self::prev_hunk))
            .on_action(cx.listener(Self::next_file))
            .on_action(cx.listener(Self::prev_file))
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
            .child(self.render_diff(&theme, window, cx))
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

fn find_row(review: &Review, row: Row) -> Option<usize> {
    let start = review.file_row(row.file());
    review.rows()[start..]
        .iter()
        .take_while(|r| r.file() == row.file())
        .position(|r| *r == row)
        .map(|offset| start + offset)
}

fn indent(depth: usize) -> gpui_kit::Pixels {
    px(12.0 + depth as f32 * 14.0)
}

fn section_label(section: Section) -> &'static str {
    match section {
        Section::Staged => "STAGED",
        Section::Unstaged => "UNSTAGED",
        Section::Untracked => "UNTRACKED",
    }
}

fn centered_message(message: &str, theme: &Theme) -> gpui_kit::AnyElement {
    div()
        .flex_1()
        .h_full()
        .flex()
        .items_center()
        .justify_center()
        .text_color(theme.muted)
        .child(SharedString::from(message.to_string()))
        .into_any_element()
}

fn file_header(review: &Review, file: usize, theme: &Theme) -> gpui_kit::Div {
    let change = &review.files[file].change;
    let (badge, color) = theme.status(change.status);
    let path = match &change.old_path {
        Some(old) => format!("{old} → {}", change.path),
        None => change.path_lossy().into_owned(),
    };
    div()
        .h(px(ROW_HEIGHT * 1.5))
        .w_full()
        .flex()
        .items_center()
        .gap_2()
        .px_3()
        .bg(theme.file_header_bg)
        .border_b_1()
        .border_t_1()
        .border_color(theme.border)
        .whitespace_nowrap()
        .child(
            div()
                .text_color(color)
                .font_weight(FontWeight::BOLD)
                .child(badge),
        )
        .child(div().font_weight(FontWeight::BOLD).child(path))
        .child(
            div()
                .text_color(theme.muted)
                .child(section_label(change.section).to_lowercase()),
        )
}

fn render_diff_row(
    review: &Review,
    ix: usize,
    is_cursor: bool,
    theme: &Theme,
) -> gpui_kit::AnyElement {
    let row = review.rows()[ix];
    let el = match row {
        Row::File { file } => file_header(review, file, theme).h(px(ROW_HEIGHT)),
        Row::Hunk { file, hunk } => {
            let diff = review.files[file].diff().expect("hunk rows have diffs");
            let h = &diff.hunks[hunk];
            let mut header = format!(
                "@@ -{},{} +{},{} @@",
                h.old_start, h.old_len, h.new_start, h.new_len
            );
            if let Some(func) = diff.func_context_bytes(h) {
                header.push(' ');
                header.push_str(&String::from_utf8_lossy(func));
            }
            div()
                .h(px(ROW_HEIGHT))
                .w_full()
                .flex()
                .items_center()
                .pl(gutter_width())
                .bg(theme.hunk_bg)
                .text_color(theme.hunk_fg)
                .whitespace_nowrap()
                .child(header)
        }
        Row::Line { file, line } => render_line(review, file, line, theme),
        Row::Note { note, .. } => div()
            .h(px(ROW_HEIGHT))
            .w_full()
            .flex()
            .items_center()
            .pl(gutter_width())
            .text_color(theme.muted)
            .whitespace_nowrap()
            .child(note_text(note)),
    };
    // An overlay bar, so the cursor neither shifts content nor recolors row borders.
    el.relative()
        .when(is_cursor, |el| {
            el.child(
                div()
                    .absolute()
                    .left_0()
                    .top_0()
                    .bottom_0()
                    .w(px(3.0))
                    .bg(theme.accent),
            )
        })
        .into_any_element()
}

fn gutter_width() -> gpui_kit::Pixels {
    // Two line-number columns plus the sign column, in monospace cells.
    px((GUTTER_DIGITS * 2 + 3) as f32 * 7.6)
}

fn note_text(note: Note) -> String {
    match note {
        Note::Loading => "Loading…".into(),
        Note::Binary { old_len, new_len } => {
            format!("Binary file changed ({old_len} → {new_len} bytes)")
        }
        Note::TooLarge { len } => format!("File too large to diff ({len} bytes)"),
        Note::Submodule => "Submodule changed".into(),
        Note::Conflict => "Unresolved merge conflict".into(),
        Note::NotAFile => "Not a regular file".into(),
        Note::NoContentChange => "No content changes (mode or rename only)".into(),
        Note::Failed => "Failed to load this file".into(),
    }
}

fn render_line(review: &Review, file: usize, line: usize, theme: &Theme) -> gpui_kit::Div {
    let entry = &review.files[file];
    let diff = entry.diff().expect("line rows have diffs");
    let l = &diff.lines[line];
    let spans = entry.highlights.as_ref().map(|h| match l.kind {
        LineKind::Removed => &h.old,
        _ => &h.new,
    });
    let bytes = diff.line_bytes(l);
    let (text, spans) = display_line(
        bytes,
        spans
            .into_iter()
            .flat_map(|spans| spans_in(spans, l.content.clone())),
    );
    let highlights: Vec<_> = spans
        .into_iter()
        .map(|(range, style)| {
            (
                range,
                HighlightStyle {
                    color: Some(theme.syntax(style)),
                    ..Default::default()
                },
            )
        })
        .collect();
    let (bg, gutter_bg, sign) = match l.kind {
        LineKind::Context => (None, None, " "),
        LineKind::Added => (Some(theme.added_bg), Some(theme.added_gutter_bg), "+"),
        LineKind::Removed => (Some(theme.removed_bg), Some(theme.removed_gutter_bg), "-"),
    };
    let number = |n: Option<u32>| {
        n.map(|n| format!("{n:>width$}", width = GUTTER_DIGITS))
            .unwrap_or_else(|| " ".repeat(GUTTER_DIGITS))
    };
    let gutter = format!("{} {} {sign} ", number(l.old_no), number(l.new_no));
    let content: SharedString = text.into();
    let mut content_el = StyledText::new(content);
    if !highlights.is_empty() {
        content_el = content_el.with_highlights(highlights);
    }
    div()
        .h(px(ROW_HEIGHT))
        .w_full()
        .flex()
        .flex_row()
        .whitespace_nowrap()
        .when_some(bg, |el, bg| el.bg(bg))
        .child(
            div()
                .flex_none()
                .w(gutter_width())
                .text_color(theme.muted)
                .when_some(gutter_bg, |el, bg| el.bg(bg))
                .child(gutter),
        )
        .child(div().pl_1().child(content_el))
        .when(l.no_newline, |el| {
            el.child(div().pl_2().text_color(theme.muted).child("⏎̸"))
        })
}
