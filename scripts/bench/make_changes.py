#!/usr/bin/env python3
"""Apply deterministic edits to a git checkout for Goro's startup benchmarks.

Usage: make_changes.py <repo> --files N --lines M

Resets the checkout, then edits N tracked text files (evenly spaced in path order),
changing M lines in each (in groups of 10), so benchmark runs are reproducible.
"""
import argparse
import subprocess
import sys
from pathlib import Path


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("repo", type=Path)
    parser.add_argument("--files", type=int, required=True)
    parser.add_argument("--lines", type=int, required=True)
    args = parser.parse_args()

    git = ["git", "-C", str(args.repo)]
    subprocess.run(git + ["reset", "-q", "--hard"], check=True)
    tracked = subprocess.run(
        git + ["ls-files", "-z"], check=True, capture_output=True
    ).stdout.split(b"\0")
    candidates = sorted(
        p.decode() for p in tracked if p.endswith((b".c", b".h", b".go", b".rs", b".py", b".ts", b".js"))
    )
    stride = max(1, len(candidates) // args.files)
    edited = 0
    # Evenly spaced first; later passes shift the offset to fill up skipped files.
    order = [rel for offset in range(stride) for rel in candidates[offset::stride]]
    for rel in order:
        if edited == args.files:
            break
        path = args.repo / rel
        try:
            lines = path.read_bytes().split(b"\n")
        except OSError:
            continue
        if len(lines) < args.lines * 4:
            continue
        changed = 0
        for i in range(len(lines)):
            if changed == args.lines:
                break
            if (i // 10) % 4 == 1:
                lines[i] += b" /* goro-bench */"
                changed += 1
        path.write_bytes(b"\n".join(lines))
        edited += 1
    print(f"edited {edited} files, {args.lines} lines each")
    return 0 if edited == args.files else 1


if __name__ == "__main__":
    sys.exit(main())
