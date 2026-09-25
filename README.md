# Goro

A native, instant-open review app for agent-written code changes. See [PRD.md](PRD.md)
and [ARCHITECTURE.md](ARCHITECTURE.md).

Status: M0. `goro [path]` opens a file tree and a syntax-highlighted diff of the
working tree (staged, unstaged, untracked). Read-only for now.

## Develop

```sh
cargo run --release -p goro -- [path]   # open a repository (defaults to the current directory)
cargo test --workspace                  # core tests against real temp repos, plus headless UI tests
```

Keys: `j`/`k` (or arrows) move the cursor, `n`/`p` next/previous hunk, `]`/`[`
next/previous file, `cmd-q`/`ctrl-q` quit.

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
