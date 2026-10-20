# Routing: `[[route]]`

By default, Maven asks every repository for every artifact, in order, until one answers. With 10 or more repositories (typical in a corporate `settings.xml` plus a few `<repositories>` in POMs) that means most requests are 404s. A `[[route]]` tells raven which repositories can have which groupIds, so it asks only those.

Routes are optional. With none, raven behaves like Maven.

## Where routes go

In either file; both are read and their routes are combined:

- `~/.raven/config.toml` (yours, all projects)
- `raven.toml` next to the `pom.xml` (committed with the project)

```toml
[[route]]
repo   = "github"            # a repository id
groups = ["com.example", "com.example.*"]
```

`repo` must be the id of a repository raven knows about. That means one from the project POM (or its on-disk parents), `settings.xml`, a `[[repo]]` in raven config, or `central`. If a `<mirror>` in `settings.xml` replaces a repository, use the mirror's id. An unknown id is an error at startup (`route refers to unknown repository '...'`).

## Matching a groupId

Each entry in `groups` is one of:

| Pattern | Matches | Specificity |
|---|---|---|
| `org.example.contrib` | exactly that groupId | highest |
| `org.example.*` | `org.example` and anything under `org.example.` (not `org.examplefoo`) | `2 + length of the prefix`, so a longer prefix wins |
| `*` | every groupId | lowest |

For an artifact's groupId, raven finds the most specific match across all routes. If several routes tie at that level, all of their repositories are asked, in the order the routes appear. If no route matches, all repositories are asked, in order: the project's, then `settings.xml`'s, then raven config's, then Central.

## The rule to remember

**Once any route matches a groupId, only the routed repositories are asked, with no fallback to the others.** If the routed repository doesn't have the artifact, resolution fails with "not found in any repository".

This is what makes routing fast, and also how it can break a build. So:

- Add a `*` route as the catch-all for everything else, usually `central`.
- A repository is only known to routes if it is declared in the project, in `settings.xml` or in a `[[repo]]`. Repositories that only appear in a dependency's POM (for example `xypron` for `org.gnu.glpk`) are added while resolving, so a route can't name them. A `*` route would also stop raven from ever asking them. If you use `*`, declare such repos in a `[[repo]]` and give them their own route.

## Example (illustrative)

```toml
# private artifacts: only GitHub Packages, never Central
[[route]]
repo   = "github"
groups = ["com.example", "com.example.*"]

[[route]]
repo   = "github2"
groups = ["org.example.*"]

# the declared extra repository
[[repo]]
id  = "xypron"
url = "https://www.xypron.de/repository"

[[route]]
repo   = "xypron"
groups = ["org.gnu.glpk"]

# everything else
[[route]]
repo   = "central"
groups = ["*"]
```

Adjust the ids and groups to your own repositories. This example has not been run against a real project; check it with `RAVEN_DEBUG_HTTP=1` (prints each request and its status) and look at the "avoided" count in the summary line.

## Related behaviour

- **Miss cache** (see below).
- **Lockfile.** `raven.lock` records the repository each artifact came from. Locked runs fetch straight from it, so routes only matter when resolving (`rvn lock`, or when `raven.lock` is stale).
- **Credentials.** Routing doesn't change them. A routed repository still uses its own credentials from `raven.toml`, `~/.raven/config.toml` or `settings.xml`.
- **Measured effect.** On the small demo project, cold resolution dropped from 61 requests to 22. On a larger real project (about 370 artifacts) with no routes, a cold resolve made 5019 requests (2207 avoided by the miss cache) and took 115 s; I haven't yet re-measured it with routes.

## Hints: soft routes

A `[[hint]]` is a route that only reorders. When no `[[route]]` matches a groupId, raven asks, in order: repositories named by matching hints (most specific first), then Central, then everything else in Maven order. Nothing is excluded, so a wrong hint costs a request, never a build.

raven ships a suggestion list for well-known public repositories (`data/default-hints.toml`, compiled in). Add your own, or override the shipped ones, in `~/.raven/config.toml` or `raven.toml`. Your hints win ties against shipped ones.

```toml
[[hint]]
url    = "https://repo.osgeo.org/repository/release"   # or repo = "<id>"
groups = ["org.geotools.*"]

default_hints = false     # ignore the shipped list
central_second = false    # don't move Central ahead of the unhinted repositories
```

A hint naming a repository that isn't in use is ignored; hints never add repositories. Use `[[route]]` for private artifacts, since a hint doesn't stop the other repositories from being asked.

Measured on a cold resolve of the same larger project (real network, empty cache): no hints 7367 requests, 177 s; shipped hints alone 4692 requests, 155 s; hints plus Central second 1471 requests, 54 s.

## Miss cache and forcing a cold run

raven remembers every 404 so it never asks the same repository for the same missing path twice. It is keyed by repository, whether the request was authenticated (private repositories answer 404 to anonymous requests), and the path. The summary counts these skips, together with routing, as `N http requests (M avoided by routes/miss cache)`.

- **Where:** the `miss` records in `<local repository>/.raven-index.tsv`, beside the artifacts (`$XDG_CACHE_HOME/raven/repository`, i.e. `~/.cache/raven/repository` by default; override with `RAVEN_LOCAL_REPO` or `localRepository` in `settings.xml`). See `proposals/0001-ci-friendly-cache.md`.
- **Expiry:** a miss is forgotten after 24 hours. Change it with `miss_ttl_hours = N` in `raven.toml` or `~/.raven/config.toml`; `0` turns the miss cache off. Found artifacts are never re-checked, since releases are immutable.
- **Old files:** `~/.raven/misses.tsv` and `.raven-misses.tsv` from earlier versions are no longer read and can be deleted.

To force a cold run:

| Goal | How |
|---|---|
| Re-resolve and re-ask repositories that answered 404, keep downloaded artifacts | `rvn <cmd> --refresh` (ignores `raven.lock` and the miss cache for that run; the lockfile is rewritten) |
| Fully cold, nothing kept | `rvn lock --cold` (temporary cache, deleted afterwards; the summary line has the request count and time). Also works with `rvn build --cold` |
| Forget misses only | Delete `<local repository>/.raven-index.tsv` (also forgets SNAPSHOT stamps and validators) |

## SNAPSHOTs and conditional requests

Releases are immutable, so a cached release is never rechecked. SNAPSHOTs change, so raven revalidates them:

- **What is tracked.** When raven downloads a SNAPSHOT it records a `snap` entry in `.raven-index.tsv` (repository, timestamped version, last check). Files raven did not download, for example `mvn install` output, have no stamp and are never touched.
- **When it rechecks.** Once the stamp is older than `snapshot_ttl_hours` (default 24, `0` = every run), or on any command with `--refresh`.
- **How.** It re-reads that repository's `maven-metadata.xml` with `If-None-Match` / `If-Modified-Since`, using the `ETag` / `Last-Modified` stored as `val` entries in `.raven-index.tsv`. A `304 Not Modified` costs one request and no body; a changed metadata file with a newer timestamped version downloads just that version. The metadata is fetched at most once per run.
- **Verified** against a local server: cold 3 requests; inside the window 0; `--refresh` unchanged 1 (`304`); `--refresh` after a new publish 3; same results with `snapshot_ttl_hours = 0`.
- **Not done.** Release artifacts, POMs and 404s don't use validators (nothing to revalidate; 404s use `miss_ttl_hours`). Server-side `Cache-Control` is ignored, and repositories that don't send `ETag` or `Last-Modified` get a full metadata download each recheck.

## CI: fetch, read-only layers, cold runs

Design and rationale: `proposals/0001-ci-friendly-cache.md`.

- **`rvn fetch`** downloads exactly the artifacts in `raven.lock` from the repositories recorded there, in parallel, then verifies every file's SHA-256 (cached files too; SNAPSHOTs and entries without a checksum are skipped). It never reads POMs. A missing or stale lock is an error (`rvn lock` first). Use the hash of `raven.lock` as the CI cache key.
- **Read-only layers.** `RAVEN_RO_REPO=/a:/b` or `read_only_repos = ["/a"]` in `raven.toml`/`config.toml` adds cache directories that are read but never written. Lookup: writable cache, then read-only layers, then network. SNAPSHOTs are never taken from a layer. Intended for caches baked into a CI image or mounted shared.
- **`--cold`** runs against a throwaway cache (implies `--refresh`, ignores read-only layers) and deletes it afterwards; not allowed with `classpath`.
- **Bookkeeping** (404s, SNAPSHOT stamps, ETags) is the single file `<cache>/.raven-index.tsv`; its paths are relative to the cache, so a restored or moved cache keeps working.
