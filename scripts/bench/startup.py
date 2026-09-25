#!/usr/bin/env python3
"""Measure Goro's time from launch to the first drawn diff.

Usage: startup.py <goro-binary> <repo> --budget-ms N [--runs 7]

Runs the binary with --bench-exit-after-first-paint, prints each run and the median,
and exits non-zero if the median exceeds the budget. The first run warms OS caches and
is excluded (the PRD budget is defined with a warm file cache).
"""
import argparse
import statistics
import subprocess
import sys


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("goro")
    parser.add_argument("repo")
    parser.add_argument("--budget-ms", type=float, required=True)
    parser.add_argument("--runs", type=int, default=7)
    args = parser.parse_args()

    times = []
    for run in range(args.runs + 1):
        out = subprocess.run(
            [args.goro, "--bench-exit-after-first-paint", args.repo],
            capture_output=True,
            text=True,
            timeout=60,
        )
        line = next((l for l in out.stdout.splitlines() if l.startswith("first_paint_ms=")), None)
        if out.returncode != 0 or line is None:
            print(f"run {run} failed (exit {out.returncode}):\n{out.stdout}{out.stderr}")
            return 1
        ms = float(line.split("=", 1)[1])
        print(f"run {run}: {ms:.1f} ms" + (" (warm-up, excluded)" if run == 0 else ""))
        if run > 0:
            times.append(ms)
    median = statistics.median(times)
    print(f"median {median:.1f} ms (budget {args.budget_ms:.0f} ms)")
    return 0 if median <= args.budget_ms else 1


if __name__ == "__main__":
    sys.exit(main())
