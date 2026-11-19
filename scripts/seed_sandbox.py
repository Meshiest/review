#!/usr/bin/env python3
"""Seed the private `review-sandbox` repositories used by the live tests.

Usage: python scripts/seed_sandbox.py [--forge github|gitlab|forgejo|all] [--ci]

The repositories come from REVIEW_SANDBOX_GITHUB, REVIEW_SANDBOX_GITLAB and
REVIEW_SANDBOX_FORGEJO (and optionally REVIEW_SANDBOX_FORGEJO_REMOTE); see
docs/sandboxes.md.

With --ci the script only adds the CI workflow to `main` of each chosen forge
(see "CI workflow" below); without it, the branches, pull requests and threads.

Idempotent: builds one deterministic source tree in .scratch/sandbox-src (fixed
identity and dates, so commit ids never change), then on each forge pushes only
the branches that are missing and creates only the pull requests, threads and
state changes that are missing. A second run prints "0 changes" per forge.

Tokens: `gh` and `glab` handle their own auth. The Forgejo token is read from
`fj`'s key file and sent only in a request header; it is never printed or saved.
Needs `git`, `gh`, `glab` and `fj` on PATH, already logged in.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import re
import shutil
import stat
import struct
import subprocess
import sys
import time
import urllib.error
import urllib.parse
import urllib.request
import zlib
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
SRC = ROOT / ".scratch" / "sandbox-src"

# The sandboxes come from the environment (see docs/sandboxes.md), filled in
# by `load_targets` for the forges being seeded.
FORGEJO_HOST = ""
GITHUB_REPO = ""
GITLAB_REPO = ""
FORGEJO_REPO = ""
REMOTES: dict[str, str] = {}

SANDBOX = "review-sandbox"


def _env_repo(var, parts, usage):
    value = os.environ.get(var, "").strip()
    if not value:
        sys.exit(f"{var} is not set ({usage}); see docs/sandboxes.md")
    segments = value.split("/")
    if len(segments) != parts or not all(segments) or segments[-1] != SANDBOX:
        sys.exit(f"{var}={value!r}: expected {usage}; only repositories named {SANDBOX} are used")
    return segments


def load_targets(names):
    global FORGEJO_HOST, GITHUB_REPO, GITLAB_REPO, FORGEJO_REPO
    if "github" in names:
        GITHUB_REPO = "/".join(_env_repo("REVIEW_SANDBOX_GITHUB", 2, "owner/review-sandbox"))
        REMOTES["github"] = f"git@github.com:{GITHUB_REPO}.git"
    if "gitlab" in names:
        GITLAB_REPO = "/".join(_env_repo("REVIEW_SANDBOX_GITLAB", 2, "owner/review-sandbox"))
        REMOTES["gitlab"] = f"git@gitlab.com:{GITLAB_REPO}.git"
    if "forgejo" in names:
        host, owner, name = _env_repo("REVIEW_SANDBOX_FORGEJO", 3, "host/owner/review-sandbox")
        FORGEJO_HOST = host
        FORGEJO_REPO = f"{owner}/{name}"
        REMOTES["forgejo"] = os.environ.get("REVIEW_SANDBOX_FORGEJO_REMOTE") or f"git@{host}:{FORGEJO_REPO}.git"


IDENTITY = ("Review Sandbox", "review-sandbox@example.invalid")
BASE_TIME = 1_794_000_000  # fixed, so commit ids are stable across runs


class Skip(Exception):
    """A capability the forge lacks; logged, never fatal."""


# ---------------------------------------------------------------- processes


def run(cmd, *, cwd=None, input=None, env=None, check=True):
    proc = subprocess.run(
        cmd,
        cwd=cwd,
        input=input,
        env=env,
        capture_output=True,
        text=True,
        encoding="utf-8",
    )
    if check and proc.returncode != 0:
        raise RuntimeError(
            f"command failed ({proc.returncode}): {' '.join(map(str, cmd[:6]))}\n"
            f"{proc.stdout[-800:]}\n{proc.stderr[-800:]}"
        )
    return proc


def git(*args, check=True, env=None):
    return run(["git", *args], cwd=SRC, check=check, env=env).stdout


# ---------------------------------------------------------------- source tree

_tick = [0]


def commit(message):
    _tick[0] += 1
    stamp = f"{BASE_TIME + _tick[0] * 60} +0000"
    env = {
        **os.environ,
        "GIT_AUTHOR_NAME": IDENTITY[0],
        "GIT_AUTHOR_EMAIL": IDENTITY[1],
        "GIT_COMMITTER_NAME": IDENTITY[0],
        "GIT_COMMITTER_EMAIL": IDENTITY[1],
        "GIT_AUTHOR_DATE": stamp,
        "GIT_COMMITTER_DATE": stamp,
    }
    git("add", "-A")
    git("commit", "-q", "-m", message, env=env)
    return git("rev-parse", "HEAD").strip()


def write(path, data):
    target = SRC / path
    target.parent.mkdir(parents=True, exist_ok=True)
    target.write_bytes(data if isinstance(data, bytes) else data.encode("utf-8"))


def read(path):
    return (SRC / path).read_bytes().decode("utf-8")


def edit(path, fn):
    write(path, fn(read(path)))


def png(color):
    def chunk(kind, data):
        body = kind + data
        return struct.pack(">I", len(data)) + body + struct.pack(">I", zlib.crc32(body))

    row = b"\x00" + bytes(color) * 8
    raw = row * 8
    return (
        b"\x89PNG\r\n\x1a\n"
        + chunk(b"IHDR", struct.pack(">IIBBBBB", 8, 8, 8, 6, 0, 0, 0))
        + chunk(b"IDAT", zlib.compress(raw, 9))
        + chunk(b"IEND", b"")
    )


def module_text(n):
    return (
        f"//! Module {n:02}.\n\n"
        f"pub fn id() -> u32 {{\n    {n}\n}}\n\n"
        f"pub fn describe() -> &'static str {{\n    \"module {n:02}\"\n}}\n"
    )


THREADS_TARGET = """\
//! Sample module used by review threads.

pub fn add(a: i32, b: i32) -> i32 {
    a + b
}

pub fn sub(a: i32, b: i32) -> i32 {
    a - b
}

pub fn mul(a: i32, b: i32) -> i32 {
    a * b
}

pub fn div(a: i32, b: i32) -> Option<i32> {
    if b == 0 {
        None
    } else {
        Some(a / b)
    }
}

pub fn clamp(x: i32, lo: i32, hi: i32) -> i32 {
    if x < lo { lo } else if x > hi { hi } else { x }
}

pub fn is_even(n: i32) -> bool {
    n % 2 == 0
}
"""


def build_main():
    write("README.md", "# review-sandbox\n\nFixtures for the review app's live tests.\n")
    write(
        "Cargo.toml",
        '[package]\nname = "review-sandbox"\nversion = "0.1.0"\nedition = "2021"\n',
    )
    write(".gitignore", "target/\n.scratch/\n")
    for name in ("intro", "guide", "faq", "changelog", "obsolete"):
        write(
            f"docs/{name}.md",
            f"# {name.title()}\n\nThis page covers {name}.\n\n- first point\n- second point\n- third point\n",
        )
    write(
        "docs/old_name.md",
        "# Old name\n\n" + "".join(f"Line {i} of the page that will be renamed.\n" for i in range(1, 13)),
    )
    write("src/lib.rs", "pub mod util;\npub mod modules;\npub mod threads_target;\n\npub fn version() -> &'static str {\n    \"0.1.0\"\n}\n")
    write("src/main.rs", "fn main() {\n    println!(\"{}\", review_sandbox::version());\n}\n")
    write("src/threads_target.rs", THREADS_TARGET)
    write("src/util/mod.rs", "pub mod fmt;\npub mod math;\npub mod strings;\n")
    write("src/util/math.rs", "pub fn double(x: i32) -> i32 {\n    x * 2\n}\n")
    write("src/util/strings.rs", "pub fn shout(s: &str) -> String {\n    s.to_uppercase()\n}\n")
    write("src/util/fmt.rs", "pub fn bracket(s: &str) -> String {\n    format!(\"[{s}]\")\n}\n")
    mods = "".join(f"pub mod m{n:02};\n" for n in range(1, 13))
    write("src/modules/mod.rs", mods)
    for n in range(1, 13):
        write(f"src/modules/m{n:02}.rs", module_text(n))
    write("src/deep/nested/dir/leaf.rs", "pub fn leaf() -> &'static str {\n    \"leaf\"\n}\n")
    write("assets/logo.png", png((30, 120, 220, 255)))
    write("data/crlf.txt", "first line\r\nsecond line\r\nthird line\r\nfourth line\r\n")
    write("data/no_newline.txt", "alpha\nbeta\ngamma without a trailing newline")
    write("scripts/run.sh", "#!/bin/sh\necho running\n")
    return commit("Initial commit")


def build_source():
    """Create the local repo; returns {branch: sha} for every branch."""
    if SRC.exists():
        def unlock(func, path, _info):
            os.chmod(path, stat.S_IWRITE)
            func(path)

        shutil.rmtree(SRC, onerror=unlock)
    SRC.mkdir(parents=True)
    git("init", "-q", "-b", "main")
    for key, value in (
        ("core.autocrlf", "false"),
        ("core.safecrlf", "false"),
        ("core.filemode", "false"),
        ("commit.gpgsign", "false"),
        ("tag.gpgsign", "false"),
        ("user.name", IDENTITY[0]),
        ("user.email", IDENTITY[1]),
    ):
        git("config", key, value)
    for name, url in REMOTES.items():
        git("remote", "add", name, url)

    shas = {"main": build_main()}

    def branch(name, base):
        git("checkout", "-q", "-b", name, base)

    # Stack: each branch builds on the previous one.
    branch("stack-1", "main")
    edit("src/lib.rs", lambda t: t.replace('"0.1.0"', '"0.2.0"'))
    edit("src/util/math.rs", lambda t: t + "\npub fn triple(x: i32) -> i32 {\n    x * 3\n}\n")
    shas["stack-1"] = commit("Bump version and add triple")
    branch("stack-2", "stack-1")
    edit("src/util/strings.rs", lambda t: t + "\npub fn whisper(s: &str) -> String {\n    s.to_lowercase()\n}\n")
    edit("docs/guide.md", lambda t: t.replace("- second point", "- second point, expanded"))
    shas["stack-2"] = commit("Add whisper and expand the guide")
    branch("stack-3", "stack-2")
    edit("src/main.rs", lambda t: t.replace("println!", "eprintln!"))
    edit("README.md", lambda t: t + "\nSee `docs/guide.md`.\n")
    shas["stack-3"] = commit("Log to stderr and link the guide")

    # Threads: two commits; the second changes a line the first one drew a comment on.
    branch("threads", "main")
    def threads_edit(t):
        t = t.replace("    a - b\n", "    a.saturating_sub(b)\n")
        t = t.replace(
            "    if b == 0 {\n        None\n    } else {\n        Some(a / b)\n    }\n",
            "    a.checked_div(b)\n",
        )
        t = t.replace("    n % 2 == 0\n", "    n & 1 == 0\n")
        return t + (
            "\npub fn pow(base: i32, exp: u32) -> i32 {\n"
            "    let mut acc = 1;\n"
            "    for _ in 0..exp {\n"
            "        acc *= base;\n"
            "    }\n"
            "    acc\n"
            "}\n"
        )

    edit("src/threads_target.rs", threads_edit)
    shas["threads-initial"] = commit("Simplify arithmetic helpers and add pow")
    edit("src/threads_target.rs", lambda t: t.replace("    n & 1 == 0\n", "    n % 2 == 0\n"))
    shas["threads"] = commit("Address review feedback on is_even")

    # Big: ~25 files with a rename, a deletion and a mode change.
    branch("big", "main")
    for n in range(1, 13):
        edit(f"src/modules/m{n:02}.rs", lambda t, n=n: t.replace(f'"module {n:02}"', f'"module {n:02} v2"') + f"\npub fn square() -> u32 {{\n    {n} * {n}\n}}\n")
    edit("src/util/mod.rs", lambda t: t + "pub mod extra;\n")
    write("src/util/extra.rs", "pub fn noop() {}\n")
    edit("src/util/fmt.rs", lambda t: t.replace("[{s}]", "<{s}>"))
    for name in ("intro", "faq", "changelog"):
        edit(f"docs/{name}.md", lambda t: t.replace("- third point", "- third point, revised"))
    edit("Cargo.toml", lambda t: t.replace("0.1.0", "0.1.1"))
    edit("src/deep/nested/dir/leaf.rs", lambda t: t.replace('"leaf"', '"leaf, edited"'))
    write("assets/logo.png", png((220, 80, 40, 255)))
    edit("data/crlf.txt", lambda t: t.replace("second line", "second line, edited"))
    edit("data/no_newline.txt", lambda t: t.replace("gamma without", "delta without"))
    edit(".gitignore", lambda t: t + "*.log\n")
    git("mv", "docs/old_name.md", "docs/new_name.md")
    edit("docs/new_name.md", lambda t: t.replace("# Old name", "# New name"))
    git("rm", "-q", "docs/obsolete.md")
    git("update-index", "--chmod=+x", "scripts/run.sh")
    git("add", "-A")
    git("update-index", "--chmod=+x", "scripts/run.sh")
    shas["big"] = commit("Rework modules, rename a page, drop an obsolete one")

    branch("feature-merged", "main")
    write("docs/merged-note.md", "# Merged note\n\nThis pull request was merged.\n")
    shas["feature-merged"] = commit("Add a note that gets merged")
    branch("feature-closed", "main")
    write("docs/closed-note.md", "# Closed note\n\nThis pull request was closed without merging.\n")
    shas["feature-closed"] = commit("Add a note that gets closed")
    branch("draft-wip", "main")
    write("src/draft.rs", "pub fn wip() -> bool {\n    true\n}\n")
    shas["draft-wip"] = commit("Start a draft feature")
    git("checkout", "-q", "main")
    return shas


# ---------------------------------------------------------------- pull requests

# Created in this order, so a fresh forge numbers them 1..8.
PRS = [
    dict(head="stack-1", base="main", title="Stack 1/3: bump version, add triple", state="open", draft=False),
    dict(head="stack-2", base="stack-1", title="Stack 2/3: add whisper, expand guide", state="open", draft=False),
    dict(head="stack-3", base="stack-2", title="Stack 3/3: log to stderr, link guide", state="open", draft=False),
    dict(head="threads", base="main", title="Threads: review comments on several lines", state="open", draft=False),
    dict(head="big", base="main", title="Big: modules, rename, deletion, mode change", state="open", draft=False),
    dict(head="feature-merged", base="main", title="Merged: add a note", state="merged", draft=False),
    dict(head="feature-closed", base="main", title="Closed: add a note, never merged", state="closed", draft=False),
    dict(head="draft-wip", base="main", title="Start a feature", state="open", draft=True),
]

BODIES = {
    "stack-1": "First pull request of a 3-PR stack. Based on `main`.",
    "stack-2": "Second pull request of a 3-PR stack. Based on `stack-1`.",
    "stack-3": "Third pull request of a 3-PR stack. Based on `stack-2`.",
    "threads": "Carries review threads: resolved, unresolved with a reply, multi-line, removed-line and outdated.",
    "big": "About 25 files: a rename, a deletion, a mode change, a binary change, CRLF and no-trailing-newline edits.",
    "feature-merged": "Merged on purpose.",
    "feature-closed": "Closed without merging on purpose.",
    "draft-wip": "Draft on purpose.",
}


def thread_specs(shas):
    """Thread comments anchored by text, with line numbers read from the files."""
    old = git("show", f"{shas['main']}:src/threads_target.rs").splitlines()
    new = git("show", f"{shas['threads-initial']}:src/threads_target.rs").splitlines()

    def line(lines, needle, after=0):
        for i, text in enumerate(lines[after:], start=after):
            if needle in text:
                return i + 1
        raise KeyError(needle)

    pow_start = line(new, "for _ in 0..exp")
    return [
        dict(key="resolved", new=line(new, "saturating_sub"),
             body="Should this saturate or wrap on overflow? Saturating is fine by me.", resolve=True),
        dict(key="unresolved", new=line(new, "checked_div"),
             body="Nice simplification. Can you add a test for `b == 0`?",
             reply="Will do, tracking it in a follow-up."),
        dict(key="multiline", new=pow_start + 2, start=pow_start,
             body="This loop can overflow for large exponents. Consider `checked_mul`."),
        dict(key="removed", old=line(old, "a - b"),
             body="Why drop the plain subtraction?"),
        dict(key="outdated", new=line(new, "n & 1 == 0"),
             body="Prefer `% 2` here, it reads better."),
    ]


# ---------------------------------------------------------------- CI workflow

# Three jobs, the same on every forge. `build` passes, `test` fails while the
# file ci/fail exists in the checkout, `slow` prints 2,000 numbered lines over
# about 60 s (a one-second pause every 33 lines), some with ANSI colors.
# POSIX sh only, so it runs on any runner image.
CI_BUILD_STEPS = [
    ("Configure", 'echo "configuring review-sandbox"'),
    ("Compile", 'echo "compiling 12 modules"'),
    ("Package", 'echo "packaging build output"'),
]

CI_TEST_SCRIPT = """\
if [ -e ci/fail ]; then
  echo "ci/fail exists: failing on purpose"
  exit 1
fi
echo "ci/fail is absent: tests pass"
"""

CI_SLOW_SCRIPT = """\
i=1
while [ "$i" -le 2000 ]; do
  case $((i % 10)) in
    0) printf '\\033[31mline %04d red: simulated error output\\033[0m\\n' "$i" ;;
    5) printf '\\033[32mline %04d green: step ok\\033[0m\\n' "$i" ;;
    7) printf '\\033[1mline %04d bold: section marker\\033[0m\\n' "$i" ;;
    *) printf 'line %04d plain output\\n' "$i" ;;
  esac
  if [ $((i % 33)) -eq 0 ]; then sleep 1; fi
  i=$((i + 1))
done
echo "done: 2000 lines"
"""


def indent(text, spaces):
    pad = " " * spaces
    return "".join(pad + row if row.strip() else row for row in text.splitlines(keepends=True))


def actions_workflow(runs_on):
    """GitHub Actions and Forgejo Actions share one syntax."""
    steps = "".join(
        f"      - name: {name}\n        run: {cmd}\n" for name, cmd in CI_BUILD_STEPS
    )
    return (
        "name: ci\n\n"
        # Push only on main: a PR's own commits run once, through pull_request.
        "on:\n  push:\n    branches: [main]\n  pull_request:\n\n"
        "jobs:\n"
        f"  build:\n    runs-on: {runs_on}\n    steps:\n{steps}\n"
        f"  test:\n    runs-on: {runs_on}\n    steps:\n"
        "      - name: Check out\n        uses: actions/checkout@v4\n"
        "      - name: Run tests\n        run: |\n" + indent(CI_TEST_SCRIPT, 10) + "\n"
        f"  slow:\n    runs-on: {runs_on}\n    steps:\n"
        "      - name: Print 2000 lines\n        run: |\n" + indent(CI_SLOW_SCRIPT, 10)
    )


def gitlab_workflow():
    build = "".join(f"    - {cmd}\n" for _name, cmd in CI_BUILD_STEPS)
    return (
        "stages:\n  - build\n  - test\n  - slow\n\n"
        "default:\n  image: alpine:3.20\n\n"
        "workflow:\n  rules:\n"
        "    - if: $CI_PIPELINE_SOURCE == \"merge_request_event\"\n"
        "    - if: $CI_COMMIT_BRANCH && $CI_OPEN_MERGE_REQUESTS\n      when: never\n"
        "    - if: $CI_COMMIT_BRANCH\n\n"
        # needs: [] starts every job at once, so a failing test never hides slow.
        f"build:\n  stage: build\n  needs: []\n  script:\n{build}\n"
        "test:\n  stage: test\n  needs: []\n  script:\n    - |\n" + indent(CI_TEST_SCRIPT, 6) + "\n"
        "slow:\n  stage: slow\n  needs: []\n  script:\n    - |\n" + indent(CI_SLOW_SCRIPT, 6)
    )


CI_FILES = {
    "github": (".github/workflows/ci.yml", lambda: actions_workflow("ubuntu-latest")),
    "gitlab": (".gitlab-ci.yml", gitlab_workflow),
    "forgejo": (".forgejo/workflows/ci.yml", lambda: actions_workflow("docker")),
}


# ---------------------------------------------------------------- forge base


class Forge:
    name = ""
    remote = ""
    mark = "#"

    def __init__(self, shas):
        self.shas = shas
        self.changes = 0
        self.skips = []

    def log(self, mark, text):
        print(f"[{self.name}] {mark} {text}", flush=True)

    def changed(self, text):
        self.changes += 1
        self.log("+", text)

    def skip(self, text):
        self.skips.append(text)
        self.log("SKIP", text)

    def remote_heads(self):
        out = git("ls-remote", "--heads", self.remote)
        heads = {}
        for row in out.splitlines():
            sha, ref = row.split("\t")
            heads[ref.removeprefix("refs/heads/")] = sha
        return heads

    def push(self, local_ref, branch):
        git("push", "-q", self.remote, f"{self.shas[local_ref]}:refs/heads/{branch}")
        self.changed(f"pushed {branch}")

    def seed(self):
        heads = self.remote_heads()
        if "main" not in heads:
            self.push("main", "main")
            heads["main"] = self.shas["main"]
        existing = self.list_prs()
        for spec in PRS:
            head = spec["head"]
            if head in existing:
                self.log("=", f"{self.mark}{existing[head]['number']} {head} exists ({existing[head]['state']})")
            else:
                if head not in heads:
                    ref = "threads-initial" if head == "threads" else head
                    self.push(ref, head)
                    heads[head] = self.shas[ref]
                self.create_pr(spec)
                existing = self.list_prs()
                self.changed(f"created {self.mark}{existing[head]['number']} {head}")
            info = existing[head]
            if spec["state"] == "merged" and info["state"] == "open":
                self.merge(info["number"])
                self.changed(f"merged {self.mark}{info['number']}")
            elif spec["state"] == "closed" and info["state"] == "open":
                self.close(info["number"])
                self.changed(f"closed {self.mark}{info['number']}")
            if head == "threads":
                self.ensure_threads(info["number"], thread_specs(self.shas))
                if self.remote_heads().get("threads") != self.shas["threads"]:
                    self.push("threads", "threads")
        print(f"[{self.name}] {self.changes} changes", flush=True)
        return self.changes

    def seed_ci(self):
        """Commit the workflow file on top of the remote `main`, if it differs.

        Fast-forward only: the commit's parent is the remote head (which already
        holds the merge commit of a merged pull request), the identity and dates
        are fixed, and nothing is rewritten. No push when the file is current.
        """
        path, render = CI_FILES[self.name]
        content = render()
        main = self.remote_heads().get("main")
        if main is None:
            raise RuntimeError(f"{self.name}: no main branch yet; run without --ci first")
        git("fetch", "-q", self.remote, "main")
        current = run(["git", "show", f"FETCH_HEAD:{path}"], cwd=SRC, check=False)
        if current.returncode == 0 and current.stdout.replace("\r\n", "\n") == content:
            self.log("=", f"{path} is current on main ({main[:10]})")
        else:
            git("checkout", "-q", "-B", "ci-work", "FETCH_HEAD")
            write(path, content)
            sha = commit(f"Add the {self.name} CI workflow")
            git("push", "-q", self.remote, f"{sha}:refs/heads/main")
            git("checkout", "-q", "main")
            self.changed(f"pushed {path} to main ({sha[:10]})")
        print(f"[{self.name}] {self.changes} changes", flush=True)
        return self.changes


# ---------------------------------------------------------------- GitHub


class GitHub(Forge):
    name = "github"
    remote = "github"

    def api(self, method, path, payload=None, paginate=False):
        cmd = ["gh", "api", "-X", method, path]
        if paginate:
            cmd.append("--paginate")
        if payload is not None:
            cmd += ["--input", "-"]
        out = run(cmd, input=json.dumps(payload) if payload is not None else None).stdout
        if not out.strip():
            return None
        if paginate:
            # Concatenated JSON arrays, one per page.
            dec, idx, items = json.JSONDecoder(), 0, []
            while idx < len(out):
                while idx < len(out) and out[idx].isspace():
                    idx += 1
                if idx >= len(out):
                    break
                value, idx = dec.raw_decode(out, idx)
                items.extend(value)
            return items
        return json.loads(out)

    def graphql(self, query, **variables):
        cmd = ["gh", "api", "graphql", "-f", f"query={query}"]
        for key, value in variables.items():
            cmd += ["-F" if isinstance(value, int) else "-f", f"{key}={value}"]
        return json.loads(run(cmd).stdout)

    def list_prs(self):
        out = run(["gh", "pr", "list", "-R", GITHUB_REPO, "--state", "all", "--limit", "100",
                   "--json", "number,headRefName,state,isDraft"]).stdout
        return {
            p["headRefName"]: dict(number=p["number"], state=p["state"].lower(), draft=p["isDraft"])
            for p in json.loads(out)
        }

    def create_pr(self, spec):
        cmd = ["gh", "pr", "create", "-R", GITHUB_REPO, "--base", spec["base"], "--head", spec["head"],
               "--title", spec["title"], "--body", BODIES[spec["head"]]]
        if spec["draft"]:
            cmd.append("--draft")
        run(cmd)

    def merge(self, number):
        run(["gh", "pr", "merge", str(number), "-R", GITHUB_REPO, "--merge"])

    def close(self, number):
        run(["gh", "pr", "close", str(number), "-R", GITHUB_REPO])

    def ensure_threads(self, number, specs):
        base = f"repos/{GITHUB_REPO}/pulls/{number}/comments"
        comments = self.api("GET", base, paginate=True) or []
        by_body = {c["body"]: c for c in comments}
        sha = self.shas["threads-initial"]
        path = "src/threads_target.rs"
        for spec in specs:
            root = by_body.get(spec["body"])
            if root is None:
                payload = dict(body=spec["body"], commit_id=sha, path=path)
                if "old" in spec:
                    payload.update(line=spec["old"], side="LEFT")
                else:
                    payload.update(line=spec["new"], side="RIGHT")
                    if "start" in spec:
                        payload.update(start_line=spec["start"], start_side="RIGHT")
                root = self.api("POST", base, payload)
                by_body[spec["body"]] = root
                self.changed(f"comment {spec['key']} on #{number}")
            if "reply" in spec and spec["reply"] not in by_body:
                reply = self.api("POST", f"{base}/{root['id']}/replies", dict(body=spec["reply"]))
                by_body[spec["reply"]] = reply
                self.changed(f"reply to {spec['key']} on #{number}")
        owner, repo = GITHUB_REPO.split("/")
        data = self.graphql(
            "query($o:String!,$r:String!,$n:Int!){repository(owner:$o,name:$r){pullRequest(number:$n){"
            "reviewThreads(first:50){nodes{id isResolved isOutdated comments(first:1){nodes{body}}}}}}}",
            o=owner, r=repo, n=number)
        threads = data["data"]["repository"]["pullRequest"]["reviewThreads"]["nodes"]
        by_first = {t["comments"]["nodes"][0]["body"]: t for t in threads}
        for spec in specs:
            if spec.get("resolve") and not by_first[spec["body"]]["isResolved"]:
                self.graphql("mutation($id:ID!){resolveReviewThread(input:{threadId:$id}){thread{isResolved}}}",
                             id=by_first[spec["body"]]["id"])
                self.changed(f"resolved thread {spec['key']} on #{number}")


# ---------------------------------------------------------------- GitLab


class GitLab(Forge):
    name = "gitlab"
    remote = "gitlab"
    mark = "!"
    project = "projects/" + urllib.parse.quote(GITLAB_REPO, safe="")

    def api(self, method, path, payload=None):
        cmd = ["glab", "api", "-X", method, path]
        if payload is not None:
            cmd += ["--input", "-", "-H", "Content-Type: application/json"]
        out = run(cmd, cwd=SRC, input=json.dumps(payload) if payload is not None else None).stdout
        return json.loads(out) if out.strip() else None

    def list_prs(self):
        mrs = self.api("GET", f"{self.project}/merge_requests?state=all&per_page=100&scope=all")
        states = {"opened": "open", "closed": "closed", "merged": "merged", "locked": "closed"}
        return {
            m["source_branch"]: dict(number=m["iid"], state=states[m["state"]], draft=m.get("draft", False))
            for m in mrs
        }

    def create_pr(self, spec):
        cmd = ["glab", "mr", "create", "-R", GITLAB_REPO, "-s", spec["head"], "-b", spec["base"],
               "-t", spec["title"], "-d", BODIES[spec["head"]], "--yes", "--no-editor"]
        if spec["draft"]:
            cmd.append("--draft")
        run(cmd, cwd=SRC)

    def merge(self, number):
        # The merge request needs a moment before GitLab considers it mergeable.
        for _ in range(10):
            mr = self.api("GET", f"{self.project}/merge_requests/{number}")
            if mr["merge_status"] == "can_be_merged" and mr["detailed_merge_status"] == "mergeable":
                break
            time.sleep(2)
        run(["glab", "mr", "merge", str(number), "-R", GITLAB_REPO, "--yes"], cwd=SRC)

    def close(self, number):
        run(["glab", "mr", "close", str(number), "-R", GITLAB_REPO], cwd=SRC)

    def line_codes(self, base_sha, head_sha, path):
        """Map new-line and old-line numbers of a diff to GitLab line codes."""
        digest = hashlib.sha1(path.encode()).hexdigest()
        diff = git("diff", "-U3", base_sha, head_sha, "--", path)
        new_codes, old_codes = {}, {}
        old = new = 0
        for row in diff.splitlines():
            if m := re.match(r"@@ -(\d+)(?:,\d+)? \+(\d+)(?:,\d+)? @@", row):
                old, new = int(m[1]) - 1, int(m[2]) - 1
            elif row.startswith("+") and not row.startswith("+++"):
                new += 1
                new_codes[new] = f"{digest}_{old}_{new}"
            elif row.startswith("-") and not row.startswith("---"):
                old += 1
                old_codes[old] = f"{digest}_{old}_{new}"
            elif row.startswith(" "):
                old += 1
                new += 1
                new_codes[new] = old_codes[old] = f"{digest}_{old}_{new}"
        return new_codes, old_codes

    def ensure_threads(self, number, specs):
        mr_path = f"{self.project}/merge_requests/{number}"
        discussions = self.api("GET", f"{mr_path}/discussions?per_page=100") or []
        by_body = {d["notes"][0]["body"]: d for d in discussions}
        mr = self.api("GET", mr_path)
        refs = mr["diff_refs"]
        path = "src/threads_target.rs"
        new_codes, old_codes = self.line_codes(self.shas["main"], self.shas["threads-initial"], path)
        # Positions only make sense against the first push; after the second one they are outdated.
        position = dict(position_type="text", base_sha=refs["base_sha"], start_sha=refs["start_sha"],
                        head_sha=self.shas["threads-initial"], old_path=path, new_path=path)
        for spec in specs:
            disc = by_body.get(spec["body"])
            if disc is None:
                pos = dict(position)
                if "old" in spec:
                    pos["old_line"] = spec["old"]
                else:
                    pos["new_line"] = spec["new"]
                    if "start" in spec:
                        start = dict(type="new", new_line=spec["start"], line_code=new_codes[spec["start"]])
                        end = dict(type="new", new_line=spec["new"], line_code=new_codes[spec["new"]])
                        pos["line_range"] = dict(start=start, end=end)
                try:
                    disc = self.api("POST", f"{mr_path}/discussions", dict(body=spec["body"], position=pos))
                except RuntimeError as err:
                    if "start" not in spec:
                        raise
                    self.skip(f"multi-line comment on !{number}: {str(err).splitlines()[-1][:160]}")
                    continue
                by_body[spec["body"]] = disc
                self.changed(f"discussion {spec['key']} on !{number}")
            if "reply" in spec and len(disc["notes"]) < 2:
                self.api("POST", f"{mr_path}/discussions/{disc['id']}/notes", dict(body=spec["reply"]))
                self.changed(f"reply to {spec['key']} on !{number}")
            if spec.get("resolve") and not disc["notes"][0].get("resolved"):
                self.api("PUT", f"{mr_path}/discussions/{disc['id']}", dict(resolved=True))
                self.changed(f"resolved discussion {spec['key']} on !{number}")


# ---------------------------------------------------------------- Forgejo


class Forgejo(Forge):
    name = "forgejo"
    remote = "forgejo"

    def __init__(self, shas):
        super().__init__(shas)
        keys = Path(os.environ["APPDATA"]) / "forgejo-cli" / "forgejo-cli" / "data" / "keys.json"
        self._token = json.loads(keys.read_text())["hosts"][FORGEJO_HOST]["token"]

    def api(self, method, path, payload=None):
        req = urllib.request.Request(
            f"https://{FORGEJO_HOST}/api/v1/{path}",
            method=method,
            data=json.dumps(payload).encode() if payload is not None else None,
            headers={"Authorization": f"token {self._token}", "Content-Type": "application/json"},
        )
        for attempt in range(4):
            try:
                with urllib.request.urlopen(req, timeout=60) as resp:
                    body = resp.read()
                return json.loads(body) if body.strip() else None
            except urllib.error.HTTPError as err:
                # A repository answers 404 for a moment after its first push.
                if err.code == 404 and method == "GET" and attempt < 3:
                    time.sleep(2)
                    continue
                raise RuntimeError(f"{method} {path}: HTTP {err.code} {err.read()[:400]!r}") from None

    def list_prs(self):
        prs = self.api("GET", f"repos/{FORGEJO_REPO}/pulls?state=all&limit=50") or []
        out = {}
        for p in prs:
            state = "merged" if p.get("merged") else p["state"]
            out[p["head"]["ref"]] = dict(number=p["number"], state=state, draft=p["title"].startswith("WIP:"))
        return out

    def create_pr(self, spec):
        title = ("WIP: " + spec["title"]) if spec["draft"] else spec["title"]
        run(["fj", "-H", FORGEJO_HOST, "pr", "create", title, "--base", spec["base"], "--head", spec["head"],
             "--body", BODIES[spec["head"]], "-r", FORGEJO_REPO])

    def merge(self, number):
        run(["fj", "-H", FORGEJO_HOST, "pr", "-R", "forgejo", "merge", str(number), "-M", "merge"], cwd=SRC)

    def close(self, number):
        run(["fj", "-H", FORGEJO_HOST, "pr", "-R", "forgejo", "close", str(number)], cwd=SRC)

    def review_comments(self, number):
        out = []
        for review in self.api("GET", f"repos/{FORGEJO_REPO}/pulls/{number}/reviews") or []:
            out += self.api("GET", f"repos/{FORGEJO_REPO}/pulls/{number}/reviews/{review['id']}/comments") or []
        return out

    def ensure_threads(self, number, specs):
        path = "src/threads_target.rs"
        have = {c["body"] for c in self.review_comments(number)}
        sha = self.shas["threads-initial"]

        def post(comments, what):
            self.api("POST", f"repos/{FORGEJO_REPO}/pulls/{number}/reviews",
                     dict(commit_id=sha, event="COMMENT", body="", comments=comments))
            self.changed(f"{what} on #{number}")

        first, replies = [], []
        for spec in specs:
            if "start" in spec:
                self.skip("multi-line comment: review comments are single-line")
                continue
            if spec.get("resolve"):
                self.skip("resolving a thread: web route only (needs a session), the comment is posted unresolved")
            if spec["body"] not in have:
                if "old" in spec:
                    pos = dict(old_position=spec["old"], new_position=0)
                else:
                    pos = dict(new_position=spec["new"])
                first.append(dict(path=path, body=spec["body"], **pos))
            if "reply" in spec and spec["reply"] not in have:
                replies.append(dict(path=path, body=spec["reply"], new_position=spec["new"]))
        if first:
            post(first, f"{len(first)} review comments")
        if replies:
            self.skip("native thread reply: no API; posted as a second comment on the same path and line")
            post(replies, "same-line reply")


FORGES = {"github": GitHub, "gitlab": GitLab, "forgejo": Forgejo}


def main():
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--forge", choices=[*FORGES, "all"], default="all")
    parser.add_argument("--ci", action="store_true", help="only add the CI workflow to main")
    args = parser.parse_args()
    names = list(FORGES) if args.forge == "all" else [args.forge]
    os.chdir(ROOT)
    load_targets(names)
    shas = build_source()
    total = 0
    for name in names:
        forge = FORGES[name](shas)
        total += forge.seed_ci() if args.ci else forge.seed()
    print(f"total: {total} changes")


if __name__ == "__main__":
    sys.exit(main())
