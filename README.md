# rayvn (`rvn`)

A fast, Maven-compatible build tool in Rust.

## Purpose

Backwards-compatible with `mvn`, dragged into modernity. Existing projects build from their existing `pom.xml` (and `settings.xml`) with no migration; the tool underneath works the way current tools (uv, Cargo, Gradle, Bazel) do: a lockfile, a fast parallel resolver, a cache that is safe in CI, and no work done twice.

Compatibility is judged by results (same classpath, same jar contents), not by reproducing Maven's internals. Where Maven's model is the obstacle (a mutable shared `~/.m2`, plugins as arbitrary code, a sequential reactor), rayvn keeps the POM as the input and changes what happens behind it. Each design note records the choice, and compares how Gradle and Bazel handle the same problem.

## Usage

```text
rvn <command> [-f pom.xml] [-m module] [--locked] [--refresh] [--cold]
```

| Command | What it does |
|---|---|
| `lock` | Resolve the dependencies and write `rayvn.lock` next to the POM. |
| `fetch` | Download exactly what `rayvn.lock` lists and verify checksums. Never reads POMs. For CI. |
| `resolve` | Print the dependency tree, one line per artifact, indented by depth. |
| `classpath` | Print the classpath as colon-separated jar paths. `--test` includes test scope. |
| `build` | Compile and package the jar into `target/`; shaded if the POM configures `maven-shade-plugin`. Prints the jar path on stdout. |
| `test` | Compile the tests and run the unit tests (surefire settings), skipping the run when nothing changed. See `docs/testing.md`. |
| `verify` | `test`, then package, then the integration tests (failsafe settings). |

| Flag | Meaning |
|---|---|
| `-f <path>` | Use this POM instead of `./pom.xml`. The lockfile goes beside it. |
| `-m <module>` | Multi-module: this module and what it depends on (only this one for `lock`, `fetch`, `resolve`, `classpath`). See `docs/workspace.md`. |
| `--locked` | Fail if `rayvn.lock` is missing or stale, instead of resolving. |
| `--refresh` | Ignore `rayvn.lock` and the miss cache for this run; the lock is rewritten. |
| `--test-cache <dir>` | `test`/`verify`: store test results here (also `RAYVN_TEST_CACHE`, `[test] cache_dir`). |
| `--rerun` | `test`/`verify`: ignore the test result cache for this run. |
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
rvn test --rerun                  # run the tests even if their inputs are unchanged
rvn resolve | grep guava          # which version of guava did we get?
java -cp "$(rvn classpath):target/classes" app.Main
rvn lock --cold                   # time a resolve from an empty cache
rvn --version                     # commit, build time and name of this binary
```

Environment: `RAYVN_LOCAL_REPO` (cache location), `RAYVN_RO_REPO` (read-only cache layers), `RAYVN_DEBUG_HTTP=1` (print each request and status), `RAYVN_NO_BANNER=1` (hide the banner), `RAYVN_TEST_CACHE` (where test results are stored). Routing and credentials: `docs/routing.md`, `docs/credentials.md`.

## Development

`scripts/check.sh` runs what a commit must pass: `cargo fmt --check` (rustfmt defaults), `cargo clippy --all-targets -- -D warnings` and `cargo test`. Run it by hand, or once per clone enable it as a pre-commit hook:

```sh
git config core.hooksPath .githooks
chmod +x .githooks/pre-commit scripts/check.sh
```

## Where things are

- `proposals/`: design decisions, numbered, not rewritten after the fact.
- `docs/`: user documentation (`routing.md`, `credentials.md`, `testing.md`, `workspace.md`, `version-ranges.md`, `plugins.md`).
- API docs: `cargo doc --no-deps --open`. Examples in them run under `cargo test`.
