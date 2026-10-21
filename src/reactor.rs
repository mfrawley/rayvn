//! Multi-module builds: follow `<modules>`, order the modules by their dependencies on each other and choose
//! which of them a command touches. A single project is a reactor of one module.
//!
//! Maven calls this the [reactor](https://maven.apache.org/guides/mini/guide-multiple-modules.html). rayvn keeps
//! its input (`<modules>` in the POM) and its meaning (a dependency on a sibling is satisfied by the sibling's
//! build output, never by a download), but locks each module on its own: see [`lock_inputs`].

use crate::model::{self, Effective, Loader};
use crate::pom::{self, Dep, Gav};
use anyhow::{Context, Result, bail};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

/// One project of the build.
#[derive(Debug, Clone)]
pub struct Module {
    pub dir: PathBuf,
    /// The module's POM and the parent POMs found on disk above it.
    pub pom_files: Vec<PathBuf>,
    pub effective: Effective,
    /// `<modules>` entries as written.
    pub modules: Vec<String>,
}

impl Module {
    pub fn name(&self) -> &str {
        &self.effective.gav.artifact
    }

    pub fn gav(&self) -> &Gav {
        &self.effective.gav
    }

    /// A `pom`-packaged project that lists modules: it builds nothing and has no dependencies of its own to lock.
    pub fn is_aggregator(&self) -> bool {
        self.effective.packaging == "pom" && !self.modules.is_empty()
    }

    pub fn lock_path(&self) -> PathBuf {
        self.dir.join("rayvn.lock")
    }
}

/// The POM a `<module>` entry names: a directory (its `pom.xml`) or a POM file.
pub fn module_pom_path(dir: &Path, entry: &str) -> PathBuf {
    let p = dir.join(entry);
    if p.is_dir() { p.join("pom.xml") } else { p }
}

/// A project found on disk, before its model is built.
#[derive(Debug, Clone)]
pub struct Found {
    pub pom_path: PathBuf,
    pub dir: PathBuf,
    /// `<modules>` entries as written.
    pub modules: Vec<String>,
    pub poms: model::LocalPoms,
}

/// The project at `root_pom` followed by its modules, recursively, in declaration order (depth first), read
/// without touching the network. A module whose directory (relative to the root) matches an `exclude` glob is
/// left out with everything below it. Fails on a module that lists an ancestor.
pub fn scan(root_pom: &Path, exclude: &[String]) -> Result<Scan> {
    let mut scan = Scan {
        found: vec![],
        unused: exclude.to_vec(),
    };
    let root = root_pom
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    scan_into(root_pom, root, exclude, &mut scan, &mut vec![])?;
    Ok(scan)
}

/// What [`scan`] found.
#[derive(Debug)]
pub struct Scan {
    pub found: Vec<Found>,
    /// `exclude` patterns that matched no module: usually a typo, so the caller should say so.
    pub unused: Vec<String>,
}

/// `dir` relative to `root`, with `/` separators.
fn relative(root: &Path, dir: &Path) -> String {
    dir.strip_prefix(root)
        .unwrap_or(dir)
        .to_string_lossy()
        .replace('\\', "/")
}

fn scan_into(
    pom_path: &Path,
    root: &Path,
    exclude: &[String],
    scan: &mut Scan,
    ancestors: &mut Vec<PathBuf>,
) -> Result<()> {
    let id = std::fs::canonicalize(pom_path)
        .with_context(|| format!("module pom not found: {}", pom_path.display()))?;
    if ancestors.contains(&id) {
        bail!(
            "{} lists itself as a module, directly or through its modules",
            pom_path.display()
        );
    }
    let raw = pom::parse(
        &std::fs::read_to_string(pom_path)
            .with_context(|| format!("reading {}", pom_path.display()))?,
    )
    .with_context(|| pom_path.display().to_string())?;
    let dir = pom_path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .map_or_else(|| PathBuf::from("."), PathBuf::from);
    let modules: Vec<String> = raw.modules.into_iter().filter(|m| !m.is_empty()).collect();
    scan.found.push(Found {
        pom_path: pom_path.to_path_buf(),
        dir: dir.clone(),
        modules: modules.clone(),
        poms: model::local_poms(pom_path)?,
    });
    ancestors.push(id);
    for m in &modules {
        let child = module_pom_path(&dir, m);
        let rel = relative(root, child.parent().unwrap_or(&dir));
        let hit = |g: &String| crate::build::glob(g.trim_end_matches('/'), &rel);
        let skipped = exclude.iter().any(hit);
        scan.unused.retain(|g| !hit(g));
        if !skipped {
            scan_into(&child, root, exclude, scan, ancestors)?;
        }
    }
    ancestors.pop();
    Ok(())
}

/// Builds the model of every found project. Fails when two modules have the same coordinates.
pub fn load(found: Vec<Found>, loader: &Loader) -> Result<Vec<Module>> {
    let modules = found
        .into_iter()
        .map(|f| {
            Ok(Module {
                effective: loader.load_file(&f.pom_path)?,
                pom_files: f.poms.files,
                modules: f.modules,
                dir: f.dir,
            })
        })
        .collect::<Result<Vec<_>>>()?;
    let mut seen: HashMap<&Gav, &Path> = HashMap::new();
    for m in &modules {
        if let Some(first) = seen.insert(m.gav(), &m.dir) {
            bail!(
                "two modules are {}: {} and {}",
                m.gav(),
                first.display(),
                m.dir.display()
            );
        }
    }
    Ok(modules)
}

/// The version a dependency gets: `<dependencyManagement>` wins over the declared one, as in resolution.
fn version_of(e: &Effective, d: &Dep) -> Option<String> {
    e.managed
        .get(&d.key())
        .and_then(|m| m.version.clone())
        .or_else(|| d.version.clone())
}

/// For each module, the indexes of the modules it depends on directly (any scope, matched on exact coordinates).
pub fn dependency_edges(modules: &[Module]) -> Vec<Vec<usize>> {
    let index: HashMap<&Gav, usize> = modules
        .iter()
        .enumerate()
        .map(|(i, m)| (m.gav(), i))
        .collect();
    modules
        .iter()
        .enumerate()
        .map(|(i, m)| {
            let mut deps: Vec<usize> = m
                .effective
                .deps
                .iter()
                .filter_map(|d| {
                    index
                        .get(&Gav::new(
                            &d.group,
                            &d.artifact,
                            &version_of(&m.effective, d)?,
                        ))
                        .copied()
                })
                .filter(|&j| j != i)
                .collect();
            deps.sort_unstable();
            deps.dedup();
            deps
        })
        .collect()
}

/// A build order in which every module follows the modules it depends on. Modules with no constraint between them
/// keep their declaration order. Fails, naming the modules involved, when they depend on each other in a cycle.
pub fn topological_order(names: &[String], deps: &[Vec<usize>]) -> Result<Vec<usize>> {
    let mut waiting: Vec<usize> = deps.iter().map(Vec::len).collect();
    let mut dependents: Vec<Vec<usize>> = vec![vec![]; names.len()];
    for (i, ds) in deps.iter().enumerate() {
        for &d in ds {
            dependents[d].push(i);
        }
    }
    let mut ready: std::collections::BTreeSet<usize> =
        (0..names.len()).filter(|&i| waiting[i] == 0).collect();
    let mut order = Vec::with_capacity(names.len());
    while let Some(i) = ready.pop_first() {
        order.push(i);
        for &d in &dependents[i] {
            waiting[d] -= 1;
            if waiting[d] == 0 {
                ready.insert(d);
            }
        }
    }
    if order.len() < names.len() {
        let stuck: Vec<&str> = (0..names.len())
            .filter(|i| !order.contains(i))
            .map(|i| names[i].as_str())
            .collect();
        bail!(
            "modules depend on each other in a cycle: {}",
            stuck.join(", ")
        );
    }
    Ok(order)
}

/// All modules in build order.
pub fn order(modules: &[Module]) -> Result<Vec<usize>> {
    let names: Vec<String> = modules.iter().map(|m| m.name().to_string()).collect();
    topological_order(&names, &dependency_edges(modules))
}

/// The module `query` names: an `artifactId`, or `groupId:artifactId` when several modules share the artifactId.
pub fn find(modules: &[Module], query: &str) -> Result<usize> {
    let hits: Vec<usize> = (0..modules.len())
        .filter(|&i| {
            let g = modules[i].gav();
            query == g.artifact || query == format!("{}:{}", g.group, g.artifact)
        })
        .collect();
    match hits.as_slice() {
        [one] => Ok(*one),
        [] => bail!(
            "no module '{query}'; modules: {}",
            modules
                .iter()
                .map(Module::name)
                .collect::<Vec<_>>()
                .join(", ")
        ),
        many => bail!(
            "'{query}' names {} modules; use groupId:artifactId",
            many.len()
        ),
    }
}

/// `start` and every module it depends on, directly or not.
fn reach(deps: &[Vec<usize>], start: usize) -> HashSet<usize> {
    let mut wanted = HashSet::from([start]);
    let mut stack = vec![start];
    while let Some(i) = stack.pop() {
        for &d in &deps[i] {
            if wanted.insert(d) {
                stack.push(d);
            }
        }
    }
    wanted
}

/// `start` and every module it depends on, in the order of `order`.
pub fn closure(order: &[usize], deps: &[Vec<usize>], start: usize) -> Vec<usize> {
    let wanted = reach(deps, start);
    order
        .iter()
        .copied()
        .filter(|i| wanted.contains(i))
        .collect()
}

/// The modules a command works on, in build order: all of them, or `target` with the modules it needs.
pub fn select(modules: &[Module], target: Option<&str>) -> Result<Vec<usize>> {
    let all = order(modules)?;
    match target {
        None => Ok(all),
        Some(t) => Ok(closure(&all, &dependency_edges(modules), find(modules, t)?)),
    }
}

/// Modules of `selected` whose test classes another selected module depends on (`test-jar`, or classifier `tests`),
/// so they must be compiled even when their own tests do not run. Only direct declarations are seen.
pub fn test_class_providers(modules: &[Module], selected: &[usize]) -> HashSet<usize> {
    let index: HashMap<&Gav, usize> = modules
        .iter()
        .enumerate()
        .map(|(i, m)| (m.gav(), i))
        .collect();
    selected
        .iter()
        .flat_map(|&i| {
            let e = &modules[i].effective;
            e.deps
                .iter()
                .filter(|d| d.typ == "test-jar" || d.classifier.as_deref() == Some("tests"))
                .filter_map(|d| {
                    index
                        .get(&Gav::new(&d.group, &d.artifact, &version_of(e, d)?))
                        .copied()
                })
                .filter(|j| selected.contains(j))
                .collect::<Vec<_>>()
        })
        .collect()
}

/// The POM files that decide whether the lock of module `own` is current: its own POM chain, then the chains of the
/// modules it depends on, directly or not, ordered by coordinates so that the answer does not depend on declaration
/// order. A change to a module it does not use, or to another module's own dependencies' lock, leaves it fresh.
pub fn lock_inputs(modules: &[Module], deps: &[Vec<usize>], own: usize) -> Vec<PathBuf> {
    let mut needed: Vec<usize> = reach(deps, own).into_iter().filter(|&i| i != own).collect();
    needed.sort_by(|&a, &b| modules[a].gav().cmp(modules[b].gav()));
    let mut files = modules[own].pom_files.clone();
    files.extend(
        needed
            .into_iter()
            .flat_map(|i| modules[i].pom_files.iter().cloned()),
    );
    files
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pom::Dep;

    fn module(artifact: &str, deps: &[(&str, &str)], packaging: &str, children: &[&str]) -> Module {
        let dep = |(a, v): &(&str, &str)| Dep {
            group: "g".into(),
            artifact: a.to_string(),
            version: Some(v.to_string()),
            typ: "jar".into(),
            ..Default::default()
        };
        Module {
            dir: PathBuf::from(artifact),
            pom_files: vec![PathBuf::from(format!("{artifact}/pom.xml"))],
            modules: children.iter().map(|c| c.to_string()).collect(),
            effective: Effective {
                gav: Gav::new("g", artifact, "1"),
                packaging: packaging.into(),
                props: Default::default(),
                managed: Default::default(),
                deps: deps.iter().map(dep).collect(),
                repos: vec![],
                shade: None,
                compiler: None,
                unrun: vec![],
                surefire: None,
                failsafe: None,
                relocation: None,
            },
        }
    }

    fn names(modules: &[Module], idx: &[usize]) -> Vec<String> {
        idx.iter().map(|&i| modules[i].name().to_string()).collect()
    }

    fn three() -> Vec<Module> {
        vec![
            module("app", &[("lib", "1"), ("util", "1")], "jar", &[]),
            module("lib", &[("util", "1")], "jar", &[]),
            module("util", &[("ext", "1")], "jar", &[]),
        ]
    }

    #[test]
    fn edges_follow_exact_coordinates_between_modules_only() {
        let mut ms = three();
        ms[1].effective.deps[0].version = Some("2".into());
        assert_eq!(
            dependency_edges(&ms),
            [vec![1, 2], vec![], vec![]],
            "lib:2 is not the module lib:1 and ext is external"
        );
        let again = ms[0].effective.deps[0].clone();
        ms[0].effective.deps.push(again);
        assert_eq!(
            dependency_edges(&ms)[0],
            [1, 2],
            "a repeated dependency is one edge"
        );
    }

    #[test]
    fn a_module_follows_the_modules_it_depends_on() {
        let ms = three();
        assert_eq!(names(&ms, &order(&ms).unwrap()), ["util", "lib", "app"]);
    }

    #[test]
    fn independent_modules_keep_their_declaration_order() {
        let ms = vec![
            module("b", &[], "jar", &[]),
            module("a", &[], "jar", &[]),
            module("c", &[("a", "1")], "jar", &[]),
        ];
        assert_eq!(names(&ms, &order(&ms).unwrap()), ["b", "a", "c"]);
    }

    #[test]
    fn a_cycle_is_reported_with_the_modules_in_it() {
        let ms = vec![
            module("a", &[("b", "1")], "jar", &[]),
            module("b", &[("a", "1")], "jar", &[]),
            module("c", &[], "jar", &[]),
        ];
        let err = order(&ms).unwrap_err().to_string();
        assert!(err.ends_with(": a, b"), "{err}");
    }

    #[test]
    fn find_accepts_artifact_or_group_and_artifact_and_lists_the_choices() {
        let mut ms = three();
        assert_eq!(find(&ms, "lib").unwrap(), 1);
        assert_eq!(find(&ms, "g:lib").unwrap(), 1);
        let err = find(&ms, "nope").unwrap_err().to_string();
        assert!(err.contains("app, lib, util"), "{err}");
        ms.push(module("lib", &[], "jar", &[]));
        ms[3].effective.gav = Gav::new("other", "lib", "1");
        assert!(find(&ms, "lib").is_err(), "ambiguous");
        assert_eq!(find(&ms, "other:lib").unwrap(), 3);
    }

    #[test]
    fn selecting_a_module_brings_the_modules_it_needs_in_build_order() {
        let ms = three();
        assert_eq!(
            names(&ms, &select(&ms, Some("lib")).unwrap()),
            ["util", "lib"]
        );
        assert_eq!(names(&ms, &select(&ms, Some("util")).unwrap()), ["util"]);
        assert_eq!(
            names(&ms, &select(&ms, None).unwrap()),
            ["util", "lib", "app"]
        );
    }

    #[test]
    fn aggregators_are_poms_that_list_modules() {
        assert!(module("root", &[], "pom", &["a"]).is_aggregator());
        assert!(!module("bom", &[], "pom", &[]).is_aggregator());
        assert!(!module("a", &[], "jar", &[]).is_aggregator());
    }

    #[test]
    fn lock_inputs_cover_the_module_and_the_modules_it_needs_in_coordinate_order() {
        let ms = three();
        let edges = dependency_edges(&ms);
        let pom = |m: &str| PathBuf::from(format!("{m}/pom.xml"));
        assert_eq!(
            lock_inputs(&ms, &edges, 0),
            [pom("app"), pom("lib"), pom("util")]
        );
        assert_eq!(lock_inputs(&ms, &edges, 1), [pom("lib"), pom("util")]);
        assert_eq!(
            lock_inputs(&ms, &edges, 2),
            [pom("util")],
            "a module that needs no module has only its own chain"
        );
    }

    #[test]
    fn a_managed_version_decides_which_module_a_dependency_is() {
        let mut ms = three();
        ms[0].effective.deps = vec![ms[0].effective.deps[0].clone()];
        ms[0].effective.deps[0].version = None;
        assert_eq!(
            dependency_edges(&ms)[0],
            Vec::<usize>::new(),
            "no version, no match"
        );
        let managed = Dep {
            version: Some("1".into()),
            ..ms[0].effective.deps[0].clone()
        };
        ms[0].effective.managed.insert(managed.key(), managed);
        assert_eq!(dependency_edges(&ms)[0], [1]);
    }

    #[test]
    fn module_entries_name_a_directory_or_a_pom_file() {
        let d = std::env::temp_dir().join(format!("rvn-reactor-{}", std::process::id()));
        std::fs::create_dir_all(d.join("sub")).unwrap();
        assert_eq!(module_pom_path(&d, "sub"), d.join("sub/pom.xml"));
        assert_eq!(
            module_pom_path(&d, "sub/other.xml"),
            d.join("sub/other.xml")
        );
        let _ = std::fs::remove_dir_all(&d);
    }

    fn tree(name: &str, files: &[(&str, &str)]) -> PathBuf {
        let d = std::env::temp_dir().join(format!("rvn-scan-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        for (f, body) in files {
            std::fs::create_dir_all(d.join(f).parent().unwrap()).unwrap();
            std::fs::write(d.join(f), body).unwrap();
        }
        d
    }

    fn pom(artifact: &str, modules: &[&str]) -> String {
        let list: String = modules
            .iter()
            .map(|m| format!("<module>{m}</module>"))
            .collect();
        format!(
            "<project><groupId>g</groupId><artifactId>{artifact}</artifactId><version>1</version><modules>{list}</modules></project>"
        )
    }

    #[test]
    fn scan_follows_modules_depth_first_through_nested_aggregators() {
        let d = tree(
            "nested",
            &[
                ("pom.xml", &pom("root", &["a", "b"])),
                ("a/pom.xml", &pom("a", &["a1"])),
                ("a/a1/pom.xml", &pom("a1", &[])),
                ("b/pom.xml", &pom("b", &[])),
            ],
        );
        let dirs: Vec<PathBuf> = scan(&d.join("pom.xml"), &[])
            .unwrap()
            .found
            .into_iter()
            .map(|f| f.dir.strip_prefix(&d).unwrap().to_path_buf())
            .collect();
        assert_eq!(dirs, ["", "a", "a/a1", "b"].map(PathBuf::from));
    }

    #[test]
    fn scan_rejects_a_module_that_lists_an_ancestor_and_a_missing_module() {
        let d = tree(
            "loop",
            &[
                ("pom.xml", &pom("root", &["a"])),
                ("a/pom.xml", &pom("a", &[".."])),
            ],
        );
        assert!(
            scan(&d.join("pom.xml"), &[])
                .unwrap_err()
                .to_string()
                .contains("lists itself")
        );
        let d = tree("missing", &[("pom.xml", &pom("root", &["gone"]))]);
        assert!(
            scan(&d.join("pom.xml"), &[])
                .unwrap_err()
                .to_string()
                .contains("module pom not found")
        );
    }

    #[test]
    fn excluded_modules_are_skipped_with_everything_below_and_unmatched_patterns_are_reported() {
        let d = tree(
            "exclude",
            &[
                ("pom.xml", &pom("root", &["a", "b"])),
                ("a/pom.xml", &pom("a", &["a1"])),
                ("a/a1/pom.xml", &pom("a1", &[])),
                ("b/pom.xml", &pom("b", &[])),
            ],
        );
        let s = scan(&d.join("pom.xml"), &["a".into(), "typo".into()]).unwrap();
        let dirs: Vec<PathBuf> = s
            .found
            .iter()
            .map(|f| f.dir.strip_prefix(&d).unwrap().to_path_buf())
            .collect();
        assert_eq!(dirs, ["", "b"].map(PathBuf::from), "a1 goes with a");
        assert_eq!(s.unused, ["typo"]);
        let nested = scan(&d.join("pom.xml"), &["a/a1/".into()]).unwrap();
        assert_eq!(
            nested.found.len(),
            3,
            "only the nested module is gone; a trailing slash is allowed"
        );
        assert!(nested.unused.is_empty());
        let wild = scan(&d.join("pom.xml"), &["**/a1".into()]).unwrap();
        assert_eq!(wild.found.len(), 3);
    }
}
