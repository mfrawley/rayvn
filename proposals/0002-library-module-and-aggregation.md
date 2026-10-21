# 0002: Building a full-toolchain library module, then `<modules>` aggregation

Status: partly implemented, 2026-10-21

- Part 1, steps 1 to 4 (run it, `rvn test`, `rvn verify`, shade parity): implemented, with a test result cache and sharding (`docs/testing.md`).
- Part 1, step 5 (convergence check): open. It will apply only where the POM binds the enforcer's `enforce` goal in an `<execution>`.
- Part 2, steps 1 to 3 (`<modules>`, ordering, sibling resolution): implemented (`docs/workspace.md`). Step 4 (parallel module builds): open.
- Not started: `[workspace] versions` (see below). The bump design moved to `0003-bump.md`.

Purpose of the project: see `README.md` (Maven-compatible, modernised). Every item below says how Maven, Gradle and Bazel handle it, and what rayvn takes.

## Why these two, in this order

The motivating case is a typical multi-module repository: a root `pom.xml` with `packaging=pom` listing about 9 modules, one of which nests three more levels of aggregators over about 19 leaf projects.

1. **The core library module** is what the others are built on and the only one using the full Maven toolchain (surefire, failsafe, enforcer, shade, compiler). Most other modules depend on a *published* build of it, pinned by timestamp version.
2. **Aggregation** is simple here: only one sibling depends on the library at the reactor version (`1.0-SNAPSHOT`); the rest pin published versions from a package registry. So sibling-output resolution is rarely needed.

Not verified yet: that `rvn build` currently fails on the library module and how. The first step below is to run it and record what actually breaks.

## Part 1: the library module

What its POM uses, and the gap in rayvn today:

| Maven feature | In the library module | rayvn today |
|---|---|---|
| Compile main | `source/target 25`, `encoding`, `fork`, `maxmem`, `Xmaxwarns/Xmaxerrs` | `javac` with `--release`; fork/memory are irrelevant (rayvn calls `javac` directly); `-proc:none` is hardcoded (check whether any dependency needs an annotation processor) |
| Compile and run tests | surefire: `includes **/Test*.java, *Test.java, *Tests.java, *TestCase.java`; `excludes *IT.java, *ITCase.java`; `useModulePath=false`; 35 test files | none (STATUS row 9) |
| Integration tests | failsafe: `*IT.java`, `*ITCase.java`; `skipITs` property (default `false` in the root POM); goals `integration-test` + `verify` | none |
| Dependency convergence | enforcer `<dependencyConvergence/>` | nothing; fails no build |
| Fat jar | shade: `finalName ${artifactId}-${version}-fat`, `shadedArtifactAttached` + classifier `shaded`, services merge, manifest entries, signature excludes | services, manifest and signature excludes work; `finalName` and `shadedArtifactAttached` are reported as ignored |

### Steps

1. **Run it.** `rvn lock` and `rvn build` on the library module; list the real failures before building anything.
2. **`rvn test`.** Compile `src/test/java` (+ `src/test/resources`) against the test classpath, run JUnit through the JUnit Platform Console Launcher, honour surefire `includes`/`excludes` and `skipTests`. Works on the plain classpath (`useModulePath=false`). No forking per class by default.
3. **`rvn verify`** (integration tests): same, with failsafe's patterns, honouring `skipITs`. Runs after `package`, as in Maven.
4. **Shade parity.** Honour `finalName` and `shadedArtifactAttached`/`shadedClassifierName` so the output file names match Maven's.
5. **Convergence check (open).** Report `dependencyConvergence` violations from the resolved graph, as a warning or an error when the POM enforces it. Needs no plugin code.

### Results of running it (2026-10-21)

- `rvn lock` and `rvn build` worked at once; the fat jar then differed from Maven's in three ways, all fixed or understood:
  - shade `finalName` / `shadedArtifactAttached` naming: done (step 4).
  - Scope mediation: an artifact reached by several paths must take the broadest scope (Maven), not the first path's. Fixed in `resolve.rs` (`widen_scopes`); `apiguardian-api` was wrongly `test` and missing from the fat jar.
  - `test-jar` dependencies were deduplicated against the plain jar of the same artifact (classifier lost); fixed.
- Remaining fat-jar diff: the project's own `META-INF/maven/<g>/<a>/pom.{properties,xml}` and directory entries. Harmless.
- `rvn test` and `rvn verify` (steps 2, 3): done. 15 unit tests and 5 integration tests pass. Step 5 (convergence) is open.

### Decision: no plugin execution

rayvn does not run Maven plugins (no JVM plugin host, no Mojo API). It reads a plugin's configuration from the POM and implements the behaviour natively for a short list of well-known plugins (compiler, shade, surefire, failsafe, enforcer's convergence rule, jar). Anything else is reported as ignored, as shade options already are. This keeps rayvn fast and single-binary, and is the main compatibility trade-off.

### How Gradle and Bazel handle it (from memory, unchecked)

- **Tests.** Gradle's `test` task is built in: it discovers test classes by scanning compiled output and runs them in a forked worker JVM (or several, `maxParallelForks`), with results cached by input hash. Bazel has `java_test` per target; each runs in a sandbox, in parallel, with the result cached by the hash of its inputs, so an unchanged test is never re-run. rayvn takes: run tests in a separate JVM and skip when nothing changed (later, via the same input hash `rayvn.lock` already gives us). It declines Bazel's per-target granularity, which Maven projects don't have.
- **Integration tests.** Gradle uses a separate source set and task (the `jvm-test-suite` plugin); Bazel uses a separate target tagged `integration`. Both treat it as a distinct build unit, not a naming convention. rayvn follows Maven's `*IT` naming because that is what the POM says, but runs them as a separate step, like both.
- **Plugins.** Gradle plugins are code, in-process, with a typed task API; Bazel rules are Starlark, run by the build engine, and actions must declare their inputs and outputs. Maven plugins are arbitrary JVM code with side effects. rayvn's native implementations are closest to Bazel's idea (a fixed, known action with declared inputs), and that is why executing unknown plugins is out of scope.
- **Convergence.** Gradle resolves one version per module by default (highest wins) and offers `failOnVersionConflict()`; Bazel's `rules_jvm_external` pins one version per artifact in a lock file (`maven_install.json`). rayvn already has the lockfile; the check just reports where the graph needed mediation.

## Part 2: `<modules>` aggregation

### Steps

1. Follow `<modules>` recursively from a `pom`-packaging root; accept `pom` packaging for aggregators (build nothing, only recurse).
2. Build children in an order consistent with their dependencies on each other.
3. Resolve a sibling at the reactor version (`lib:1.0-SNAPSHOT`) from its build output, not from a repository.
4. Build independent modules in parallel (open).

One `rayvn.lock` per module, as now; no root lockfile until we need one.

### Results (2026-10-21)

Steps 1 to 3 are done (`src/reactor.rs`, `docs/workspace.md`); step 4 is not: modules build one after another. A sibling is matched on exact coordinates and read from `target/classes`; the lock records it as `workspace = true`. Checked on a two-module demo, including `fetch`, `--locked` and a shaded app containing its sibling, and on matsim-moia (single module, unchanged).

### Results on a real workspace (2026-10-21)

A whole-checkout build from the root POM of a 40-POM repository: 23 modules built, `rvn build` 35.6 s (warm cache) after a 4 min 44 s cold `rvn lock`, exit 0. Getting there needed four changes, each found by the run:

- `[workspace] exclude = [...]` in `rayvn.toml`: the checkout has an `archive` aggregator (legacy and GUI code) that is not part of this build. Documented in `docs/workspace.md`.
- Profile activation `<property><name>x</name><value>!true</value></property>` now holds when `x` is unset. JavaFX picks its platform jar this way (`${javafx.platform}`), so any JavaFX dependency failed before.
- `<relocation>` is followed (`org.lz4:lz4-java:1.8.1` moved to `at.yawk.lz4`), with a warning. Only a node that wins mediation is loaded, so a losing candidate can never fail on its POM.
- Shade `<filters>` with `*:*` matched nothing (the pattern was turned into a glob that cannot cross `:`), leaving `.SF`/`.RSA` signature files in fat jars.

Jar diff against Maven (same commit, all 23 modules, 26 jars; `mvn package` 2:29, `rvn build` 36 s with warm caches). After the fixes below 22 jars are identical entry for entry, apart from the manifest and `META-INF/maven`:

- The `maven-compiler-plugin` `<source>`/`<target>`/`<release>` configuration was ignored, so classes came out at the JDK's own level (class version 69 instead of 65). It is now read, beats the `maven.compiler.*` properties, and an unresolved `${...}` counts as unset, as in Maven.
- Merged `META-INF/services` files: Maven keeps an ordered set of lines. Shade 3.4.0 and older also keep every repeated comment and blank line; 3.4.1 and newer do not. rayvn follows the plugin's `<version>`.
- Resources: Maven never copies `.gitignore`, `.DS_Store`, editor backups and the like (plexus default excludes).

What is left is not rayvn: Maven writes empty files as four NUL bytes; and one module resolved a different `analysis-0.4.7.jar` because the copy in `~/.m2` does not match its own `.sha1`. Gradle (`reproducibleFileOrder`, `preserveFileTimestamps = false`) and Bazel (fixed timestamps, sorted entries by default) make jars reproducible; rayvn now does too: every entry has the zip epoch as its time and mode 0644, entries are sorted, and `target/classes` is emptied before each compile so a deleted source or resource cannot linger. Two builds of all 23 modules, seconds apart, give identical jars.

Not supported: the `protoc` and `exec` plugins (not run). No validated module needs them yet (`docs/plugins.md`): `routing-map-conversion` has its protoc plugin commented out, and `aws-infra` declares `exec-maven-plugin` only for a manual `mvn exec:java`, bound to no phase. Left out on purpose: `archive`, which holds legacy and GUI code.

### Versions across modules

Per-module resolution means modules may differ; they conflict only where both reach one classpath, and the JVM's flat namespace allows one version there. Planned, opt-in:

```toml
[workspace]
versions = "per-module"   # default
# "check":   fail if modules resolve different versions of an artifact
# "unified": highest wins, every module re-resolved with it imposed
allow = ["org.matsim:matsim"]
```

`check` first. Gradle: one version per configuration by default, `failOnVersionConflict()`, platforms for alignment. Bazel: one version per artifact is the rule (`maven_install` per repo to opt out). uv: one lock for the workspace, resolved together. Nix: separate store paths, which does not help on a single classpath.

### Tests across the workspace

`rvn test` at the root of the transport-modeling workspace runs the 3 modules that have tests (matsim-moia 15, routing-map-conversion 4, 21 modules have none) and reports a per-module table; a second run is served from the test cache (28.8 s total, mostly resolving and compiling). Fixed on the way: the test JVM runs in the module directory, so a relative classpath from a root-level `-f pom.xml` failed with `ClassNotFoundException`; entries are now absolute.

### Version ranges

`aws-infra` (constructs `[10.0.0,11.0.0)`) now locks: 10.8.1, the highest 10.x listed, and 71 artifacts. It builds with the rest of the workspace (it was excluded earlier only because of the range; the `exec` plugin it declares is bound to no phase and is used only by hand). See `docs/version-ranges.md`.

### Bumping a dependency across modules (0-day response)

Moved to its own proposal: `0003-bump.md`.

## Deferred

- Skipping unchanged modules (Gradle's up-to-date checks, Bazel's action cache). Natural once tests exist; needs a hash of sources + classpath + config.
- `-P` / `-D` profile activation, shade relocations and `artifactSet` (STATUS next steps).
- `install` / `deploy`, protobuf generation (one module uses `protoc-jar-maven-plugin`; its executions appear to be commented out in its POM, which is unverified).
