#!/bin/sh
# Rebuild the API docs from scratch so pages for removed items do not linger in target/doc.
set -e
cd "$(dirname "$0")/.."
cargo clean --doc
cargo doc --no-deps --document-private-items "$@"
echo "docs: target/doc/rayvn/index.html"
