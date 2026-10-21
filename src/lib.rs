#![doc = include_str!("../README.md")]
//!
//! # Architecture
//!
//! `rvn` is a thin command-line wrapper (`src/main.rs`) over this library. A run goes through these modules
//! in order:
//!
//! 1. [`settings`]: read `settings.xml`, `~/.rayvn/config.toml` and `rayvn.toml` (repositories, mirrors,
//!    credentials, routes, hints).
//! 2. [`pom`] and [`model`]: parse `pom.xml`, then build the effective model (parents, BOM imports,
//!    properties, dependency management).
//! 3. [`resolve`]: walk the dependency graph with Maven's mediation rules, fetching POMs through [`repos`].
//! 4. [`repos`]: ask repositories (routed, hinted, cached, with a miss cache) and keep the local cache;
//!    bookkeeping lives in one file, see [`index`].
//! 5. [`lock`]: write or read `rayvn.lock`, which pins versions, repositories and checksums.
//! 6. [`build`]: compile with `javac` and package the jar (shaded when the POM asks for it).
//!
//! Credentials never appear in these types as plain config values; [`secret`] resolves them from the
//! environment or a helper command at the moment of use.

pub mod build;
pub mod index;
pub mod lock;
pub mod model;
pub mod pom;
pub mod repos;
pub mod resolve;
pub mod secret;
pub mod settings;

/// The one-line version string: package version, git commit, build time and a name derived from the commit.
///
/// A `-dirty` suffix on the commit means the working tree had uncommitted changes when it was built.
///
/// ```
/// let v = rayvn::version();
/// assert!(v.starts_with("rvn "));
/// assert!(v.contains(env!("CARGO_PKG_VERSION")));
/// ```
pub fn version() -> String {
    format!(
        "rvn {} ({}, {}) \"{}\"",
        env!("CARGO_PKG_VERSION"),
        env!("RAYVN_COMMIT"),
        env!("RAYVN_BUILT"),
        env!("RAYVN_NAME")
    )
}
