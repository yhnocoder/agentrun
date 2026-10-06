#!/bin/sh
set -eu
cd "$(dirname "$0")/.."
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
uv run scripts/check_comment.py
if command -v shellcheck >/dev/null 2>&1; then
  shellcheck scripts/accept/accept.sh scripts/accept/lib/*.sh
else
  echo "shellcheck not found, skipped"
fi
