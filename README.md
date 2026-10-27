# uv-bump

90 deps bumped in 0.167s with 0 unrelated diff lines. That number is one cold run of `python3 scripts/measure.py` on the vendored FastAPI `pyproject.toml` ([79cab5be](https://github.com/fastapi/fastapi/commit/79cab5bed28b24d92c185d236feb33f5783dfeb3), 2026-10-27). The clock is the process wall time, and the unrelated-line count is computed by the script, not by uv-bump.

`uv-bump` raises dependency bounds in `pyproject.toml` to the latest release on PyPI. Comments, key order, quotes, and spacing stay as they were. The only bytes that change are version specifiers.

People have been asking uv for this since [astral-sh/uv#6794](https://github.com/astral-sh/uv/issues/6794). This is a separate program. It does not require a uv account, and it does not publish anything.

## Quickstart

```sh
git clone https://github.com/eurafaeldecarvalho/uv-bump.git
cd uv-bump
cargo run --release -- --dry-run
```

That prints a diff and leaves `pyproject.toml` untouched. Drop `--dry-run` to write the file. The commands above use the default branch.

## What it edits

- `[project.dependencies]`
- `[project.optional-dependencies]`
- `[dependency-groups]`
- `[tool.uv] dev-dependencies`
- `[build-system] requires`

Each requirement is read as a PEP 508 string. Extras and environment markers stay put. `{ include-group = "..." }` entries are not requirements and are skipped. Direct URL, path, and VCS requirements (`name @ ...`) are skipped.

`requires-python`, Poetry tables, and uv's constraint, override, and build-constraint tables are not edited. A uv workspace is not walked; point the tool at each member, or at the member file.

## Version policy

The newest release that still satisfies the ceilings and exclusions is written into each floor.

| Specifier | Result |
| --- | --- |
| `>=2.28` | `>=` the newest eligible release, full version text (`>=2.32.3`, not `>=2.32`) |
| `>2.28` | `>=` that release, and only when it is strictly newer. `>2.32.3` stays put when `2.32.3` is current, because `>2.32.3` would exclude it |
| `~=2.28` | stays inside the same series and keeps the component count: `~=2.32` (`==2.*`). `~=2.28.1` becomes the newest `2.28.x` |
| `~=2.28` with `--expand-compatible` | the absolute latest release, still at the original component count |
| `==1.2.3`, `=1.2.3`, `==1.*` | left alone. `--bump-pins` rewrites `==` and legacy `=` |
| `===1.2.3` | never rewritten |
| `<5`, `<=2` | not widened. The floor moves up to the newest release that still fits |
| `!=2.8` | kept. The chosen release must satisfy it |
| no specifier | left alone |

`>=2.32` is already current when the published release is `2.32.0` (PEP 440 treats them as equal).

Pre-releases and development releases are ignored unless you pass `--pre`, or the specifier already names one (`>=0.65b0`). Yanked releases are ignored when every uploaded file is yanked. Releases with no files are ignored. Local versions (`1.2.3+local`) are ignored.

With `--pre`, a pre-release is written into `~=` only when that spelling still admits it and does not narrow the series. `~=2.28.0` can become `~=2.28.5rc1`. `~=2.28` does not become `~=2.33.0rc1`, because that would turn `==2.*` into `==2.33.*`.

## Filters

```text
--package requests,flask     only these packages
--exclude flask              skip these packages
--group project,dev          only these groups
--exclude-group build-system skip these groups
```

Group names:

- `project`, `dependencies`, or `project.dependencies` for `[project.dependencies]`
- an extra name, or `optional:<name>`
- a dependency-group name, or `group:<name>`
- `dev` or `tool.uv.dev-dependencies` for `[tool.uv] dev-dependencies`
- `build-system` for `[build-system] requires`

`dev` matches a dependency group named `dev` and `[tool.uv] dev-dependencies`. The same short name matches an extra of that name.

## Other flags

```text
--check                 exit 1 when a bound would change; do not write
--bump-pins             rewrite exact == and = pins
--expand-compatible     let ~= leave its current series
--pre                   consider pre-releases and dev releases
--lock                  run `uv lock` after a write; restore pyproject.toml if it fails
--index-url URL         JSON API root (default https://pypi.org/pypi)
--offline               use only the on-disk cache
--no-cache              do not read or write the cache
--cache-dir PATH
--cache-ttl SECS        default 3600; 0 stores a response and never reuses it
--jobs N                default 16
--timeout SECS          default 30, per request
--keep-going            leave a package unchanged when its lookup fails
--verbose               list skips (pins, missing packages, direct references)
--quiet                 summary off; a dry-run diff is still printed
--format text|json
```

A URL that ends in `/simple` is treated as the sibling `/pypi` JSON root. Userinfo in an index URL is redacted in errors.

Cache directory: `$UV_BUMP_CACHE_DIR`, or `$XDG_CACHE_HOME/uv-bump`, or `~/.cache/uv-bump`.

Exit codes: `0` success, `1` `--check` found updates, `2` a parse error, a lookup error, or a failed `uv lock`.

`--lock` does not restore `uv.lock` if `uv` rewrote it and then failed. The resolver is not run before the edit, so the newest in-range release can still be incompatible with the rest of the set.

## Tests

```sh
cargo test
UV_BUMP_PYPI_LIVE=1 cargo test --test live
python3 scripts/measure.py
```

CI runs formatting, clippy, and `cargo test`. The live test talks to PyPI and is off unless `UV_BUMP_PYPI_LIVE=1`. Golden files under `tests/fixtures/expected/` are checked against a fixed version grid, not against PyPI; see `tests/fixtures/real/SOURCES.md`.

Cursor's coding agent (Grok 4.7) wrote the initial implementation. Tests cover specifier parsing, format preservation (golden files and property tests), a mocked PyPI in CI, and a live PyPI check gated by `UV_BUMP_PYPI_LIVE=1`.

## License

MIT. Copyright (c) 2026 Rafael de Carvalho.
