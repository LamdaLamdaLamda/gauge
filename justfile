# Developer tasks — run `just` to list them. Install `just` with
# `cargo install just`, the checkers with `just setup`.

# List the available recipes
default:
    @just --list

# Everything CI checks: formatting, lints, dependency audit, license/ban rules, tests
check:
    cargo fmt --check
    cargo clippy --all-targets -- -D warnings
    cargo audit
    cargo deny check
    cargo test

# Install the tools `check` needs
setup:
    cargo install cargo-audit
    cargo install cargo-deny

# Format the code
fmt:
    cargo fmt

# Run the whole test suite
test:
    cargo test

# Run clippy with warnings denied
lint:
    cargo clippy --all-targets -- -D warnings

# Optimized build at target/release/gauge (use this for real measurements)
build:
    cargo build --release

# Install the release binary into ~/.cargo/bin
install: check
    cargo install --path .
    @echo "✓ gauge installed"

# Run the suite and the lints on Linux in a container (the Linux-only code paths: TCP_INFO, TCP_CONGESTION)
linux:
    docker run --rm -v "{{justfile_directory()}}":/src:ro rust:latest bash -c ' \
      mkdir /work && cp -r /src/Cargo.toml /src/Cargo.lock /src/src /src/tests /work/ && \
      cd /work && rustup component add clippy >/dev/null 2>&1 && \
      cargo clippy --all-targets -- -D warnings && cargo test'

# Quick demo: a server in the background, then a 3 second client run against it
demo: build
    #!/usr/bin/env bash
    set -euo pipefail
    ./target/release/gauge server --one-off --port 5299 >/dev/null &
    sleep 1
    ./target/release/gauge client --port 5299 -t 3
