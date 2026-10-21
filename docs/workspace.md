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

## Versions across modules

Each module resolves on its own, so two modules may use different versions of a library; nothing conflicts until both reach one classpath, where Maven's nearest-wins applies as always. The JVM has one version per class on a classpath, so rayvn does not try to load two (Nix-style isolation would need relocation or separate classloaders).

Not built yet: `[workspace] versions = "check" | "unified"` to require one version per artifact (see proposal 0002), and building independent modules in parallel (they run one after another today).
