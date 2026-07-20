# Contributing

## Toolchain

Nightly, pinned in `rust-toolchain.toml`. Tools come from Nix via
`nix develop` (host) or `nix develop .#embedded` (cross-compiling for
board targets). Don't install tools globally -- add them to `flake.nix`
instead, so everyone gets the same environment.

## Commit rules

Every commit must pass CI independently. No fixup commits left in a PR --
use `git commit --fixup` + `git rebase --autosquash` locally, then push
the squashed result.

Commit messages follow the seven rules:

1. Separate subject from body with a blank line
2. Limit the subject line to 50 characters
3. Capitalize the subject line
4. Do not end the subject line with a period
5. Use the imperative mood in the subject line
6. Wrap the body at 72 characters
7. Use the body to explain what and why, not how

No `chore:` or other conventional-commit prefixes. No `Co-Authored-By`
trailers in commits -- if an AI agent helped, note it once in the PR body
instead (`Disclosure: co-authored by <agent>`), not per commit.

## Pre-commit checks

Fast, every commit:

```sh
cargo fmt --all -- --check
cargo clippy --all-targets --keep-going --all-features -- -D warnings
codespell
```

Full, before push:

```sh
./contrib/lint.sh
RUSTDOCFLAGS="-D warnings" cargo doc --no-deps --all-features --document-private-items
./contrib/test_local.sh
nix fmt -- --ci
codespell
```

## Lockfiles

After any dependency change:

```sh
bash contrib/update-lock-files.sh
```

## Review style

GitHub comments: no hyphens, plain sentences. Prefer utACK/tACK/cACK with
an explicit statement of what was and wasn't tested (host tests only vs.
real hardware). Inline suggestions use GitHub's suggestion-block syntax.

## Reporting a bug or opening a hardware task

Use the issue templates -- they ask for the crate(s) affected and, for
hardware issues, which board and what's blocking it. That's usually
enough to reproduce or pick up the work without back-and-forth.
