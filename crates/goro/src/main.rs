//! `goro`: open a review of a repository's working tree.
//!
//! On Windows this is a GUI program (no console window when started from Explorer); CLI
//! commands attach to the calling terminal's console for their output.
//!
//! One instance per user: if Goro is already running, this hands the request to it and
//! exits. Started from a terminal, it detaches so the shell gets its prompt back.

#![cfg_attr(windows, windows_subsystem = "windows")]

mod ipc;

use std::io::{IsTerminal, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Stdio};
use std::time::{Duration, Instant};

use clap::{Parser, Subcommand};
use futures::channel::mpsc::unbounded;
use goro_core::comments::to_markdown;
use goro_core::hooks::{self, Agent, HookFiles, HookPayload};
use goro_core::repo::Repo;
use goro_core::store::Store;
use goro_core::turns::{SnapshotMeta, TurnEvent};
use goro_ui::{AppRequest, Startup};

/// Set in the detached child so it doesn't detach again.
const NO_DETACH: &str = "GORO_NO_DETACH";
/// How long `--wait` waits for a freshly started instance to accept connections.
const START_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Parser)]
#[command(
    version,
    about = "Instant, native review for agent-written code changes",
    args_conflicts_with_subcommands = true
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Cmd>,

    /// Repository (or any path inside it). Defaults to the current directory's repository
    /// when run from a terminal, else the one an agent worked in most recently.
    path: Option<PathBuf>,

    /// Block until the review is submitted in Goro, then print its comments as markdown
    /// (for agents: "run `goro --wait` and address the comments").
    #[arg(long)]
    wait: bool,

    /// Print the time to first diff paint on stdout and exit. Never hands off to a
    /// running instance and never detaches.
    #[arg(long, hide = true)]
    bench_exit_after_first_paint: bool,
}

#[derive(Subcommand)]
enum Cmd {
    /// Install, remove, or check Goro's Claude Code and Codex hooks.
    Hooks {
        #[command(subcommand)]
        action: HooksAction,
    },
    /// Print pending review comments as markdown.
    Comments {
        /// Repository (defaults to the current directory's).
        path: Option<PathBuf>,
        /// Delete the comments after printing them.
        #[arg(long)]
        clear: bool,
    },
    /// Called by agent hooks: records a turn snapshot. Prints nothing, never fails.
    #[command(hide = true)]
    Hook { agent: String, event: String },
    /// The detached half of `hook`.
    #[command(hide = true)]
    HookWorker {
        agent: String,
        event: String,
        at_ms: u64,
    },
}

#[derive(Subcommand)]
enum HooksAction {
    /// Add Goro's hooks to Claude Code and Codex (other hooks are kept).
    Install {
        /// Don't ask for confirmation.
        #[arg(long)]
        yes: bool,
    },
    /// Remove Goro's hooks.
    Uninstall {
        #[arg(long)]
        yes: bool,
    },
    /// Show whether the hooks are installed.
    Status,
}

fn main() -> ExitCode {
    let t0 = Instant::now();
    attach_parent_console();
    let cli = Cli::parse();
    match cli.command {
        Some(Cmd::Hook { agent, event }) => {
            hook(&agent, &event);
            ExitCode::SUCCESS
        }
        Some(Cmd::HookWorker {
            agent,
            event,
            at_ms,
        }) => {
            hook_worker(&agent, &event, at_ms);
            ExitCode::SUCCESS
        }
        Some(Cmd::Hooks { action }) => hooks_command(action),
        Some(Cmd::Comments { path, clear }) => comments_command(path, clear),
        None => open(cli, t0),
    }
}

/// Stop child processes from inheriting this process's stdin/stdout/stderr. Unix closes
/// them for children already (Rust sets close-on-exec); Windows inherits every
/// inheritable handle, which would keep the agent's pipes open until the worker exits.
fn keep_std_handles_private() {
    #[cfg(windows)]
    {
        use windows_sys::Win32::Foundation::{HANDLE_FLAG_INHERIT, SetHandleInformation};
        use windows_sys::Win32::System::Console::{
            GetStdHandle, STD_ERROR_HANDLE, STD_INPUT_HANDLE, STD_OUTPUT_HANDLE,
        };
        for which in [STD_INPUT_HANDLE, STD_OUTPUT_HANDLE, STD_ERROR_HANDLE] {
            // SAFETY: plain Win32 calls on this process's own standard handles.
            unsafe {
                let handle = GetStdHandle(which);
                if !handle.is_null() {
                    SetHandleInformation(handle, HANDLE_FLAG_INHERIT, 0);
                }
            }
        }
    }
}

/// Windows GUI programs have no console; reattach to the caller's so `goro comments`,
/// `--wait` and friends print where they were run. Output to pipes works either way.
fn attach_parent_console() {
    #[cfg(windows)]
    {
        use windows_sys::Win32::System::Console::{ATTACH_PARENT_PROCESS, AttachConsole};
        // SAFETY: plain Win32 call; failure (no parent console) is fine.
        unsafe { AttachConsole(ATTACH_PARENT_PROCESS) };
    }
}

fn open(cli: Cli, t0: Instant) -> ExitCode {
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
        None if from_terminal || cli.wait => std::env::current_dir()
            .ok()
            .filter(|cwd| Repo::discover(cwd).is_ok()),
        None => None,
    };
    let socket = ipc::socket_name(&socket_id());

    if cli.wait {
        return wait(socket, target);
    }
    if !startup.bench_exit
        && let Ok(socket) = &socket
        && ipc::send_open(socket, target.as_deref()).is_ok()
    {
        startup.mark("handed off to running instance");
        return ExitCode::SUCCESS;
    }
    if from_terminal && !startup.bench_exit && !trace && std::env::var_os(NO_DETACH).is_none() {
        match relaunch_detached(target.as_ref()) {
            Ok(()) => return ExitCode::SUCCESS,
            Err(err) => {
                eprintln!("goro: could not detach from the terminal ({err}); staying attached")
            }
        }
    }

    let (requests_tx, requests_rx) = unbounded();
    if !startup.bench_exit
        && let Ok(socket) = socket
        && let Err(err) = ipc::serve(socket, move |request| {
            let _ = requests_tx.unbounded_send(match request {
                ipc::Request::Open(target) => AppRequest::Open(target),
                ipc::Request::TurnRecorded(root) => AppRequest::TurnRecorded(root),
                ipc::Request::Wait(target, reply) => AppRequest::Wait(target, reply),
            });
        })
    {
        eprintln!("goro: single-instance socket unavailable ({err})");
    }

    let store = Store::open_default();
    // Git work runs in parallel with platform and window setup.
    let events = goro_ui::spawn_loader(target, store.clone(), startup);
    goro_ui::run(startup, events, requests_rx, store);
    startup.mark("event loop exited");
    ExitCode::SUCCESS
}

/// `--wait`: have the running instance (starting one if needed) show the review, then
/// print the submitted comments.
fn wait(
    socket: std::io::Result<interprocess::local_socket::Name<'static>>,
    target: Option<PathBuf>,
) -> ExitCode {
    let socket = match socket {
        Ok(socket) => socket,
        Err(err) => {
            eprintln!("goro: single-instance socket unavailable ({err})");
            return ExitCode::FAILURE;
        }
    };
    let started = Instant::now();
    let mut launched = false;
    loop {
        match ipc::wait_for_review(&socket, target.as_deref()) {
            Ok(Some(markdown)) => {
                print!("{markdown}");
                let _ = std::io::stdout().flush();
                return ExitCode::SUCCESS;
            }
            Ok(None) => {
                eprintln!("goro: the review was closed without being sent");
                return ExitCode::FAILURE;
            }
            Err(_) if !launched => {
                launched = true;
                if let Err(err) = relaunch_detached(target.as_ref()) {
                    eprintln!("goro: could not start Goro ({err})");
                    return ExitCode::FAILURE;
                }
            }
            Err(_) if started.elapsed() < START_TIMEOUT => {
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(err) => {
                eprintln!("goro: Goro didn't start ({err})");
                return ExitCode::FAILURE;
            }
        }
    }
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
    detach(&mut command);
    command.spawn().map(drop)
}

fn detach(command: &mut Command) {
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
}

fn parse_event(event: &str) -> Option<TurnEvent> {
    match event {
        "prompt" => Some(TurnEvent::Start),
        "stop" => Some(TurnEvent::End),
        _ => None,
    }
}

/// Agent hook entry point. It must not slow down or break the agent's turn: read the
/// payload, hand it to a detached worker, and exit 0 without printing anything (a
/// prompt hook's output would reach the model).
fn hook(agent: &str, event: &str) {
    let at_ms = SnapshotMeta::now_ms();
    let mut payload = Vec::new();
    if std::io::stdin().read_to_end(&mut payload).is_err() {
        return;
    }
    // The agent waits until our stdout and stderr close; the worker must not hold them.
    keep_std_handles_private();
    let spawned = std::env::current_exe().and_then(|exe| {
        let mut command = Command::new(exe);
        command
            .args(["hook-worker", agent, event, &at_ms.to_string()])
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        detach(&mut command);
        let mut child = command.spawn()?;
        child
            .stdin
            .take()
            .expect("stdin is piped")
            .write_all(&payload)
    });
    if let Err(err) = spawned {
        log_error(&format!("hook {agent} {event}: {err}"));
    }
}

fn hook_worker(agent: &str, event: &str, at_ms: u64) {
    let result = (|| {
        let agent = Agent::parse(agent).ok_or("unknown agent")?;
        let event = parse_event(event).ok_or("unknown event")?;
        let mut payload = Vec::new();
        std::io::stdin()
            .read_to_end(&mut payload)
            .map_err(|e| e.to_string())?;
        let payload: HookPayload = serde_json::from_slice(&payload).map_err(|e| e.to_string())?;
        let Some(root) = hooks::handle(agent, event, &payload, at_ms)? else {
            return Ok(());
        };
        if let Some(store) = Store::open_default() {
            let _ = store.touch_activity(&root);
        }
        if let Ok(socket) = ipc::socket_name(&socket_id()) {
            // Only if Goro is running.
            let _ = ipc::send_turn_recorded(&socket, &root);
        }
        Ok::<_, String>(())
    })();
    if let Err(err) = result {
        log_error(&format!("hook-worker {agent} {event}: {err}"));
    }
}

/// The single-instance socket's id. `GORO_SOCKET_ID` keeps tests and development builds
/// apart from an installed Goro.
fn socket_id() -> String {
    std::env::var("GORO_SOCKET_ID").unwrap_or_else(|_| "goro".into())
}

/// Errors from hooks go to a log file, never to the agent.
fn log_error(message: &str) {
    let Some(store) = Store::open_default() else {
        return;
    };
    let _ = store.append_log(message);
}

fn hooks_command(action: HooksAction) -> ExitCode {
    let files = HookFiles::default_locations();
    let (edits, yes, verb) = match action {
        HooksAction::Status => {
            for (agent, installed) in hooks::installed(&files) {
                println!(
                    "{:<7} {}",
                    agent.name(),
                    if installed {
                        "installed"
                    } else {
                        "not installed"
                    }
                );
            }
            return ExitCode::SUCCESS;
        }
        HooksAction::Install { yes } => {
            let exe = std::env::current_exe()
                .and_then(|p| p.canonicalize())
                .expect("the running executable has a path");
            (hooks::plan_install(&files, &exe), yes, "install")
        }
        HooksAction::Uninstall { yes } => (hooks::plan_uninstall(&files), yes, "uninstall"),
    };
    let edits = match edits {
        Ok(edits) => edits,
        Err(err) => {
            eprintln!("goro: {err}");
            return ExitCode::FAILURE;
        }
    };
    let changes: Vec<_> = edits.iter().filter(|e| e.changes_something()).collect();
    if changes.is_empty() {
        println!("Nothing to change.");
        return ExitCode::SUCCESS;
    }
    for edit in &changes {
        println!("{} ({}):", edit.path.display(), edit.agent.name());
        match verb {
            "install" => println!(
                "  run `{}` on UserPromptSubmit and `… stop` on Stop (existing hooks are kept)",
                hooks::hook_command(
                    &std::env::current_exe().unwrap_or_default(),
                    edit.agent,
                    "prompt"
                )
            ),
            _ => println!("  remove Goro's UserPromptSubmit and Stop hooks"),
        }
    }
    if !yes {
        if !std::io::stdin().is_terminal() {
            eprintln!("goro: pass --yes to {verb} without a prompt");
            return ExitCode::FAILURE;
        }
        print!("Proceed? [y/N] ");
        let _ = std::io::stdout().flush();
        let mut answer = String::new();
        let _ = std::io::stdin().read_line(&mut answer);
        if !matches!(answer.trim(), "y" | "Y" | "yes") {
            println!("Nothing changed.");
            return ExitCode::FAILURE;
        }
    }
    for edit in &changes {
        if let Err(err) = edit.apply() {
            eprintln!("goro: {}: {err}", edit.path.display());
            return ExitCode::FAILURE;
        }
    }
    println!("Done.");
    if verb == "install" && changes.iter().any(|e| e.agent == Agent::Codex) {
        println!(
            "Codex asks you to trust new hooks: run /hooks in Codex once and trust Goro's two hooks."
        );
    }
    ExitCode::SUCCESS
}

fn comments_command(path: Option<PathBuf>, clear: bool) -> ExitCode {
    let start = path.unwrap_or_else(|| std::env::current_dir().unwrap_or_default());
    let repo = match Repo::discover(&start) {
        Ok(repo) => repo,
        Err(err) => {
            eprintln!("goro: {err}");
            return ExitCode::FAILURE;
        }
    };
    let Some(store) = Store::open_default() else {
        eprintln!("goro: no data directory");
        return ExitCode::FAILURE;
    };
    let root: &Path = repo.root();
    let mut state = store.repo_state(root);
    print!("{}", to_markdown(&state.comments));
    if clear && !state.comments.is_empty() {
        state.comments.clear();
        if let Err(err) = store.save_repo_state(root, &state) {
            eprintln!("goro: {err}");
            return ExitCode::FAILURE;
        }
    }
    ExitCode::SUCCESS
}
