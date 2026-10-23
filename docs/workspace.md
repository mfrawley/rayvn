# Multi-module builds

A POM that lists `<modules>` makes a workspace. Run `rvn` at the root, or with `-f` on any module.

```xml
<packaging>pom</packaging>
<modules><module>core</module><module>app</module></modules>
```

| Command | Does |
|---|---|
| `rvn build` / `test` / `verify` / `stage` / `deploy` | Every module, in dependency order. |
| `... -m app`, `-pl app` (`--projects`) | `app` and the upstream modules it depends on (`-am` by default). `-m g:app` if two modules share an artifactId. |
| `... -pl app --only` | Strictly `app` only, without compiling upstream sibling modules. |
| `lock` / `fetch` / `resolve` / `classpath` with `-m app` | Only `app`. `classpath` needs `-m` when there are several modules. |

## What it does

- **Order.** Module A before module B when B declares a dependency on A, or when B's `<parent>` is A. The match is on exact `groupId:artifactId:version`, with `<dependencyManagement>` applied, so a dependency on another version is an ordinary download. Modules with no constraint between them keep their `<modules>` order. A cycle is an error naming the modules.
- **Siblings are not downloaded.** A dependency on a module is its `target/classes` (its `test-jar` is `target/test-classes`), read from disk. A shaded jar packs those classes into the fat jar like any other dependency. Because the build output stands in for the jar, a module whose test classes are used by another is compiled (tests included) even under `rvn build -m`.
- **Aggregators.** A `pom`-packaged module that lists modules packages its POM to `target/<artifact>-<version>.pom` (with checksums) during `rvn build`, `rvn stage`, and `rvn deploy`. Aggregators have no code or unit tests, and require no `rayvn.lock`.
- **One lock per module**, `<module>/rayvn.lock`, no root lock. A sibling is recorded as `workspace = true` with no checksum. A module's lock goes stale when the POMs of the module, its parents on disk, or any module it depends on (directly or not) change; a change elsewhere leaves it alone.
- **The test cache** hashes a sibling by its class tree, so editing `core` re-runs `app`'s tests.
- **`rvn test` / `rvn verify` at the root** run each module's tests in build order, from the module's own directory (classpath entries are absolute, so this works however the POM was named). A module without test sources is skipped. A failing module stops the run. The run ends with a table, for example:

  ```
  tests by module:
    matsim-moia: 15 passed (15 cached)
    routing-map-conversion: 4 passed
    total: 19 passed in 2 modules (15 cached); 21 modules have no tests
  ```

  Gradle runs `gradle test` per project with task-level caching and `--continue` to go past failures; Bazel runs `bazel test //...` with a result cache and `--keep_going`. rayvn matches the caching; continuing past a failure is not implemented yet.

## Parallel builds

Modules build side by side: `-j N` (default one per CPU) sets how many at once, `-j 1` goes one after another in the reactor's order. Each module is two steps. The first resolves its dependencies and compiles (main, and tests where they are used); the second packages the jar and runs the tests. A module's first step waits only for the first steps of the modules it depends on, so the tests of one module run while the modules above it compile. Compiles are preferred over test runs when both are ready, as they are what other modules wait for.

- Output: every line is prefixed `[module]`, padded so the lines line up. `javac` output is captured and printed whole, so error messages are not torn apart. With `-j 1` the output is as it was, with a `==> module` header. The download counters are printed once at the end instead of after each module, and the single self-rewriting progress line is off.
- Results on stdout (jar paths, `resolve`, `classpath`) are printed at the end in the reactor's order, so they do not depend on timing. `resolve` and `classpath` always go in order.
- The first failure stops new modules from starting; those already running finish, and the others that fail are listed as `also failed:`. The error names the module. `--keep-going` is not implemented.
- Integration tests of different modules never run at the same time (`docs/testing.md`).
- Measured on a 24-module workspace with warm caches, `rvn build`: 33 s with `-j 1`, 11 s by default.

Maven needs `-T` for this and its modules share `~/.m2`, which is why parallel Maven builds are treated with caution; Gradle runs the tasks of different projects at once with `--parallel` and `--max-workers`, and decides from the task graph, as here; Bazel runs whatever the dependency graph allows, up to `--jobs`, which is the default and not an option. rayvn's unit of work is the module, not the single task (a javac call, a jar): finer than that would need to know the inputs and outputs of each plugin, which a POM does not say.

## Leaving modules out

A checkout often has sub-projects that should not be part of the build: archived code, infrastructure that needs other tooling, experiments. List them in `rayvn.toml` next to the root POM:

```toml
[workspace]
exclude = ["archive", "projects/year_2024/*", "**/scratch"]
```

Patterns are ant-style globs (`*` within a directory name, `**` across them) on the module's directory relative to the root POM. An excluded module is dropped with everything below it, and its POM is never read. A pattern that matches no module draws a warning, since it is usually a typo. `exclude` entries from `~/.rayvn/config.toml` and `rayvn.toml` add up. The Maven source POMs stay untouched, so `mvn` still builds everything.

Gradle does this in `settings.gradle` by leaving out an `include`; Bazel keeps a package out of a build with `.bazelignore` or `--deleted_packages`.

## Versions across modules

Each module resolves on its own, so two modules may use different versions of a library; nothing conflicts until both reach one classpath, where Maven's nearest-wins applies as always. The JVM has one version per class on a classpath, so rayvn does not try to load two (Nix-style isolation would need relocation or separate classloaders).

Not built yet: `[workspace] versions = "check" | "unified"` to require one version per artifact (see proposal 0002), and building independent modules in parallel (they run one after another today).
