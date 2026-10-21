# Multi-module builds

A POM that lists `<modules>` makes a workspace. Run `rvn` at the root, or with `-f` on any module.

```xml
<packaging>pom</packaging>
<modules><module>core</module><module>app</module></modules>
```

| Command | Does |
|---|---|
| `rvn build` / `test` / `verify` | Every module, in dependency order. |
| `... -m app` (`--module`) | `app` and the modules it depends on. `-m g:app` if two modules share an artifactId. |
| `lock` / `fetch` / `resolve` / `classpath` with `-m app` | Only `app`. `classpath` needs `-m` when there are several modules. |

## What it does

- **Order.** Module A before module B when B declares a dependency on A. The match is on exact `groupId:artifactId:version`, with `<dependencyManagement>` applied, so a dependency on another version is an ordinary download. Modules with no constraint between them keep their `<modules>` order. A cycle is an error naming the modules.
- **Siblings are not downloaded.** A dependency on a module is its `target/classes` (its `test-jar` is `target/test-classes`), read from disk. A shaded jar packs those classes into the fat jar like any other dependency. Because the build output stands in for the jar, a module whose test classes are used by another is compiled (tests included) even under `rvn build -m`.
- **Aggregators.** A `pom`-packaged module that lists modules builds nothing.
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
