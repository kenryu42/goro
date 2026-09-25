//! `goro hook` end to end: agents run it on every prompt and stop, so it must return at
//! once, print nothing, and leave a turn snapshot behind.

use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

fn git(root: &Path, args: &[&str]) -> String {
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
    String::from_utf8(out.stdout).unwrap()
}

fn run_hook(
    data: &Path,
    agent: &str,
    event: &str,
    payload: &str,
) -> (std::process::Output, Duration) {
    let started = Instant::now();
    let mut child = Command::new(env!("CARGO_BIN_EXE_goro"))
        .args(["hook", agent, event])
        .env("GORO_DATA_DIR", data)
        .env(
            "GORO_SOCKET_ID",
            format!("goro-hook-test-{}", std::process::id()),
        )
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(payload.as_bytes())
        .unwrap();
    let out = child.wait_with_output().unwrap();
    (out, started.elapsed())
}

fn wait_for_refs(root: &Path, count: usize) -> Vec<String> {
    let started = Instant::now();
    loop {
        let refs: Vec<String> = git(
            root,
            &["for-each-ref", "--format=%(refname)", "refs/goro/turns"],
        )
        .lines()
        .map(String::from)
        .collect();
        if refs.len() >= count || started.elapsed() > Duration::from_secs(10) {
            return refs;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn hooks_return_immediately_silently_and_record_turns() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap().join("repo");
    std::fs::create_dir_all(&root).unwrap();
    let data = dir.path().join("data");
    git(&root, &["init", "-q"]);
    std::fs::write(root.join("a.txt"), "one\n").unwrap();
    let cwd = serde_json::to_string(root.to_str().unwrap()).unwrap();

    // The first run of a freshly built binary pays for the OS scanning it; agents run
    // hooks many times, so time the steady state.
    Command::new(env!("CARGO_BIN_EXE_goro"))
        .arg("--version")
        .output()
        .unwrap();

    let prompt = format!(
        r#"{{"session_id":"sess-1","cwd":{cwd},"hook_event_name":"UserPromptSubmit","prompt":"make it two"}}"#
    );
    let (out, elapsed) = run_hook(&data, "claude", "prompt", &prompt);
    assert!(out.status.success());
    assert!(
        out.stdout.is_empty(),
        "a prompt hook's stdout would reach the model"
    );
    assert!(out.stderr.is_empty());
    assert!(
        elapsed < Duration::from_millis(200),
        "hook took {elapsed:?}"
    );
    assert_eq!(wait_for_refs(&root, 1).len(), 1);

    std::fs::write(root.join("a.txt"), "two\n").unwrap();
    let stop = format!(
        r#"{{"session_id":"sess-1","cwd":{cwd},"hook_event_name":"Stop","stop_hook_active":false}}"#
    );
    let (out, _) = run_hook(&data, "claude", "stop", &stop);
    assert!(out.status.success() && out.stdout.is_empty());
    let refs = wait_for_refs(&root, 2);
    assert_eq!(refs.len(), 2, "{refs:?}");
    let diff = git(&root, &["diff", &refs[0], &refs[1]]);
    assert!(diff.contains("-one") && diff.contains("+two"), "{diff}");
    // The hook remembers where agents work, for auto-detect.
    let activity = std::fs::read_to_string(data.join("activity.json")).unwrap();
    assert!(activity.contains("repo"), "{activity}");
}

#[test]
fn bad_input_never_fails_the_agent() {
    let dir = tempfile::tempdir().unwrap();
    for (agent, event, payload) in [
        ("claude", "prompt", "not json"),
        ("nobody", "prompt", "{}"),
        (
            "codex",
            "stop",
            r#"{"session_id":"s","cwd":"/definitely/not/a/repo"}"#,
        ),
    ] {
        let (out, _) = run_hook(dir.path(), agent, event, payload);
        assert!(out.status.success(), "{agent} {event}");
        assert!(out.stdout.is_empty() && out.stderr.is_empty());
    }
}
