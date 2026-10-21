//! Dependency resolution: breadth-first over the graph, Maven's "nearest wins" version mediation, scope
//! mediation, exclusions and optional dependencies. POMs at each depth are loaded in parallel.

use crate::model::{Effective, Loader, LocalModules};
use crate::pom::{Dep, Gav, Relocation};
use crate::repos::{Located, Repos};
use anyhow::Result;
use rayon::prelude::*;
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Mutex;

/// One artifact of the mediated dependency tree, as `mvn dependency:tree` would list it, plus where it came from.
#[derive(Debug, Clone)]
pub struct Resolved {
    pub gav: Gav,
    /// Scope after mediation: the broadest any path to the artifact gives, narrowed by the scopes along that path.
    /// [Scope table](https://maven.apache.org/guides/introduction/introduction-to-dependency-mechanism.html#dependency-scope)
    pub scope: String,
    pub classifier: Option<String>,
    /// 1 for the project's own dependencies; the nearest path decides the version.
    /// [Transitive dependencies](https://maven.apache.org/guides/introduction/introduction-to-dependency-mechanism.html#transitive-dependencies)
    pub depth: usize,
    /// Coordinates of the parent through which the chosen version was reached.
    pub via: Option<String>,
    /// Local file in the cache; `None` for `pom`-type dependencies, which only contribute their own dependencies.
    pub jar: Option<PathBuf>,
    pub typ: String,
    /// Name of the repository the file came from (for the lockfile).
    pub repo: Option<String>,
    pub sha256: Option<String>,
    /// Satisfied by a module of the same reactor build: `jar` is that module's classes directory, and there is
    /// nothing to download or checksum.
    pub workspace: bool,
}

#[derive(Clone)]
struct Node {
    dep: Dep,
    version: String,
    scope: String,
    /// Scope as the declaring POM wrote it, before mediation against the parent's scope.
    declared: String,
    depth: usize,
    via: Option<String>,
    parent: Option<String>,
    exclusions: Vec<(String, String)>,
}

fn excluded(ex: &[(String, String)], d: &Dep) -> bool {
    ex.iter().any(|(g, a)| (g == "*" || *g == d.group) && (a == "*" || *a == d.artifact))
}

/// Scope of a transitive dependency given its parent's scope; `None` means it is dropped. This is the table in
/// [Maven's dependency scope](https://maven.apache.org/guides/introduction/introduction-to-dependency-mechanism.html#dependency-scope):
/// `test`, `provided` and `system` dependencies are never transitive, and a `runtime` parent makes everything `runtime`.
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
    /// Every (parent, child, declared scope) seen, including edges to nodes that lost version mediation.
    edges: Vec<(String, String, String)>,
}

/// Maven's scope selection when one artifact is reached by several paths: compile beats everything,
/// runtime beats provided only in combination with it (they merge to compile), provided and runtime beat test.
fn merge_scopes<'a>(a: &'a str, b: &'a str) -> &'a str {
    match (a, b) {
        _ if a == b => a,
        ("compile", _) | (_, "compile") | ("runtime", "provided") | ("provided", "runtime") => "compile",
        ("runtime", _) | (_, "runtime") => "runtime",
        ("provided", _) | (_, "provided") => "provided",
        _ => a,
    }
}

impl Tree {
    /// Records `node` unless its key is already taken; true if it was new.
    fn admit(&mut self, node: &Node) -> bool {
        let key = node.dep.key();
        if let Some(parent) = &node.parent {
            self.edges.push((parent.clone(), key.clone(), node.declared.clone()));
        }
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

    /// Widens the scope of each transitive node to the broadest any path to it gives, until nothing changes.
    /// Direct dependencies keep their declared scope.
    fn widen_scopes(&mut self) {
        loop {
            let mut changed = false;
            for (parent, child, declared) in &self.edges {
                let Some(via) = mediate(&self.chosen[parent].scope, declared) else { continue };
                let node = &self.chosen[child];
                if node.parent.is_none() {
                    continue;
                }
                let merged = merge_scopes(&node.scope, via);
                if merged != node.scope {
                    let merged = merged.to_string();
                    self.chosen.get_mut(child).unwrap().scope = merged;
                    changed = true;
                }
            }
            if !changed {
                return;
            }
        }
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
        nodes.push(Node { dep: d.clone(), version, declared: scope.clone(), scope, depth: 1, via: None, parent: None, exclusions: d.exclusions.clone() });
    }
    Ok(nodes)
}

/// Moves `n` to where its POM says the artifact now lives. A version the root manages for the new coordinates wins,
/// as for any dependency. Returns false when `r` changes nothing.
fn apply_relocation(n: &mut Node, r: &Relocation, managed: &HashMap<String, Dep>) -> bool {
    let before = (n.dep.group.clone(), n.dep.artifact.clone(), n.version.clone());
    n.dep.group = r.group.clone().unwrap_or_else(|| n.dep.group.clone());
    n.dep.artifact = r.artifact.clone().unwrap_or_else(|| n.dep.artifact.clone());
    n.version = managed.get(&n.dep.key()).and_then(|m| m.version.clone()).or_else(|| r.version.clone()).unwrap_or_else(|| n.version.clone());
    before != (n.dep.group.clone(), n.dep.artifact.clone(), n.version.clone())
}

/// Follows `<relocation>` from the node's POM until the artifact is where it says it is, as Maven does.
fn relocated(mut n: Node, loader: &Loader, managed: &HashMap<String, Dep>) -> Result<Node> {
    for _ in 0..MAX_RELOCATIONS {
        let from = Gav::new(&n.dep.group, &n.dep.artifact, &n.version);
        let Some(r) = loader.load(&from)?.relocation.clone() else { return Ok(n) };
        if !apply_relocation(&mut n, &r, managed) {
            return Ok(n);
        }
        eprintln!("warning: {from} has been relocated to {}", Gav::new(&n.dep.group, &n.dep.artifact, &n.version));
    }
    anyhow::bail!("{}:{} is relocated more than {MAX_RELOCATIONS} times; is it a loop?", n.dep.group, n.dep.artifact)
}

const MAX_RELOCATIONS: usize = 10;

/// The nodes `parent` pulls in: optional and excluded dependencies are dropped, scopes mediated, versions taken
/// from the root's dependencyManagement first.
fn child_nodes(parent: &Node, deps: &[Dep], managed: &HashMap<String, Dep>) -> Vec<Node> {
    let via = Gav::new(&parent.dep.group, &parent.dep.artifact, &parent.version).to_string();
    let mut kids = vec![];
    for c in deps {
        if c.optional || excluded(&parent.exclusions, c) {
            continue;
        }
        let declared = c.scope.as_deref().unwrap_or("compile");
        let Some(scope) = mediate(&parent.scope, declared) else { continue };
        let Some(version) = managed.get(&c.key()).and_then(|m| m.version.clone()).or_else(|| c.version.clone()) else { continue };
        let mut exclusions = parent.exclusions.clone();
        exclusions.extend(c.exclusions.iter().cloned());
        kids.push(Node { dep: c.clone(), version, scope: scope.into(), declared: declared.into(), depth: parent.depth + 1, via: Some(via.clone()), parent: Some(parent.dep.key()), exclusions });
    }
    kids
}

/// Nearest-wins resolution, level by level; each level's POMs are fetched in parallel.
pub fn resolve(root: &Effective, loader: &Loader, repos: &Repos, include_test: bool) -> Result<Vec<Resolved>> {
    let mut tree = Tree::default();
    let roots = root_nodes(&root.deps, include_test)?.into_par_iter().map(|n| relocated(n, loader, &root.managed)).collect::<Result<Vec<_>>>()?;
    let mut frontier: Vec<Node> = roots.into_iter().filter(|n| tree.admit(n)).collect();

    // Jar downloads start as soon as a node is chosen, overlapping the POM walk; results are kept so the pass below reports failures and the origin repo.
    let fetched: Fetched = Mutex::new(HashMap::new());
    let locals = loader.locals();
    rayon::scope(|scope| -> Result<()> {
        while !frontier.is_empty() {
            let expanded: Vec<Result<Vec<Node>>> = frontier
                .par_iter()
                .map(|n| {
                    let kids = child_nodes(n, &loader.load(&Gav::new(&n.dep.group, &n.dep.artifact, &n.version))?.deps, &root.managed);
                    // A candidate that has already lost mediation is never loaded, so it cannot fail for its POM's sake.
                    kids.into_par_iter().map(|k| if tree.chosen.contains_key(&k.dep.key()) { Ok(k) } else { relocated(k, loader, &root.managed) }).collect::<Result<Vec<_>>>()
                })
                .collect();

            let mut next = vec![];
            for kids in expanded {
                for k in kids? {
                    if tree.admit(&k) {
                        let (fetched, kc) = (&fetched, k.clone());
                        scope.spawn(move |_| {
                            let r = fetch_jar(repos, locals, &kc);
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

    tree.widen_scopes();
    let results: Vec<Result<Resolved>> = tree
        .classpath_order()
        .par_iter()
        .map(|k| {
            let n = &tree.chosen[k];
            let done = fetched.lock().unwrap().remove(k);
            to_resolved(n, locate(repos, locals, n, done)?, locals.get(&Gav::new(&n.dep.group, &n.dep.artifact, &n.version)).is_some())
        })
        .collect();
    unique_artifacts(results)
}

/// The jar of `n`: the prefetched result if there is one, else fetched now. POM-only dependencies have none.
fn locate(repos: &Repos, locals: &LocalModules, n: &Node, prefetched: Option<Result<Option<Located>>>) -> Result<Option<Located>> {
    if n.dep.typ == "pom" {
        return Ok(None);
    }
    match prefetched.unwrap_or_else(|| fetch_jar(repos, locals, n))? {
        Some(l) => Ok(Some(l)),
        None => anyhow::bail!("artifact not found in any repository: {}", Gav::new(&n.dep.group, &n.dep.artifact, &n.version)),
    }
}

/// The classifier a dependency is stored under: `test-jar` is the `tests` classifier unless one is given.
/// [Maven: type `test-jar`](https://maven.apache.org/ref/current/maven-core/artifact-handlers.html)
fn classifier_of(d: &Dep) -> Option<&str> {
    d.classifier.as_deref().or(if d.typ == "test-jar" { Some("tests") } else { None })
}

fn to_resolved(n: &Node, loc: Option<Located>, workspace: bool) -> Result<Resolved> {
    let sha256 = if workspace { None } else { loc.as_ref().map(|l| crate::repos::sha256_file(&l.path)).transpose()? };
    Ok(Resolved {
        gav: Gav::new(&n.dep.group, &n.dep.artifact, &n.version),
        scope: n.scope.clone(),
        classifier: classifier_of(&n.dep).map(String::from),
        depth: n.depth,
        via: n.via.clone(),
        repo: loc.as_ref().and_then(|l| l.repo.clone()),
        jar: loc.map(|l| l.path),
        typ: n.dep.typ.clone(),
        sha256,
        workspace,
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

/// A workspace module's build output when `n` is one; otherwise the artifact from a repository.
fn fetch_jar(repos: &Repos, locals: &LocalModules, n: &Node) -> Result<Option<Located>> {
    if n.dep.typ == "pom" {
        return Ok(None);
    }
    let gav = Gav::new(&n.dep.group, &n.dep.artifact, &n.version);
    if let Some(dir) = locals.output(&gav, classifier_of(&n.dep), &n.dep.typ) {
        return Ok(Some(Located { path: dir, downloaded: false, repo: None }));
    }
    if locals.get(&gav).is_some() {
        anyhow::bail!("{gav} is a module of this build, but the build does not produce its {} artifact", classifier_of(&n.dep).unwrap_or(&n.dep.typ));
    }
    let typ = n.dep.typ.as_str();
    let ext = if matches!(typ, "bundle" | "maven-plugin" | "test-jar") { "jar" } else { typ };
    repos.get(&n.dep.group, &n.dep.artifact, &n.version, classifier_of(&n.dep), ext)
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
            declared: "compile".into(),
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
    fn a_relocation_moves_only_the_fields_it_names() {
        let mut n = node(dep("old", Some("1"), None), None);
        let r = Relocation { group: Some("new-g".into()), ..Default::default() };
        assert!(apply_relocation(&mut n, &r, &HashMap::new()));
        assert_eq!((n.dep.group.as_str(), n.dep.artifact.as_str(), n.version.as_str()), ("new-g", "old", "1"));
        assert!(!apply_relocation(&mut n, &r, &HashMap::new()), "already there: nothing changes, so a loop ends");
        let r = Relocation { artifact: Some("newer".into()), version: Some("2".into()), ..Default::default() };
        assert!(apply_relocation(&mut n, &r, &HashMap::new()));
        assert_eq!((n.dep.artifact.as_str(), n.version.as_str()), ("newer", "2"));
    }

    #[test]
    fn a_version_managed_for_the_new_coordinates_beats_the_relocation_version() {
        let mut n = node(dep("old", Some("1"), None), None);
        let r = Relocation { artifact: Some("new".into()), version: Some("2".into()), ..Default::default() };
        let pinned = dep("new", Some("9"), None);
        apply_relocation(&mut n, &r, &HashMap::from([(pinned.key(), pinned)]));
        assert_eq!(n.version, "9");
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

    fn kid(artifact: &str, parent: &Node, declared: &str) -> Node {
        let mut n = node(dep(artifact, Some("1"), Some(declared)), Some(parent));
        n.declared = declared.into();
        n.scope = mediate(&parent.scope, declared).unwrap().into();
        n
    }

    #[test]
    fn merging_scopes_follows_maven() {
        assert_eq!(merge_scopes("test", "compile"), "compile");
        assert_eq!(merge_scopes("test", "runtime"), "runtime");
        assert_eq!(merge_scopes("test", "provided"), "provided");
        assert_eq!(merge_scopes("provided", "runtime"), "compile");
        assert_eq!(merge_scopes("runtime", "runtime"), "runtime");
    }

    #[test]
    fn a_later_compile_path_widens_a_nearer_test_path_and_its_subtree() {
        let mut t = Tree::default();
        let mut junit = node(dep("junit", Some("1"), Some("test")), None);
        junit.scope = "test".into();
        junit.declared = "test".into();
        let lib = node(dep("lib", Some("1"), None), None);
        let api_via_junit = kid("api", &junit, "compile");
        let api_via_lib = kid("api", &lib, "compile");
        let leaf = kid("leaf", &api_via_junit, "compile");
        for n in [&junit, &lib, &api_via_junit, &leaf] {
            assert!(t.admit(n));
        }
        assert!(!t.admit(&api_via_lib));
        assert_eq!(t.chosen[&api_via_junit.dep.key()].scope, "test");
        t.widen_scopes();
        assert_eq!(t.chosen[&api_via_junit.dep.key()].scope, "compile");
        assert_eq!(t.chosen[&leaf.dep.key()].scope, "compile");
        assert_eq!(t.chosen[&junit.dep.key()].scope, "test");
    }

    #[test]
    fn direct_dependencies_keep_their_declared_scope() {
        let mut t = Tree::default();
        let mut junit = node(dep("junit", Some("1"), Some("test")), None);
        junit.scope = "test".into();
        let lib = node(dep("lib", Some("1"), None), None);
        let junit_via_lib = kid("junit", &lib, "compile");
        assert!(t.admit(&junit) && t.admit(&lib) && !t.admit(&junit_via_lib));
        t.widen_scopes();
        assert_eq!(t.chosen[&junit.dep.key()].scope, "test");
    }

    #[test]
    fn test_jars_are_stored_under_the_tests_classifier() {
        let mut d = dep("lib", Some("1"), None);
        assert_eq!(classifier_of(&d), None);
        d.typ = "test-jar".into();
        assert_eq!(classifier_of(&d), Some("tests"));
        d.classifier = Some("custom".into());
        assert_eq!(classifier_of(&d), Some("custom"));
    }
}
