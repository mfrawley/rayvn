//! The effective model of a project: a POM with its parent chain, BOM imports, properties and
//! dependency management folded in. [`Loader`] fetches parents and imports through [`crate::repos`].

use crate::pom::{self, Dep, Gav, ParentRef, RawPom, RepoDecl};
use crate::repos::Repos;
use anyhow::{Context, Result, bail};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::Path;
use std::sync::{Arc, Mutex};

/// A POM with parent inheritance, property interpolation and BOM imports applied.
#[derive(Debug, Clone)]
pub struct Effective {
    pub gav: Gav,
    pub packaging: String,
    pub props: BTreeMap<String, String>,
    pub managed: HashMap<String, Dep>,
    pub deps: Vec<Dep>,
    pub repos: Vec<RepoDecl>,
    pub shade: Option<pom::ShadeCfg>,
}

pub struct Loader<'a> {
    repos: &'a Repos,
    cache: Mutex<HashMap<Gav, Arc<Effective>>>,
}

impl<'a> Loader<'a> {
    pub fn new(repos: &'a Repos) -> Self {
        Loader { repos, cache: Mutex::new(HashMap::new()) }
    }

    pub fn from_file(&self, path: &Path) -> Result<Effective> {
        let xml = std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        let raw = pom::parse(&xml).with_context(|| path.display().to_string())?;
        self.build(raw, 0, path.parent())
    }

    pub fn load(&self, gav: &Gav) -> Result<Arc<Effective>> {
        self.load_depth(gav, 0)
    }

    fn load_depth(&self, gav: &Gav, depth: usize) -> Result<Arc<Effective>> {
        if let Some(e) = self.cache.lock().unwrap().get(gav) {
            return Ok(e.clone());
        }
        if depth > 30 {
            bail!("parent/import chain too deep at {gav}");
        }
        let loc = self
            .repos
            .get(&gav.group, &gav.artifact, &gav.version, None, "pom")?
            .with_context(|| format!("pom not found in any repository: {gav}"))?;
        let xml = std::fs::read_to_string(&loc.path)?;
        let raw = pom::parse(&xml).with_context(|| format!("parsing pom of {gav}"))?;
        let eff = Arc::new(self.build(raw, depth, None)?);
        self.repos.add_repos(&eff.repos);
        self.cache.lock().unwrap().insert(gav.clone(), eff.clone());
        Ok(eff)
    }

    /// The parent POM on disk (`relativePath`, default `../pom.xml`), used only if its coordinates match.
    fn local_parent(&self, p: &ParentRef, dir: &Path, depth: usize) -> Result<Option<Arc<Effective>>> {
        let Some(path) = local_parent_path(p, dir) else { return Ok(None) };
        if depth > 30 {
            bail!("parent chain too deep at {}", path.display());
        }
        let xml = std::fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
        let raw = pom::parse(&xml).with_context(|| path.display().to_string())?;
        let eff = self.build(raw, depth + 1, path.parent())?;
        Ok((eff.gav == p.gav).then(|| Arc::new(eff)))
    }

    fn build(&self, raw: RawPom, depth: usize, dir: Option<&Path>) -> Result<Effective> {
        let parent = match &raw.parent {
            Some(p) => match dir.map(|d| self.local_parent(p, d, depth)).transpose()?.flatten() {
                Some(e) => Some(e),
                None => Some(self.load_depth(&p.gav, depth + 1)?),
            },
            None => None,
        };
        let gav = project_gav(&raw, parent.as_deref())?;
        let props = effective_props(&raw.props, &gav, parent.as_deref());
        let managed = self.managed_versions(raw.managed, parent.as_deref(), &props, depth)?;
        let deps = merge_dependencies(raw.deps, parent.as_deref(), &managed, &props);

        let mut repos = raw.repos;
        if let Some(p) = &parent {
            repos.extend(p.repos.iter().cloned());
        }
        let shade = raw.shade.or_else(|| parent.as_ref().and_then(|p| p.shade.clone()));
        Ok(Effective { gav, packaging: raw.packaging, props, managed, deps, repos, shade })
    }

    /// The parent's dependencyManagement plus this POM's own; imported BOMs fill in only what is not managed yet.
    fn managed_versions(&self, own: Vec<Dep>, parent: Option<&Effective>, props: &BTreeMap<String, String>, depth: usize) -> Result<HashMap<String, Dep>> {
        let mut managed: HashMap<String, Dep> = parent.map(|p| p.managed.clone()).unwrap_or_default();
        for d in own.into_iter().map(|d| interpolate_dep(d, props)) {
            if d.scope.as_deref() == Some("import") && d.typ == "pom" {
                let v = d.version.clone().with_context(|| format!("BOM {}:{} has no version", d.group, d.artifact))?;
                let bom = self.load_depth(&Gav::new(&d.group, &d.artifact, &v), depth + 1)?;
                for (k, bd) in &bom.managed {
                    managed.entry(k.clone()).or_insert_with(|| bd.clone());
                }
            } else {
                managed.insert(d.key(), d);
            }
        }
        Ok(managed)
    }
}

/// groupId and version fall back to the parent's.
fn project_gav(raw: &RawPom, parent: Option<&Effective>) -> Result<Gav> {
    let group = raw.group.clone().or_else(|| parent.map(|p| p.gav.group.clone()));
    let version = raw.version.clone().or_else(|| parent.map(|p| p.gav.version.clone()));
    match (group, version) {
        (Some(g), Some(v)) => Ok(Gav::new(&g, &raw.artifact, &v)),
        _ => bail!("cannot determine groupId/version of {}", raw.artifact),
    }
}

/// The parent's properties, overridden by the POM's own, plus the implicit `project.*` ones.
fn effective_props(own: &BTreeMap<String, String>, gav: &Gav, parent: Option<&Effective>) -> BTreeMap<String, String> {
    let mut props = parent.map(|p| p.props.clone()).unwrap_or_default();
    props.extend(own.clone());
    props.insert("project.groupId".into(), gav.group.clone());
    props.insert("project.artifactId".into(), gav.artifact.clone());
    for k in ["project.version", "pom.version", "version"] {
        props.insert(k.into(), gav.version.clone());
    }
    if let Some(p) = parent {
        props.insert("project.parent.version".into(), p.gav.version.clone());
        props.insert("project.parent.groupId".into(), p.gav.group.clone());
    }
    props
}

fn interpolate_dep(mut d: Dep, props: &BTreeMap<String, String>) -> Dep {
    d.group = interpolate(&d.group, props);
    d.artifact = interpolate(&d.artifact, props);
    d.version = d.version.map(|v| interpolate(&v, props));
    d.scope = d.scope.map(|v| interpolate(&v, props));
    d.classifier = d.classifier.map(|v| interpolate(&v, props));
    d
}

/// Maven merges a parent's `<dependencies>` into the child: the child's own come first and win per key;
/// a missing version, scope or exclusions is taken from dependencyManagement.
fn merge_dependencies(own: Vec<Dep>, parent: Option<&Effective>, managed: &HashMap<String, Dep>, props: &BTreeMap<String, String>) -> Vec<Dep> {
    let own_keys: HashSet<String> = own.iter().map(|d| d.key()).collect();
    let mut deps: Vec<Dep> = own
        .into_iter()
        .map(|d| interpolate_dep(d, props))
        .map(|mut d| {
            if let Some(m) = managed.get(&d.key()) {
                d.version = d.version.or_else(|| m.version.clone());
                d.scope = d.scope.or_else(|| m.scope.clone());
                if d.exclusions.is_empty() {
                    d.exclusions = m.exclusions.clone();
                }
            }
            d
        })
        .collect();
    if let Some(p) = parent {
        deps.extend(p.deps.iter().filter(|d| !own_keys.contains(&d.key())).cloned());
    }
    deps
}

/// Replaces `${key}` from `props`, repeatedly (properties may refer to properties); unknown keys stay as written.
///
/// ```
/// use std::collections::BTreeMap;
/// let props = BTreeMap::from([
///     ("v".to_string(), "1.2".to_string()),
///     ("full".to_string(), "lib-${v}".to_string()),
/// ]);
/// assert_eq!(rayvn::model::interpolate("${full}.jar", &props), "lib-1.2.jar");
/// assert_eq!(rayvn::model::interpolate("${missing}", &props), "${missing}");
/// ```
pub fn interpolate(s: &str, props: &BTreeMap<String, String>) -> String {
    let mut cur = s.to_string();
    for _ in 0..16 {
        if !cur.contains("${") {
            break;
        }
        let mut out = String::new();
        let mut rest = cur.as_str();
        let mut changed = false;
        while let Some(i) = rest.find("${") {
            out.push_str(&rest[..i]);
            match rest[i..].find('}') {
                Some(j) => {
                    let key = &rest[i + 2..i + j];
                    match props.get(key) {
                        Some(v) => {
                            out.push_str(v);
                            changed = true;
                        }
                        None => out.push_str(&rest[i..=i + j]),
                    }
                    rest = &rest[i + j + 1..];
                }
                None => break,
            }
        }
        out.push_str(rest);
        if !changed {
            return out;
        }
        cur = out;
    }
    cur
}

fn local_parent_path(p: &ParentRef, dir: &Path) -> Option<std::path::PathBuf> {
    let rel = match p.relative_path.as_deref() {
        Some("") => return None,
        Some(r) => r,
        None => "../pom.xml",
    };
    let mut path = dir.join(rel);
    if path.is_dir() {
        path = path.join("pom.xml");
    }
    path.is_file().then_some(path)
}

/// `<repositories>` of a POM and its on-disk parents, nearest first (needed before any repo can be queried).
pub fn local_repos(pom_path: &Path) -> Result<Vec<RepoDecl>> {
    let mut out = vec![];
    let mut cur = pom_path.to_path_buf();
    for _ in 0..30 {
        let raw = pom::parse(&std::fs::read_to_string(&cur).with_context(|| format!("reading {}", cur.display()))?)
            .with_context(|| cur.display().to_string())?;
        out.extend(raw.repos.iter().cloned());
        let Some(next) = raw.parent.as_ref().and_then(|p| local_parent_path(p, cur.parent().unwrap_or(Path::new(".")))) else { break };
        cur = next;
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dep(artifact: &str, version: Option<&str>) -> Dep {
        Dep { group: "g".into(), artifact: artifact.into(), version: version.map(Into::into), typ: "jar".into(), ..Default::default() }
    }

    fn effective(gav: Gav, props: &[(&str, &str)], deps: Vec<Dep>) -> Effective {
        Effective {
            gav,
            packaging: "jar".into(),
            props: props.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect(),
            managed: HashMap::new(),
            deps,
            repos: vec![],
            shade: None,
        }
    }

    fn parent() -> Effective {
        effective(Gav::new("pg", "parent", "7"), &[("a", "from-parent"), ("b", "from-parent")], vec![dep("inherited", Some("1")), dep("shared", Some("1"))])
    }

    fn raw(group: Option<&str>, version: Option<&str>) -> RawPom {
        let mut raw = RawPom::default();
        raw.group = group.map(Into::into);
        raw.artifact = "x".into();
        raw.version = version.map(Into::into);
        raw

    }

    #[test]
    fn gav_falls_back_to_the_parent() {
        assert_eq!(project_gav(&raw(None, None), Some(&parent())).unwrap(), Gav::new("pg", "x", "7"));
        assert_eq!(project_gav(&raw(Some("g"), Some("2")), Some(&parent())).unwrap(), Gav::new("g", "x", "2"));
        assert!(project_gav(&raw(None, None), None).is_err());
    }

    #[test]
    fn own_properties_override_the_parents_and_project_ones_are_added() {
        let own = BTreeMap::from([("b".to_string(), "own".to_string())]);
        let props = effective_props(&own, &Gav::new("g", "x", "2"), Some(&parent()));
        assert_eq!(props["a"], "from-parent");
        assert_eq!(props["b"], "own");
        assert_eq!((props["project.version"].as_str(), props["version"].as_str(), props["project.artifactId"].as_str()), ("2", "2", "x"));
        assert_eq!((props["project.parent.version"].as_str(), props["project.parent.groupId"].as_str()), ("7", "pg"));
    }

    #[test]
    fn dependencies_get_managed_versions_and_win_over_the_parents() {
        let managed = HashMap::from([(dep("a", None).key(), dep("a", Some("managed")))]);
        let props = BTreeMap::from([("v".to_string(), "9".to_string())]);
        let own = vec![dep("a", None), dep("shared", Some("${v}"))];
        let deps = merge_dependencies(own, Some(&parent()), &managed, &props);
        let got: Vec<_> = deps.iter().map(|d| (d.artifact.as_str(), d.version.as_deref())).collect();
        assert_eq!(got, [("a", Some("managed")), ("shared", Some("9")), ("inherited", Some("1"))]);
    }

    #[test]
    fn interpolation_applies_to_every_coordinate_field() {
        let props = BTreeMap::from([("p".to_string(), "X".to_string())]);
        let mut d = dep("a-${p}", Some("${p}"));
        d.group = "g-${p}".into();
        d.scope = Some("${p}".into());
        d.classifier = Some("${p}".into());
        let d = interpolate_dep(d, &props);
        assert_eq!((d.group.as_str(), d.artifact.as_str(), d.version.as_deref(), d.scope.as_deref(), d.classifier.as_deref()), ("g-X", "a-X", Some("X"), Some("X"), Some("X")));
    }
}
