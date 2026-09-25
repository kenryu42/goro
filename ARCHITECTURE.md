# Goro — Architecture

Companion to [PRD.md](PRD.md). Describes the shape of v1 and the decisions that are
expensive to change later.

## Stack

| Concern | Choice | Why |
|---|---|---|
| Language | Rust (stable, edition 2024) | One codebase for 3 OSes; native speed; same language as the UI layer |
| UI | GPUI via `gpui-kit =0.6.6` (pins `gpui-pre =0.3.6`), **without** gpui-component | GPU-rendered, platform text systems, proven by Zed on macOS/Windows/Linux. gpui-component's `init` alone cost ~105 ms of startup (measured in M0), so Goro builds its few widgets (tree, diff list) on plain GPUI |
| Git reads | `gix` (gitoxide) | Fast status, tree/blob/index access, filter pipeline, pure Rust (clean Windows builds) |
| Git writes | `git` CLI | Hooks, signing, filters, LFS, `core.*` config behave exactly as the user expects |
| Diff | `imara-diff` (histogram) | Fast, and the same algorithm family git uses |
| Syntax | `tree-sitter` + bundled grammars and highlight queries | Accurate, incremental, UI-independent |
| FS watch | `notify` (FSEvents / ReadDirectoryChangesW / inotify) | Standard cross-platform watcher |
| IPC | `interprocess` local sockets (Unix socket / named pipe) | Single-instance handoff and hook → app nudges |

**Version pinning.** GPUI changes most weeks. `gpui-kit` is pinned with `=` and pins the
matching `gpui-pre-*` crates; upgrades are deliberate, one-commit changes. The pinned
sources in `~/.cargo/registry` (`gpui-pre-0.3.6/examples`, `gpui-kit-0.6.6`) are the
reference for agents working on the UI. Some useful APIs are `test-support`-only (e.g.
`UniformListScrollHandle::logical_scroll_top_index`); reimplement them from public state
rather than enabling test features in release builds.

**License constraint.** Zed's `editor` and `git_ui` crates are GPL-3.0. Goro is
Apache-2.0 OR MIT, so their code is **not** copied; GPUI and gpui-kit are Apache-2.0
and fine to depend on.

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
 ├─ [fg] parse args → IPC probe (instance alive? hand off and exit)            (M2)
 ├─ [bg] resolve target repo (arg → activity log → agent logs → MRU)           (M2: arg/cwd only in M0)
 ├─ [bg] gix status → diff + highlight the first file → `Opened` event
 ├─ [fg] GPUI platform init (~50 ms on M3 Pro)
 ├─ [fg] wait for `Opened` (≤ 250 ms), so the first frame already shows the diff
 ├─ [fg] window creation + first frame drawn (~60 ms)
 └─ [bg] remaining files diffed/highlighted in parallel (rayon), streamed to the UI in batches
```

Rules:
- Nothing on the UI thread does IO.
- The window waits up to 250 ms for the review so there is no empty-window flash; a
  slower repository opens immediately and streams in.
- Rows are virtualized: only visible rows are laid out and shaped. Whole files are
  parsed for highlighting (correctness needs context), but only spans on displayed
  lines are kept.
- Startup timing is instrumented (`GORO_TRACE_STARTUP=1` prints phase timings;
  `--bench-exit-after-first-paint` prints `first_paint_ms=` once the first frame
  containing the diff has been drawn, then quits).
- macOS pauses drawing for occluded windows. First paint is measured when the frame is
  drawn, not via a later frame callback, so benchmarks don't hang behind other windows.

M0 measurements (M3 Pro, warm cache, median of 7):

| Repository | Change | First diff drawn |
|---|---|---|
| this repo | 17 files | 115 ms |
| golang/go (14.5k files) | 20 files | 135 ms |
| linux (89.8k files) | 20 files (+13 case-collision files) | 272 ms |
| linux | 1,013 files / 50k lines | 285 ms (all files loaded 50 ms later) |

On linux, gix status (~200 ms, vs ~250 ms for `git status`) is the critical path; half
of it is the untracked-file walk. Streaming status results into the tree is the next
lever if that budget tightens.

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

**Writes (`goro_core::git::Git`: `git --no-pager --literal-pathspecs`, `GIT_TERMINAL_PROMPT=0`,
always off the UI thread):**

| Action | Mechanism (`goro_core::ops`, `goro_core::patch`) |
|---|---|
| Stage lines / hunk | Exact patch for the selection → `git apply --cached`. Forward patches keep unselected removals as context and drop unselected additions; untracked files get a `new file mode` header |
| Unstage lines / hunk | Reverse patch (unselected additions become context, unselected removals are dropped) → `git apply --cached -R` |
| Discard lines / hunk | Compare-and-swap first: the worktree's clean-filtered hash must equal the reviewed diff's new side, else `Stale`. Save raw bytes (below), then reverse patch → `git apply -R` |
| Whole file | Stage `git add -A`; unstage `git restore --staged` (`git rm --cached` before the first commit); discard `git restore --worktree` or delete an untracked file, after the same save |
| A selection covering every changed line | Treated as the whole file, so new, deleted and mode-changed files behave as expected |
| Undo | Index actions record stage-0 entries before/after (`ls-files -s`) and restore with `update-index`; discards restore the saved bytes and mode. Both refuse (`Stale`) unless the current state is exactly what the action left |
| Commit | `git commit -F -` (message on stdin), `--amend`/`--signoff` as chosen, in the background; hooks and signing work as usual. Failures show the hook's full output. The undo stack is cleared after a commit (its entries refer to the old HEAD) |
| Snapshot (M3) | Temp index (`GIT_INDEX_FILE`) seeded from the real index → `git add -A` → `git write-tree` → `git commit-tree`. The user's index is never touched. |

Patches are tested by applying random line selections with real `git apply` and comparing
the result with an independent model (`tests/patch_apply.rs`), including CRLF, missing
final newlines and new files. A selection that would split an end-of-file newline change
is refused with a message rather than producing an invalid patch. The undo stack is per
session; the saved content stays recoverable from the refs below.

Any write that races with an external change fails loudly and triggers a refresh. Never
retry blindly.

## Private refs

Goro stores its state as ordinary git objects under `refs/goro/`, so it's gc-safe,
portable with the repo, and removable with `goro clean`:

```
refs/goro/turns/<session>/<ms>-<ev> turn snapshots (commit; message = JSON metadata)
refs/goro/undo                      chain of commits holding pre-discard content (raw blobs)
refs/goro/undo-previous             the previous undo generation
```

Retention: undo history keeps two generations of up to 200 discards each; when
`refs/goro/undo` fills up it becomes `undo-previous` (dropping the generation before) and
a new chain starts, so nothing is ever rewritten. Turn snapshots are pruned to the newest
400 by the hook worker. Refs under `refs/goro/` show up in `git log --all`; that trade-off is
documented, and it's how GitButler and others persist state too.

Non-git state lives in the platform data dir (`goro_core::store`, via `dirs`:
`~/Library/Application Support/Goro`, `~/.local/share/Goro`, `%LOCALAPPDATA%\Goro`).
`GORO_DATA_DIR` overrides it, and tests always set it to a temp dir. Files are JSON,
written atomically (temp file + rename); unreadable state reads as empty.

```
recent.json                  recently opened repositories (most recent first, 50 max)
repos/<fnv(root)>.json       per repository: lines seen at the last look, reviewed hunk hashes
```

Keys are FNV-1a hashes (`store::stable_hash`, pinned by a test) so they stay valid across
Rust versions.

## Watch mode

- One `notify` watcher per open repo on the worktree root plus `.git/index`, `.git/HEAD`,
  `.git/refs` (and `packed-refs`); `.git/objects` and ignored paths are filtered out.
- Events are debounced (25 ms trailing, at most 100 ms while events keep coming), then
  paths git ignores (checked with the path and every parent directory, as git does) and
  `.git` internals other than index/HEAD/refs are dropped. A new or removed directory
  makes every path dirty.
- Every reload re-runs status, then reuses already loaded files whose section, paths and
  blob ids are unchanged and whose worktree side isn't dirty (`review::reuse_loads`);
  only the rest is re-diffed.
- One reload runs at a time. Changes arriving meanwhile are merged and trigger exactly one
  follow-up, so a busy agent can't starve the view with cancelled reloads.
- The app itself is a writer (stage, discard, commit); self-inflicted events are not
  special-cased. The refresh is cheap and idempotent.
- **New since last look** is per changed line: `(kind, content)` hashed per path
  (`review::line_hash`), independent of line numbers and section, so staging or moving a
  line never makes it new. A look is recorded when the window loses focus after ≥ 1 s,
  on `m`, and once on first open (so a fresh repository starts with nothing new). This
  replaces the planned `refs/goro/seen` tree: no git writes, and it survives staging.
- **Reviewed** marks key a hunk by its path and changed lines (`review::hunk_hash`), so any
  edit to the hunk clears the mark. Reviewed hunks collapse to their header; stale marks
  are pruned on save.

Measured on golang/go (M3 Pro): write → watcher report ≈ 58 ms, status ≈ 78 ms, so an edit
appears in about 150 ms; small repositories are much faster.

## Agent integration

**Repo auto-detect (read-only, no setup; `goro_core::detect`):**
- Used when no path is given and the CLI isn't run from inside a repository (a desktop
  launch starts in `/` or `$HOME`, which says nothing about intent).
- Claude Code (`$CLAUDE_CONFIG_DIR` or `~/.claude`): the newest `projects/*/*.jsonl` by
  mtime; the last `cwd` in its final 64 KB.
- Codex (`$CODEX_HOME` or `~/.codex`): rollouts in the two newest `sessions/YYYY/MM/DD`
  folders; `cwd` from the `session_meta` line.
- The newest activity inside a repository wins (a deleted directory resolves from its
  nearest existing ancestor); then recently opened repositories. Unreadable logs are
  skipped; formats are covered by fixture tests. (The `goro hook` activity log joins in M3.)

**Single instance (`goro/src/ipc.rs`):** a per-user local socket (Linux abstract
namespace, Windows named pipe, a socket file in the per-user temp dir on macOS). A new
`goro` sends `open\t<path>` and exits (≈ 5 ms round trip); the running app focuses the
window already showing that repository or opens a new one. From a terminal, the first
instance re-launches itself detached (`GORO_NO_DETACH` marks the child) so the shell gets
its prompt back.

**Hooks (`goro_core::hooks`, `goro hooks install`):**
- Claude Code (`$CLAUDE_CONFIG_DIR/settings.json` or `~/.claude/settings.json`) and Codex
  (`$CODEX_HOME/hooks.json` or `~/.codex/hooks.json`) both get `UserPromptSubmit` →
  `goro hook <agent> prompt` and `Stop` → `goro hook <agent> stop` (timeout 5 s). Installing
  keeps other settings, hooks and key order; Goro's entries are recognized by their command
  (neither agent has hook ids), so reinstalling replaces them and uninstalling restores the
  file. The CLI shows the planned edits and asks first (`--yes` for scripts).
- Codex only runs hooks the user has trusted (`/hooks`); Goro never writes trust entries.
  Codex's single `notify` slot is left alone (it's often taken).
- `goro hook` reads the payload, hands it to a detached `goro hook-worker` (stdin pipe),
  and exits 0 without output: a prompt hook's stdout would reach the model. Measured ≈
  7–12 ms warm. Errors go to `goro.log` in the data dir.
- The worker snapshots the worktree (`goro_core::turns`: temp index seeded from the real
  one, `git add -A`, `write-tree`, `commit-tree`) to
  `refs/goro/turns/<session>/<unix ms>-<start|end>`, metadata (agent, session, turn id,
  prompt) as JSON in the message; prunes to the newest 400 snapshots; records the repo in
  `activity.json` (auto-detect prefers it); and tells a running Goro over IPC (`turn`).
- Turns pair start/end per session (by Codex's `turn_id`, else in order). A turn's view
  diffs its start tree to its end tree (or to a fresh worktree tree while it runs); a
  session's view diffs its first snapshot to the worktree now. Both use `git diff-tree`
  and show read-only `Snapshot` changes.

**Comments and `--wait`:** comments (`goro_core::comments`) anchor to a path, side and
line range, keep the diff excerpt they were written on, persist per repository, and
export as markdown. `goro --wait` sends `wait` over IPC (starting Goro if needed) and
blocks; Send review (⌘⇧↵) answers every waiter with the markdown and clears the
comments; closing the window answers `cancelled` (exit 1).

**IPC verbs:** `open`, `turn`, `wait` (see `goro/src/ipc.rs`). `GORO_SOCKET_ID` isolates
tests and development builds from an installed Goro.

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

## Settings, layout and packaging (M4)

- **Settings** (`goro_core::settings`): JSON in the platform config dir; unknown keys are
  ignored, missing ones default, invalid files keep the defaults and report. The app
  watches the file and re-applies on save: theme (explicit override > setting > OS),
  font, default layout, keybindings (defaults, then user overrides in the diff context;
  an empty action binds `NoAction`) and the global hotkey.
- **Split layout**: `Row::Pair` rows pair each run of removals with the additions that
  follow; context lines sit on both sides. Actions, comments, reviewed marks and "new"
  markers work on pairs as on lines.
- **Images**: PNG, JPEG, GIF, WebP, BMP, TIFF and ICO load both sides' bytes
  (`Loaded::Image`) and render before/after in `IMAGE_ROWS` reserved rows (uniform row
  height is kept). Decoded images are cached per view. SVG stays a text diff.
- **Global hotkey** (`global-hotkey`: macOS, Windows, X11; not Wayland): brings the last
  window forward or opens the detected repository. On macOS the app keeps running with
  no windows, as Mac apps do; the Dock icon reopens one.
- **Windows**: a GUI-subsystem executable (no console window from Explorer) that
  reattaches to the caller's console for CLI output; the icon is embedded at build time.
- **Packaging** (`scripts/package/`, `.github/workflows/release.yml`, on `v*` tags):
  macOS universal `Goro.app` (signed and notarized when the Apple secrets exist, ad-hoc
  signed otherwise) zipped for a Homebrew cask; Windows x64/arm64 zips (signed when a
  certificate secret exists) with Scoop and winget manifests; Linux x86_64/arm64 tarballs,
  `.deb`s and AppImages. `manifests.py` fills the manifests from the artifacts' SHA-256;
  everything lands in a draft GitHub release. Icons are generated by
  `scripts/package/make_icons.py` and checked in under `assets/icons/`.
- **CI** runs tests (clippy `-D warnings`) and the benchmarks on macOS, Windows and Linux
  (Xvfb + Mesa Vulkan). Shared CI VMs aren't reference hardware (a first launch there can
  take seconds), so CI gates startup at 600 ms (2× the budget) to catch regressions; the
  300 ms budget is checked on reference hardware before a release.

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
- **goro-ui:** `tests/visual.rs` renders the real window through GPUI's
  `HeadlessAppContext` (real text shaping; real Metal rendering on macOS) against a
  fixture repository, drives keyboard actions, and asserts view state. It runs without
  the libtest harness because the macOS platform must be created on the main thread.
  Set `GORO_SCREENSHOT_DIR` to save PNGs for visual review.
- **Performance:** `scripts/bench/startup.py` runs `--bench-exit-after-first-paint`
  against pinned reference repos prepared by `scripts/bench/make_changes.py` (golang/go
  in CI; linux locally); `cargo bench -p goro-ui --bench scroll` times frames while
  jumping through a 1,000-file, 50k-changed-line review (M0: p99 ≈ 1 ms CPU per
  frame). CI fails when a budget is exceeded; tracking >10% regressions against a stored
  baseline is still to do. Absolute budgets are checked on reference hardware before
  each release.
- Logs go to the data dir; tests capture and assert log output and never write outside
  their temp dir.

## Risks

| Risk | Mitigation |
|---|---|
| GPUI's macOS frame loop checks every vsync while a window is visible (≈ 1–2 % CPU idle on an M3 Pro, 0 when hidden) | Upstream behavior; revisit if it matters in practice. Not patched to avoid forking GPUI |
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
