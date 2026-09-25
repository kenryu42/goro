//! Agent hooks: installing Goro into Claude Code and Codex, and handling what they send.
//!
//! Both agents run `goro hook <agent> <prompt|stop>` with a JSON payload on stdin at the
//! start and end of every turn. Goro snapshots the working tree for each (see
//! [`crate::turns`]). Hooks must never delay or break the agent: see the binary's `hook`
//! command, which hands the payload to a detached worker and exits at once.

use std::path::{Path, PathBuf};

use serde::Deserialize;
use serde_json::{Map, Value, json};

use crate::git::Git;
use crate::repo::Repo;
use crate::turns::{self, KEEP_SNAPSHOTS, SnapshotMeta, TurnEvent};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Agent {
    Claude,
    Codex,
}

impl Agent {
    pub fn name(self) -> &'static str {
        match self {
            Agent::Claude => "claude",
            Agent::Codex => "codex",
        }
    }

    pub fn parse(name: &str) -> Option<Self> {
        match name {
            "claude" => Some(Agent::Claude),
            "codex" => Some(Agent::Codex),
            _ => None,
        }
    }
}

/// The fields Goro uses from either agent's hook payload.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct HookPayload {
    pub session_id: String,
    pub cwd: PathBuf,
    #[serde(default)]
    pub prompt: Option<String>,
    #[serde(default)]
    pub turn_id: Option<String>,
}

/// Snapshot the working tree for a hook event. Returns the repository root, or `None`
/// when the agent isn't working inside a repository.
pub fn handle(
    agent: Agent,
    event: TurnEvent,
    payload: &HookPayload,
    at_ms: u64,
) -> Result<Option<PathBuf>, String> {
    let Ok(repo) = Repo::discover(&payload.cwd) else {
        return Ok(None);
    };
    let root = repo.root().to_path_buf();
    let git = Git::new(&root);
    let meta = SnapshotMeta {
        agent: agent.name().into(),
        event,
        session_id: payload.session_id.clone(),
        turn_id: payload.turn_id.clone(),
        prompt: payload.prompt.clone(),
        at_ms,
    };
    turns::snapshot(&git, &meta).map_err(|e| e.to_string())?;
    turns::prune(&git, KEEP_SNAPSHOTS).map_err(|e| e.to_string())?;
    Ok(Some(root))
}

/// Where each agent keeps its user-level hook configuration.
#[derive(Debug, Clone)]
pub struct HookFiles {
    /// `~/.claude/settings.json` (or under `$CLAUDE_CONFIG_DIR`).
    pub claude_settings: Option<PathBuf>,
    /// `~/.codex/hooks.json` (or under `$CODEX_HOME`).
    pub codex_hooks: Option<PathBuf>,
}

impl HookFiles {
    pub fn default_locations() -> Self {
        let home = dirs::home_dir();
        let claude = std::env::var_os("CLAUDE_CONFIG_DIR")
            .map(PathBuf::from)
            .or_else(|| home.as_ref().map(|h| h.join(".claude")));
        let codex = std::env::var_os("CODEX_HOME")
            .map(PathBuf::from)
            .or_else(|| home.as_ref().map(|h| h.join(".codex")));
        Self {
            claude_settings: claude.map(|d| d.join("settings.json")),
            codex_hooks: codex.map(|d| d.join("hooks.json")),
        }
    }

    fn files(&self) -> Vec<(Agent, &Path)> {
        [
            (Agent::Claude, self.claude_settings.as_deref()),
            (Agent::Codex, self.codex_hooks.as_deref()),
        ]
        .into_iter()
        .filter_map(|(agent, path)| Some((agent, path?)))
        .collect()
    }
}

/// A planned change to one configuration file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileEdit {
    pub agent: Agent,
    pub path: PathBuf,
    /// Current contents (`None` if the file doesn't exist).
    pub before: Option<String>,
    pub after: String,
}

impl FileEdit {
    pub fn changes_something(&self) -> bool {
        self.before.as_deref() != Some(self.after.as_str())
    }

    pub fn apply(&self) -> std::io::Result<()> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let tmp = self.path.with_extension("json.goro-tmp");
        std::fs::write(&tmp, &self.after)?;
        std::fs::rename(&tmp, &self.path)
    }
}

const EVENTS: [(&str, &str); 2] = [("UserPromptSubmit", "prompt"), ("Stop", "stop")];

/// The command an agent runs for `event`.
pub fn hook_command(goro: &Path, agent: Agent, event: &str) -> String {
    format!(
        "{} hook {} {event}",
        shell_quote(&goro.to_string_lossy()),
        agent.name()
    )
}

/// Plan installing Goro's hooks (replacing any earlier Goro hooks; other hooks are kept).
pub fn plan_install(files: &HookFiles, goro: &Path) -> Result<Vec<FileEdit>, String> {
    plan(files, Some(goro))
}

/// Plan removing Goro's hooks.
pub fn plan_uninstall(files: &HookFiles) -> Result<Vec<FileEdit>, String> {
    plan(files, None)
}

/// Whether each agent's configuration currently runs Goro's hooks.
pub fn installed(files: &HookFiles) -> Vec<(Agent, bool)> {
    files
        .files()
        .into_iter()
        .map(|(agent, path)| {
            let installed = std::fs::read_to_string(path)
                .ok()
                .and_then(|text| serde_json::from_str::<Value>(&text).ok())
                .is_some_and(|config| {
                    EVENTS
                        .iter()
                        .all(|(event, _)| has_goro_hook(&config, event, agent))
                });
            (agent, installed)
        })
        .collect()
}

fn plan(files: &HookFiles, goro: Option<&Path>) -> Result<Vec<FileEdit>, String> {
    let mut edits = Vec::new();
    for (agent, path) in files.files() {
        let before = match std::fs::read_to_string(path) {
            Ok(text) => Some(text),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => None,
            Err(err) => return Err(format!("{}: {err}", path.display())),
        };
        if before.is_none() && goro.is_none() {
            continue;
        }
        let mut config: Value = match &before {
            Some(text) if !text.trim().is_empty() => serde_json::from_str(text).map_err(|e| {
                format!(
                    "{} is not valid JSON ({e}); not touching it",
                    path.display()
                )
            })?,
            _ => json!({}),
        };
        let Some(root) = config.as_object_mut() else {
            return Err(format!(
                "{} is not a JSON object; not touching it",
                path.display()
            ));
        };
        let hooks = root.entry("hooks").or_insert_with(|| json!({}));
        let Some(hooks) = hooks.as_object_mut() else {
            return Err(format!(
                "{}: \"hooks\" is not an object; not touching it",
                path.display()
            ));
        };
        for (event, arg) in EVENTS {
            remove_goro_hooks(hooks, event, agent);
            if let Some(goro) = goro {
                let groups = hooks.entry(event).or_insert_with(|| json!([]));
                if let Some(groups) = groups.as_array_mut() {
                    groups.push(json!({
                        "hooks": [{
                            "type": "command",
                            "command": hook_command(goro, agent, arg),
                            "timeout": 5
                        }]
                    }));
                }
            }
        }
        if hooks.is_empty() {
            root.remove("hooks");
        }
        let mut after = serde_json::to_string_pretty(&config).expect("JSON serializes");
        after.push('\n');
        edits.push(FileEdit {
            agent,
            path: path.to_path_buf(),
            before,
            after,
        });
    }
    Ok(edits)
}

fn is_goro_command(command: &str, agent: Agent) -> bool {
    EVENTS.iter().any(|(_, arg)| {
        command.ends_with(&format!(" hook {} {arg}", agent.name())) && command.contains("goro")
    })
}

fn has_goro_hook(config: &Value, event: &str, agent: Agent) -> bool {
    config["hooks"][event].as_array().is_some_and(|groups| {
        groups.iter().any(|group| {
            group["hooks"].as_array().is_some_and(|hooks| {
                hooks.iter().any(|h| {
                    h["command"]
                        .as_str()
                        .is_some_and(|c| is_goro_command(c, agent))
                })
            })
        })
    })
}

/// Remove Goro's commands for `event`, dropping groups and events left empty.
fn remove_goro_hooks(hooks: &mut Map<String, Value>, event: &str, agent: Agent) {
    let Some(groups) = hooks.get_mut(event).and_then(Value::as_array_mut) else {
        return;
    };
    for group in groups.iter_mut() {
        if let Some(list) = group.get_mut("hooks").and_then(Value::as_array_mut) {
            list.retain(|h| {
                !h["command"]
                    .as_str()
                    .is_some_and(|c| is_goro_command(c, agent))
            });
        }
    }
    groups.retain(|group| group["hooks"].as_array().is_none_or(|l| !l.is_empty()));
    if groups.is_empty() {
        hooks.remove(event);
    }
}

fn shell_quote(s: &str) -> String {
    if s.bytes()
        .all(|b| b.is_ascii_alphanumeric() || b"/._-+".contains(&b))
    {
        s.to_string()
    } else if cfg!(windows) {
        format!("\"{s}\"")
    } else {
        format!("'{}'", s.replace('\'', r"'\''"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn files(dir: &Path) -> HookFiles {
        HookFiles {
            claude_settings: Some(dir.join("claude/settings.json")),
            codex_hooks: Some(dir.join("codex/hooks.json")),
        }
    }

    fn apply_all(edits: &[FileEdit]) {
        for edit in edits {
            edit.apply().unwrap();
        }
    }

    const EXISTING: &str = r#"{
  "theme": "dark",
  "hooks": {
    "SessionStart": [
      { "matcher": "*", "hooks": [{ "type": "command", "command": "bash other.sh", "timeout": 10 }] }
    ],
    "Stop": [
      { "hooks": [{ "type": "command", "command": "notify-me" }] }
    ]
  },
  "zebra": 1
}
"#;

    #[test]
    fn install_keeps_other_settings_and_hooks_and_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let files = files(dir.path());
        std::fs::create_dir_all(dir.path().join("claude")).unwrap();
        std::fs::write(files.claude_settings.as_ref().unwrap(), EXISTING).unwrap();
        let goro = Path::new("/Applications/Goro.app/Contents/MacOS/goro");

        let edits = plan_install(&files, goro).unwrap();
        assert_eq!(edits.len(), 2, "both agents");
        apply_all(&edits);
        let claude: Value = serde_json::from_str(
            &std::fs::read_to_string(files.claude_settings.as_ref().unwrap()).unwrap(),
        )
        .unwrap();
        assert_eq!(claude["theme"], "dark");
        assert_eq!(
            claude["hooks"]["SessionStart"][0]["hooks"][0]["command"],
            "bash other.sh"
        );
        assert_eq!(
            claude["hooks"]["Stop"][0]["hooks"][0]["command"],
            "notify-me"
        );
        assert_eq!(
            claude["hooks"]["Stop"][1]["hooks"][0]["command"],
            "/Applications/Goro.app/Contents/MacOS/goro hook claude stop"
        );
        assert_eq!(
            claude["hooks"]["UserPromptSubmit"][0]["hooks"][0]["command"],
            "/Applications/Goro.app/Contents/MacOS/goro hook claude prompt"
        );
        // Key order of the user's file is preserved.
        let text = std::fs::read_to_string(files.claude_settings.as_ref().unwrap()).unwrap();
        assert!(text.find("\"theme\"").unwrap() < text.find("\"zebra\"").unwrap());
        assert!(installed(&files).iter().all(|(_, ok)| *ok));

        // Reinstalling (even from a moved binary) replaces rather than duplicates.
        let again = plan_install(&files, Path::new("/usr/local/bin/goro")).unwrap();
        apply_all(&again);
        let claude: Value = serde_json::from_str(
            &std::fs::read_to_string(files.claude_settings.as_ref().unwrap()).unwrap(),
        )
        .unwrap();
        assert_eq!(claude["hooks"]["Stop"].as_array().unwrap().len(), 2);
        assert_eq!(
            claude["hooks"]["Stop"][1]["hooks"][0]["command"],
            "/usr/local/bin/goro hook claude stop"
        );
        assert!(
            !plan_install(&files, Path::new("/usr/local/bin/goro"))
                .unwrap()
                .iter()
                .any(FileEdit::changes_something)
        );
    }

    #[test]
    fn uninstall_restores_other_hooks_only() {
        let dir = tempfile::tempdir().unwrap();
        let files = files(dir.path());
        std::fs::create_dir_all(dir.path().join("claude")).unwrap();
        std::fs::write(files.claude_settings.as_ref().unwrap(), EXISTING).unwrap();
        apply_all(&plan_install(&files, Path::new("/bin/goro")).unwrap());
        apply_all(&plan_uninstall(&files).unwrap());
        let claude: Value = serde_json::from_str(
            &std::fs::read_to_string(files.claude_settings.as_ref().unwrap()).unwrap(),
        )
        .unwrap();
        let original: Value = serde_json::from_str(EXISTING).unwrap();
        assert_eq!(claude, original);
        let codex: Value = serde_json::from_str(
            &std::fs::read_to_string(files.codex_hooks.as_ref().unwrap()).unwrap(),
        )
        .unwrap();
        assert_eq!(
            codex,
            json!({}),
            "a file Goro created ends up empty, not deleted"
        );
        assert!(installed(&files).iter().all(|(_, ok)| !*ok));
    }

    #[test]
    fn invalid_config_is_left_alone() {
        let dir = tempfile::tempdir().unwrap();
        let files = files(dir.path());
        std::fs::create_dir_all(dir.path().join("claude")).unwrap();
        std::fs::write(files.claude_settings.as_ref().unwrap(), "{ oops").unwrap();
        let err = plan_install(&files, Path::new("/bin/goro")).unwrap_err();
        assert!(err.contains("not valid JSON"), "{err}");
    }

    #[test]
    fn commands_quote_unusual_paths() {
        let cmd = hook_command(Path::new("/Users/me/My Apps/goro"), Agent::Codex, "prompt");
        if cfg!(windows) {
            assert_eq!(cmd, "\"/Users/me/My Apps/goro\" hook codex prompt");
        } else {
            assert_eq!(cmd, "'/Users/me/My Apps/goro' hook codex prompt");
        }
    }

    #[test]
    fn payloads_from_both_agents_parse() {
        let claude: HookPayload = serde_json::from_str(
            r#"{"session_id":"abc","transcript_path":"/t.jsonl","cwd":"/w","hook_event_name":"UserPromptSubmit","prompt":"fix it"}"#,
        )
        .unwrap();
        assert_eq!(claude.prompt.as_deref(), Some("fix it"));
        let codex: HookPayload = serde_json::from_str(
            r#"{"session_id":"s","turn_id":"t9","cwd":"/w","hook_event_name":"Stop","stop_hook_active":false,"last_assistant_message":null,"transcript_path":null,"permission_mode":"default"}"#,
        )
        .unwrap();
        assert_eq!(codex.turn_id.as_deref(), Some("t9"));
    }
}
