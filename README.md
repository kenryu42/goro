# Goro

A native, instant-open review app for agent-written code changes. See [PRD.md](PRD.md)
and [ARCHITECTURE.md](ARCHITECTURE.md).

Status: M3. `goro [path]` opens a file tree and a syntax-highlighted diff of the
working tree (staged, unstaged, untracked) that updates live as an agent works, marks
what changed since you last looked, and lets you stage, unstage, discard, undo and commit
in the same window. Without a path it opens the repository an agent (Claude Code, Codex)
worked in most recently. One instance: later `goro` calls hand off to the open app.
With agent hooks installed, every agent turn is snapshotted so you can review one turn (or
a whole session) on its own, and comments go back to the agent as markdown.

## Agents

```sh
goro hooks install      # Claude Code (~/.claude/settings.json) and Codex (~/.codex/hooks.json)
goro hooks status
goro hooks uninstall
```

The hooks run `goro hook <agent> prompt|stop` at the start and end of each turn. They
return in a few milliseconds, print nothing, never fail the agent's turn, and snapshot
the working tree under `refs/goro/turns/` (your index is never touched). Codex asks you to
trust new hooks once: run `/hooks` in Codex after installing.

To have an agent ask for your review, tell it (or put in `AGENTS.md` / `CLAUDE.md`):

> When you finish a change, run `goro --wait` and address every comment it prints.

`goro --wait` opens the review and blocks until you press ⌘⇧↵ (Send review); the
comments come back on stdout as markdown. `goro comments [--clear]` prints pending
comments, and `y` in the app copies them.

## Develop

```sh
cargo run --release -p goro -- [path]   # open a repository (defaults to the current directory)
cargo test --workspace                  # core tests against real temp repos, plus headless UI tests
```

| Key | Action |
|---|---|
| `j` `k` (arrows) | move the cursor; with `shift`, select lines |
| `n` `p` · `]` `[` | next/previous hunk · file |
| `s` · `S` | stage the hunk, selection or file under the cursor (unstage if staged) · whole file |
| `x` · `X` | discard it (refused if the file changed since you looked) · whole file |
| `u` (`cmd-z`) | undo the last stage/unstage/discard |
| `tab` · `shift-tab` · `m` | next/previous line changed since you last looked · mark all seen |
| `r` | mark the hunk (or file, on its header) reviewed; it collapses |
| `cmd-p` / `ctrl-p` | switch repository (recent and agent-active ones) |
| `t` · `<` `>` · `w` | review one agent turn or session · previous/next turn · back to the working tree |
| `a` · `enter` · `x` | comment on the line or selection · edit the comment under the cursor · delete it |
| `y` · `cmd-shift-enter` | copy comments as markdown · send them to the agent waiting on `goro --wait` |
| `c` · `cmd-enter` · `esc` | focus the commit message · commit · back to the diff |
| `cmd-q` / `ctrl-q` | quit |

Header buttons do the same with the mouse; shift-click extends a selection.

Linux build dependencies (Debian/Ubuntu): `libxkbcommon-dev libxkbcommon-x11-dev
libxcb1-dev libx11-xcb-dev libfontconfig-dev libfreetype-dev libwayland-dev`.

## Performance budgets

```sh
GORO_TRACE_STARTUP=1 target/release/goro .                     # per-phase startup timings
cargo bench -p goro-ui --bench scroll                            # frame time, 50k-line review

git clone --depth 1 --branch go1.25.0 https://github.com/golang/go.git /tmp/go
scripts/bench/make_changes.py /tmp/go --files 20 --lines 10      # deterministic change
scripts/bench/startup.py target/release/goro /tmp/go --budget-ms 300
```

`GORO_SCREENSHOT_DIR=<dir> cargo test -p goro-ui --test visual` saves headless
screenshots of the UI (macOS).
