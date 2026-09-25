# Goro — Architecture

Companion to [PRD.md](PRD.md). Describes the shape of v1 and the decisions that are
expensive to change later.

## Stack

| Concern | Choice | Why |
|---|---|---|
| Language | Rust (stable, edition 2024) | One codebase for 3 OSes; native speed; same language as the UI layer |
| UI | GPUI (`gpui-pre =0.3.6`) + gpui-component (`=0.6.6`) | GPU-rendered, platform text systems, proven by Zed on macOS/Windows/Linux; gpui-component supplies tree, virtual lists, inputs, theming |
| Git reads | `gix` (gitoxide) | Fast status, tree/blob/index access, filter pipeline, pure Rust (clean Windows builds) |
| Git writes | `git` CLI | Hooks, signing, filters, LFS, `core.*` config behave exactly as the user expects |
| Diff | `imara-diff` (histogram) | Fast, and the same algorithm family git uses |
| Syntax | `tree-sitter` + bundled grammars and highlight queries | Accurate, incremental, UI-independent |
| FS watch | `notify` (FSEvents / ReadDirectoryChangesW / inotify) | Standard cross-platform watcher |
| IPC | `interprocess` local sockets (Unix socket / named pipe) | Single-instance handoff and hook → app nudges |

**Version pinning.** GPUI changes most weeks. Both GPUI crates are pinned with `=`, and
upgrades are deliberate, one-commit changes. Reference sources for agents working on the
UI live next to the code as a pinned git submodule or a documented checkout path, never
in context by default: Zed at the matching tag, and gpui-component examples.

**License constraint.** Zed's `editor` and `git_ui` crates are GPL-3.0. Goro is
Apache-2.0 OR MIT, so their code is **not** copied; GPUI and gpui-component are
Apache-2.0 and fine to depend on.

## Workspace layout

```
crates/
  goro-core/     UI-free: repo discovery, status, diff model, staging/discard, snapshots,
                 watch, agent-log discovery, hooks, comments, persistence. Most tests live here.
  goro-ui/       GPUI views and elements. Depends on goro-core. No git or fs calls.
  goro/          The single binary: CLI parsing, single-instance handoff, hook subcommands,
                 launching goro-ui.
```

Three crates, no more until a real boundary appears. Syntax highlighting lives in
`goro-core::syntax` (it's UI-free); if grammar compile times hurt, it becomes its own
crate.

## One binary, two modes

`goro` is both the CLI and the app.

```
goro [path] [--wait]      → try IPC handoff to a running instance (≤ 10 ms), else start the GUI
goro hook <event>         → read hook JSON on stdin, snapshot, nudge app via IPC, exit. Never inits GPUI.
goro hooks install|remove → edit agent hook configs (shows the diff and asks first)
goro comments             → print pending comments as markdown
```

Hook and CLI paths must not link-time-initialize anything GPU-related. The GUI path
starts as early in `main` as possible.

On macOS the binary ships inside `Goro.app`; `goro` in `PATH` is a symlink to it. When
started from a terminal, the GUI detaches from the terminal session (re-exec detached on
Unix, `DETACHED_PROCESS` on Windows), except with `--wait`, which stays attached and
receives the result over IPC.

## Startup pipeline (the budget)

Target: first diff painted in ≤ 300 ms. Work runs in parallel wherever it can:

```
t0 main()
 ├─ [fg] parse args → IPC probe (instance alive? hand off and exit)
 ├─ [bg] resolve target repo (arg → activity log → agent logs → MRU); ≤ 20 ms, stats + first/last lines only
 ├─ [fg] GPUI app + window creation (GPU device, fonts)
 ├─ [bg] gix status (index vs HEAD, worktree vs index), streamed per file
 ├─ [bg] diff + highlight the FIRST visible file only
 └─ [fg] first paint: tree + first file's hunks
     then: remaining files diffed/highlighted in viewport-priority order
```

Rules:
- Nothing on the UI thread does IO.
- Only the visible viewport ± one screen is highlighted and shaped; the rest waits.
- Startup timing is instrumented (`GORO_TRACE_STARTUP=1` prints phase timings;
  `--bench-exit-after-first-paint` for CI).

## Core data model

```
Repo            root, gitdir, gix handle
ChangeSet       Arc<[FileChange]>, an immutable snapshot of "what changed" for one comparison
Comparison      HeadToIndex | IndexToWorktree | Tree(a) → Worktree | Tree(a) → Tree(b)
FileChange      path, old_path?, status, old/new blob ids, old/new mode, binary?, hunks (lazy)
Hunk            header ranges, lines, content_hash (stable id for reviewed / new-since state)
Line            kind (ctx/add/del), old_no?, new_no?, text range, word-diff spans
```

The UI only ever holds `Arc` snapshots. Core produces a new `ChangeSet` on every refresh;
the UI diffs old vs new by `(path, hunk.content_hash)` to keep scroll position, selection,
reviewed marks, and "new since last look" highlights stable.

## Git: reads vs writes

**Reads (gix):** status, index, HEAD tree, blob contents, `.gitattributes`
(`linguist-generated`, `binary`, `diff`), ignore rules, and the worktree **clean filter**
(so CRLF and other filters produce the same diff `git diff` would).

**Writes (git CLI, spawned with a fixed env, `LC_ALL=C`, no pager, `GIT_OPTIONAL_LOCKS=0`
for reads that must shell out):**

| Action | Mechanism |
|---|---|
| Stage hunk / lines | Build an exact patch for the selection (zero fuzz) → `git apply --cached --recount` |
| Unstage | Same, reversed against the index → `git apply --cached -R` |
| Discard hunk / lines | Save undo first (below) → `git apply -R` on the worktree. `git apply` refuses if context or removed lines no longer match: that is the compare-and-swap. Goro also checks the file's blob hash before applying. |
| Discard whole file | Save undo → `git checkout -- <path>` / delete untracked |
| Commit | `git commit -F -` (message on stdin), `--amend`/`--signoff` as chosen. Runs in background; hooks and pinentry work as usual; stderr is surfaced on failure. |
| Snapshot | Temp index (`GIT_INDEX_FILE`) seeded from the real index → `git add -A` → `git write-tree` → `git commit-tree`. The user's index is never touched. |

Any write that races with an external change fails loudly and triggers a refresh. Never
retry blindly.

## Private refs

Goro stores its state as ordinary git objects under `refs/goro/`, so it's gc-safe,
portable with the repo, and removable with `goro clean`:

```
refs/goro/seen                      last-look snapshot (tree) → "new since last look"
refs/goro/turns/<session>/<n>       turn snapshots (commit; message = prompt metadata)
refs/goro/undo                      chain of commits holding pre-discard content
```

Retention: turns and undo entries older than 14 days or beyond 200 per repo are pruned
on launch. Refs under `refs/goro/` show up in `git log --all`; that trade-off is
documented, and it's how GitButler and others persist state too.

Non-git state (window layout, reviewed marks, comments, MRU repos, activity log) lives in
the platform data dir (`dirs`): `~/Library/Application Support/Goro`,
`$XDG_STATE_HOME/goro`, `%LOCALAPPDATA%\Goro`. `GORO_DATA_DIR` overrides it, and tests
always set it to a temp dir.

## Watch mode

- One `notify` watcher per open repo on the worktree root plus `.git/index`, `.git/HEAD`,
  `.git/refs` (and `packed-refs`); `.git/objects` and ignored paths are filtered out.
- Events are debounced (≈40 ms trailing) and coalesced into a path set.
  - Worktree paths → re-status and re-diff only those paths.
  - Index / HEAD / refs → full status refresh (still streamed).
- The app itself is a writer (stage, discard, commit); self-inflicted events are not
  special-cased. The refresh is cheap and idempotent.
- "Last look" = the snapshot taken when the Goro window loses focus after being focused
  for ≥ 1 s, or when you press "mark all seen". New-since-last-look = diff(`refs/goro/seen`,
  worktree) intersected with the current change set.

## Agent integration

**Repo auto-detect (read-only, no setup):**
- Goro activity log (written by `goro hook`) is checked first.
- Claude Code: newest `~/.claude/projects/*/*.jsonl` by mtime; read `cwd` from its
  records.
- Codex: newest `~/.codex/sessions/YYYY/MM/DD/rollout-*.jsonl` (scan today and yesterday
  only); `cwd` from the first `session_meta` line.
- `cwd` → repo root via git discovery. Missing or unparseable logs are skipped silently;
  formats are treated as unstable and covered by fixture tests.

**Hooks:**
- Claude Code: `UserPromptSubmit` → snapshot "turn start"; `Stop` → snapshot "turn end" and
  nudge the app. Hook payload (`session_id`, `cwd`, `prompt`, `transcript_path`) is stored
  in the turn commit's message, which is the anchor for v2 intent linking.
- Codex: the `notify` turn-complete program (plus richer hooks if available when M3
  starts; verify then).
- `goro hook` must finish in ≤ 50 ms, exit 0 on any internal error, and write errors only
  to Goro's log file, never to the agent's output.

**Blocking review (`--wait`):** the CLI registers a waiter over IPC; submitting the review
in the app sends back the markdown and the CLI prints it to stdout and exits 0.

## Rendering the diff

- A single virtualized list (`uniform_list`-style) of **display rows**: file header, hunk
  header, line, collapsed-context marker, binary summary. Row heights are uniform per
  layout mode, so scroll math is O(1).
- A custom GPUI element per row paints gutter, background, word-diff spans, and
  highlighted text runs. Shaped lines are cached by `(blob, line, theme, font)`.
- Side-by-side uses the same row model with paired old/new columns.
- Highlighting: tree-sitter parses the whole old and new file on a background thread (it
  has to for correct results); until it arrives, rows paint as plain text and then swap
  in highlighting.
- Files over 1 MB or 20k lines: diffed, but highlighted only in the viewport window, with
  an explicit "load full highlighting" action.

## Concurrency

GPUI's foreground executor owns all UI state. Core work runs on GPUI's background
executor, returning `Arc` snapshots via `Task`s; stale results (older generation number
than the latest request) are dropped. git CLI calls run on background threads with
timeouts, except `commit`, which waits for hooks.

## Testing

- **goro-core:** each test creates a temp repo (`tempfile`) with real `git` and sets
  `GORO_DATA_DIR` to a temp dir. Stage, discard, and undo tests assert against
  `git diff`/`git diff --cached` output, not Goro's own model. Property tests: random
  line selections staged then unstaged are a no-op; discard then undo restores
  byte-for-byte (CRLF, filters, and no-trailing-newline cases included).
- **Agent-log fixtures:** checked-in, trimmed real Claude Code / Codex logs; parsers tested
  against them.
- **goro-ui:** GPUI `TestAppContext` for view logic (selection, keyboard, row model).
- **Performance:** startup benchmark via `--bench-exit-after-first-paint` on macOS, Windows,
  and Linux CI runners against pinned reference repos (git/git, a 10k-file repo,
  linux); frame-time benchmark on a generated 50k-line changeset. Regressions > 10%
  fail CI. Absolute budgets are checked on reference hardware before each release.
- Logs go to the data dir; tests capture and assert log output and never write outside
  their temp dir.

## Risks

| Risk | Mitigation |
|---|---|
| GPUI API churn, sparse docs, weaker agent output | Exact pins; reference checkouts; thin UI crate; most logic in goro-core behind tests. M0 is the gate. |
| Windows cold start (Defender scan, DirectX init) | Measure in M0; signed binaries; keep the binary small |
| Linux: Wayland hotkeys, inotify watch limits on huge repos | Portal hotkey where supported; watch limits surfaced to the user (see open questions) |
| GPUI accessibility is limited | Track upstream; keyboard-complete UI from day one |
| Agent log formats change | Read-only, best-effort, fixture-tested; hooks are the primary signal |

## Open questions (decide when reached)

- inotify watch-limit exhaustion on very large repos: warn only, or offer a polling mode?
  (A polling mode is a fallback, so it needs explicit approval.)
- Codex hook surface at M3 time.
- Accessibility bar for v1 (screen readers) given GPUI's current support.

## v2 hook points (not built in v1)

- Turn commits already carry `session_id` + `transcript_path`, so per-hunk intent linking is a
  join between turn diffs and transcript tool calls (Edit/Write/apply_patch), rendered in
  a side panel.
- The `Comparison` enum and git backend in goro-core are the seams for jj later.
