//! `rayvn.lock`: the resolved dependency set with versions, source repositories and SHA-256 checksums.
//! It is valid only while the hash of `pom.xml` it records still matches ([`read_fresh`]).

use crate::pom::{Gav, RepoDecl};
use crate::repos::{ArtifactFile, Repos};
use crate::resolve::Resolved;
use anyhow::{Context, Result};
use rayon::prelude::*;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::path::Path;

const VERSION: u32 = 4;

#[derive(Serialize, Deserialize)]
pub struct Lock {
    pub version: u32,
    pub pom_sha256: String,
    #[serde(default)]
    pub package: Vec<Pkg>,
}

#[derive(Serialize, Deserialize)]
pub struct Pkg {
    pub group: String,
    pub artifact: String,
    pub version: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub classifier: Option<String>,
    #[serde(rename = "type")]
    pub typ: String,
    pub scope: String,
    pub depth: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub via: Option<String>,
    /// Absent only when the origin is unknown: the file was put in the cache by something other than rayvn.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repo: Option<String>,
    /// Needed to re-find repositories that only a dependency's POM declared.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repo_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sha256: Option<String>,
    /// A module of the same reactor build, resolved to its build output; no repository, no checksum.
    #[serde(default, skip_serializing_if = "is_false")]
    pub workspace: bool,
}

fn is_false(b: &bool) -> bool {
    !*b
}

/// Lower-case hex SHA-256, used for both the POM fingerprint and artifact checksums.
///
/// ```
/// assert_eq!(
///     rayvn::lock::hash_bytes(b"abc"),
///     "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
/// );
/// ```
pub fn hash_bytes(b: &[u8]) -> String {
    crate::repos::hex(&Sha256::digest(b))
}

/// One hash for a set of POM files (a module, its parents on disk, and the modules it depends on): it changes when
/// any of their contents changes, and does not depend on where the checkout lives.
pub fn fingerprint(files: &[std::path::PathBuf]) -> Result<String> {
    let mut lines = vec![];
    for f in files {
        lines.push(hash_bytes(
            &std::fs::read(f).with_context(|| format!("reading {}", f.display()))?,
        ));
    }
    Ok(hash_bytes(lines.join("\n").as_bytes()))
}

/// The lock if it exists, parses, and matches the current pom; otherwise `None`.
pub fn read_fresh(path: &Path, pom_sha256: &str) -> Option<Lock> {
    let l: Lock = toml::from_str(&std::fs::read_to_string(path).ok()?).ok()?;
    (l.version == VERSION && l.pom_sha256 == pom_sha256).then_some(l)
}

/// Repository ids recorded in an existing lock (whatever its pom hash), keyed by coordinates.
fn previous_repos(path: &Path) -> HashMap<String, String> {
    let Some(old) = std::fs::read_to_string(path)
        .ok()
        .and_then(|t| toml::from_str::<Lock>(&t).ok())
    else {
        return HashMap::new();
    };
    old.package
        .iter()
        .filter_map(|p| {
            Some((
                coordinates(&p.group, &p.artifact, &p.version, p.classifier.as_deref()),
                p.repo.clone()?,
            ))
        })
        .collect()
}

fn coordinates(group: &str, artifact: &str, version: &str, classifier: Option<&str>) -> String {
    format!("{group}:{artifact}:{version}:{}", classifier.unwrap_or(""))
}

/// The repository to record for `r`: the one that supplied it this run, else the one the previous lock knew
/// (covers cache entries that predate origin tracking).
fn repo_for(r: &Resolved, previous: &HashMap<String, String>) -> Option<String> {
    r.repo.clone().or_else(|| {
        previous
            .get(&coordinates(
                &r.gav.group,
                &r.gav.artifact,
                &r.gav.version,
                r.classifier.as_deref(),
            ))
            .cloned()
    })
}

pub fn write(path: &Path, pom_sha256: &str, resolved: &[Resolved], repos: &Repos) -> Result<()> {
    let previous = previous_repos(path);
    // Order is classpath order (Maven-style depth-first), so it is deliberately not sorted.
    let package: Vec<Pkg> = resolved
        .iter()
        .map(|r| {
            let repo = repo_for(r, &previous);
            Pkg {
                group: r.gav.group.clone(),
                artifact: r.gav.artifact.clone(),
                version: r.gav.version.clone(),
                classifier: r.classifier.clone(),
                typ: r.typ.clone(),
                scope: r.scope.clone(),
                depth: r.depth,
                via: r.via.clone(),
                repo_url: repo.as_deref().and_then(|id| repos.url_of(id)),
                repo,
                sha256: r.sha256.clone(),
                workspace: r.workspace,
            }
        })
        .collect();
    let lock = Lock {
        version: VERSION,
        pom_sha256: pom_sha256.into(),
        package,
    };
    let body = toml::to_string_pretty(&lock)?;
    std::fs::write(path, format!("# Generated by rayvn. Do not edit.\n{body}"))
        .with_context(|| format!("writing {}", path.display()))
}

/// Materialise the locked set: no POM parsing, no graph walk; missing jars come straight from their recorded repo.
pub fn realize(
    lock: &Lock,
    repos: &Repos,
    locals: &crate::model::LocalModules,
) -> Result<Vec<Resolved>> {
    let known: Vec<RepoDecl> = lock
        .package
        .iter()
        .filter_map(|p| {
            Some(RepoDecl {
                id: p.repo.clone()?,
                url: p.repo_url.clone()?,
            })
        })
        .collect();
    repos.add_repos(&known);
    lock.package
        .par_iter()
        .map(|p| {
            let gav = Gav::new(&p.group, &p.artifact, &p.version);
            let jar = if p.typ == "pom" {
                None
            } else if p.workspace {
                Some(locals.output(&gav, p.classifier.as_deref().or(if p.typ == "test-jar" { Some("tests") } else { None }), &p.typ).with_context(|| {
                    format!("rayvn.lock names {gav} as a module of the build, but it is not one of the modules being built; run `rvn lock`")
                })?)
            } else {
                let ext = if matches!(p.typ.as_str(), "bundle" | "maven-plugin" | "test-jar") { "jar" } else { p.typ.as_str() };
                let classifier = p.classifier.as_deref().or(if p.typ == "test-jar" { Some("tests") } else { None });
                let loc = repos
                    .get_locked(p.repo.as_deref(), &ArtifactFile { group: &p.group, artifact: &p.artifact, version: &p.version, classifier, ext }, p.sha256.as_deref())?
                    .with_context(|| format!("locked artifact not found in any repository: {}:{}:{}", p.group, p.artifact, p.version))?;
                Some(loc.path)
            };
            Ok(Resolved {
                gav,
                scope: p.scope.clone(),
                classifier: p.classifier.clone(),
                depth: p.depth,
                via: p.via.clone(),
                jar,
                typ: p.typ.clone(),
                repo: p.repo.clone(),
                sha256: p.sha256.clone(),
                workspace: p.workspace,
            })
        })
        .collect()
}

/// Checks every locked jar on disk against its recorded SHA-256, in parallel. SNAPSHOTs change legitimately and are skipped.
/// Returns `(verified, unverifiable)`; fails listing every mismatch.
pub fn verify(lock: &Lock, resolved: &[Resolved]) -> Result<(usize, usize)> {
    let checks: Vec<(&Pkg, &Path)> = lock
        .package
        .iter()
        .zip(resolved)
        .filter_map(|(p, r)| Some((p, r.jar.as_deref()?)))
        .collect();
    let results: Vec<Result<Option<()>, String>> = checks
        .par_iter()
        .map(
            |(p, jar)| match (&p.sha256, p.version.ends_with("-SNAPSHOT")) {
                (Some(want), false) => match crate::repos::sha256_file(jar) {
                    Ok(got) if &got == want => Ok(Some(())),
                    Ok(got) => Err(format!(
                        "{}:{}:{}: lockfile {want}, on disk {got} ({})",
                        p.group,
                        p.artifact,
                        p.version,
                        jar.display()
                    )),
                    Err(e) => Err(format!("{}:{}:{}: {e:#}", p.group, p.artifact, p.version)),
                },
                _ => Ok(None),
            },
        )
        .collect();
    let bad: Vec<&String> = results.iter().filter_map(|r| r.as_ref().err()).collect();
    if !bad.is_empty() {
        anyhow::bail!(
            "{} locked file(s) do not match rayvn.lock:\n  {}",
            bad.len(),
            bad.iter()
                .map(|s| s.as_str())
                .collect::<Vec<_>>()
                .join("\n  ")
        );
    }
    let verified = results.iter().filter(|r| matches!(r, Ok(Some(())))).count();
    Ok((verified, checks.len() - verified))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn resolved(repo: Option<&str>) -> Resolved {
        Resolved {
            gav: Gav {
                group: "g".into(),
                artifact: "a".into(),
                version: "1".into(),
            },
            scope: "compile".into(),
            classifier: None,
            depth: 1,
            via: None,
            jar: None,
            typ: "jar".into(),
            repo: repo.map(String::from),
            sha256: None,
            workspace: false,
        }
    }

    fn known(repo: &str) -> HashMap<String, String> {
        HashMap::from([(coordinates("g", "a", "1", None), repo.to_string())])
    }

    #[test]
    fn the_fingerprint_follows_content_not_location() {
        let d = std::env::temp_dir().join(format!("rvn-fp-{}", std::process::id()));
        std::fs::create_dir_all(d.join("x")).unwrap();
        let (a, b) = (d.join("a.xml"), d.join("x/b.xml"));
        std::fs::write(&a, "one").unwrap();
        std::fs::write(&b, "two").unwrap();
        let before = fingerprint(&[a.clone(), b.clone()]).unwrap();
        std::fs::copy(&b, d.join("copy.xml")).unwrap();
        assert_eq!(
            fingerprint(&[a.clone(), d.join("copy.xml")]).unwrap(),
            before,
            "a moved file with the same content"
        );
        std::fs::write(&b, "changed").unwrap();
        assert_ne!(fingerprint(&[a.clone(), b]).unwrap(), before);
        assert_ne!(
            fingerprint(&[a]).unwrap(),
            before,
            "a file missing from the set"
        );
        assert!(fingerprint(&[d.join("missing.xml")]).is_err());
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn repo_seen_this_run_wins_over_the_previous_lock() {
        assert_eq!(
            repo_for(&resolved(Some("new")), &known("old")).as_deref(),
            Some("new")
        );
    }

    #[test]
    fn cache_hit_without_origin_keeps_the_repo_from_the_previous_lock() {
        assert_eq!(
            repo_for(&resolved(None), &known("old")).as_deref(),
            Some("old")
        );
        assert_eq!(repo_for(&resolved(None), &HashMap::new()), None);
    }

    #[test]
    fn previous_repos_are_read_from_any_lock_and_keyed_by_classifier() {
        let dir = std::env::temp_dir().join(format!("rvn-lock-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("rayvn.lock");
        let text = "version = 4\npom_sha256 = \"stale\"\n\n[[package]]\ngroup = \"g\"\nartifact = \"a\"\nversion = \"1\"\nclassifier = \"tests\"\ntype = \"jar\"\nscope = \"test\"\ndepth = 1\nrepo = \"r\"\n";
        std::fs::write(&path, text).unwrap();
        let prev = previous_repos(&path);
        assert_eq!(
            prev.get(&coordinates("g", "a", "1", Some("tests")))
                .map(String::as_str),
            Some("r")
        );
        assert_eq!(
            repo_for(&resolved(None), &prev),
            None,
            "the plain jar is a different artifact from the tests classifier"
        );
        assert!(previous_repos(&dir.join("missing.lock")).is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
