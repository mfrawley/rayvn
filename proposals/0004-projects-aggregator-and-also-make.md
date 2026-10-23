# 0004: Projects selection, aggregator POM publishing, and `-am` / `--also-make`

Status: agreed design, 2026-10-22
Date: 2026-10-22

## Problem

rayvn supports multi-module builds via `<modules>` in root and nested POMs (`src/reactor.rs`), but reactor control and aggregator lifecycle have two gaps compared to Maven:

1. **Aggregator POM publishing.**
   In multi-module repositories, the root or intermediate aggregator POM (`packaging=pom`) often serves a dual role:
   - It lists child `<modules>` for build ordering and reactor navigation.
   - It serves as the `<parent>` POM for child modules, defining `<dependencyManagement>`, plugin configurations, repository definitions, and shared properties.
   
   When child modules are built and published to a repository, downstream consumers cannot resolve child artifacts unless the parent aggregator POM is also published to that repository.
   
   Currently in rayvn, `Module::is_aggregator` marks a pom-packaged module with `<modules>` as "builds nothing and has no dependencies of its own to lock." Consequently, `rvn build` skips packaging for aggregators, producing no installable/publishable POM artifact or checksums.

2. **Reactor selection control.**
   rayvn currently provides `-m <module>` / `--module <module>` (`src/cli.rs`), which automatically computes the transitive closure of dependencies via `closure(...)` and builds all of them in topological order.
   
   This auto-upstream behavior matches Maven's `-pl <module> -am` (`--also-make`), which is what users want 99% of the time. However:
   - There is no way to turn this off if someone wants strict isolation to build *only* that module (`--only`).
   - Maven users frequently pass `-am` out of muscle memory or in existing CI scripts, and `-pl` is the standard flag alias.

## Agreed Decisions

1. **Aggregator POM generation:**
   - For modules with `packaging=pom`, `rvn build` generates `target/<artifactId>-<version>.pom` along with checksums (`.sha1`, `.sha256`, `.md5`).
   - We **preserve `<modules>` intact** in the generated POM file, strictly faithful to standard Maven default behavior. We do not invent custom stripping logic.

2. **Selection semantics:**
   - Support `-pl, --projects` as an alias for `-m, --module`.
   - **Default behavior:** building a selected module includes all upstream dependencies (equivalent to `-am`).
   - **`--only`:** an explicit flag to build *only* the named module without compiling upstream siblings.
   - **`-am, --also-make`:** accepted as an alias/no-op for compatibility with Maven muscle memory and existing CI scripts.
