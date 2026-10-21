# Testing: `rvn test` and `rvn verify`

rayvn does not run Maven's surefire or failsafe plugins. It reads their configuration from `pom.xml` and runs the
same tests itself, with the JUnit Platform Console Launcher.

| Command | Does |
|---|---|
| `rvn test` | Compile `src/main/java`, then `src/test/java`; run the unit tests. |
| `rvn verify` | `rvn test`, then package the jar (shaded if configured), then run the integration tests. |

Both take `--rerun` (ignore the result cache for this run), `--test-cache <dir>` and `--reports-dir <dir>` (see below) and the usual `-f`, `--locked`, `--refresh`.

## Which tests run

As in Maven, taken from the POM:

| | Unit tests (surefire) | Integration tests (failsafe) |
|---|---|---|
| Default includes | `**/Test*.java`, `**/*Test.java`, `**/*Tests.java`, `**/*TestCase.java` | `**/IT*.java`, `**/*IT.java`, `**/*ITCase.java` |
| `<includes>` / `<excludes>` | replace the defaults / remove from the result | same |
| Skipped when | `skip` or `skipTests` interpolates to `true` | `skip`, `skipITs` or `skipExec` interpolates to `true` |
| `<argLine>` | passed to the test JVM, `${...}` interpolated | same |

Only JUnit 5 (Jupiter) is supported. The Console Launcher is fetched at the version of the `junit-platform-engine`
already in your dependencies; add `junit-jupiter` as a test dependency. Options rayvn does not implement
(`groups`, `forkCount`, `systemPropertyVariables`, ...) are printed as warnings, never silently dropped.

## JUnit XML reports

Every run writes JUnit XML where Maven would: `target/surefire-reports/` for unit tests and `target/failsafe-reports/` for integration tests, in each module. CI systems that already read Maven's reports (Jenkins `junit`, GitLab `artifacts:reports:junit`, GitHub test reporters, IDEs) need no change: point them at `**/surefire-reports/TEST-*.xml` as before.

- One file per test JVM: `TEST-junit-jupiter.xml`, or `TEST-junit-jupiter-1.xml`, `-2.xml`, ... when `shards` is set. Each JVM writes into a private directory first, so parallel shards cannot overwrite each other.
- The reports of an earlier run are removed before the next run, so a report never lists tests that no longer exist.
- A result served from the cache starts no JVM, so the XML of the run it stands for is stored beside the cache record (`<group>.<artifact>.<phase>.reports/`) and copied back. The report is there on exactly the runs where the cache works. A record from before reports existed has no XML to copy and is treated as a miss, so the tests run once more.
- `--reports-dir <dir>` writes `<dir>/surefire-reports` and `<dir>/failsafe-reports` instead of `target/`. In a multi-module build every module writes to the same directory, so use it for a single module (`-m`) or leave the default.
- Failed runs keep their reports (that is when you need them) but are never stored in the cache.

The XML is the Console Launcher's own (`--reports-dir`), in the Open Test Reporting-compatible "legacy" format that surefire's tooling reads.

## Result cache

A run that passed is remembered under `target/rayvn-test-cache/` (one small file per project and phase, named `<group>.<artifact>.<unit|integration>.toml`). The next run with identical inputs prints
`unit tests up to date: N passed (cached, ...)` and starts no JVM.

The key is a hash of:

- every compiled main and test class and every copied resource (a comment-only source change does not change the bytes, so it is still a hit),
- the SHA-256 of every jar in `rayvn.lock`,
- the `java -version` banner,
- the JVM arguments and the list of selected classes,
- any files you declare with `inputs` below.

Where it lives, first match wins: `--test-cache <dir>`, the `RAYVN_TEST_CACHE` environment variable, `cache_dir` in `[test]` (relative to the project), then the default above. Several projects can share one directory. In CI, point it at a directory your CI system saves and restores between jobs, for example `RAYVN_TEST_CACHE=$HOME/.cache/rayvn-tests`; the key contains no absolute paths other than a jar fallback for jars without checksums, so a restored record is valid on another machine only if the class files hash the same.

Failures are never cached, and a failing run deletes the old pass. There is no sandbox, so rayvn cannot see a file a
test reads on its own. Declare such files, or leave caching off for that phase.

## `rayvn.toml`

```toml
[test]
cache = true               # unit tests (default true)
cache_integration = false  # integration tests (default false: they usually read files rayvn cannot see)
cache_dir = "build/test-results" # where results are stored (default target/rayvn-test-cache)
shards = 3                 # run the selected classes in 3 JVMs at once (default 1)
inputs = ["test/input/**"] # extra files that can change a result; ant-style globs from the project root
```

A project file overrides the user file (`~/.rayvn/config.toml`) field by field; `inputs` from both are added together.

## Sharding

With `shards = N` the selected classes are sorted, dealt out round-robin into N groups, and each group runs in
its own JVM at the same time. Output lines are prefixed `[1]`, `[2]`, ... The split is stable between runs.
Start-up of a JVM and shared static state in tests are the costs; a few slow classes gain, many fast ones lose.

## How Gradle and Bazel compare

- **Gradle:** the `test` task is up to date when its declared inputs are unchanged, and failed runs are not cached. It forks worker JVMs with `maxParallelForks`. Inputs are whatever plugin authors and you annotate; nothing enforces them.
- **Bazel:** each `java_test` is a cached action. A sandbox makes the declared inputs the only inputs, so an undeclared read fails. `shard_count` splits one target.
- **Reports:** Maven's surefire writes `target/surefire-reports/TEST-*.xml` and rayvn uses the same paths. Gradle writes `build/test-results/test/*.xml` and replays them when the task is up to date from its build cache. Bazel writes one `test.xml` per target under `bazel-testlogs/` and streams it to the build event service. Replay on a cache hit is what Gradle and Bazel do and what rayvn does.
- **rayvn:** Bazel's key and rule that failures are not cached, Gradle's trust in declared inputs (`inputs = [...]`) and no sandbox, so that a plain `pom.xml` keeps working.
