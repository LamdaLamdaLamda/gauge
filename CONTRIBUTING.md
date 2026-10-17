# Contributing to gauge

Thanks for your interest in gauge! Bug reports, ideas and pull requests are
all welcome. This guide explains how to get set up and what a good change
looks like here.

By contributing you agree that your contribution is licensed under the
project's [MIT License](LICENSE).

## Table of contents

- [Reporting bugs and requesting features](#reporting-bugs-and-requesting-features) · [Security](SECURITY.md)
- [Development setup](#development-setup)
- [Project layout](#project-layout)
- [Making a change](#making-a-change)
- [Tests](#tests)
- [Code conventions](#code-conventions)
- [Documentation and changelog](#documentation-and-changelog)
- [Commit message conventions](#commit-message-conventions)
- [Pull requests](#pull-requests)
- [Releasing](#releasing)

## Reporting bugs and requesting features

Open an issue and include as much of this as applies:

- the exact command line or the TUI screen and keys you used, plus the
  server command on the other end,
- what you expected and what happened — for the TUI, a copy of the screen
  helps; the `gauge.log` file (see the README) is useful too,
- `gauge --version`, your OS, and your Rust version if you built from source,
- whether it was on loopback or a real network, and any firewall or VPN in
  between.

**Security problems** are different: do not open a public issue, follow
[SECURITY.md](SECURITY.md) and report them privately.

Please **do not post secrets or private addresses** you would not want public.
gauge has no authentication by design (see the README's security notes), so
don't expose a test server to untrusted networks while reproducing a problem.

For a feature request, describe the problem you want to solve first; the
design can follow from that.

## Development setup

You need Linux or macOS and **Rust 1.88 or newer** (gauge uses the 2024
edition). Windows is not supported.

```bash
git clone <this repository>
cd gauge

cargo build                                   # debug build
cargo run                                     # the TUI
cargo run -- server                           # a server, in another terminal
cargo run -- client --host 127.0.0.1          # a client
cargo test                                    # everything, about 20 seconds
```

The everyday tasks are wrapped in a [`justfile`](justfile) (`cargo install just`):

```bash
just setup    # once: installs cargo-audit and cargo-deny
just check    # what CI checks: fmt, clippy -D warnings, audit, deny, tests
just fmt      # format the code
just linux    # lints + tests inside a Linux container (needs Docker)
just demo     # server + 3 s client run against a release build
```

`just check` is the thing to run before you push.

Use `cargo build --release` for any real throughput numbers — debug builds are
far slower, especially for QUIC.

Because the TCP congestion-control and kernel-statistics code only exists on
Linux, it is worth running the suite on both operating systems before a PR
(CI does this for you). On a Mac, a Linux container is enough:

```bash
docker run --rm -v "$PWD":/src:ro rust:latest bash -c '
  mkdir /work && cp -r /src/Cargo.toml /src/Cargo.lock /src/src /src/tests /src/docs /work/ &&
  cd /work && rustup component add clippy &&
  cargo clippy --all-targets -- -D warnings && cargo test'
```

## Project layout

```text
src/main.rs        CLI-vs-TUI dispatch and the TUI event loop
src/cli.rs         the `client` / `server` subcommands
src/app.rs         TUI state, key handling, reactions to network events
src/config.rs      config.toml (defaults shared by CLI and TUI)
src/store.rs       history persistence and JSON export
src/filelog.rs     the gauge.log event-log file
src/net/           the engine: Tokio runtime, TCP / UDP / QUIC, protocol
src/ui/            Ratatui screens (live, config, history, server) + render tests
tests/tui_pty.rs   end-to-end tests that drive the real binary
```

The network engine lives on its own thread and talks to the UI only through
`NetCommand` / `NetEvent` channels; the CLI uses the same channels. See the
README's architecture section for details.

## Making a change

A few conventions the project follows — please keep to them:

- **Nothing on screen may be decorative.** A setting in the UI must change
  real behavior. If an option only applies to some protocols, say so in its
  label and in `--help`, and refuse (with a clear error) rather than silently
  ignore it when it cannot be honored.
- **One engine, two front ends.** Behavior belongs in `src/net/`; the TUI and
  CLI should only configure it and display its events. Don't re-implement
  logic in both.
- **Be honest about measurements.** If a number is an estimate (like TCP loss
  derived from retransmits), the docs and labels must say so.
- **Never trust the network.** Fields from a handshake are untrusted input;
  validate and clamp them.

### Adding or changing a test option

A new option usually touches several places. Walk through this checklist:

1. **Engine:** add the field to `ClientParams` (and to `Handshake` in
   `src/net/protocol.rs` if the server must act on it — mark new handshake
   fields `#[serde(default)]` so older peers keep working, and document the
   field and what its absence means in [docs/protocol.md](docs/protocol.md)),
   and implement it in `src/net/`. A change that older peers *cannot* cope
   with needs a new `PROTOCOL_VERSION` instead; see the versioning rules in
   that document.
2. **CLI:** add the flag in `src/cli.rs`, with the config-file default noted in
   its help text, and merge it in the order *flag > config file > default*.
3. **Config file:** add the key to `ClientDefaults` / `ServerDefaults` in
   `src/config.rs`.
4. **TUI:** add the field to the Config (or Server) screen state in
   `src/app.rs`, to `apply_defaults` / `to_defaults`, to `cli_preview`, and to
   the screen in `src/ui/`. Update the label so it says which protocols it
   applies to.
5. **History:** if the option changes results, record it in `RunSettings`
   (`src/store.rs`; new fields must stay `#[serde(default)]`-compatible so old
   `history.json` files still load).
6. **Docs:** the README's option table and examples, and the changelog.
7. **Tests:** at least one engine test proving the option *takes effect*,
   plus a render test if the UI wording changed.

## Tests

The suite has four layers; put a new test where it fits:

| Layer | Where | Use it for |
|---|---|---|
| State machine | `#[cfg(test)]` modules in `src/*.rs` | key handling, field logic, config merging, calculations |
| Engine integration | `src/net/mod.rs` tests | real loopback sockets (`127.0.0.1:0`): does the option or direction actually change what happens on the wire? |
| Rendered screens | `src/ui/tests.rs` | labels, panels, footer keys, small-terminal safety — drawn into ratatui's in-memory `TestBackend` |
| End to end | `tests/tui_pty.rs` | the real binary in a pseudo-terminal, driven with key bytes and read through a terminal emulator |

Guidelines:

- **Tests must not touch your real files.** Config and log writes are stubbed
  under `cfg(test)`; history and config tests use temporary paths; the
  end-to-end tests run with a throw-away `HOME`. Note that `App::default()`
  loads the real history file, so tests that render or inspect history set
  `app.history.runs` explicitly.
- **Use port 0** (let the OS choose) and bind to `127.0.0.1` so tests never
  collide with each other or with a running gauge.
- **Prove the behavior, not the plumbing.** For example, the buffer-size test
  checks that the total bytes written are a multiple of the configured chunk
  size, not just that the setting was stored.
- **Keep end-to-end tests few and meaningful.** They are the slowest layer;
  cover whole user flows and leave detail to the faster layers.
- If a test needs a mutation check, break the behavior temporarily and make
  sure the test actually fails.

## Code conventions

- `cargo fmt --check` and `cargo clippy --all-targets -- -D warnings` must
  pass; the CI enforces both. Formatting is configured in `rustfmt.toml` and
  `.editorconfig` — run `cargo fmt` (or `just fmt`) rather than formatting by
  hand.
- Match the surrounding code: naming, idiom and comment density. Comments
  explain *why* (a constraint, a platform quirk, a trap), not what the next
  line does.
- Platform-specific code goes behind `#[cfg(target_os = "…")]` with a defined
  fallback behavior, and both branches must compile (`cargo check --target
  x86_64-unknown-linux-gnu` is a quick check from macOS).
- Don't add `unsafe` outside the small socket-option wrappers
  (`src/net/sockopt.rs`, `src/net/tcpinfo.rs`); give each block a `SAFETY`
  comment.
- Don't reformat code you are not otherwise changing — keep formatting-only
  changes out of functional commits.
- Keep new dependencies to a minimum and prefer ones that are already in the
  tree. QUIC uses the `ring` crypto provider; avoid pulling in a second one.
  `cargo deny check` (rules in `deny.toml`) enforces the allowed licenses,
  trusted sources and known-vulnerability checks; a new license has to be
  added there deliberately. Dependabot opens weekly update PRs.

## Documentation and changelog

User-facing changes need two updates:

- **README.md** — options, examples and the "which option applies to which
  protocol" table. Test counts and the architecture tree should stay accurate.
- **CHANGELOG.md** — add a line under `[Unreleased]` in the right group
  (`Added`, `Changed`, `Deprecated`, `Removed`, `Fixed`, `Security`), written
  for users rather than for the diff.

## Commit message conventions

This project uses [Conventional Commits](https://www.conventionalcommits.org/):

```
<type>(<scope>): <short summary>

[optional body]

[optional footer]
```

Common types: `feat`, `fix`, `docs`, `refactor`, `test`, `chore`, `perf`

| Type | Use it for | Changelog group |
|---|---|---|
| `feat` | a new user-visible capability | Added |
| `fix` | a bug fix | Fixed |
| `perf` | a performance improvement without behavior change | Changed |
| `refactor` | restructuring without behavior change | — |
| `test` | adding or changing tests only | — |
| `docs` | README, CONTRIBUTING, comments, help texts | — |
| `chore` | build, CI, dependencies, releases, housekeeping | — |

The **scope** names the area you touched. Typical scopes here: `cli`, `tui`,
`config`, `history`, `net`, `tcp`, `udp`, `quic`, `ui`, `tests`, `ci`, `deps`,
`readme`, `release`. Omit it when a change spans everything.

Examples:

```
feat(cli): add --interval flag for the live report period
fix(udp): count out-of-order datagrams as received, not lost
docs(readme): clarify which options apply to QUIC
test(tui): drive a full client test through the pseudo-terminal
chore(ci): run the suite on macOS as well as Linux
```

Guidelines:

- Write the summary in the **imperative mood** ("add", not "added"), keep it
  to about 70 characters, don't end it with a period, and make it say *what*
  the change does.
- Use the **body** for the *why*: the problem, the trade-off or the platform
  quirk — not a restatement of the diff. Wrap it at about 72 columns.
- Mark a **breaking change** (a changed flag, config key, history format or
  handshake that older versions can't read) with `!` after the scope —
  `feat(cli)!: rename --window to --buffer-size` — and explain it in a
  `BREAKING CHANGE:` footer.
- Reference issues in the footer: `Closes #123`.
- One logical change per commit; keep `feat`/`fix` commits free of unrelated
  formatting changes.

## Pull requests

1. Fork and create a branch from `main`. Keep the change focused — one topic
   per PR; unrelated cleanups belong in their own.
2. Make sure `just check` passes (formatting, clippy, dependency checks and
   tests), ideally on Linux and macOS. The PR template has a checklist.
3. Describe **what** changed and **why**, and how you verified it (for example
   the exact commands you ran, or which test proves it). Include before/after
   output for UI changes.
4. Link the issue it fixes, if any.
5. Be ready for review feedback; small, well-explained PRs are merged fastest.

Commit messages follow the [conventions above](#commit-message-conventions);
the PR title should do too, since it often becomes the squash-commit message.

## Releasing

For maintainers:

1. Make sure `main` is green in CI.
2. Bump `version` in `Cargo.toml` (following [SemVer](https://semver.org/);
   what counts as a breaking change is in [docs/stability.md](docs/stability.md)).
   If the wire protocol changed in a way older peers can't cope with, also
   bump `PROTOCOL_VERSION` in `src/net/protocol.rs` and update
   [docs/protocol.md](docs/protocol.md).
3. In `CHANGELOG.md`, rename `[Unreleased]` to the new version with the date and
   add a fresh empty `[Unreleased]` section above it.
4. Run `cargo build --release` and do a short real-network test with the
   release binary.
5. Commit the release (`chore(release): release version X.Y.Z …`), open a pull
   request and merge it. **Tag the merge commit on `main`**, after the merge —
   the tag must point at a commit whose `Cargo.toml` already says the new
   version (check with `gauge --version` from that commit):
   `git tag -s vX.Y.Z -m "gauge X.Y.Z" && git push origin vX.Y.Z`.
6. Create the GitHub release from the tag, with the `CHANGELOG.md` section as
   its notes. `gauge` is not published to crates.io (`publish = false` in
   `Cargo.toml`).
