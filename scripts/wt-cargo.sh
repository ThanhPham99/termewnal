#!/usr/bin/env bash
set -euo pipefail

# Run cargo from any git worktree while sharing the main repository's target/ directory
# so external crates are compiled once and incremental/test disk usage stays compact.
if [[ -z "${CARGO_TARGET_DIR:-}" ]]; then
    if common_dir="$(git rev-parse --path-format=absolute --git-common-dir 2>/dev/null)"; then
        export CARGO_TARGET_DIR="$(dirname "$common_dir")/target"
    fi
fi

exec cargo "$@"
