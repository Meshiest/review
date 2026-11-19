"""Runs a group of checks and exits 0 only if all pass.

    python scripts/checks.py bench       # timing benchmarks, release builds
    python scripts/checks.py live        # checks against the forge sandboxes
    python scripts/checks.py bench --only diff_scroll_unified

Benchmarks print `PERF {json}` lines (see crates/perf) that must meet their
budget. They take the lock in .scratch/bench.lock so two never run at once.
Each check's output goes to .scratch/check-logs.

The live checks need the REVIEW_SANDBOX_* variables of docs/sandboxes.md; they
are read from the untracked .scratch/sandbox.env (KEY=VALUE lines) when not
already in the environment.
"""

from __future__ import annotations

import argparse
import json
import os
import shutil
import subprocess
import sys
import time
from dataclasses import dataclass
from pathlib import Path
from typing import Callable

ROOT = Path(__file__).resolve().parent.parent
LOCK = ROOT / ".scratch" / "bench.lock"
LOGS = ROOT / ".scratch" / "check-logs"


def load_sandbox_env() -> None:
    path = ROOT / ".scratch" / "sandbox.env"
    if not path.is_file():
        return
    for line in path.read_text(encoding="utf-8").splitlines():
        line = line.strip().removeprefix("export ")
        if not line or line.startswith("#") or "=" not in line:
            continue
        key, value = line.split("=", 1)
        os.environ.setdefault(key.strip(), value.strip().strip("\"'"))


load_sandbox_env()


@dataclass
class Result:
    name: str
    passed: bool
    detail: str


def run(cmd: list[str], log_name: str, env: dict[str, str] | None = None) -> tuple[int, str]:
    LOGS.mkdir(parents=True, exist_ok=True)
    proc = subprocess.run(cmd, cwd=ROOT, capture_output=True, text=True, encoding="utf-8", errors="replace",
                          env={**os.environ, **(env or {})})
    out = proc.stdout + proc.stderr
    (LOGS / f"{log_name}.log").write_text(out, encoding="utf-8")
    return proc.returncode, out


class BenchLock:
    def __enter__(self) -> None:
        LOCK.parent.mkdir(parents=True, exist_ok=True)
        while True:
            try:
                LOCK.mkdir()
                break
            except FileExistsError:
                time.sleep(3)
        (LOCK / "owner").write_text(f"checks.py {time.strftime('%Y-%m-%dT%H:%M:%S')}\n")

    def __exit__(self, *_: object) -> None:
        shutil.rmtree(LOCK, ignore_errors=True)


def perf_reports(output: str) -> dict[str, dict]:
    reports = {}
    for line in output.splitlines():
        if line.startswith("PERF "):
            report = json.loads(line[5:])
            reports[report["metric"]] = report
    return reports


def bench(name: str, cmd: list[str], metrics: list[str]) -> Callable[[], list[Result]]:
    """A benchmark command whose PERF lines must contain each metric, passing."""

    def check() -> list[Result]:
        # Build first, outside the lock, so the timed run is only the run.
        cargo_args = cmd[:cmd.index("--")] if "--" in cmd else cmd
        if cargo_args[1] == "test":
            build = [*cargo_args, "--no-run"]
        else:
            build = ["build" if c == "run" else c for c in cargo_args]
        code, _ = run(build, f"{name}-build")
        if code != 0:
            return [Result(m, False, f"build failed, see .scratch/check-logs/{name}-build.log") for m in metrics]
        with BenchLock():
            code, out = run(cmd, name)
        reports = perf_reports(out)
        results = []
        for metric in metrics:
            r = reports.get(metric)
            if r is None:
                results.append(Result(metric, False, f"no PERF line (exit {code}), see .scratch/check-logs/{name}.log"))
                continue
            extra = "".join(f", {k} {v}" for k, v in r.items() if k not in REPORT_KEYS)
            results.append(Result(metric, bool(r["pass"]),
                                  f"p99 {r['p99_ms']:.2f} ms, p95 {r['p95_ms']:.2f}, p50 {r['p50_ms']:.2f}, "
                                  f"max {r['max_ms']:.2f} (budget {r['budget_ms']:g} at p{r['percentile']:g}), "
                                  f"n {r['count']}{extra}"))
        return results

    return check


REPORT_KEYS = {"metric", "count", "p50_ms", "p95_ms", "p99_ms", "max_ms", "percentile", "budget_ms", "pass"}


def command(name: str, cmd: list[str], env: dict[str, str] | None = None) -> Callable[[], list[Result]]:
    """A check that passes when the command exits 0."""

    def check() -> list[Result]:
        code, _ = run(cmd, name, env)
        return [Result(name, code == 0, "ok" if code == 0 else f"exit {code}, see .scratch/check-logs/{name}.log")]

    return check


FORGES = ("github", "gitlab", "forgejo")


def scroll_bench(*args: str) -> list[str]:
    return ["cargo", "run", "-p", "diff_view", "--release", "--example", "scroll_bench", "--", *args]


def app_bench(example: str, *args: str) -> list[str]:
    return ["cargo", "run", "-p", "app", "--release", "--example", example, "--", *args]


GROUPS: dict[str, list[tuple[str, Callable[[], list[Result]]]]] = {
    "bench": [
        ("diff_scroll_unified", bench("diff-scroll-unified", scroll_bench("--rows", "100000"),
            ["diff_scroll_unified", "diff_autoscroll"])),
        ("diff_scroll_split", bench("diff-scroll-split", scroll_bench("--rows", "100000", "--split"),
            ["diff_scroll_split"])),
        *[(f"scroll@{theme}@{font}", bench(f"scroll-{theme}-{font.replace(' ', '-')}",
            scroll_bench("--split", "--theme", f"crates/theme/themes/{theme}.toml", "--font", font,
                         "--autoscroll-frames", "0"),
            [f"diff_scroll_split@{theme}@{font}"]))
          for theme in ("vercel-dark", "vercel-light")
          for font in ("Geist Mono", "Cascadia Mono", "Consolas")],
        ("style_switch", bench("style-switch",
            scroll_bench("--split", "--theme", "crates/theme/themes/vercel-dark.toml",
                         "--switch-style-every", "60", "--autoscroll-frames", "0"),
            ["diff_style_switch@vercel-dark@Geist Mono"])),
        ("dock_scroll", bench("dock-scroll", app_bench("dock_bench"), ["dock_scroll", "dock_resize"])),
        ("open_bench", bench("open-bench", app_bench("open_bench", "--unthrottled"),
            ["open_cached", "scroll_50k_unified", "scroll_50k_split"])),
        ("launch_bench", bench("launch-bench", app_bench("launch_bench"), ["launch_to_inbox"])),
        ("theme_bench", bench("theme-bench", app_bench("theme_bench"), ["theme_switch", "buffer_font_switch"])),
        ("composer_bench", bench("composer-bench", app_bench("composer_bench", "--unthrottled"),
            ["composer_keypress"])),
        ("term_bench", bench("term-bench", app_bench("term_bench", "--unthrottled"),
            ["term_keypress", "term_stream"])),
        ("inbox_bench", bench("inbox-bench", app_bench("inbox_bench", "--unthrottled"), ["inbox_scroll"])),
        ("log_bench", bench("log-bench",
            ["cargo", "run", "-p", "log_view", "--release", "--example", "log_bench", "--", "--lines", "200000"],
            ["log_scroll", "log_tail"])),
        ("file_list", bench("file-list",
            ["cargo", "test", "-p", "repo", "--release", "--test", "git_oracle", "bench", "--", "--ignored", "--nocapture"],
            ["repo.file_list_2000.first_result"])),
        ("store_latency", bench("store-latency",
            ["cargo", "test", "-p", "store", "--release", "--test", "bench", "--", "--ignored", "--nocapture"],
            ["store_inbox_read_100_of_2000", "store_inbox_read_2000", "store_snapshot_write",
             "store_snapshot_read"])),
    ],
    "live": [
        ("live_smoke", command("live_smoke", ["cargo", "run", "-p", "forge", "--example", "live_smoke"])),
        *[(f"inbox_{forge}", command(f"inbox_{forge}",
            ["cargo", "run", "-p", "forge", "--example", f"inbox_check_{forge}"]))
          for forge in FORGES],
        *[(f"round_trip_{forge}", command(f"round_trip_{forge}",
            ["cargo", "run", "-p", "forge", "--example", "round_trip", "--", "--forge", forge]))
          for forge in FORGES],
        *[(f"ci_live_{forge}", command(f"ci_live_{forge}",
            ["cargo", "run", "-p", "forge", "--example", "ci_live", "--", "--forge", forge]))
          for forge in FORGES],
        ("notify_bench", bench("notify-bench", app_bench("notify_bench"), ["change_to_toast"])),
        ("worktree_check", command("worktree_check", ["cargo", "run", "-p", "app", "--example", "worktree_check"])),
    ],
}


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("group", choices=sorted(GROUPS))
    parser.add_argument("--only", action="append", help="run only these checks (by name)")
    args = parser.parse_args()

    results: list[Result] = []
    for name, check in GROUPS[args.group]:
        if args.only and name not in args.only:
            continue
        print(f"... {name}", flush=True)
        results.extend(check())

    width = max(len(r.name) for r in results)
    for r in results:
        print(f"{'PASS' if r.passed else 'FAIL'}  {r.name:<{width}}  {r.detail}")
    failed = sum(not r.passed for r in results)
    print(f"\n{args.group}: {len(results) - failed} of {len(results)} passed")
    return 1 if failed else 0


if __name__ == "__main__":
    sys.exit(main())
