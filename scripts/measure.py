#!/usr/bin/env python3
"""Time a cold release run of uv-bump against the vendored FastAPI pyproject.

The clock starts when the release binary is spawned and stops when it exits.
`cargo build` is not included. The cache directory is empty.

Unrelated lines are difflib opcodes on the two files after every specifier
token (operator plus version) is replaced with the same placeholder. That
count does not use uv-bump's edit spans.
"""

from __future__ import annotations

import difflib
import json
import os
import re
import subprocess
import sys
import tempfile
import time
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
FIXTURE = ROOT / "tests" / "fixtures" / "real" / "fastapi.toml"
BIN = ROOT / "target" / "release" / "uv-bump"

SPECIFIER = re.compile(r"(?:===|==|!=|~=|<=|>=|<|>)\s*[vV]?[0-9][0-9A-Za-z.*+!_-]*")


def normalize_specifiers(text: str) -> str:
    return SPECIFIER.sub("SPEC", text)


def unrelated_diff_lines(before: str, after: str) -> int:
    """Lines that still differ after every specifier token is replaced.

    Difflib opcodes run on that normalized text. A pure specifier edit,
    including `>` rewritten to `>=`, contributes nothing.
    """
    left = normalize_specifiers(before)
    right = normalize_specifiers(after)
    if left == right:
        return 0
    diff = difflib.unified_diff(left.splitlines(), right.splitlines(), lineterm="")
    return sum(
        1
        for line in diff
        if (line.startswith("+") or line.startswith("-"))
        and not line.startswith("+++")
        and not line.startswith("---")
    )


def self_test() -> None:
    original = 'deps = ["pkg>=1.2.3", "other > 2"] # keep pkg>=9.9.9\n'
    updated = 'deps = ["pkg>=9.0.0", "other >= 3.1"] # keep pkg>=9.9.9\n'
    if unrelated_diff_lines(original, updated) != 0:
        raise SystemExit("self-test: specifier edits were counted as unrelated")
    dirty = original.replace("deps", "dependencies")
    if unrelated_diff_lines(original, dirty) == 0:
        raise SystemExit("self-test: a non-specifier edit was ignored")
    comment = original.replace("# keep", "# changed")
    if unrelated_diff_lines(original, comment) == 0:
        raise SystemExit("self-test: a comment edit was ignored")


def main() -> int:
    self_test()
    if not FIXTURE.is_file():
        print(f"missing fixture {FIXTURE}", file=sys.stderr)
        return 2
    subprocess.check_call(["cargo", "build", "--release"], cwd=ROOT)
    original = FIXTURE.read_text()
    with tempfile.TemporaryDirectory(prefix="uv-bump-measure-") as tmp:
        directory = Path(tmp)
        project = directory / "pyproject.toml"
        cache = directory / "cache"
        project.write_text(original)
        cache.mkdir()
        env = os.environ.copy()
        env["UV_BUMP_CACHE_DIR"] = str(cache)
        started = time.perf_counter()
        proc = subprocess.run(
            [
                str(BIN),
                str(project),
                "--format",
                "json",
                "--cache-dir",
                str(cache),
            ],
            check=False,
            capture_output=True,
            text=True,
            env=env,
        )
        elapsed = time.perf_counter() - started
        if proc.returncode != 0:
            sys.stderr.write(proc.stderr)
            sys.stderr.write(proc.stdout)
            return proc.returncode or 1
        updated = project.read_text()
    payload = json.loads(proc.stdout)
    bumped = int(payload["bumped"])
    unrelated = unrelated_diff_lines(original, updated)
    print(f"{bumped} deps bumped in {elapsed:.3f}s with {unrelated} unrelated diff lines")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
