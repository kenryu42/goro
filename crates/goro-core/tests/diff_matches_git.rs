//! Goro's hunks must match `git diff` byte for byte (from the first `@@` on).

use std::path::Path;
use std::process::Command;

use goro_core::diff::{DEFAULT_CONTEXT, FileDiff};

fn git_hunks(dir: &Path, old: &[u8], new: &[u8]) -> Vec<u8> {
    std::fs::write(dir.join("old"), old).unwrap();
    std::fs::write(dir.join("new"), new).unwrap();
    let out = Command::new("git")
        .current_dir(dir)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .args([
            "diff",
            "--no-index",
            "--no-color",
            "--no-ext-diff",
            "--histogram",
            "-U3",
            "old",
            "new",
        ])
        .output()
        .unwrap();
    assert!(
        out.status.code() == Some(0) || out.status.code() == Some(1),
        "git diff failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = out.stdout;
    match stdout.windows(3).position(|w| w == b"\n@@") {
        Some(pos) => stdout[pos + 1..].to_vec(),
        None => Vec::new(),
    }
}

fn assert_matches_git(name: &str, old: &[u8], new: &[u8]) {
    let dir = tempfile::tempdir().unwrap();
    let expected = git_hunks(dir.path(), old, new);
    let actual = FileDiff::compute(old.to_vec(), new.to_vec(), DEFAULT_CONTEXT).to_unified_hunks();
    assert_eq!(
        String::from_utf8_lossy(&actual),
        String::from_utf8_lossy(&expected),
        "case {name}"
    );
}

fn lines(n: usize) -> String {
    (1..=n).map(|i| format!("line {i}\n")).collect()
}

#[test]
fn identical() {
    assert_matches_git("identical", b"a\nb\n", b"a\nb\n");
}

#[test]
fn single_modification() {
    let old = lines(20);
    let new = old.replace("line 10\n", "line ten\n");
    assert_matches_git("single", old.as_bytes(), new.as_bytes());
}

#[test]
fn edits_at_edges() {
    let old = lines(10);
    let new = format!("first\n{}", old.replace("line 10\n", ""));
    assert_matches_git("edges", old.as_bytes(), new.as_bytes());
}

#[test]
fn close_changes_merge_and_far_changes_split() {
    let old = lines(40);
    // 6 lines apart (merges with -U3), then 20 lines apart (splits).
    let new = old
        .replace("line 5\n", "five\n")
        .replace("line 11\n", "eleven\n")
        .replace("line 32\n", "thirty-two\n");
    assert_matches_git("merge/split", old.as_bytes(), new.as_bytes());
    // Exactly 7 unchanged lines between changes: does not merge.
    let new = old
        .replace("line 5\n", "five\n")
        .replace("line 13\n", "13\n");
    assert_matches_git("gap 7", old.as_bytes(), new.as_bytes());
}

#[test]
fn empty_to_content_and_back() {
    assert_matches_git("create", b"", b"a\nb\n");
    assert_matches_git("delete", b"a\nb\n", b"");
}

#[test]
fn missing_final_newline() {
    assert_matches_git("add newline", b"a\nb", b"a\nb\n");
    assert_matches_git("remove newline", b"a\nb\n", b"a\nb");
    assert_matches_git("edit without newline", b"a\nb", b"a\nc");
    assert_matches_git("context without newline", b"a\nb\nc", b"A\nb\nc");
}

#[test]
fn crlf_lines() {
    assert_matches_git("crlf edit", b"a\r\nb\r\nc\r\n", b"a\r\nB\r\nc\r\n");
    assert_matches_git("crlf to lf", b"a\r\nb\r\n", b"a\nb\n");
}

#[test]
fn function_context() {
    let old = "fn alpha() {\n    one();\n    two();\n    three();\n    four();\n    five();\n}\n\nfn beta() {\n    six();\n    seven();\n    eight();\n    nine();\n    ten();\n    eleven();\n}\n";
    let new = old
        .replace("five();", "FIVE();")
        .replace("ten();", "TEN();");
    assert_matches_git("func", old.as_bytes(), new.as_bytes());
    let long = format!("{}\n{}", "x".repeat(120), lines(10));
    let new = long.replace("line 8\n", "eight\n");
    assert_matches_git("long func line", long.as_bytes(), new.as_bytes());
}

/// Deterministic pseudo-random edits over a larger file.
#[test]
fn generated_edits() {
    let mut seed: u64 = 0x9e3779b97f4a7c15;
    let mut next = move || {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        seed
    };
    for round in 0..40 {
        let old: Vec<String> = (0..300)
            .map(|i| match next() % 5 {
                0 => "}\n".to_string(),
                1 => format!("fn f{i}() {{\n"),
                _ => format!("    stmt({});\n", next() % 50),
            })
            .collect();
        let mut new = old.clone();
        for _ in 0..(next() % 12 + 1) {
            let at = (next() as usize) % new.len();
            match next() % 3 {
                0 => {
                    new.remove(at);
                }
                1 => new.insert(at, format!("    added({});\n", next() % 50)),
                _ => new[at] = format!("    changed({});\n", next() % 50),
            }
        }
        assert_matches_git(
            &format!("generated round {round}"),
            old.concat().as_bytes(),
            new.concat().as_bytes(),
        );
    }
}
