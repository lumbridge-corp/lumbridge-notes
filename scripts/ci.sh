#!/usr/bin/env bash
# What CI runs, and what to run before a pull request: scripts/ci.sh
# Scrub check, format, clippy, tests. Offline once the crates are fetched (--locked).
set -euo pipefail
cd "$(dirname "$0")/.."
scripts/scrub-check
# A throwaway home: nothing here may read or write a real config, state or socket. cargo and rustup keep their own
# directories, so the build uses the caches that are already there.
export CARGO_HOME="${CARGO_HOME:-$HOME/.cargo}" RUSTUP_HOME="${RUSTUP_HOME:-$HOME/.rustup}"
T=$(mktemp -d "${TMPDIR:-/tmp}/lci.XXXXXX")
trap 'rm -rf "$T"' EXIT
mkdir -p "$T/home" "$T/config" "$T/data" "$T/state" "$T/cache" "$T/run"
chmod 700 "$T/run"
export HOME="$T/home" XDG_CONFIG_HOME="$T/config" XDG_DATA_HOME="$T/data" XDG_STATE_HOME="$T/state" XDG_CACHE_HOME="$T/cache" XDG_RUNTIME_DIR="$T/run"
unset TMUX TMUX_PANE
for v in $(env | grep -oE '^LUMBRIDGE_[A-Za-z0-9_]*' || true); do unset "$v"; done
rustc --version
cargo fmt --check
cargo clippy --all-targets --locked -- -D warnings
cargo test --locked
