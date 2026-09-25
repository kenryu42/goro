//! `goro`: open a review of a repository's working tree.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use clap::Parser;
use futures::channel::mpsc::{UnboundedSender, unbounded};
use goro_core::repo::Repo;
use goro_core::review::{load_file, load_files};
use goro_ui::{Event, Startup};

#[derive(Parser)]
#[command(
    version,
    about = "Instant, native review for agent-written code changes"
)]
struct Cli {
    /// Repository (or any path inside it). Defaults to the current directory.
    path: Option<PathBuf>,

    /// Print the time to first diff paint on stdout and exit.
    #[arg(long, hide = true)]
    bench_exit_after_first_paint: bool,
}

fn main() {
    let t0 = Instant::now();
    let cli = Cli::parse();
    let startup = Startup {
        t0,
        trace: std::env::var_os("GORO_TRACE_STARTUP").is_some_and(|v| v != "0"),
        bench_exit: cli.bench_exit_after_first_paint,
    };
    let path = match cli.path {
        Some(path) => path,
        None => std::env::current_dir().expect("current directory is not accessible"),
    };
    let (tx, rx) = unbounded();
    // Git work runs in parallel with platform and window setup.
    std::thread::Builder::new()
        .name("goro-loader".into())
        .spawn(move || load_repository(path, tx, startup))
        .expect("failed to spawn loader thread");
    goro_ui::run(startup, rx);
}

fn load_repository(path: PathBuf, tx: UnboundedSender<Event>, startup: Startup) {
    let repo = match Repo::discover(&path) {
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
    let repo = Arc::new(repo);
    let _ = tx.unbounded_send(Event::Opened {
        repo: repo.clone(),
        changes: changes.clone(),
        first,
    });
    load_files(&repo, &changes, 1..changes.len(), |ix, load| {
        let _ = tx.unbounded_send(Event::Loaded(ix, load));
    });
    startup.mark("all files loaded");
}
