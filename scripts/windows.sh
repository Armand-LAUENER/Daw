#!/usr/bin/env bash
# Runs cargo on the Windows host from WSL, on a snapshot of the current
# working tree (uncommitted and untracked files included).
#
#   scripts/windows.sh test               # cargo test --workspace
#   scripts/windows.sh check              # clippy -D warnings + tests (debug and release)
#   scripts/windows.sh phase0 [ARGS...]   # real-time run on the Windows audio device
#
# The Windows side is a clone of this repository (DAW_WIN_DIR, default
# C:\dev\Daw) used as a mirror: do not edit files there. phase0 logs are
# saved under target/windows-logs/.
set -euo pipefail
shopt -s inherit_errexit
cd "$(dirname "$0")/.."

win_dir="${DAW_WIN_DIR:-C:\\dev\\Daw}"
powershell=/mnt/c/Windows/System32/WindowsPowerShell/v1.0/powershell.exe

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

# Runs a PowerShell command in the Windows repo; propagates its exit code.
win() {
    "$powershell" -NoProfile -NonInteractive -Command "
        [Console]::OutputEncoding = [Text.Encoding]::UTF8
        \$ErrorActionPreference = 'Stop'
        Set-Location '$win_dir'
        $1
        exit \$LASTEXITCODE" 2>&1 | tr -d '\r'
    return "${PIPESTATUS[0]}"
}

sync() {
    local commit
    commit="$(snapshot)"
    echo "==> syncing ${commit:0:10} to $win_dir"
    win "git fetch -q origin +refs/windows/snapshot:refs/windows/snapshot
         if (\$LASTEXITCODE -eq 0) { git checkout -q --detach refs/windows/snapshot }"
}

cmd="${1:-test}"
shift || true
sync
case "$cmd" in
    test)
        win "cargo test --workspace"
        ;;
    check)
        win "cargo clippy --workspace --all-targets -- -D warnings
             if (\$LASTEXITCODE -eq 0) { cargo test --workspace }
             if (\$LASTEXITCODE -eq 0) { cargo test --workspace --release }"
        ;;
    phase0)
        mkdir -p target/windows-logs
        log="target/windows-logs/phase0-$(date +%Y%m%d-%H%M%S).log"
        echo "==> logging to $log"
        # Quote each argument for PowerShell.
        args=""
        for a in "$@"; do args+=" '${a//\'/\'\'}'"; done
        set +e
        win "cargo run --release -q -p engine-io --example phase0 --$args" | tee "$log"
        status="${PIPESTATUS[0]}"
        set -e
        exit "$status"
        ;;
    *)
        echo "unknown command $cmd (test | check | phase0 [ARGS...])" >&2
        exit 2
        ;;
esac
