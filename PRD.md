# Goro — Product Requirements (v1)

## One line

A native, instant-open desktop app that shows exactly what a coding agent changed in a
git repo and lets you decide what to keep: stage it, discard it, commit it, all in one
window, with no editor and no AI in the way.

## Problem

Agents write most of the code; the developer's job is reviewing it. Today that means
opening an AI-first IDE (slow, pushes its own agent), a TUI, or a browser tab served by a
local server. Native git GUIs (Sublime Merge) are fast but know nothing about agents:
they can't tell you what changed *in this turn* or *since you last looked*.

## Positioning

Native speed is the entry ticket, not the pitch. The pitch is **the review tool that
understands agent turns**: what changed since you last looked, what each prompt produced,
and a review that ends in a commit. The free TUIs (Hunk, tuicr) and web viewers
(Plannotator, Diffity) are read-and-comment tools; Goro is review-then-act.

## Principles

1. **Instant.** Launch to rendered diff faster than you can switch windows. Performance
   budgets are release blockers, not goals.
2. **Quiet.** No AI, no model calls, no chat, no telemetry, no network access at all in v1.
3. **Review ends in action.** Every view has a keep/discard/commit path.
4. **Safe with a live agent.** Goro never silently overwrites agent work, and every
   discard is undoable.
5. **Agent-aware, agent-neutral.** Reads Claude Code and Codex artifacts; depends on none.

## Users

Primary: developers who run coding agents (Claude Code, Codex, others) against local git
repos and review their output before committing. Keyboard-heavy, multi-repo, often with
an agent running in the next window.

## Platforms

macOS 13+ (arm64, x86_64), Windows 10 22H2+ / 11 (x86_64, arm64), Linux (x86_64, arm64;
X11 and Wayland). Feature parity across all three, except where the OS forbids it (noted
below).

## v1 scope

### 1. Opening the right thing

- Launch from dock / Start menu / app launcher, `goro [path]`, a global hotkey, or an
  agent hook.
- **Auto-detect the repo with no arguments**, in this order:
  1. explicit path argument, or the CLI's working directory;
  2. the repo with the most recent agent activity: Goro hook events, then Claude Code
     session logs (`~/.claude/projects/*/*.jsonl`, `cwd` field), then Codex rollouts
     (`~/.codex/sessions/YYYY/MM/DD/*.jsonl`, `session_meta.cwd`);
  3. the most recently used repo with the newest working-tree change.
- Quick switcher (`Cmd/Ctrl+P`-style) listing recent repos with their changed-file counts.
- Single instance: a second `goro` invocation hands off to the running window.
- Global hotkey on macOS and Windows; on Linux via the XDG GlobalShortcuts portal where
  the desktop supports it, otherwise the CLI or an agent hook.

### 2. Seeing the change

- File tree of changes grouped as Staged / Unstaged / Untracked, with counts and status
  (added, modified, deleted, renamed, mode change, binary, submodule, conflicted).
- One continuous, virtualized diff stream across all files with sticky file headers; the
  file tree scrolls it.
- Unified and side-by-side views; word-level intra-line highlighting; tree-sitter syntax
  highlighting; expandable context.
- Generated and lock files (`linguist-generated` in `.gitattributes`, known lockfiles)
  collapsed by default.
- Binary files: size and type summary; images shown before/after.
- Conflicted files shown read-only with conflict markers (resolution is out of scope).
- Follows OS light/dark theme.

### 3. Acting on it

- Stage / unstage a hunk, a file, or a selection of lines.
- Discard a hunk, a file, or a selection of lines, with **compare-and-swap**: the discard
  applies only if the file on disk still matches what you are looking at; otherwise it is
  refused and the view refreshes.
- **Undo** for every stage, unstage, and discard (discarded content is stored as git
  objects under Goro's private refs before it is removed).
- Commit box: message, amend, and sign-off. Runs through `git commit`, so hooks and
  signing behave exactly as on the command line; hook output shown on failure.
- Mark a file or hunk as reviewed; reviewed state survives restarts and resets
  automatically if that hunk's content changes.

### 4. Watch mode (always on)

- The view updates live as the working tree, index, or HEAD change (target: ≤150 ms).
- **"New since last look"**: hunks that appeared or changed since you last focused Goro
  are highlighted, with a jump-to-next-new command.
- With agent hooks installed: a **turn timeline**, where each agent turn (prompt → stop) is
  a snapshot, and you can view the diff of any single turn, any range of turns, or
  everything since the session started, even if the agent committed along the way.

### 5. Agent integration

- `goro hooks install` wires Claude Code (and Codex, where its hook surface allows)
  to call `goro hook <event>`, which records turn snapshots and nudges the running app.
  Hooks never block the agent for more than 50 ms and never fail its turn.
- **Blocking review**: `goro --wait` opens the review and blocks until you submit; it
  prints your comments as markdown on stdout and exits. Agents can use this as a skill.
- Line and range comments; export as structured markdown to the clipboard or stdout (file,
  line range, the diff excerpt, the comment).

### 6. Keyboard and settings

- Every action has a shortcut; standard platform shortcuts plus `j/k`, `n/p` (next/prev
  hunk), `s` (stage), `x` (discard), `u` (undo), `c` (commit).
- Settings file (font, font size, theme, default diff layout, keybindings). No settings UI
  beyond opening the file.

## Non-goals (v1)

- Editing files (not even inline). Goro is not an editor.
- Any AI or model calls, chat, or suggestions.
- Agent orchestration, worktree management, or running agents.
- Branching, merging, rebasing, pushing, pulling, or forge (GitHub/GitLab) integration.
- Merge-conflict resolution.
- jj, Mercurial, Perforce. Git only.
- Linking hunks to agent reasoning or tool calls. That's the v2 moat; v1 turn snapshots
  and session-log discovery lay the groundwork.
- Auto-update. Package managers and GitHub Releases handle distribution.

## Performance budget (release blockers)

Measured on each OS on reference hardware (Apple M1; a 2023 mid-range x86_64 laptop running
Windows 11 and Ubuntu LTS), with a warm OS file cache.

| Metric | Budget |
|---|---|
| Cold start → first diff painted, 10k-file repo, 20-file change | ≤ 300 ms |
| Cold start → first diff painted, Linux-kernel-size repo (~90k files) | ≤ 500 ms |
| Handoff to a running instance → window focused on the new repo | ≤ 100 ms |
| Scroll / next-hunk / keystroke → frame | ≤ 16 ms (p99 ≤ 33 ms) |
| 50k-line, 1k-file changeset: time to first paint | same as above; the rest streams in |
| Working tree write → UI updated | ≤ 150 ms |
| `goro hook` wall time (snapshot included), 10k-file repo | ≤ 50 ms |
| Idle CPU with watch mode on | ~0% |
| Memory, 10k-line changeset | ≤ 150 MB RSS |
| Installed size | ≤ 40 MB |

## Distribution

Open source (Apache-2.0 OR MIT). GitHub Releases for all platforms; Homebrew cask,
winget and Scoop, AppImage / `.deb` / tarball. macOS builds signed and notarized; Windows
builds signed when a certificate is available. Paid tiers are a later decision.

## Success signals

- Budgets met on all three OSes in CI and on reference hardware.
- Maintainers use it daily as their only review surface for agent output.
- Organic adoption: stars, package-manager installs, and issues from people who didn't
  hear about it from us.

## Milestones

| | Milestone | Done when |
|---|---|---|
| **M0** | Instant diff (the gate) | `goro [path]` → file tree + highlighted unified diff stream on all 3 OSes, startup budget measured in CI |
| M1 | Act | Stage, unstage, and discard for hunks, files, and lines; undo; commit box |
| M2 | Live | Watch mode, new-since-last-look, reviewed marks, single instance, auto-detect, switcher |
| M3 | Agent-aware | Hooks, turn timeline, `--wait` blocking review, comment export |
| M4 | Ship v1 | Split view, images, settings and keybindings, global hotkey, signed packages for all 3 OSes |
