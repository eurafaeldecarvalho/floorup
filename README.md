# uv-bump

**Raise every version floor in `pyproject.toml` to the latest PyPI release, in one command, without touching anything else.**

[![CI](https://github.com/eurafaeldecarvalho/uv-bump/actions/workflows/ci.yml/badge.svg?branch=main)](https://github.com/eurafaeldecarvalho/uv-bump/actions/workflows/ci.yml)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)

> On FastAPI's real `pyproject.toml`: **90 dependencies bumped in 0.165 s, 0 unrelated diff lines.**
> Median of 7 cold runs of [`scripts/measure.py`](scripts/measure.py) (wall clock including PyPI lookups, empty cache) on 2026-10-27. [Full diff](docs/fastapi-dry-run.diff).

`uv lock --upgrade` moves your lock file. The bounds you wrote in `pyproject.toml` stay where they were, so a project can say `httpx>=0.23` for years while it actually runs 0.28. uv has no command to fix that yet ([astral-sh/uv#6794](https://github.com/astral-sh/uv/issues/6794), open since August 2024, 537 👍). `uv-bump` fills that gap until uv ships it natively.

## In 10 seconds

```console
$ uv-bump --dry-run
bumped 5 dependencies in 0.195s; 1 exact pins unchanged
httpx>=0.23  ->  httpx>=0.28.1  (project.dependencies)
pydantic>=2.0,<3  ->  pydantic>=2.14.1,<3  (project.dependencies)
rich~=13.0  ->  rich~=13.9  (project.dependencies)
pytest>=7  ->  pytest>=9.1.1  (dependency-groups.dev)
ruff>=0.1.0  ->  ruff>=0.17.3  (dependency-groups.dev)
```

```diff
--- pyproject.toml
+++ pyproject.toml
@@ -3,11 +3,11 @@
 version = "0.1.0"
 requires-python = ">=3.10"
 dependencies = [
-    "httpx>=0.23",
-    "pydantic>=2.0,<3",  # cap stays
-    "rich~=13.0",        # stays on 13.x
+    "httpx>=0.28.1",
+    "pydantic>=2.14.1,<3",  # cap stays
+    "rich~=13.9",        # stays on 13.x
     "click==8.1.3",      # exact pin, left alone
 ]
 
 [dependency-groups]
-dev = ["pytest>=7", "ruff>=0.1.0"]
+dev = ["pytest>=9.1.1", "ruff>=0.17.3"]
```

Only version specifiers change. Comments, key order, quoting, spacing and blank lines come back byte for byte. (Real output from 2026-10-27; your versions will be newer.)

## Install

Requires Rust 1.88 or newer.

```sh
cargo install --locked --git https://github.com/eurafaeldecarvalho/uv-bump
```

There is no crates.io, PyPI or Homebrew release yet. Note that the `uv-bump` package on PyPI is a different project ([zundertj/uv-bump](https://github.com/zundertj/uv-bump)), so `uvx uv-bump` does not run this tool.

## Usage

```sh
uv-bump                      # bump ./pyproject.toml in place
uv-bump --dry-run            # print a unified diff, write nothing (-n)
uv-bump --check              # exit 1 if any bound would change; write nothing
uv-bump path/to/project      # a pyproject.toml, or a directory containing one
uv-bump --lock               # write, then run `uv lock`; pyproject.toml is restored if locking fails

uv-bump -p requests,flask    # only these packages (--package)
uv-bump --exclude django     # skip these packages
uv-bump --group dev          # only these groups: project, dev, an extra, a dependency group, build-system
uv-bump --exclude-group build-system

uv-bump --bump-pins          # also rewrite exact == pins
uv-bump --expand-compatible  # let ~= leave its current series
uv-bump --pre                # consider pre-releases and dev releases
```

<details>
<summary>Full <code>uv-bump --help</code></summary>

```text
Bump dependency bounds in pyproject.toml to the latest PyPI releases.

Usage: uv-bump [OPTIONS] [PYPROJECT]

Arguments:
  [PYPROJECT]  Path to pyproject.toml, or a directory containing one [default: pyproject.toml]

Options:
  -n, --dry-run                         Print a unified diff and do not write the file
      --check                           Exit 1 when any bound would change. Does not write the file
      --pre                             Consider pre-releases and development releases
      --bump-pins                       Rewrite exact `==` pins to the latest release. `===` is never rewritten
      --expand-compatible               Let `~=` track the absolute latest release instead of its current series
  -p, --package <PACKAGES>              Only bump these packages. Repeat the flag or separate names with commas
      --exclude <EXCLUDE>               Skip these packages
      --group <GROUPS>                  Only bump these groups (`project`, `dev`, an extra name, a dependency group, `build-system`)
      --exclude-group <EXCLUDE_GROUPS>  Skip these groups
      --lock                            Run `uv lock` after a successful write. The pyproject.toml is restored if locking fails
      --index-url <INDEX_URL>           PyPI JSON API root. A URL ending in `/simple` is rewritten to the sibling `/pypi` root [default: https://pypi.org/pypi]
      --no-cache                        Do not read or write the on-disk response cache
      --offline                         Use only the on-disk cache
      --cache-dir <CACHE_DIR>           Directory for cached PyPI responses
      --cache-ttl <CACHE_TTL>           Cache lifetime in seconds. 0 stores responses but never reuses them [default: 3600]
      --jobs <JOBS>                     Concurrent package lookups [default: 16]
      --timeout <TIMEOUT>               HTTP timeout in seconds for each lookup [default: 30]
      --keep-going                      Continue after a lookup failure. Packages that could not be looked up are left unchanged
  -v, --verbose                         List pins, missing packages, and other skips
  -q, --quiet                           Suppress the summary. A dry-run diff is still printed
      --format <FORMAT>                 `text` prints a summary. `json` prints one object [default: text] [possible values: text, json]
  -h, --help                            Print help
  -V, --version                         Print version

Exact pins (==, ===) and upper caps (<, <=) are left in place. >=, >, and ~= move forward. Pass --bump-pins to rewrite exact pins, and --dry-run to print a diff.
```

</details>

## What it touches, what it leaves alone

**Tables it edits:** `[project] dependencies`, `[project.optional-dependencies]`, `[dependency-groups]`, `[tool.uv] dev-dependencies`, `[build-system] requires`.

| You wrote | uv-bump writes | Why |
| --- | --- | --- |
| `>=2.28` | `>=2.32.3` (newest release) | Floors move forward |
| `>2.28` | `>=2.32.3` | Rewritten to `>=` only when the new release is strictly newer and still satisfies the old `>` |
| `>=2.0,<3` | `>=2.14.1,<3` | Caps (`<`, `<=`) are never widened; the floor moves to the newest release under the cap |
| `~=13.0` | `~=13.9` | Stays in its series with the same component count. `--expand-compatible` lets it leave |
| `!=2.8` | kept | The chosen release must satisfy every exclusion |
| `==1.2.3` | unchanged | Exact pins stay unless you pass `--bump-pins` |
| `==1.*` | unchanged | A wildcard is a prefix constraint, not a pin, so even `--bump-pins` leaves it |
| `===1.2.3` | never rewritten | Arbitrary equality is a string match |
| `requests` (no bound) | unchanged | It never invents a bound you did not write |
| `pkg @ https://…`, paths, VCS | unchanged | Direct references are skipped |

Extras and environment markers are carried through untouched. Pre-releases and dev releases are ignored unless you pass `--pre` or the specifier already names one (`>=0.65b0`). Fully yanked releases, releases with no files, and local versions are ignored.

## In CI

`--check` writes nothing and exits **1** when any bound would move, so a stale `pyproject.toml` can fail a job the same way unformatted code does.

```yaml
# .github/workflows/deps.yml
name: Dependency floors
on:
  schedule:
    - cron: "0 6 * * 1"   # Mondays
  workflow_dispatch:
jobs:
  check:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - uses: dtolnay/rust-toolchain@stable
      - run: cargo install --locked --git https://github.com/eurafaeldecarvalho/uv-bump
      - run: uv-bump --check --verbose
```

Exit codes: `0` nothing to change (or written successfully), `1` `--check` found updates, `2` parse error, lookup error, or a failed `uv lock`. Use `--format json` for machine-readable output (counts, every change, skip reasons, and the diff).

## How it compares

Several people have built tools for the same gap. They make different trade-offs, and one of them may suit you better.

| Tool | Written in / install | Where new versions come from | Notes from its README (as of 2026-10-27) |
| --- | --- | --- | --- |
| **uv-bump** (this) | Rust, `cargo install --git` | PyPI JSON API, directly | No lock file or venv needed. Format-preserving. `--check` for CI. No workspace walk |
| [zundertj/uv-bump](https://github.com/zundertj/uv-bump) | Python, PyPI `uv-bump` | `uv sync`, then the versions it installed | Floors follow what uv actually resolved. Workspace support. Expects a clean, synced project |
| [Rezarys/uvbump](https://github.com/Rezarys/uvbump) | Python, PyPI `uv-pyproject-bump` | Package index (PEP 691) | Format-preserving, `--check`, runs `uv lock` by default, `--widen` for caps. Also edits uv constraint/override tables |
| [Alirex/uv_upgrade](https://github.com/Alirex/uv_upgrade) | Python, PyPI `uv-upx` | `uv.lock` after `uv sync --upgrade` | Workspace support, rollback on failure. Adds floors to unbounded deps unless `--keep-unconstrained` |
| [duriantaco/fyn](https://github.com/duriantaco/fyn) | Rust, a fork of uv | Its own resolver (`fyn upgrade`) | Resolves, widens blocking constraints, updates lock and env. First version covers `[project].dependencies` |
| [rafaelsq/uvu](https://github.com/rafaelsq/uvu) | Python, `uv tool install git+…` | uv | Interactive, one package at a time, with changelog links and a release-age cooldown |
| [janusheide/uppd](https://github.com/janusheide/uppd) | Python, PyPI `uppd` | Package index | `pur`-style; operators to update are configurable (default `==`, `<=`, `~=`) |
| [poetry-plugin-up](https://github.com/MousaZeidBaker/poetry-plugin-up) | Python, Poetry plugin | Poetry | For Poetry projects (`[tool.poetry]` tables) |

The main design choice here: uv-bump reads the index, not a resolver. That makes it fast and independent of your environment, and it means the newest release in range is not checked against the rest of your dependency set. Run `uv lock` (or pass `--lock`) to find out whether everything still resolves.

## Limitations

- **No uv workspace walk.** Run it once per member: `uv-bump packages/foo`.
- **`requires-python` is not edited.**
- **No Poetry tables** (`[tool.poetry.*]`). Use poetry-plugin-up.
- uv's `constraint-dependencies`, `override-dependencies` and `build-constraint-dependencies` are not edited.
- **No resolver.** Each package gets its newest eligible release independently. `--lock` runs `uv lock` afterwards and restores `pyproject.toml` if it fails; it does not restore `uv.lock` if uv rewrote it before failing.
- Talks to the PyPI JSON API (`--index-url` for a mirror; a URL ending in `/simple` is mapped to the sibling `/pypi`). Indexes that only serve the simple API are not supported.

Responses are cached for an hour in `$UV_BUMP_CACHE_DIR`, `$XDG_CACHE_HOME/uv-bump`, or `~/.cache/uv-bump`. `--offline` uses only the cache; `--no-cache` skips it.

## Until uv ships this natively

The right long-term home for this is uv itself: [astral-sh/uv#6794](https://github.com/astral-sh/uv/issues/6794). When uv has a native command, use it. uv-bump is a small, separate tool meant to be useful until then. It is not affiliated with Astral.

## Contributing

Bug reports with a `pyproject.toml` that gets mangled, or a specifier that gets the wrong answer, are the most useful thing you can send. Use the [issue templates](.github/ISSUE_TEMPLATE); see [SUPPORT.md](SUPPORT.md) and [SECURITY.md](SECURITY.md).

```sh
cargo test                                   # unit, golden-file, property, and mocked-PyPI tests
UV_BUMP_PYPI_LIVE=1 cargo test --test live   # hits real PyPI
python3 scripts/measure.py                   # the headline number
```

CI runs `cargo fmt --check`, `cargo clippy -D warnings`, and `cargo test`. Golden files in `tests/fixtures/expected/` are real projects' `pyproject.toml` files (FastAPI, Flask, httpx, Pydantic, Rich, Ruff; see [SOURCES.md](tests/fixtures/real/SOURCES.md)) bumped against a fixed version grid, so they do not drift with PyPI.

The initial implementation was written with an AI coding agent and is covered by the test suite above.

## License

[MIT](LICENSE). Copyright (c) 2026 Rafael de Carvalho.
