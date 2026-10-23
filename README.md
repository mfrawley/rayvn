# rayvn (`rvn`)

A fast, Maven-compatible build tool in Rust.

## Purpose

Backwards-compatible with `mvn`, dragged into modernity. Existing projects build from their existing `pom.xml` (and `settings.xml`) with no migration; the tool underneath works the way current tools (uv, Cargo, Gradle, Bazel) do: a lockfile, a fast parallel resolver, a cache that is safe in CI, and no work done twice.

Compatibility is judged by results (same classpath, same jar contents), not by reproducing Maven's internals. Where Maven's model is the obstacle (a mutable shared `~/.m2`, plugins as arbitrary code, a sequential reactor), rayvn keeps the POM as the input and changes what happens behind it. Each design note records the choice, and compares how Gradle and Bazel handle the same problem.

## Usage

```text
rvn <command> [-f pom.xml] [-m module] [-j N] [--locked] [--refresh] [--cold]
```

`rvn --help` lists the commands and the environment variables; `rvn <command> --help` shows that command's flags with examples. A flag that means nothing for a command (`rvn classpath --rerun`) is an error, and a mistyped command gets a suggestion.

| Command | What it does |
|---|---|
| `lock` | Resolve the dependencies and write `rayvn.lock` next to the POM. |
| `fetch` | Download exactly what `rayvn.lock` lists and verify checksums. Never reads POMs. For CI. |
| `resolve` | Print the dependency tree, one line per artifact, indented by depth. |
| `classpath` | Print the classpath as colon-separated jar paths. `--test` includes test scope. |
| `build` | Compile and package the jar into `target/`; shaded if the POM configures `maven-shade-plugin`. Prints the jar path on stdout. |
| `test` | Compile the tests and run the unit tests (surefire settings), skipping the run when nothing changed. Writes JUnit XML to `target/surefire-reports/`. See `docs/testing.md`. |
| `verify` | `test`, then package, then the integration tests (failsafe settings). |
| `stage` | Build and stage all artifacts and checksums (`.sha256`, `.sha1`, `.md5`) into `target/staging/` in Maven repository layout. |
| `deploy` | Build and stage artifacts locally, then upload them via HTTP PUT to a remote Maven repository. |

| Flag | Meaning |
|---|---|
| `-f <path>` | Use this POM instead of `./pom.xml`. The lockfile goes beside it. |
| `-m <module>`, `-pl <modules>` | Multi-module: this module or comma-separated list of modules and what they depend on. See `docs/workspace.md`. |
| `--only` | Build only the selected module, without compiling upstream siblings. |
| `--set-version <ver>` | Override the workspace version across all modules (like `versions:set -DnewVersion=...`). |
| `-am`, `--also-make` | Also build upstream dependencies (default behavior; alias for Maven compatibility). |
| `-j <N>`, `--jobs <N>` | `lock`, `fetch`, `build`, `test`, `verify`, `stage`, `deploy`: work on up to N modules at once (default: one per CPU). A module starts when the modules it depends on are compiled. `-j 1` goes one after another. See `docs/workspace.md`. |
| `--stage-dir <dir>` | `stage`/`deploy`: directory to stage artifacts into (default: `target/staging`). |
| `--dry-run` | `deploy`: build and stage all artifacts locally without uploading. |
| `--skip-existing` | `deploy`: skip files that already exist on the remote repository (HTTP 409 Conflict) instead of failing. |
| `--alt-repo <id::url>` | `deploy`: publish to this repository instead of POM `<distributionManagement>`. |
| `--locked` | Fail if `rayvn.lock` is missing or stale, instead of resolving. |
| `--refresh` | Ignore `rayvn.lock` and the miss cache for this run; the lock is rewritten. |
| `--test-cache <dir>` | `test`/`verify`: store test results here (also `RAYVN_TEST_CACHE`, `[test] cache_dir`). |
| `--rerun` | `test`/`verify`: ignore the test result cache for this run. |
| `--reports-dir <dir>` | `test`/`verify`: write JUnit XML to `<dir>/<artifact>/surefire-reports` and `<dir>/<artifact>/failsafe-reports` instead of `target/`. |
| `--cold` | Use a throwaway cache, deleted afterwards. Implies `--refresh`. Not allowed with `classpath`. |

Progress and summaries go to stderr and results to stdout, so output pipes cleanly.

```sh
rvn lock                          # resolve and write rayvn.lock
rvn build                         # compile and package target/<artifact>-<version>.jar
rvn build -f services/api/pom.xml # a POM elsewhere
rvn fetch                         # CI: restore exactly the locked set
rvn build --locked                # CI: fail rather than re-resolve
rvn test                          # compile src/test/java, run surefire-selected JUnit 5 tests
rvn verify                        # test, package, then run failsafe-selected integration tests
rvn build -j 4                    # a multi-module build, four modules at a time
rvn stage                         # stage all artifacts + checksums into target/staging
rvn deploy --dry-run              # verify full local staging without uploading
rvn deploy --set-version 20261022.1 --alt-repo my-repo::https://repo.example.com/releases
rvn test --rerun                  # run the tests even if their inputs are unchanged
rvn resolve | grep guava          # which version of guava did we get?
java -cp "$(rvn classpath):target/classes" app.Main
rvn lock --cold                   # time a resolve from an empty cache
rvn --version                     # commit, build time and name of this binary
```

Environment: `RAYVN_LOCAL_REPO` (cache location), `RAYVN_RO_REPO` (read-only cache layers), `RAYVN_MAX_PER_HOST` (concurrency limit per host, default 4), `RAYVN_DEPLOY_USER`/`RAYVN_DEPLOY_PASSWORD` (deploy credentials), `RAYVN_DEBUG_HTTP=1` (print each request and status), `RAYVN_NO_BANNER=1` (hide the banner), `RAYVN_TEST_CACHE` (where test results are stored). Routing and credentials: `docs/routing.md`, `docs/credentials.md`.

## Development

`scripts/check.sh` runs what a commit must pass: `cargo fmt --check` (rustfmt defaults), `cargo clippy --all-targets -- -D warnings` and `cargo test`. Run it by hand, or once per clone enable it as a pre-commit hook:

```sh
git config core.hooksPath .githooks
chmod +x .githooks/pre-commit scripts/check.sh
```

`scripts/doc.sh` rebuilds the API docs (`target/doc/rayvn/index.html`) from scratch: it runs `cargo clean --doc` first, so pages for renamed or removed items do not linger.

`scripts/release.sh <patch|minor|major|X.Y.Z>` automates releases: runs checks, updates `Cargo.toml` and `Cargo.lock`, and tags `vX.Y.Z` (`--dry-run` to preview, `--publish` to push and publish).

## Where things are

- `proposals/`: design decisions, numbered, not rewritten after the fact.
- `docs/`: user documentation (`routing.md`, `credentials.md`, `testing.md`, `workspace.md`, `version-ranges.md`, `plugins.md`).
- Code: one crate, `rayvn`, which builds the single binary `rvn`. It is not a library; the module map is at the top of `src/main.rs`, and unit tests sit beside the code (`cargo test`).
