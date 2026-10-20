use crate::model::{Effective, Loader};
use crate::pom::{Dep, Gav};
use crate::repos::{Located, Repos};
use anyhow::Result;
use rayon::prelude::*;
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Mutex;

#[derive(Debug, Clone)]
pub struct Resolved {
    pub gav: Gav,
    pub scope: String,
    pub classifier: Option<String>,
    pub depth: usize,
    pub via: Option<String>,
    pub jar: Option<PathBuf>,
    pub typ: String,
    pub repo: Option<String>,
    pub sha256: Option<String>,
}

#[derive(Clone)]
struct Node {
    dep: Dep,
    version: String,
    scope: String,
    depth: usize,
    via: Option<String>,
    parent: Option<String>,
    exclusions: Vec<(String, String)>,
}

fn excluded(ex: &[(String, String)], d: &Dep) -> bool {
    ex.iter().any(|(g, a)| (g == "*" || *g == d.group) && (a == "*" || *a == d.artifact))
}

/// Scope of a transitive dependency given its parent's scope; `None` means it is dropped.
fn mediate(parent: &str, child: &str) -> Option<&'static str> {
    match (parent, child) {
        (_, "test" | "provided" | "system" | "import") => None,
        ("compile", "compile") => Some("compile"),
        ("compile", "runtime") => Some("runtime"),
        ("runtime", _) => Some("runtime"),
        ("provided", _) => Some("provided"),
        ("test", _) => Some("test"),
        _ => None,
    }
}

type Fetched = Mutex<HashMap<String, Result<Option<Located>>>>;

/// The mediated dependency tree: first node seen for a `groupId:artifactId:type:classifier` wins (nearest, then declaration order).
#[derive(Default)]
struct Tree {
    chosen: HashMap<String, Node>,
    children: HashMap<String, Vec<String>>,
    roots: Vec<String>,
}

impl Tree {
    /// Records `node` unless its key is already taken; true if it was new.
    fn admit(&mut self, node: &Node) -> bool {
        let key = node.dep.key();
        if self.chosen.contains_key(&key) {
            return false;
        }
        match &node.parent {
            Some(parent) => self.children.entry(parent.clone()).or_default().push(key.clone()),
            None => self.roots.push(key.clone()),
        }
        self.chosen.insert(key, node.clone());
        true
    }

    /// Maven's classpath order: depth-first, pre-order walk of the mediated tree; duplicate classes resolve to the first jar.
    fn classpath_order(&self) -> Vec<String> {
        let mut order = Vec::with_capacity(self.chosen.len());
        let mut stack: Vec<&String> = self.roots.iter().rev().collect();
        while let Some(k) = stack.pop() {
            order.push(k.clone());
            if let Some(kids) = self.children.get(k) {
                stack.extend(kids.iter().rev());
            }
        }
        order
    }
}

/// The project's own dependencies as depth-1 nodes.
fn root_nodes(deps: &[Dep], include_test: bool) -> Result<Vec<Node>> {
    let mut nodes = vec![];
    for d in deps {
        let scope = d.scope.clone().unwrap_or_else(|| "compile".into());
        if scope == "test" && !include_test {
            continue;
        }
        let Some(version) = d.version.clone() else {
            anyhow::bail!("dependency {}:{} has no version", d.group, d.artifact);
        };
        nodes.push(Node { dep: d.clone(), version, scope, depth: 1, via: None, parent: None, exclusions: d.exclusions.clone() });
    }
    Ok(nodes)
}

/// The nodes `parent` pulls in: optional and excluded dependencies are dropped, scopes mediated, versions taken
/// from the root's dependencyManagement first.
fn child_nodes(parent: &Node, deps: &[Dep], managed: &HashMap<String, Dep>) -> Vec<Node> {
    let via = Gav::new(&parent.dep.group, &parent.dep.artifact, &parent.version).to_string();
    let mut kids = vec![];
    for c in deps {
        if c.optional || excluded(&parent.exclusions, c) {
            continue;
        }
        let Some(scope) = mediate(&parent.scope, c.scope.as_deref().unwrap_or("compile")) else { continue };
        let Some(version) = managed.get(&c.key()).and_then(|m| m.version.clone()).or_else(|| c.version.clone()) else { continue };
        let mut exclusions = parent.exclusions.clone();
        exclusions.extend(c.exclusions.iter().cloned());
        kids.push(Node { dep: c.clone(), version, scope: scope.into(), depth: parent.depth + 1, via: Some(via.clone()), parent: Some(parent.dep.key()), exclusions });
    }
    kids
}

/// Nearest-wins resolution, level by level; each level's POMs are fetched in parallel.
pub fn resolve(root: &Effective, loader: &Loader, repos: &Repos, include_test: bool) -> Result<Vec<Resolved>> {
    let mut tree = Tree::default();
    let mut frontier: Vec<Node> = root_nodes(&root.deps, include_test)?.into_iter().filter(|n| tree.admit(n)).collect();

    // Jar downloads start as soon as a node is chosen, overlapping the POM walk; results are kept so the pass below reports failures and the origin repo.
    let fetched: Fetched = Mutex::new(HashMap::new());
    rayon::scope(|scope| -> Result<()> {
        while !frontier.is_empty() {
            let expanded: Vec<Result<Vec<Node>>> = frontier
                .par_iter()
                .map(|n| Ok(child_nodes(n, &loader.load(&Gav::new(&n.dep.group, &n.dep.artifact, &n.version))?.deps, &root.managed)))
                .collect();

            let mut next = vec![];
            for kids in expanded {
                for k in kids? {
                    if tree.admit(&k) {
                        let (fetched, kc) = (&fetched, k.clone());
                        scope.spawn(move |_| {
                            let r = fetch_jar(repos, &kc);
                            fetched.lock().unwrap().insert(kc.dep.key(), r);
                        });
                        next.push(k);
                    }
                }
            }
            frontier = next;
        }
        Ok(())
    })?;

    let results: Vec<Result<Resolved>> = tree
        .classpath_order()
        .par_iter()
        .map(|k| {
            let n = &tree.chosen[k];
            let done = fetched.lock().unwrap().remove(k);
            to_resolved(n, locate(repos, n, done)?)
        })
        .collect();
    unique_artifacts(results)
}

/// The jar of `n`: the prefetched result if there is one, else fetched now. POM-only dependencies have none.
fn locate(repos: &Repos, n: &Node, prefetched: Option<Result<Option<Located>>>) -> Result<Option<Located>> {
    if n.dep.typ == "pom" {
        return Ok(None);
    }
    match prefetched.unwrap_or_else(|| fetch_jar(repos, n))? {
        Some(l) => Ok(Some(l)),
        None => anyhow::bail!("artifact not found in any repository: {}", Gav::new(&n.dep.group, &n.dep.artifact, &n.version)),
    }
}

fn to_resolved(n: &Node, loc: Option<Located>) -> Result<Resolved> {
    let sha256 = loc.as_ref().map(|l| crate::repos::sha256_file(&l.path)).transpose()?;
    Ok(Resolved {
        gav: Gav::new(&n.dep.group, &n.dep.artifact, &n.version),
        scope: n.scope.clone(),
        classifier: n.dep.classifier.clone(),
        depth: n.depth,
        via: n.via.clone(),
        repo: loc.as_ref().and_then(|l| l.repo.clone()),
        jar: loc.map(|l| l.path),
        typ: n.dep.typ.clone(),
        sha256,
    })
}

/// First failure wins; later entries for the same artifact and classifier are dropped.
fn unique_artifacts(results: Vec<Result<Resolved>>) -> Result<Vec<Resolved>> {
    let mut out = vec![];
    let mut seen = HashSet::new();
    for r in results {
        let r = r?;
        if seen.insert((r.gav.clone(), r.classifier.clone())) {
            out.push(r);
        }
    }
    Ok(out)
}

fn fetch_jar(repos: &Repos, n: &Node) -> Result<Option<Located>> {
    if n.dep.typ == "pom" {
        return Ok(None);
    }
    let typ = n.dep.typ.as_str();
    let ext = if matches!(typ, "bundle" | "maven-plugin" | "test-jar") { "jar" } else { typ };
    let classifier = n.dep.classifier.as_deref().or(if typ == "test-jar" { Some("tests") } else { None });
    repos.get(&n.dep.group, &n.dep.artifact, &n.version, classifier, ext)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dep(artifact: &str, version: Option<&str>, scope: Option<&str>) -> Dep {
        Dep { group: "g".into(), artifact: artifact.into(), version: version.map(Into::into), scope: scope.map(Into::into), typ: "jar".into(), ..Default::default() }
    }

    fn node(d: Dep, parent: Option<&Node>) -> Node {
        Node {
            version: d.version.clone().unwrap_or_default(),
            scope: "compile".into(),
            depth: parent.map_or(1, |p| p.depth + 1),
            via: None,
            parent: parent.map(|p| p.dep.key()),
            exclusions: vec![],
            dep: d,
        }
    }

    fn artifacts(nodes: &[Node]) -> Vec<&str> {
        nodes.iter().map(|n| n.dep.artifact.as_str()).collect()
    }

    #[test]
    fn mediation_follows_maven_scope_table() {
        assert_eq!(mediate("compile", "compile"), Some("compile"));
        assert_eq!(mediate("compile", "runtime"), Some("runtime"));
        assert_eq!(mediate("runtime", "compile"), Some("runtime"));
        assert_eq!(mediate("provided", "compile"), Some("provided"));
        assert_eq!(mediate("compile", "test"), None);
        assert_eq!(mediate("compile", "provided"), None);
    }

    #[test]
    fn root_nodes_skip_test_scope_on_request_and_reject_missing_versions() {
        let deps = [dep("a", Some("1"), None), dep("t", Some("1"), Some("test"))];
        assert_eq!(artifacts(&root_nodes(&deps, false).unwrap()), ["a"]);
        assert_eq!(artifacts(&root_nodes(&deps, true).unwrap()), ["a", "t"]);
        assert!(root_nodes(&[dep("v", None, None)], true).is_err());
    }

    #[test]
    fn child_nodes_drop_optional_excluded_and_unversioned() {
        let mut parent = node(dep("p", Some("1"), None), None);
        parent.exclusions = vec![("g".into(), "gone".into())];
        let mut optional = dep("opt", Some("1"), None);
        optional.optional = true;
        let deps = [dep("ok", Some("1"), None), optional, dep("gone", Some("1"), None), dep("nover", None, None), dep("t", Some("1"), Some("test"))];
        let kids = child_nodes(&parent, &deps, &HashMap::new());
        assert_eq!(artifacts(&kids), ["ok"]);
        assert_eq!((kids[0].depth, kids[0].via.as_deref(), kids[0].parent.as_deref()), (2, Some("g:p:1"), Some("g:p:jar:")));
    }

    #[test]
    fn child_versions_come_from_dependency_management_first() {
        let parent = node(dep("p", Some("1"), None), None);
        let managed = HashMap::from([(dep("c", None, None).key(), dep("c", Some("9"), None))]);
        let kids = child_nodes(&parent, &[dep("c", Some("1"), None)], &managed);
        assert_eq!(kids[0].version, "9");
    }

    #[test]
    fn child_exclusions_accumulate_from_the_parent() {
        let mut parent = node(dep("p", Some("1"), None), None);
        parent.exclusions = vec![("x".into(), "*".into())];
        let mut c = dep("c", Some("1"), None);
        c.exclusions = vec![("y".into(), "z".into())];
        assert_eq!(child_nodes(&parent, &[c], &HashMap::new())[0].exclusions, [("x".into(), "*".into()), ("y".into(), "z".into())]);
    }

    #[test]
    fn tree_keeps_the_first_node_and_orders_depth_first() {
        let mut t = Tree::default();
        let (a, b) = (node(dep("a", Some("1"), None), None), node(dep("b", Some("1"), None), None));
        let (a1, a2) = (node(dep("a1", Some("1"), None), Some(&a)), node(dep("a2", Some("1"), None), Some(&a)));
        let b_dup_of_a1 = node(dep("a1", Some("2"), None), Some(&b));
        assert!([&a, &b, &a1, &a2].iter().all(|n| t.admit(n)));
        assert!(!t.admit(&b_dup_of_a1));
        let order: Vec<_> = t.classpath_order().iter().map(|k| k.split(':').nth(1).unwrap().to_string()).collect();
        assert_eq!(order, ["a", "a1", "a2", "b"]);
        assert_eq!(t.chosen[&a1.dep.key()].version, "1");
    }
}
