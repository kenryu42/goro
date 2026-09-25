//! Which repository to open when none is given: the one an agent touched most recently,
//! read from Claude Code and Codex session logs, then the most recently opened one.
//!
//! Log formats aren't a stable API, so everything here is best effort: unreadable or
//! unrecognized files are skipped.

use std::io::{BufRead, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use crate::repo::Repo;
use crate::store::RecentRepo;

/// How much of a Claude Code log's tail to scan for the session's working directory.
const TAIL_BYTES: u64 = 64 * 1024;
/// Session logs to inspect per agent, newest first.
const LOGS_PER_AGENT: usize = 5;

#[derive(Debug, Clone, Default)]
pub struct AgentLogs {
    /// Claude Code's `projects` directory (`~/.claude/projects`).
    pub claude_projects: Option<PathBuf>,
    /// Codex's `sessions` directory (`~/.codex/sessions`).
    pub codex_sessions: Option<PathBuf>,
}

impl AgentLogs {
    /// The standard locations, honoring `CLAUDE_CONFIG_DIR` and `CODEX_HOME`.
    pub fn default_locations() -> Self {
        let home = dirs::home_dir();
        let claude = std::env::var_os("CLAUDE_CONFIG_DIR")
            .map(PathBuf::from)
            .or_else(|| home.as_ref().map(|h| h.join(".claude")));
        let codex = std::env::var_os("CODEX_HOME")
            .map(PathBuf::from)
            .or_else(|| home.as_ref().map(|h| h.join(".codex")));
        Self {
            claude_projects: claude.map(|d| d.join("projects")),
            codex_sessions: codex.map(|d| d.join("sessions")),
        }
    }
}

/// A directory an agent worked in, and when its session log last changed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentActivity {
    pub cwd: PathBuf,
    pub at: SystemTime,
}

/// Recent agent working directories, newest first.
pub fn agent_activity(logs: &AgentLogs) -> Vec<AgentActivity> {
    let mut activity = Vec::new();
    if let Some(dir) = &logs.claude_projects {
        for (at, file) in newest_files(claude_logs(dir)) {
            if let Some(cwd) = claude_cwd(&file) {
                activity.push(AgentActivity { cwd, at });
            }
        }
    }
    if let Some(dir) = &logs.codex_sessions {
        for (at, file) in newest_files(codex_logs(dir)) {
            if let Some(cwd) = codex_cwd(&file) {
                activity.push(AgentActivity { cwd, at });
            }
        }
    }
    activity.sort_by_key(|a| std::cmp::Reverse(a.at));
    activity
}

/// The repository to open: where a Goro hook last recorded a turn, else the newest agent
/// activity in the session logs, else the most recently opened repository that still
/// exists.
pub fn detect_repo(
    logs: &AgentLogs,
    hooked: &[RecentRepo],
    recent: &[RecentRepo],
) -> Option<PathBuf> {
    let hooked_at = hooked.first().map(|h| h.opened_at);
    let mut logged = agent_activity(logs);
    // Session logs newer than the latest hook win (hooks may not be installed everywhere).
    if let Some(at) = hooked_at {
        let at = std::time::UNIX_EPOCH + std::time::Duration::from_secs(at);
        logged.retain(|a| a.at > at);
    }
    logged
        .into_iter()
        .map(|a| a.cwd)
        .chain(hooked.iter().map(|r| r.root.clone()))
        .chain(recent.iter().map(|r| r.root.clone()))
        .find_map(|dir| {
            // Directories in old logs may be gone; start from the nearest one that exists.
            let existing = dir.ancestors().find(|d| d.is_dir())?;
            Repo::discover(existing)
                .ok()
                .map(|r| r.root().to_path_buf())
        })
}

fn newest_files(files: Vec<PathBuf>) -> Vec<(SystemTime, PathBuf)> {
    let mut stamped: Vec<(SystemTime, PathBuf)> = files
        .into_iter()
        .filter_map(|f| Some((f.metadata().ok()?.modified().ok()?, f)))
        .collect();
    stamped.sort_by_key(|s| std::cmp::Reverse(s.0));
    stamped.truncate(LOGS_PER_AGENT);
    stamped
}

fn read_dir_sorted(dir: &Path) -> Vec<PathBuf> {
    let mut entries: Vec<PathBuf> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .collect();
    entries.sort();
    entries
}

fn is_jsonl(path: &Path) -> bool {
    path.extension().is_some_and(|e| e == "jsonl")
}

/// `projects/<encoded cwd>/<session>.jsonl`
fn claude_logs(projects: &Path) -> Vec<PathBuf> {
    read_dir_sorted(projects)
        .into_iter()
        .flat_map(|project| read_dir_sorted(&project))
        .filter(|f| is_jsonl(f))
        .collect()
}

/// `sessions/YYYY/MM/DD/rollout-*.jsonl`; only the two most recent days are scanned.
fn codex_logs(sessions: &Path) -> Vec<PathBuf> {
    let mut days = Vec::new();
    'years: for year in read_dir_sorted(sessions).into_iter().rev() {
        for month in read_dir_sorted(&year).into_iter().rev() {
            for day in read_dir_sorted(&month).into_iter().rev() {
                days.push(day);
                if days.len() == 2 {
                    break 'years;
                }
            }
        }
    }
    days.iter()
        .flat_map(|day| read_dir_sorted(day))
        .filter(|f| is_jsonl(f))
        .collect()
}

/// The last `cwd` recorded in a Claude Code session log.
fn claude_cwd(file: &Path) -> Option<PathBuf> {
    let mut f = std::fs::File::open(file).ok()?;
    let len = f.metadata().ok()?.len();
    let start = len.saturating_sub(TAIL_BYTES);
    f.seek(SeekFrom::Start(start)).ok()?;
    let mut tail = Vec::new();
    f.read_to_end(&mut tail).ok()?;
    let mut lines: Vec<&[u8]> = tail.split(|b| *b == b'\n').collect();
    if start > 0 {
        // The first line is probably cut off.
        lines.remove(0);
    }
    lines
        .into_iter()
        .rev()
        .filter(|line| line.windows(6).any(|w| w == b"\"cwd\":"))
        .find_map(|line| {
            let value: serde_json::Value = serde_json::from_slice(line).ok()?;
            value.get("cwd")?.as_str().map(PathBuf::from)
        })
}

/// The `cwd` from a Codex rollout's `session_meta` line.
fn codex_cwd(file: &Path) -> Option<PathBuf> {
    let f = std::fs::File::open(file).ok()?;
    std::io::BufReader::new(f)
        .lines()
        .take(5)
        .map_while(Result::ok)
        .find_map(|line| {
            let value: serde_json::Value = serde_json::from_str(&line).ok()?;
            if value.get("type")?.as_str()? != "session_meta" {
                return None;
            }
            value
                .get("payload")?
                .get("cwd")?
                .as_str()
                .map(PathBuf::from)
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, UNIX_EPOCH};

    fn touch(path: &Path, contents: &str, secs: u64) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, contents).unwrap();
        let f = std::fs::File::options().write(true).open(path).unwrap();
        f.set_modified(UNIX_EPOCH + Duration::from_secs(secs))
            .unwrap();
    }

    fn init_repo(dir: &Path) {
        std::fs::create_dir_all(dir).unwrap();
        let out = std::process::Command::new("git")
            .args(["init", "-q"])
            .current_dir(dir)
            .output()
            .unwrap();
        assert!(out.status.success());
    }

    #[test]
    fn newest_agent_session_wins_across_agents() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let (claude_repo, codex_repo) = (root.join("work/claude"), root.join("work/codex"));
        init_repo(&claude_repo);
        init_repo(&codex_repo);
        let logs = AgentLogs {
            claude_projects: Some(root.join("claude/projects")),
            codex_sessions: Some(root.join("codex/sessions")),
        };
        let claude_line = |cwd: &Path| {
            format!(
                "{{\"type\":\"user\",\"cwd\":{},\"message\":\"hi\"}}\n",
                serde_json::to_string(cwd.to_str().unwrap()).unwrap()
            )
        };
        touch(
            &root.join("claude/projects/-work-claude/s1.jsonl"),
            &format!(
                "{{\"type\":\"queue\"}}\n{}",
                claude_line(&claude_repo.join("src"))
            ),
            2_000,
        );
        touch(
            &root.join("codex/sessions/2026/09/24/rollout-a.jsonl"),
            &format!(
                "{{\"type\":\"session_meta\",\"payload\":{{\"cwd\":{}}}}}\n{{\"type\":\"other\"}}\n",
                serde_json::to_string(codex_repo.to_str().unwrap()).unwrap()
            ),
            1_000,
        );
        let activity = agent_activity(&logs);
        assert_eq!(activity.len(), 2);
        assert_eq!(activity[0].cwd, claude_repo.join("src"));
        // A subdirectory resolves to its repository root.
        assert_eq!(detect_repo(&logs, &[], &[]), Some(claude_repo.clone()));

        // Codex becomes the most recent.
        touch(
            &root.join("codex/sessions/2026/09/25/rollout-b.jsonl"),
            &format!(
                "{{\"type\":\"session_meta\",\"payload\":{{\"cwd\":{}}}}}\n",
                serde_json::to_string(codex_repo.to_str().unwrap()).unwrap()
            ),
            3_000,
        );
        assert_eq!(detect_repo(&logs, &[], &[]), Some(codex_repo));
    }

    #[test]
    fn falls_back_to_recent_repositories_and_skips_non_repos() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let repo = root.join("repo");
        init_repo(&repo);
        let logs = AgentLogs {
            claude_projects: Some(root.join("claude/projects")),
            codex_sessions: None,
        };
        // An agent worked outside any repository.
        touch(
            &root.join("claude/projects/-tmp/s.jsonl"),
            &format!(
                "{{\"cwd\":{}}}\n",
                serde_json::to_string(root.join("not-a-repo").to_str().unwrap()).unwrap()
            ),
            5_000,
        );
        let recent = [
            RecentRepo {
                root: root.join("deleted"),
                opened_at: 2,
            },
            RecentRepo {
                root: repo.clone(),
                opened_at: 1,
            },
        ];
        assert_eq!(detect_repo(&logs, &[], &recent), Some(repo));
        assert_eq!(detect_repo(&AgentLogs::default(), &[], &[]), None);
    }

    #[test]
    fn a_newer_hook_record_beats_older_session_logs() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let (logged, hooked) = (root.join("logged"), root.join("hooked"));
        init_repo(&logged);
        init_repo(&hooked);
        let logs = AgentLogs {
            claude_projects: Some(root.join("claude/projects")),
            codex_sessions: None,
        };
        touch(
            &root.join("claude/projects/-x/s.jsonl"),
            &format!(
                "{{\"cwd\":{}}}\n",
                serde_json::to_string(logged.to_str().unwrap()).unwrap()
            ),
            1_000,
        );
        let hook = |at| RecentRepo {
            root: hooked.clone(),
            opened_at: at,
        };
        assert_eq!(
            detect_repo(&logs, &[hook(2_000)], &[]),
            Some(hooked.clone())
        );
        assert_eq!(detect_repo(&logs, &[hook(500)], &[]), Some(logged));
    }

    #[test]
    fn unrecognized_logs_are_skipped() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        touch(
            &root.join("p/x/garbage.jsonl"),
            "not json\n{\"cwd\": 5}\n",
            1,
        );
        touch(
            &root.join("s/2026/01/01/rollout.jsonl"),
            "{\"type\":\"session_meta\"}\n",
            1,
        );
        let logs = AgentLogs {
            claude_projects: Some(root.join("p")),
            codex_sessions: Some(root.join("s")),
        };
        assert!(agent_activity(&logs).is_empty());
    }
}
