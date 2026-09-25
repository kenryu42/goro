//! Running the `git` CLI for everything that writes, so hooks, signing, filters and the
//! user's config behave exactly as on the command line.

use std::ffi::{OsStr, OsString};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

#[derive(Debug, thiserror::Error)]
pub enum GitError {
    #[error("failed to run git: {0}")]
    Spawn(#[from] std::io::Error),
    #[error("git {args} failed: {message}")]
    Failed {
        args: String,
        /// Exit code, if git exited normally.
        code: Option<i32>,
        /// stderr, or stdout if stderr was empty (hooks print to either).
        message: String,
    },
}

/// Identity for Goro's internal commits (undo history), so they work without a
/// configured `user.name`/`user.email`.
const INTERNAL_IDENTITY: [(&str, &str); 4] = [
    ("GIT_AUTHOR_NAME", "Goro"),
    ("GIT_AUTHOR_EMAIL", "goro@localhost"),
    ("GIT_COMMITTER_NAME", "Goro"),
    ("GIT_COMMITTER_EMAIL", "goro@localhost"),
];

#[derive(Debug, Clone)]
pub struct Git {
    root: PathBuf,
    env: Vec<(OsString, OsString)>,
}

pub struct Output {
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

impl Git {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            env: Vec::new(),
        }
    }

    /// Extra environment for every git invocation (tests use it to isolate config).
    pub fn with_env(mut self, key: impl Into<OsString>, value: impl Into<OsString>) -> Self {
        self.env.push((key.into(), value.into()));
        self
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Run git in the repository root; fails on a non-zero exit.
    pub fn run<I, S>(&self, args: I, stdin: Option<&[u8]>) -> Result<Output, GitError>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        self.run_with_env(args, stdin, &[])
    }

    /// Run git for Goro's own bookkeeping commits.
    pub fn run_internal<I, S>(&self, args: I, stdin: Option<&[u8]>) -> Result<Output, GitError>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        self.run_with_env(args, stdin, &INTERNAL_IDENTITY)
    }

    fn run_with_env<I, S>(
        &self,
        args: I,
        stdin: Option<&[u8]>,
        env: &[(&str, &str)],
    ) -> Result<Output, GitError>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let args: Vec<_> = args.into_iter().map(|a| a.as_ref().to_owned()).collect();
        let mut command = Command::new("git");
        command
            .current_dir(&self.root)
            .args(["--no-pager", "--literal-pathspecs"])
            .args(&args)
            .env("GIT_TERMINAL_PROMPT", "0")
            .envs(self.env.iter().map(|(k, v)| (k, v)))
            .envs(env.iter().copied())
            .stdin(if stdin.is_some() {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = command.spawn()?;
        if let Some(input) = stdin {
            // Write on a thread so a chatty child can't deadlock on a full stdout pipe.
            let mut pipe = child.stdin.take().expect("stdin is piped");
            let input = input.to_vec();
            std::thread::spawn(move || {
                let _ = pipe.write_all(&input);
            });
        }
        let out = child.wait_with_output()?;
        if out.status.success() {
            Ok(Output {
                stdout: out.stdout,
                stderr: out.stderr,
            })
        } else {
            Err(GitError::Failed {
                args: args
                    .iter()
                    .map(|a| a.to_string_lossy())
                    .collect::<Vec<_>>()
                    .join(" "),
                code: out.status.code(),
                message: {
                    let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
                    if stderr.is_empty() {
                        String::from_utf8_lossy(&out.stdout).trim().to_string()
                    } else {
                        stderr
                    }
                },
            })
        }
    }
}
