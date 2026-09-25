//! Partial patches must apply with real `git apply` and produce exactly the selected
//! change, for random selections over generated diffs.

use std::path::PathBuf;
use std::process::{Command, Stdio};

use gix::bstr::ByteSlice;
use goro_core::diff::{FileDiff, LineKind};
use goro_core::patch::{self, Direction, Header, PatchError};

struct Repo {
    _dir: tempfile::TempDir,
    root: PathBuf,
}

impl Repo {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let repo = Self { _dir: dir, root };
        repo.git(&["init", "-q"], None);
        repo.git(&["config", "user.name", "Goro Test"], None);
        repo.git(&["config", "user.email", "goro@example.invalid"], None);
        repo.git(&["config", "core.autocrlf", "false"], None);
        repo
    }

    fn git(&self, args: &[&str], stdin: Option<&[u8]>) -> Vec<u8> {
        let mut child = Command::new("git")
            .current_dir(&self.root)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        {
            use std::io::Write;
            let mut pipe = child.stdin.take().unwrap();
            if let Some(input) = stdin {
                pipe.write_all(input).unwrap();
            }
        }
        let out = child.wait_with_output().unwrap();
        assert!(
            out.status.success(),
            "git {args:?}: {}\n{}",
            String::from_utf8_lossy(&out.stderr),
            stdin
                .map(|s| s.to_str_lossy().into_owned())
                .unwrap_or_default()
        );
        out.stdout
    }

    fn path(&self, name: &str) -> PathBuf {
        self.root.join(name)
    }

    fn commit_file(&self, name: &str, contents: &[u8]) {
        std::fs::write(self.path(name), contents).unwrap();
        self.git(&["add", name], None);
        self.git(&["commit", "-q", "-m", "base"], None);
    }

    fn index_content(&self, name: &str) -> Vec<u8> {
        self.git(&["show", &format!(":{name}")], None)
    }
}

/// Old side with selected changes applied (what staging should produce).
fn expected_forward(diff: &FileDiff, selected: &dyn Fn(usize) -> bool) -> Vec<u8> {
    let mut out = Vec::new();
    let mut old_pos = 0;
    for hunk in &diff.hunks {
        let first_old = hunk.lines.clone().find_map(|ix| {
            let l = &diff.lines[ix];
            (l.kind != LineKind::Added).then(|| l.old_no.unwrap() as usize - 1)
        });
        let start = first_old.unwrap_or(hunk.old_start as usize);
        for ix in old_pos..start {
            out.extend_from_slice(line(&diff.old, ix));
        }
        for ix in hunk.lines.clone() {
            let l = &diff.lines[ix];
            match l.kind {
                LineKind::Context => {
                    out.extend_from_slice(line(&diff.old, l.old_no.unwrap() as usize - 1))
                }
                LineKind::Removed if !selected(ix) => {
                    out.extend_from_slice(line(&diff.old, l.old_no.unwrap() as usize - 1))
                }
                LineKind::Removed => {}
                LineKind::Added if selected(ix) => {
                    out.extend_from_slice(line(&diff.new, l.new_no.unwrap() as usize - 1))
                }
                LineKind::Added => {}
            }
        }
        old_pos = start + hunk.old_len as usize;
    }
    for ix in old_pos..diff.old.line_count() {
        out.extend_from_slice(line(&diff.old, ix));
    }
    out
}

/// New side with selected changes undone (what unstaging/discarding should produce).
fn expected_reverse(diff: &FileDiff, selected: &dyn Fn(usize) -> bool) -> Vec<u8> {
    let mut out = Vec::new();
    let mut new_pos = 0;
    for hunk in &diff.hunks {
        let first_new = hunk.lines.clone().find_map(|ix| {
            let l = &diff.lines[ix];
            (l.kind != LineKind::Removed).then(|| l.new_no.unwrap() as usize - 1)
        });
        let start = first_new.unwrap_or(hunk.new_start as usize);
        for ix in new_pos..start {
            out.extend_from_slice(line(&diff.new, ix));
        }
        for ix in hunk.lines.clone() {
            let l = &diff.lines[ix];
            match l.kind {
                LineKind::Context => {
                    out.extend_from_slice(line(&diff.new, l.new_no.unwrap() as usize - 1))
                }
                LineKind::Added if !selected(ix) => {
                    out.extend_from_slice(line(&diff.new, l.new_no.unwrap() as usize - 1))
                }
                LineKind::Added => {}
                LineKind::Removed if selected(ix) => {
                    out.extend_from_slice(line(&diff.old, l.old_no.unwrap() as usize - 1))
                }
                LineKind::Removed => {}
            }
        }
        new_pos = start + hunk.new_len as usize;
    }
    for ix in new_pos..diff.new.line_count() {
        out.extend_from_slice(line(&diff.new, ix));
    }
    out
}

fn line(text: &goro_core::diff::Text, ix: usize) -> &[u8] {
    &text.bytes()[text.line_range(ix)]
}

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
}

fn generate(rng: &mut Rng) -> (Vec<u8>, Vec<u8>) {
    let old: Vec<String> = (0..60)
        .map(|i| format!("line {i} {}\n", rng.next() % 7))
        .collect();
    let mut new = old.clone();
    for _ in 0..(rng.next() % 8 + 1) {
        let at = (rng.next() as usize) % new.len();
        match rng.next() % 3 {
            0 => {
                new.remove(at);
            }
            1 => new.insert(at, format!("added {}\n", rng.next() % 100)),
            _ => new[at] = format!("changed {}\n", rng.next() % 100),
        }
    }
    let mut old = old.concat().into_bytes();
    let mut new = new.concat().into_bytes();
    // Sometimes drop the final newline on either side.
    if rng.next().is_multiple_of(5) {
        old.pop();
    }
    if rng.next().is_multiple_of(5) {
        new.pop();
    }
    (old, new)
}

fn random_selection(rng: &mut Rng, diff: &FileDiff) -> Vec<bool> {
    diff.lines
        .iter()
        .map(|_| rng.next().is_multiple_of(2))
        .collect()
}

#[derive(Default)]
struct Tally {
    applied: usize,
    refused: usize,
}

fn run_rounds(mut check: impl FnMut(&Repo, &FileDiff, &[bool], &mut Tally)) {
    let mut rng = Rng(0x2545f4914f6cdd1d);
    let mut tally = Tally::default();
    for _ in 0..40 {
        let repo = Repo::new();
        let (old, new) = generate(&mut rng);
        let diff = FileDiff::compute(old.clone(), new.clone(), 3);
        if diff.hunks.is_empty() {
            continue;
        }
        repo.commit_file("f.txt", &old);
        std::fs::write(repo.path("f.txt"), &new).unwrap();
        let selection = random_selection(&mut rng, &diff);
        check(&repo, &diff, &selection, &mut tally);
    }
    assert!(
        tally.applied >= 25,
        "too few rounds applied ({})",
        tally.applied
    );
    assert!(tally.refused < 10, "too many refused ({})", tally.refused);
}

#[test]
fn stage_selected_lines() {
    run_rounds(|repo, diff, selection, tally| {
        let sel = |ix: usize| selection[ix];
        match patch::build(
            b"f.txt".as_bstr(),
            diff,
            sel,
            Direction::Forward,
            Header::Modify,
        ) {
            Ok(Some(p)) => {
                repo.git(&["apply", "--cached", "-"], Some(&p));
                assert_eq!(
                    repo.index_content("f.txt").as_bstr(),
                    expected_forward(diff, &sel).as_bstr()
                );
                tally.applied += 1;
            }
            Ok(None) => {}
            Err(PatchError::SplitsFinalNewline) => tally.refused += 1,
        }
    });
}

#[test]
fn discard_selected_lines_from_worktree() {
    run_rounds(|repo, diff, selection, tally| {
        let sel = |ix: usize| selection[ix];
        match patch::build(
            b"f.txt".as_bstr(),
            diff,
            sel,
            Direction::Reverse,
            Header::Modify,
        ) {
            Ok(Some(p)) => {
                repo.git(&["apply", "-R", "-"], Some(&p));
                assert_eq!(
                    std::fs::read(repo.path("f.txt")).unwrap().as_bstr(),
                    expected_reverse(diff, &sel).as_bstr()
                );
                tally.applied += 1;
            }
            Ok(None) => {}
            Err(PatchError::SplitsFinalNewline) => tally.refused += 1,
        }
    });
}

#[test]
fn unstage_selected_lines() {
    run_rounds(|repo, diff, selection, tally| {
        repo.git(&["add", "f.txt"], None);
        let sel = |ix: usize| selection[ix];
        match patch::build(
            b"f.txt".as_bstr(),
            diff,
            sel,
            Direction::Reverse,
            Header::Modify,
        ) {
            Ok(Some(p)) => {
                repo.git(&["apply", "--cached", "-R", "-"], Some(&p));
                assert_eq!(
                    repo.index_content("f.txt").as_bstr(),
                    expected_reverse(diff, &sel).as_bstr()
                );
                tally.applied += 1;
            }
            Ok(None) => {}
            Err(PatchError::SplitsFinalNewline) => tally.refused += 1,
        }
    });
}

#[test]
fn stage_part_of_a_new_file() {
    let repo = Repo::new();
    repo.commit_file("base.txt", b"base\n");
    let new = b"one\ntwo\nthree\nfour\n";
    std::fs::write(repo.path("new file.txt"), new).unwrap();
    let diff = FileDiff::compute(Vec::new(), new.to_vec(), 3);
    let p = patch::build(
        b"new file.txt".as_bstr(),
        &diff,
        |ix| ix == 1 || ix == 3,
        Direction::Forward,
        Header::NewFile { mode: "100644" },
    )
    .unwrap()
    .unwrap();
    repo.git(&["apply", "--cached", "-"], Some(&p));
    assert_eq!(repo.index_content("new file.txt"), b"two\nfour\n");
}

#[test]
fn crlf_lines_round_trip() {
    let repo = Repo::new();
    let old = b"a\r\nb\r\nc\r\nd\r\n";
    let new = b"a\r\nB\r\nc\r\nD\r\n";
    repo.commit_file("win.txt", old);
    std::fs::write(repo.path("win.txt"), new).unwrap();
    let diff = FileDiff::compute(old.to_vec(), new.to_vec(), 3);
    // Stage only the b → B change.
    let selected = |ix: usize| {
        let l = &diff.lines[ix];
        l.old_no == Some(2) || l.new_no == Some(2) && l.kind == LineKind::Added
    };
    let p = patch::build(
        b"win.txt".as_bstr(),
        &diff,
        selected,
        Direction::Forward,
        Header::Modify,
    )
    .unwrap()
    .unwrap();
    repo.git(&["apply", "--cached", "-"], Some(&p));
    assert_eq!(repo.index_content("win.txt"), b"a\r\nB\r\nc\r\nd\r\n");
}
