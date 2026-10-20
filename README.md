# raven (`rvn`)

A fast, Maven-compatible build tool in Rust.

## Purpose

Backwards-compatible with `mvn`, dragged into modernity. Existing projects build from their existing `pom.xml` (and `settings.xml`) with no migration; the tool underneath works the way current tools (uv, Cargo, Gradle, Bazel) do: a lockfile, a fast parallel resolver, a cache that is safe in CI, and no work done twice.

Compatibility is judged by results (same classpath, same jar contents), not by reproducing Maven's internals. Where Maven's model is the obstacle (a mutable shared `~/.m2`, plugins as arbitrary code, a sequential reactor), raven keeps the POM as the input and changes what happens behind it. Each design note records the choice, and compares how Gradle and Bazel handle the same problem.

## Where things are

- `proposals/`: design decisions, numbered, not rewritten after the fact.
- `docs/`: user documentation (`routing.md`, `credentials.md`).
