# 0002: Building a full-toolchain library module, then `<modules>` aggregation

Status: proposed, 2026-10-21

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
5. **Convergence check.** Report `dependencyConvergence` violations from the resolved graph, as a warning or an error when the POM enforces it. Needs no plugin code.

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
4. Build independent modules in parallel.

One `rayvn.lock` per module, as now; no root lockfile until we need one.

### Results (2026-10-21)

Steps 1 to 3 are done (`src/reactor.rs`, `docs/workspace.md`); step 4 is not: modules build one after another. A sibling is matched on exact coordinates and read from `target/classes`; the lock records it as `workspace = true`. Checked on a two-module demo, including `fetch`, `--locked` and a shaded app containing its sibling, and on matsim-moia (single module, unchanged).

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

### Bumping a dependency across modules (0-day response)

Goal: one command that raises a library everywhere it is declared, for a person or a bot, with a result that can be reviewed and merged.

```sh
rvn bump org.apache.logging.log4j:log4j-core 2.17.1 [--dry-run] [--json] [--only-direct]
```

Design, not built:

1. **Find** every place a module's version comes from, per module: a literal `<version>`, a `${property}` (change the property, once), a `<dependencyManagement>` entry (change that), an imported BOM (cannot be bumped; say so and suggest an override entry). Transitive-only use is reported, and handled by adding a managed entry only with `--pin-transitive`.
2. **Edit in place**, touching only the version text (comments and layout kept), never rewriting the XML.
3. **Re-lock** each changed module and every module that depends on one (the sibling graph is known), so the diff is POMs plus `rayvn.lock`.
4. **Report**: human table by default; `--json` lists module, file, kind of edit (literal, property, managed) and old and new version. Exit codes: 0 changed, 3 nothing to change, 1 error; `--dry-run` writes nothing and uses the same codes.
5. **Verify**: with `--test`, run the affected modules' tests (the test cache keeps this cheap).
6. Later: `rvn audit` to list locked artifacts against an advisory feed, so the bump can be driven by it (`--from-advisory GHSA-...`), and a minimum-age option for the target version.

Gradle: a version catalog or platform holds the version once; Renovate or Dependabot edit it, and `dependencyInsight` shows who pulls a library. Bazel: versions live in `maven_install` / `MODULE.bazel`, and `bazel run @maven//:pin` regenerates the lock. Maven: `versions:use-dep-version`, `versions:set-property`, `versions:update-properties` edit POMs, but do not know about lock files or which modules are affected. rayvn's edge is that the module graph and the lock are already known, so one command can do edit, re-lock and report.

### How Gradle and Bazel handle it (from memory, unchecked)

- **Gradle** lists projects in `settings.gradle` and builds a task graph over all of them: `:core:jar` before `:app:compileJava`. Independent tasks run in parallel; up-to-date tasks are skipped. A sibling is a `project(":core")` dependency.
- **Bazel** has no module list: every directory with a `BUILD` file is a package, targets name their dependencies by label (`//core:lib`), and the graph is the whole build. Sibling and external dependencies use the same mechanism.
- **Maven's reactor** sorts modules into a linear order and builds sequentially (`-T` adds module-level parallelism). rayvn takes Gradle's idea: build a graph, run what is independent in parallel. It keeps Maven's input (`<modules>` in the POM).

## Deferred

- Skipping unchanged modules (Gradle's up-to-date checks, Bazel's action cache). Natural once tests exist; needs a hash of sources + classpath + config.
- `-P` / `-D` profile activation, shade relocations and `artifactSet` (STATUS next steps).
- `install` / `deploy`, protobuf generation (one module uses `protoc-jar-maven-plugin`; its executions appear to be commented out in its POM, which is unverified).
