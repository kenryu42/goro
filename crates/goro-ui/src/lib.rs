//! Goro's app: one window per repository, each a file tree and commit box beside one
//! continuous, virtualized diff stream.

mod diff_rows;
mod switcher;
mod text_buffer;
mod text_editor;
mod theme;
mod view;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use futures::StreamExt;
use futures::channel::mpsc::{UnboundedReceiver, UnboundedSender, unbounded};
use goro_core::detect::{AgentLogs, detect_repo};
use goro_core::repo::{FileChange, Repo};
use goro_core::review::{FileLoad, load_file, load_files};
use goro_core::store::{RepoState, Store};
use gpui_kit::{
    App, AppContext, Bounds, Focusable, KeyBinding, Menu, MenuItem, TitlebarOptions, WindowBounds,
    WindowHandle, WindowOptions, actions, px, size,
};

pub use view::GoroView;

/// Messages from a background loader to its window.
pub enum Event {
    /// The repository was read. `first` is the first file, loaded before first paint.
    Opened {
        repo: Arc<Repo>,
        changes: Vec<FileChange>,
        first: Option<FileLoad>,
        state: RepoState,
    },
    Loaded(usize, FileLoad),
    /// No repository was given and none could be detected.
    NoRepository,
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
        /// Next line that changed since the last look.
        NextNew,
        PrevNew,
        /// Treat everything currently shown as seen.
        MarkSeen,
        /// Mark the hunk (or, on a file header, the file) reviewed, or unmark it.
        ToggleReviewed,
        /// Stage the target, or unstage it if it's already staged.
        Stage,
        StageFile,
        Discard,
        DiscardFile,
        UndoLast,
        FocusCommit,
        FocusDiff,
        Commit,
        OpenSwitcher
    ]
);

/// Keys that only apply while the diff (not a text field) has focus.
const DIFF_CONTEXT: &str = "Goro && !TextEditor";

/// How long to hold the first window for its review, so the first frame already shows the
/// diff instead of an empty window. Slow repositories open immediately and stream in.
const OPEN_WAIT: Duration = Duration::from_millis(250);

pub fn bind_keys(cx: &mut App) {
    let diff = Some(DIFF_CONTEXT);
    cx.bind_keys([
        KeyBinding::new("cmd-q", Quit, None),
        KeyBinding::new("ctrl-q", Quit, None),
        KeyBinding::new("cmd-w", CloseWindow, None),
        KeyBinding::new("ctrl-w", CloseWindow, None),
        KeyBinding::new("cmd-p", OpenSwitcher, Some("Goro && !Switcher")),
        KeyBinding::new("ctrl-p", OpenSwitcher, Some("Goro && !Switcher")),
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
        KeyBinding::new("tab", NextNew, diff),
        KeyBinding::new("shift-tab", PrevNew, diff),
        KeyBinding::new("m", MarkSeen, diff),
        KeyBinding::new("r", ToggleReviewed, diff),
        KeyBinding::new("s", Stage, diff),
        KeyBinding::new("shift-s", StageFile, diff),
        KeyBinding::new("x", Discard, diff),
        KeyBinding::new("shift-x", DiscardFile, diff),
        KeyBinding::new("u", UndoLast, diff),
        KeyBinding::new("cmd-z", UndoLast, diff),
        KeyBinding::new("ctrl-z", UndoLast, diff),
        KeyBinding::new("c", FocusCommit, diff),
        KeyBinding::new("escape", FocusDiff, Some("CommitBox > TextEditor")),
        KeyBinding::new("cmd-enter", Commit, Some("Goro && !Switcher")),
        KeyBinding::new("ctrl-enter", Commit, Some("Goro && !Switcher")),
    ]);
    text_editor::bind_keys(cx);
    switcher::bind_keys(cx);
}

/// Load a repository on a background thread: `target`, or the detected one when `None`.
/// Events stream to the returned receiver; the first file is loaded before `Opened`.
pub fn spawn_loader(
    target: Option<PathBuf>,
    store: Option<Store>,
    startup: Startup,
) -> UnboundedReceiver<Event> {
    let (tx, rx) = unbounded();
    std::thread::Builder::new()
        .name("goro-loader".into())
        .spawn(move || load_repository(target, store, tx, startup))
        .expect("failed to spawn loader thread");
    rx
}

fn load_repository(
    target: Option<PathBuf>,
    store: Option<Store>,
    tx: UnboundedSender<Event>,
    startup: Startup,
) {
    let target = target.or_else(|| {
        let recent = store.as_ref().map(Store::recent).unwrap_or_default();
        let detected = detect_repo(&AgentLogs::default_locations(), &recent);
        startup.mark("repo detected");
        detected
    });
    let Some(target) = target else {
        let _ = tx.unbounded_send(Event::NoRepository);
        return;
    };
    let repo = match Repo::discover(&target) {
        Ok(repo) => repo,
        Err(err) => {
            let _ = tx.unbounded_send(Event::Failed(err.to_string()));
            return;
        }
    };
    startup.mark("repo discovered");
    let changes = match repo.status() {
        Ok(changes) => changes,
        Err(err) => {
            let _ = tx.unbounded_send(Event::Failed(err.to_string()));
            return;
        }
    };
    startup.mark(&format!("status ({} changes)", changes.len()));
    let first = changes
        .first()
        .map(|change| load_file(&repo.thread_local(), change));
    startup.mark("first file loaded");
    let state = store
        .as_ref()
        .map(|s| s.repo_state(repo.root()))
        .unwrap_or_default();
    let repo = Arc::new(repo);
    let _ = tx.unbounded_send(Event::Opened {
        repo: repo.clone(),
        changes: changes.clone(),
        first,
        state,
    });
    if let Some(store) = &store {
        let _ = store.touch_recent(repo.root());
    }
    load_files(&repo, &changes, 1..changes.len(), |ix, load| {
        let _ = tx.unbounded_send(Event::Loaded(ix, load));
    });
    startup.mark("all files loaded");
}

/// Run the app with its first window loading `first`. `opens` delivers later open
/// requests (from other `goro` invocations): a path, or `None` to detect.
pub fn run(
    startup: Startup,
    mut first: UnboundedReceiver<Event>,
    mut opens: UnboundedReceiver<Option<PathBuf>>,
    store: Option<Store>,
) {
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
        cx.set_global(AppStore(store.clone()));

        let initial = wait_for_open(&mut first, startup.t0 + OPEN_WAIT);
        startup.mark("review ready for first frame");
        open_window(startup, initial, first, cx);
        startup.mark("window opened");
        cx.activate(true);

        cx.spawn(async move |cx| {
            while let Some(target) = opens.next().await {
                cx.update(|cx| open_repo(target, cx));
            }
        })
        .detach();
    });
}

struct AppStore(Option<Store>);
impl gpui_kit::Global for AppStore {}

fn app_store(cx: &App) -> Option<Store> {
    cx.try_global::<AppStore>().and_then(|s| s.0.clone())
}

/// Focus the window already showing `target`'s repository, or open a new one.
pub fn open_repo(target: Option<PathBuf>, cx: &mut App) {
    if let Some(target) = &target
        && let Some(window) = window_for(target, cx)
    {
        let _ = window.update(cx, |_, window, _| window.activate_window());
        cx.activate(true);
        return;
    }
    let startup = Startup {
        t0: Instant::now(),
        trace: false,
        bench_exit: false,
    };
    let events = spawn_loader(target, app_store(cx), startup);
    open_window(startup, Vec::new(), events, cx);
    cx.activate(true);
}

fn window_for(target: &Path, cx: &App) -> Option<WindowHandle<GoroView>> {
    let target = target.canonicalize().ok()?;
    cx.windows().into_iter().find_map(|window| {
        let window = window.downcast::<GoroView>()?;
        let root = window.read(cx).ok()?.root()?.to_path_buf();
        target.starts_with(root).then_some(window)
    })
}

fn open_window(
    startup: Startup,
    initial: Vec<Event>,
    events: UnboundedReceiver<Event>,
    cx: &mut App,
) {
    let bounds = Bounds::centered(None, size(px(1280.0), px(820.0)), cx);
    let store = app_store(cx);
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
            let view = cx.new(|cx| GoroView::new(startup, initial, events, store, window, cx));
            window.focus(&view.focus_handle(cx), cx);
            view
        },
    )
    .expect("failed to open window");
}

/// Collect events until the repository is opened (or failed), or `deadline` passes.
fn wait_for_open(events: &mut UnboundedReceiver<Event>, deadline: Instant) -> Vec<Event> {
    let mut initial = Vec::new();
    loop {
        match events.try_recv() {
            Ok(event) => {
                let done = matches!(
                    event,
                    Event::Opened { .. } | Event::Failed(_) | Event::NoRepository
                );
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
