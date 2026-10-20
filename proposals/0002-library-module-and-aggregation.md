# 0002: Building a full-toolchain library module, then `<modules>` aggregation

Status: proposed, 2026-10-21

Purpose of the project: see `README.md` (Maven-compatible, modernised). Every item below says how Maven, Gradle and Bazel handle it, and what raven takes.

## Why these two, in this order

The motivating case is a typical multi-module repository: a root `pom.xml` with `packaging=pom` listing about 9 modules, one of which nests three more levels of aggregators over about 19 leaf projects.

1. **The core library module** is what the others are built on and the only one using the full Maven toolchain (surefire, failsafe, enforcer, shade, compiler). Most other modules depend on a *published* build of it, pinned by timestamp version.
2. **Aggregation** is simple here: only one sibling depends on the library at the reactor version (`1.0-SNAPSHOT`); the rest pin published versions from a package registry. So sibling-output resolution is rarely needed.

Not verified yet: that `rvn build` currently fails on the library module and how. The first step below is to run it and record what actually breaks.

## Part 1: the library module

What its POM uses, and the gap in raven today:

| Maven feature | In the library module | raven today |
|---|---|---|
| Compile main | `source/target 25`, `encoding`, `fork`, `maxmem`, `Xmaxwarns/Xmaxerrs` | `javac` with `--release`; fork/memory are irrelevant (raven calls `javac` directly); `-proc:none` is hardcoded (check whether any dependency needs an annotation processor) |
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

### Decision: no plugin execution

raven does not run Maven plugins (no JVM plugin host, no Mojo API). It reads a plugin's configuration from the POM and implements the behaviour natively for a short list of well-known plugins (compiler, shade, surefire, failsafe, enforcer's convergence rule, jar). Anything else is reported as ignored, as shade options already are. This keeps raven fast and single-binary, and is the main compatibility trade-off.

### How Gradle and Bazel handle it (from memory, unchecked)

- **Tests.** Gradle's `test` task is built in: it discovers test classes by scanning compiled output and runs them in a forked worker JVM (or several, `maxParallelForks`), with results cached by input hash. Bazel has `java_test` per target; each runs in a sandbox, in parallel, with the result cached by the hash of its inputs, so an unchanged test is never re-run. raven takes: run tests in a separate JVM and skip when nothing changed (later, via the same input hash `raven.lock` already gives us). It declines Bazel's per-target granularity, which Maven projects don't have.
- **Integration tests.** Gradle uses a separate source set and task (the `jvm-test-suite` plugin); Bazel uses a separate target tagged `integration`. Both treat it as a distinct build unit, not a naming convention. raven follows Maven's `*IT` naming because that is what the POM says, but runs them as a separate step, like both.
- **Plugins.** Gradle plugins are code, in-process, with a typed task API; Bazel rules are Starlark, run by the build engine, and actions must declare their inputs and outputs. Maven plugins are arbitrary JVM code with side effects. raven's native implementations are closest to Bazel's idea (a fixed, known action with declared inputs), and that is why executing unknown plugins is out of scope.
- **Convergence.** Gradle resolves one version per module by default (highest wins) and offers `failOnVersionConflict()`; Bazel's `rules_jvm_external` pins one version per artifact in a lock file (`maven_install.json`). raven already has the lockfile; the check just reports where the graph needed mediation.

## Part 2: `<modules>` aggregation

### Steps

1. Follow `<modules>` recursively from a `pom`-packaging root; accept `pom` packaging for aggregators (build nothing, only recurse).
2. Build children in an order consistent with their dependencies on each other.
3. Resolve a sibling at the reactor version (`lib:1.0-SNAPSHOT`) from its build output, not from a repository.
4. Build independent modules in parallel.

One `raven.lock` per module, as now; no root lockfile until we need one.

### How Gradle and Bazel handle it (from memory, unchecked)

- **Gradle** lists projects in `settings.gradle` and builds a task graph over all of them: `:core:jar` before `:app:compileJava`. Independent tasks run in parallel; up-to-date tasks are skipped. A sibling is a `project(":core")` dependency.
- **Bazel** has no module list: every directory with a `BUILD` file is a package, targets name their dependencies by label (`//core:lib`), and the graph is the whole build. Sibling and external dependencies use the same mechanism.
- **Maven's reactor** sorts modules into a linear order and builds sequentially (`-T` adds module-level parallelism). raven takes Gradle's idea: build a graph, run what is independent in parallel. It keeps Maven's input (`<modules>` in the POM).

## Deferred

- Skipping unchanged modules (Gradle's up-to-date checks, Bazel's action cache). Natural once tests exist; needs a hash of sources + classpath + config.
- `-P` / `-D` profile activation, shade relocations and `artifactSet` (STATUS next steps).
- `install` / `deploy`, protobuf generation (one module uses `protoc-jar-maven-plugin`; its executions appear to be commented out in its POM, which is unverified).
