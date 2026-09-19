#!/usr/bin/env bash
# Every check CI runs, in order of cost. `--fast` stops after lints.
set -euo pipefail
cd "$(dirname "$0")/.."

# The pulseaudio feature only makes sense on Linux.
features=()
[[ "$(uname -s)" == Linux ]] && features=(--all-features)

step() { printf '\n==> %s\n' "$*"; }

step fmt
cargo fmt --all --check
step clippy
cargo clippy --workspace --all-targets "${features[@]}" -- -D warnings
[[ "${1:-}" == --fast ]] && exit 0

step "test (debug: no-alloc check active)"
cargo test --workspace "${features[@]}"
step "test (release: optimised float code)"
cargo test --workspace --release "${features[@]}"
step doc
RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps "${features[@]}"
