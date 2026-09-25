//! `goro`: open a review of a repository's working tree.
//!
//! One instance per user: if Goro is already running, this hands the request to it and
//! exits. Started from a terminal, it detaches so the shell gets its prompt back.

mod ipc;

use std::io::IsTerminal;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::Instant;

use clap::Parser;
use futures::channel::mpsc::unbounded;
use goro_core::repo::Repo;
use goro_core::store::Store;
use goro_ui::Startup;

/// Set in the detached child so it doesn't detach again.
const NO_DETACH: &str = "GORO_NO_DETACH";

#[derive(Parser)]
#[command(
    version,
    about = "Instant, native review for agent-written code changes"
)]
struct Cli {
    /// Repository (or any path inside it). Defaults to the current directory's repository
    /// when run from a terminal, else the one an agent worked in most recently.
    path: Option<PathBuf>,

    /// Print the time to first diff paint on stdout and exit. Never hands off to a
    /// running instance and never detaches.
    #[arg(long, hide = true)]
    bench_exit_after_first_paint: bool,
}

fn main() {
    let t0 = Instant::now();
    let cli = Cli::parse();
    let trace = std::env::var_os("GORO_TRACE_STARTUP").is_some_and(|v| v != "0");
    let startup = Startup {
        t0,
        trace,
        bench_exit: cli.bench_exit_after_first_paint,
    };
    let from_terminal = std::io::stdout().is_terminal();
    let target = match cli.path {
        Some(path) => Some(std::path::absolute(&path).unwrap_or(path)),
        // A desktop launch starts in `/` or `$HOME`, which says nothing about intent.
        None if from_terminal => std::env::current_dir()
            .ok()
            .filter(|cwd| Repo::discover(cwd).is_ok()),
        None => None,
    };

    let socket = ipc::socket_name("goro");
    if !startup.bench_exit
        && let Ok(socket) = &socket
        && ipc::send_open(socket, target.as_deref()).is_ok()
    {
        startup.mark("handed off to running instance");
        return;
    }

    if from_terminal && !startup.bench_exit && !trace && std::env::var_os(NO_DETACH).is_none() {
        match relaunch_detached(target.as_ref()) {
            Ok(()) => return,
            Err(err) => {
                eprintln!("goro: could not detach from the terminal ({err}); staying attached")
            }
        }
    }

    let (open_tx, open_rx) = unbounded();
    if !startup.bench_exit
        && let Ok(socket) = socket
        && let Err(err) = ipc::serve(socket, move |target| {
            let _ = open_tx.unbounded_send(target);
        })
    {
        eprintln!("goro: single-instance socket unavailable ({err})");
    }

    let store = Store::open_default();
    // Git work runs in parallel with platform and window setup.
    let events = goro_ui::spawn_loader(target, store.clone(), startup);
    goro_ui::run(startup, events, open_rx, store);
}

/// Start the GUI as a detached background process with the resolved target.
fn relaunch_detached(target: Option<&PathBuf>) -> std::io::Result<()> {
    let mut command = Command::new(std::env::current_exe()?);
    command
        .args(target)
        .env(NO_DETACH, "1")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        // A new process group, so the terminal's Ctrl-C doesn't reach it.
        command.process_group(0);
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const DETACHED_PROCESS: u32 = 0x0000_0008;
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
        command.creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP);
    }
    command.spawn().map(drop)
}
