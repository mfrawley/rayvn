#!/bin/sh
# Format check, lints (warnings are errors) and tests: what a commit must pass. Run by .githooks/pre-commit.
set -e
cd "$(dirname "$0")/.."
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test --quiet
