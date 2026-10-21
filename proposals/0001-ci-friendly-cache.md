# 0001: CI-friendly cache

Status: implemented (all four steps), 2026-10-20. Open: the content-addressed store (below). Related, later: the test result cache and its CI location (`docs/testing.md`), and the `--cold` run's cache flag, which is still only `RAYVN_LOCAL_REPO`.
Date: 2026-10-20

`proposals/` records design changes before or as they are made: what we chose, why, and what we left out. Number them in order; don't rewrite history, supersede with a new proposal.

## Problem

rayvn inherited Maven's cache model: one mutable directory (`~/.m2/repository`) holding artifacts and bookkeeping together. That is awkward in CI:

- The cache key is guesswork (hash of `pom.xml`), and the restored directory holds whatever earlier builds fetched, not what this build needs.
- Every job both reads and writes the same directory, so shared caches can race and grow.
- Measuring a cold run takes a recipe (`RAYVN_LOCAL_REPO=$(mktemp -d) …`).
- Bookkeeping is scattered over sidecar files (`.rayvn-misses.tsv`, `*.rayvn-snapshot`, `*.validators`).

## Decision

Compatibility with Maven's `~/.m2` is no longer a goal. Only `settings.xml` (repositories, mirrors, servers, proxies, `localRepository`) stays supported. We keep the Maven directory layout inside the cache for now because it is simple and nothing forces a change; the location and contents are ours to change.

## Steps

1. **`rvn fetch`.** Downloads exactly what `rayvn.lock` lists, in parallel, from the recorded repositories, and verifies every file's SHA-256 (cached files too). It never reads POMs or resolves; a missing or stale lock is an error. In CI the cache key becomes the hash of `rayvn.lock`, so the cache holds only what the build needs.
2. **Read-only cache layers.** `RAYVN_RO_REPO` (or `read_only_repos` in config) lists extra cache directories that are read but never written. Lookup order: writable cache, then read-only layers, then network. Downloads and bookkeeping only touch the writable cache. Use: bake a cache into a CI image or mount a shared one; jobs neither restore nor save it, and concurrent jobs cannot corrupt it. Safe because releases are immutable and the lockfile pins checksums. SNAPSHOTs are never served from a read-only layer.
3. **One metadata index.** `<cache>/.rayvn-index.tsv` replaces `.rayvn-misses.tsv`, `*.rayvn-snapshot` and `*.validators`: one append-only file (miss, snapshot-stamp, validator and download-origin records; later lines win; compacted when mostly stale). Paths in it are relative to the cache, so a cache directory can be moved or restored elsewhere. Old sidecar files are ignored.
4. **`--cold`.** Runs the command against a fresh temporary cache with `--refresh` semantics (ignores the lockfile, the miss cache and read-only layers), prints the cache path, and deletes it afterwards. Refused for `classpath`, whose output would point into the deleted directory.

## Deferred

- **Content-addressed store** (`store/sha256/…`, hardlinked into place, as uv and pnpm do). Dedupes across projects and makes verification trivial, but it is the largest change; revisit once steps 1-3 show what CI actually needs.
- ~~Default cache location~~ — done: the default is now `$XDG_CACHE_HOME/rayvn/repository` (`~/.cache/rayvn/repository`), a location the OS may clear. Existing users re-download once; `localRepository` in `settings.xml` and `RAYVN_LOCAL_REPO` still override.

## Prior art (from memory, unchecked)

Gradle (checksum-keyed cache, `GRADLE_RO_DEP_CACHE`), Cargo (`Cargo.lock`, `cargo fetch`), Go (module cache, `go mod download`), uv and pnpm (content-addressed store, frozen sync / fetch), Bazel (content-addressed repository cache), Coursier (Maven-layout cache keyed by URL, 24 h TTL for changing artifacts).

## What was verified (2026-10-20)

Against a local HTTP server, since no real CI was involved:

- `fetch` without a lockfile fails; into an empty cache it downloads only the locked jars (1 request for 1 artifact, no POM) and verifies them; a second run makes no requests; a jar overwritten in the cache makes it fail with the expected and actual hash.
- A read-only layer supplied via `RAYVN_RO_REPO` satisfied a full `fetch` with 0 requests and nothing written to the writable cache; `classpath` then pointed into the read-only copy.
- `lock --cold` on the demo project ran against a fresh temp directory and removed it afterwards.
- 13 unit tests, including the index file format (expiry, later-wins, round trip, garbage lines).

Not verified: a real CI run, concurrent jobs sharing one read-only layer on a network filesystem, and `fetch` against a large (about 370 artifacts) real lockfile.

## CI recipe

```sh
# cache key: hash of rayvn.lock only
rvn fetch                     # restore ~/.cache/rayvn from the CI cache first
rvn build --locked
```

For prebaked images: run `rvn fetch` while building the image, then in jobs set `RAYVN_RO_REPO=/baked/repository` and `RAYVN_LOCAL_REPO` to an empty scratch directory.
