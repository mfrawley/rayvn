# 0006: Cache inspection (`rvn info`), cache sizing, and tiered cleaning

Status: proposed, 2026-10-22
Date: 2026-10-22

## Problem

rayvn manages three distinct cache layers:
1. **Downloaded repository cache:** Immutable release jars and metadata index (`~/.cache/rayvn/repository`, or overridden via `RAYVN_LOCAL_REPO` or `settings.xml`).
2. **Test result cache:** Keyed test records (`.rayvn-test-cache` or `RAYVN_TEST_CACHE`).
3. **Local build outputs:** `target/classes`, `target/*.jar`, and `target/staging`.

Currently, developers and CI engineers have no built-in way to answer basic questions:
- *Where is rayvn actually storing my downloaded artifacts right now?* (Did `RAYVN_LOCAL_REPO`, `settings.xml <localRepository>`, or the OS default win?)
- *How much disk space has rayvn consumed?*
- *How do I inspect or wipe the test cache without deleting downloaded dependencies?*

In Maven, developers manually poke around in `~/.m2`. In Gradle, cache discovery is notoriously opaque. In contrast, tools like Bazel (`bazel info`) and uv (`uv cache dir`, `uv cache prune`) treat cache transparency and sizing as first-class CLI capabilities.

## Proposed Design

### Part 1: `rvn info [KEY]` (Bazel-Style Transparency)

A fast query command that prints active workspace and cache paths, their resolution sources, and disk usage.

```sh
rvn info [KEY]
```

#### Bare `rvn info` (Human-Readable Overview)

Running `rvn info` with no arguments prints an aligned table:

```text
cache-dir:        /Users/alice/.cache/rayvn/repository (from $XDG_CACHE_HOME)
cache-size:       1.8 GB (3,412 files)
read-only-repos:  none
settings-file:    /Users/alice/.m2/settings.xml
user-config:      none
project-config:   /Users/alice/work/project/rayvn.toml
test-cache-dir:   /Users/alice/work/project/.rayvn-test-cache
test-cache-size:  4.2 MB (18 records)
staging-dir:      /Users/alice/work/project/target/staging
max-per-host:     4
```

#### Scriptable Single-Key Queries (`rvn info <key>`)

Querying a specific key outputs strictly that value to stdout (no labels or trailing commentary), making it composable with standard Unix tools:

```sh
# Navigate directly to the repository cache
cd "$(rvn info cache-dir)"

# Inspect disk breakdown
du -sh "$(rvn info cache-dir)"/*

# CI cache key or artifact verification
tar -czf cache.tar.gz -C "$(rvn info cache-dir)" .
```

#### Supported Keys

| Key | Output |
|---|---|
| `cache-dir` | Resolved absolute path of the writable repository cache. |
| `cache-size` | Human-readable disk usage of the repository cache (e.g. `1.8 GB`). |
| `read-only-repos` | Colon-separated list of read-only cache layers, or `none`. |
| `settings-file` | Absolute path to `~/.m2/settings.xml`, or `none`. |
| `user-config` | Absolute path to `~/.rayvn/config.toml`, or `none`. |
| `project-config` | Absolute path to `./rayvn.toml`, or `none`. |
| `test-cache-dir` | Resolved path of the test result cache. |
| `test-cache-size` | Human-readable size of the test cache. |
| `staging-dir` | Path to the local staging directory. |
| `max-per-host` | Effective concurrency limit per host. |

---

### Part 2: Tiered Cache Cleaning (`rvn clean`)

Following Bazel's separation of concerns, cleaning build outputs should never force re-downloading gigabytes of third-party dependencies over the network.

```sh
rvn clean [--test-cache] [--cache] [--all]
```

#### Cleaning Tiers

1. **`rvn clean` (Default: Project build outputs only)**
   - Deletes `target/` across all reactor modules.
   - Leaves the test cache and downloaded dependency cache intact.
   - Fast, matches `mvn clean` and `cargo clean`.

2. **`rvn clean --test-cache`**
   - Deletes `.rayvn-test-cache` for the workspace.
   - Forces all tests to re-run on the next invocation without wiping compiled classes or downloaded jars.

3. **`rvn clean --all` (The Bazel `clean --expunge` equivalent)**
   - Deletes `target/` across all modules AND wipes `.rayvn-test-cache`.
   - Leaves the global downloaded dependency cache intact.

4. **`rvn clean --cache` (The Nuclear Option)**
   - Wipes the shared repository download cache (`rvn info cache-dir`).
   - Requires `--force` (or interactive TTY confirmation) if not run in CI, to prevent accidental multi-gigabyte deletions.

---

### Part 3: Isolated Ephemeral Runs (`--cold`)

rayvn already provides `--cold`:
```sh
rvn build --cold
```
This runs the command against an isolated temporary directory with `--refresh` semantics and automatically deletes it when finished. It provides the gold standard for measuring cold build performance in CI without mutating the user's persistent cache.

## Prior Art

- **Bazel:** `bazel info [key]`, `bazel clean`, `bazel clean --expunge`. Bazel strictly isolates project outputs from the repository cache (`repository_cache`), which must be queried via `bazel info` and wiped separately.
- **uv:** `uv cache dir`, `uv cache prune`, `uv cache clean`.
- **Go:** `go env GOCACHE`, `go clean -cache`, `go clean -modcache`.
- **Cargo:** `cargo clean`, `cargo clean --doc`, `cargo-cache` plugin for global cache inspection.
