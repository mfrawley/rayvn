#!/bin/sh
# Release script for rayvn.
# Bumps version (patch, minor, major, or explicit X.Y.Z), updates Cargo.lock,
# runs pre-flight checks, and creates git release commit and tag.
#
# Usage:
#   ./scripts/release.sh <patch|minor|major|X.Y.Z> [--dry-run] [--publish]

set -e

if [ -z "$1" ]; then
    echo "Usage: $0 <patch|minor|major|X.Y.Z> [--dry-run] [--publish]"
    exit 1
fi

BUMP_TYPE="$1"
DRY_RUN=0
PUBLISH=0

for arg in "$@"; do
    case "$arg" in
        --dry-run) DRY_RUN=1 ;;
        --publish) PUBLISH=1 ;;
    esac
done

cd "$(dirname "$0")/.."

echo "==> Running pre-flight checks..."

# Check clean working directory
if [ -n "$(git status --porcelain)" ]; then
    if [ "$DRY_RUN" -eq 1 ]; then
        echo "Warning: Working directory is not clean (continuing due to --dry-run)." >&2
    else
        echo "Error: Working directory is not clean. Commit or stash changes first." >&2
        exit 1
    fi
fi

# Check branch
BRANCH=$(git rev-parse --abbrev-ref HEAD 2>/dev/null || echo "unknown")
if [ "$BRANCH" != "main" ]; then
    echo "Warning: Current branch is '$BRANCH' (expected 'main')." >&2
fi

# Format, clippy, test
echo "==> Running format, lint, and test suite..."
./scripts/check.sh

# Cargo dry run packaging check
echo "==> Verifying cargo package manifest and files..."
if [ -n "$(git status --porcelain)" ]; then
    cargo publish --dry-run --allow-dirty
else
    cargo publish --dry-run
fi

# Get current version from Cargo.toml
CURRENT_VERSION=$(python3 -c '
import tomllib
with open("Cargo.toml", "rb") as f:
    data = tomllib.load(f)
print(data["package"]["version"])
')

# Compute new version based on bump type
NEW_VERSION=$(python3 -c '
import sys

current = "'"$CURRENT_VERSION"'"
bump = "'"$BUMP_TYPE"'"

parts = [int(p) for p in current.split(".")]
if len(parts) != 3:
    sys.exit(f"Invalid current version format: {current}")

major, minor, patch = parts

if bump == "patch":
    patch += 1
elif bump == "minor":
    minor += 1
    patch = 0
elif bump == "major":
    major += 1
    minor = 0
    patch = 0
else:
    v_clean = bump.lstrip("v")
    v_parts = v_clean.split(".")
    if len(v_parts) == 3 and all(p.isdigit() for p in v_parts):
        print(v_clean)
        sys.exit(0)
    else:
        sys.exit(f"Invalid bump type or version string: '\''{bump}'\''. Use patch, minor, major, or X.Y.Z")

print(f"{major}.{minor}.{patch}")
')

echo "==> Bump target: $CURRENT_VERSION -> $NEW_VERSION"

if [ "$DRY_RUN" -eq 1 ]; then
    echo "==> [--dry-run] Pre-flight passed. Release v$NEW_VERSION ready to be created."
    exit 0
fi

# Update Cargo.toml
python3 -c '
import re

new_ver = "'"$NEW_VERSION"'"
with open("Cargo.toml", "r") as f:
    content = f.read()

updated = re.sub(r"(\[package\][\s\S]*?\bversion\s*=\s*)\"[^\"]+\"", f"\\1\"{new_ver}\"", content, count=1)

with open("Cargo.toml", "w") as f:
    f.write(updated)
'

# Sync Cargo.lock
echo "==> Updating Cargo.lock..."
cargo check --quiet

# Git commit & tag
echo "==> Creating release commit and tag..."
git add Cargo.toml Cargo.lock
git commit -m "release: v$NEW_VERSION"
git tag -a "v$NEW_VERSION" -m "Release v$NEW_VERSION"

echo ""
echo "======================================================="
echo " Release v$NEW_VERSION prepared and tagged successfully!"
echo "======================================================="

if [ "$PUBLISH" -eq 1 ]; then
    echo "==> Pushing commit and tags to origin..."
    git push origin main --tags
    echo "==> Publishing to crates.io..."
    cargo publish
    echo "==> Successfully published v$NEW_VERSION!"
else
    echo " To publish this release, execute:"
    echo "   git push origin main --tags"
    echo "   cargo publish"
    echo "======================================================="
fi
