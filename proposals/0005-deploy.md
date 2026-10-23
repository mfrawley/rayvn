# 0005: `rvn deploy`, publishing artifacts to remote repositories

Status: agreed design, 2026-10-22
Date: 2026-10-22

## Problem

rayvn currently builds, tests, verifies, packages jars, and manages lockfiles. However, getting artifacts out of a local workspace and into a package repository (Maven Central, Nexus, Artifactory, AWS CodeArtifact, GitHub Packages) requires dropping back to `mvn deploy` or wrapping custom curl scripts.

Maven's `deploy:deploy` has severe design flaws that rayvn eliminates:
1. **Partial / non-atomic deployment failures:** In a multi-module build, if module 9 of 10 fails, modules 1 through 8 are already permanently published on the remote server, leaving the release corrupted and broken.
2. **Lack of local inspectability:** Maven does not stage artifacts into a clean local directory before attempting network upload.
3. **Rigid credentials:** Requires generating dummy `~/.m2/settings.xml` files in CI instead of accepting environment variables or a project configuration file.

## Agreed Decisions

### 1. All-Locally First

Before a single byte is sent over the network:
- All selected modules are built.
- All artifacts (JARs, aggregator/parent POMs, attached artifacts) and their checksums (`.sha1`, `.sha256`, `.md5`) are generated and verified into a local staging directory (default: `target/staging/`).
- If **any** module fails to compile, test, or stage, the command fails immediately and **zero HTTP requests are made**.

### 2. Explicit Staging Inspection

Support staging without deploying:
- `rvn stage [--stage-dir <path>]` (or `rvn deploy --dry-run` / `--stage-only`) builds and populates the staging directory, prints the layout, and exits 0 without uploading.
- Allows CI or developers to inspect the exact artifacts, run compliance checks, or archive the release bundle.

### 3. Credential Resolution Hierarchy

Credentials for the target repository are resolved in this order:
1. `rayvn.toml` (`[deploy]` repository block or `user_env` / `password_env` / `token_env`).
2. Environment variables (`RAYVN_DEPLOY_USER`, `RAYVN_DEPLOY_PASSWORD`).
3. `~/.m2/settings.xml` `<servers>` entry matching the repository `<id>`.

### 4. GPG Signing

- **Strictly optional:** Internal packages (Nexus, Artifactory, CodeArtifact) do not need signing.
- **Pluggable:** When signing is required (e.g. Maven Central), delegated via `--sign-cmd <path>` or `rayvn.toml` (`sign_cmd = [...]`), allowing use of `gpg`, `sigstore`, or custom CI key runners without hardcoding gpg-agent daemon interactions.

## Staging Directory Layout

A Maven repository repository layout under `<stage-dir>/`:

```
<stage-dir>/
└── <group-as-path>/
    └── <artifact>/
        └── <version>/
            ├── <artifact>-<version>.pom
            ├── <artifact>-<version>.pom.sha1
            ├── <artifact>-<version>.pom.sha256
            ├── <artifact>-<version>.pom.md5
            ├── <artifact>-<version>.jar
            ├── <artifact>-<version>.jar.sha1
            ├── <artifact>-<version>.jar.sha256
            └── <artifact>-<version>.jar.md5
```
