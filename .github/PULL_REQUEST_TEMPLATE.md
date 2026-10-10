<!-- PR title: follow the Conventional Commits format, e.g. `feat(cli): add --interval flag` -->

## What and why

<!-- What does this change do, and what problem does it solve? Link the issue: Closes #123 -->

## How I verified it

<!-- Commands you ran, tests that prove it, or before/after output for UI changes. -->

## Checklist

- [ ] `cargo fmt --check` passes
- [ ] `cargo clippy --all-targets -- -D warnings` passes
- [ ] `cargo test` passes (ideally on Linux **and** macOS — `just linux` runs the Linux side)
- [ ] New behavior has a test at the right layer (see CONTRIBUTING.md → Tests)
- [ ] README updated if options, examples or the protocol table changed
- [ ] `CHANGELOG.md` entry added under `[Unreleased]` for user-facing changes
- [ ] Breaking change? Marked with `!` and a `BREAKING CHANGE:` footer
