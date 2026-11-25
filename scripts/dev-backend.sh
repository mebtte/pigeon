#!/bin/sh
set -eu

pigeon_root=$(CDPATH= cd "$(dirname "$0")/.." && pwd -P)

if ! command -v watchexec >/dev/null 2>&1; then
    printf '%s\n' 'Install Watchexec before starting the backend watcher. On macOS, run brew install watchexec.' >&2
    exit 127
fi

if ! command -v cargo >/dev/null 2>&1 && command -v rustup >/dev/null 2>&1; then
    pigeon_cargo=$(rustup which cargo)
    PATH="$(dirname "$pigeon_cargo"):$PATH"
    export PATH
fi

PIGEON_VITE_ORIGIN=${PIGEON_VITE_ORIGIN:-http://127.0.0.1:5173}
export PIGEON_VITE_ORIGIN

# Poll because native filesystem notifications can fail to start.
# Keep metadata events enabled because polling reports edits as mtime changes.
exec watchexec --restart --quiet --shell=none --debounce 300ms --poll=500ms \
    --project-origin "$pigeon_root" \
    --watch "$pigeon_root/backend/src" --watch-non-recursive "$pigeon_root/backend" \
    --filter '/backend/src/**' --filter '/backend/Cargo.toml' \
    --filter '/backend/Cargo.lock' --filter '/backend/build.rs' \
    -- cargo run --quiet --manifest-path "$pigeon_root/backend/Cargo.toml" -- serve "$@"
