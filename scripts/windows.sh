#!/usr/bin/env bash
# Runs cargo on the Windows host from WSL, on a snapshot of the current
# working tree (uncommitted and untracked files included).
#
#   scripts/windows.sh test               # cargo test --workspace
#   scripts/windows.sh check              # clippy -D warnings + tests (debug and release)
#   scripts/windows.sh bench              # DSP throughput, on the real target CPU
#   scripts/windows.sh phase0 [ARGS...]   # real-time run on the Windows audio device
#
# The Windows side is a clone of this repository (DAW_WIN_DIR, default
# C:\dev\Daw) used as a mirror: do not edit files there. phase0 logs are
# saved under target/windows-logs/. Needs cargo.exe and git.exe on the PATH
# (WSL appends the Windows PATH by default).
set -euo pipefail
shopt -s inherit_errexit
cd "$(dirname "$0")/.."

win_dir="${DAW_WIN_DIR:-C:\\dev\\Daw}"

# Commit the working tree into a private ref without touching the index,
# the working tree or any branch.
snapshot() {
    local index
    index="$(mktemp)"
    # Start from the real index so unchanged files are not re-hashed.
    cp "$(git rev-parse --git-path index)" "$index"
    GIT_INDEX_FILE="$index" git add -A
    local tree
    tree="$(GIT_INDEX_FILE="$index" git write-tree)"
    rm -f "$index"
    local commit
    commit="$(git commit-tree "$tree" -p HEAD -m "windows snapshot")"
    git update-ref refs/windows/snapshot "$commit"
    echo "$commit"
}

# Runs a Windows program. Windows tools dislike a \\wsl.localhost working
# directory, so start from C:\. `sed -u` strips CRs without buffering, so
# long runs stream line by line.
win() {
    (cd /mnt/c && "$@") 2>&1 | sed -u 's/\r$//'
    return "${PIPESTATUS[0]}"
}

cargo_win() {
    local sub="$1"
    shift
    win cargo.exe "$sub" --manifest-path "$win_dir\\Cargo.toml" "$@"
}

sync() {
    local commit
    commit="$(snapshot)"
    echo "==> syncing ${commit:0:10} to $win_dir"
    win git.exe -C "$win_dir" fetch -q origin +refs/windows/snapshot:refs/windows/snapshot
    win git.exe -C "$win_dir" checkout -q --detach refs/windows/snapshot
}

cmd="${1:-test}"
shift || true
sync
case "$cmd" in
    test)
        cargo_win test --workspace
        ;;
    check)
        cargo_win clippy --workspace --all-targets -- -D warnings
        cargo_win test --workspace
        cargo_win test --workspace --release
        ;;
    bench)
        cargo_win run --release -q -p engine-dsp --example bench
        ;;
    phase0)
        mkdir -p target/windows-logs
        log="target/windows-logs/phase0-$(date +%Y%m%d-%H%M%S).log"
        echo "==> logging to $log"
        cargo_win run --release -q -p engine-io --example phase0 -- "$@" | tee "$log"
        ;;
    *)
        echo "unknown command $cmd (test | check | bench | phase0 [ARGS...])" >&2
        exit 2
        ;;
esac
