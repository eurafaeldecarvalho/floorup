# Security

floorup reads a `pyproject.toml`, fetches package metadata, and rewrites version
specifiers in that file. It does not execute dependency code.

## Reporting a vulnerability

Please use a [private security advisory](https://github.com/eurafaeldecarvalho/floorup/security/advisories/new)
on this repository. Do not open a public issue for a vulnerability report.

There is no bug bounty and no guaranteed response time.

## What to expect from the tool

- Only version-specifier text is supposed to change. A bug that rewrites other
  bytes in `pyproject.toml` is in scope.
- Index credentials in a URL (`https://user:token@host/...`) are redacted in
  error text. They still appear in the process command line if you pass
  `--index-url` that way, and they are part of the cache-key input. Prefer a
  URL without a secret, or a netrc/setup you do not have to put in `argv`.
- `--lock` runs `uv lock` in the project directory. A failing lock restores
  `pyproject.toml`. It does not roll back `uv.lock`.

The package is MIT-licensed and provided without warranty.
