## What and why

## Testing

- [ ] `cargo fmt --all -- --check`
- [ ] `cargo clippy --all-targets --keep-going --all-features -- -D warnings`
- [ ] `codespell`
- [ ] `cargo test` (state which crates, and whether against real hardware
      or in-memory doubles)

## Checklist

- [ ] Each commit passes CI independently (no fixup commits left in)
- [ ] Commit messages follow the seven-rule format
- [ ] Lockfiles updated if dependencies changed (`contrib/update-lock-files.sh`)

<!--
If an AI agent helped write this PR, add a line here, e.g.:
Disclosure: co-authored by Claude
Delete this comment block if not applicable.
-->
