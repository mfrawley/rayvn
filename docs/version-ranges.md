# Version ranges

A dependency `<version>` may be a Maven range instead of a single version:

| Written | Allows |
|---|---|
| `[1.0,2.0)` | 1.0 up to, not including, 2.0 |
| `[1.0,2.0]`, `(1.0,2.0)` | both ends in, both ends out |
| `[1.5,)`, `(,1.5]` | open-ended |
| `[1.5]` | exactly 1.5 (needs no lookup) |
| `(,1.0],[1.2,)` | a union; the highest match wins |

`rvn lock` replaces each range by the highest version in it that the repositories list, and prints `range g:a [1.0,2.0) -> 1.9`. The lockfile records the chosen version, so `rvn fetch` and `--locked` never look at the range again: **a range floats only when you re-lock**. This is what Maven does on every build; rayvn does it once and writes it down.

- **Where versions come from.** Each candidate repository's `maven-metadata.xml` for the artifact (routes and hints apply as for any download), merged across repositories. A listing is reused for `snapshot_ttl_hours` (24 by default); `--refresh` always asks again.
- **Order.** Maven's: `1.2 < 1.10`, `1.0-alpha < 1.0-rc < 1.0 < 1.0-sp < 1.0.1`, `1.0 == 1`. `-SNAPSHOT` versions are never chosen from a range.
- **Mediation.** The version a range settles on takes part in nearest-wins like any other, and a `<dependencyManagement>` entry still overrides what a dependency declares. A range in a dependency's POM is settled the same way as one in yours.
- **Errors.** A malformed range, or one no listed version satisfies, stops the lock and names the dependency and the range.

Not supported: `LATEST` and `RELEASE`, which Maven 3 deprecated; use a range, or pin.

## How Gradle and Bazel treat it

Gradle accepts ranges and dynamic versions (`1.+`, `latest.release`) and re-resolves them per build unless dependency locking is on, then `--write-locks` freezes them; the cache for dynamic versions lasts 24 hours by default (`cacheDynamicVersionsFor`). rayvn behaves as if locking were always on. Bazel's `rules_jvm_external` asks for exact versions and pins them in `maven_install.json`; a range there is an error, which is the strictest answer. rayvn accepts the POMs as they are, which is the point of being a drop-in for `mvn`, and makes the outcome reproducible through `rayvn.lock`.
