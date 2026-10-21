//! The `rvn` command line: parses arguments and sequences the steps. See the README for usage.
//!
//! # Architecture
//!
//! `rvn` is a thin command-line wrapper (`src/main.rs`) over the modules below. A run goes through these modules
//! in order:
//!
//! 1. `settings`: read `settings.xml`, `~/.rayvn/config.toml` and `rayvn.toml` (repositories, mirrors,
//!    credentials, routes, hints).
//! 2. `pom` and `model`: parse `pom.xml`, then build the effective model (parents, BOM imports,
//!    properties, dependency management).
//! 3. `reactor`: for a POM with `<modules>`, find the modules, order them by their dependencies on each other and
//!    resolve a sibling to its build output; each module is then locked and built on its own.
//! 4. `resolve`: walk the dependency graph with Maven's mediation rules, fetching POMs through `repos`.
//! 5. `repos`: ask repositories (routed, hinted, cached, with a miss cache) and keep the local cache;
//!    bookkeeping lives in one file, see `index`.
//! 6. `lock`: write or read `rayvn.lock`, which pins versions, repositories and checksums.
//! 7. `build`: compile with `javac` and package the jar (shaded when the POM asks for it).
//! 8. `verify` and `testcache`: compile the tests and run them with JUnit, selected as surefire and failsafe
//!    would, sharded across JVMs and skipped when nothing they depend on has changed.
//!
//! Credentials never appear in these types as plain config values; `secret` resolves them from the
//! environment or a helper command at the moment of use.

use anyhow::{Context, Result, bail};
use std::path::PathBuf;
use std::time::Instant;

mod build;
mod index;
mod lock;
mod model;
mod pom;
mod reactor;
mod repos;
mod resolve;
mod secret;
mod settings;
mod testcache;
mod verify;
mod version;

fn main() {
    if let Err(e) = run() {
        eprintln!("error: {e:#}");
        std::process::exit(1);
    }
}

const BANNER: &str = include_str!("../data/banner.txt");

/// Printed to stdout, but only when stdout is a terminal, so piped output (`rvn classpath | ...`) stays clean.
fn print_banner() {
    use std::io::IsTerminal;
    if std::io::stdout().is_terminal() && std::env::var_os("RAYVN_NO_BANNER").is_none() {
        println!("{BANNER}");
        println!("  {}\n", version());
    }
}

const COMMANDS: [&str; 7] = [
    "lock",
    "fetch",
    "resolve",
    "classpath",
    "build",
    "test",
    "verify",
];
const USAGE: &str = "usage: rvn <lock|fetch|resolve|classpath|build|test|verify> [-f pom.xml] [--locked] [--refresh] [--cold] [--rerun] [--test-cache dir] [-m module]";

#[derive(Debug, PartialEq)]
struct Args {
    cmd: String,
    pom_path: PathBuf,
    locked: bool,
    refresh: bool,
    cold: bool,
    test: bool,
    rerun: bool,
    test_cache: Option<String>,
    module: Option<String>,
}

impl Args {
    /// `None` for no or an unknown command (print usage).
    fn parse(args: &[String]) -> Result<Option<Args>> {
        let Some(cmd) = args.first().filter(|c| COMMANDS.contains(&c.as_str())) else {
            return Ok(None);
        };
        let flag = |name: &str| args.iter().any(|a| a == name);
        let pom_path = match args.iter().position(|a| a == "-f") {
            Some(i) => PathBuf::from(args.get(i + 1).context("-f needs a path")?),
            None => PathBuf::from("pom.xml"),
        };
        let test_cache = match args.iter().position(|a| a == "--test-cache") {
            Some(i) => Some(
                args.get(i + 1)
                    .context("--test-cache needs a directory")?
                    .clone(),
            ),
            None => None,
        };
        let module = match args.iter().position(|a| a == "-m" || a == "--module") {
            Some(i) => Some(args.get(i + 1).context("-m needs a module name")?.clone()),
            None => None,
        };
        let cold = flag("--cold");
        if cold && cmd == "classpath" {
            bail!(
                "--cold deletes its cache afterwards, so `classpath` would print paths that no longer exist"
            );
        }
        Ok(Some(Args {
            cmd: cmd.clone(),
            pom_path,
            locked: flag("--locked"),
            refresh: cold || flag("--refresh"),
            cold,
            test: flag("--test"),
            rerun: flag("--rerun"),
            test_cache,
            module,
        }))
    }

    fn project_dir(&self) -> PathBuf {
        self.pom_path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("."))
    }
}

fn cold_cache_dir() -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    std::env::temp_dir().join(format!("rvn-cold-{}-{nanos}", std::process::id()))
}

/// The dependency set of `module` for `args`: from its fresh `rayvn.lock` when there is one, else resolved from the
/// POM (and the lock rewritten). `lock_hash` covers the module's POMs and those of the modules it depends on.
fn dependencies(
    args: &Args,
    module: &reactor::Module,
    lock_hash: &str,
    repos: &repos::Repos,
    loader: &model::Loader,
) -> Result<Vec<resolve::Resolved>> {
    let lock_path = module.lock_path();
    let fetch = args.cmd == "fetch";
    let fresh = if args.cmd == "lock" || (args.refresh && !fetch) {
        None
    } else {
        lock::read_fresh(&lock_path, lock_hash)
    };
    if let Some(l) = fresh {
        eprintln!("  using rayvn.lock ({} packages)", l.package.len());
        let resolved = lock::realize(&l, repos, loader.locals())?;
        if fetch {
            let (ok, skipped) = lock::verify(&l, &resolved)?;
            eprintln!(
                "  verified {ok} files against rayvn.lock ({skipped} not checkable: no checksum recorded, SNAPSHOT, or a module of this build)"
            );
        }
        return Ok(resolved);
    }
    eprintln!("  resolving dependencies");
    if fetch || args.locked {
        bail!(
            "{} is missing or out of date; run `rvn lock`",
            lock_path.display()
        );
    }
    let resolved = resolve::resolve(&module.effective, loader, repos, true)?;
    lock::write(&lock_path, lock_hash, &resolved, repos)?;
    repos.clear_progress();
    eprintln!("  wrote {}", lock_path.display());
    Ok(resolved)
}

/// Colon-separated jar paths; test-scoped ones only if `include_test`.
fn classpath_line(resolved: &[resolve::Resolved], include_test: bool) -> String {
    let jars: Vec<String> = resolved
        .iter()
        .filter(|r| r.scope != "test" || include_test)
        .filter_map(|r| r.jar.as_ref().map(|p| p.display().to_string()))
        .collect();
    jars.join(":")
}

/// One line per artifact, indented by depth.
fn resolve_lines(resolved: &[resolve::Resolved]) -> Vec<String> {
    resolved
        .iter()
        .map(|r| {
            format!(
                "{}{}:{}  [{}] {}",
                "  ".repeat(r.depth - 1),
                r.gav.group,
                r.gav.artifact,
                r.scope,
                r.gav.version
            )
        })
        .collect()
}

/// Plugin executions that Maven would run during this build and rayvn does not.
fn unrun_warnings(root: &model::Effective) -> Vec<String> {
    root.unrun
        .iter()
        .map(|u| format!("warning: {}: Maven runs {u} during the build; rayvn does not, so the output may differ (docs/plugins.md)", root.gav.artifact))
        .collect()
}

fn warn_unrun(root: &model::Effective) {
    unrun_warnings(root).iter().for_each(|w| eprintln!("{w}"));
}

/// What `args.cmd` does for one module, once its dependencies are known. Returns `false` when nothing was built
/// (so there is nothing to report).
fn run_module(
    args: &Args,
    module: &reactor::Module,
    resolved: &[resolve::Resolved],
    provides_tests: bool,
    cx: &Services,
) -> Result<Vec<verify::TestOutcome>> {
    let (root, dir) = (&module.effective, module.dir.as_path());
    match args.cmd.as_str() {
        "lock" | "fetch" => {}
        "resolve" => resolve_lines(resolved).iter().for_each(|l| println!("{l}")),
        "classpath" => println!("{}", classpath_line(resolved, args.test)),
        "build" | "test" | "verify" => {
            if root.packaging != "jar" {
                bail!(
                    "packaging '{}' is not supported yet (only jar)",
                    root.packaging
                );
            }
            warn_unrun(root);
            let main_classes = build::compile_main(root, dir, resolved)?;
            if args.cmd == "build" {
                if provides_tests {
                    verify::compile_tests(root, dir, &main_classes, resolved)?;
                }
                build::package(root, dir, &main_classes, resolved)?;
                return Ok(vec![]);
            }
            let test_classes = verify::compile_tests(root, dir, &main_classes, resolved)?;
            let run = verify::TestRun {
                root,
                dir,
                main_classes: &main_classes,
                test_classes: &test_classes,
                resolved,
                repos: cx.repos,
                settings: &cx.config.test,
                rerun: args.rerun,
                cache_flag: args.test_cache.as_deref(),
            };
            let mut outcomes = vec![verify::run(verify::Phase::Unit, &run)?];
            if args.cmd == "verify" {
                build::package(root, dir, &main_classes, resolved)?;
                outcomes.push(verify::run(verify::Phase::Integration, &run)?);
            }
            return Ok(outcomes);
        }
        _ => unreachable!(),
    }
    Ok(vec![])
}

/// The closing table of a multi-module `test` or `verify`: one line per module and a total.
fn test_report(rows: &[(String, Vec<verify::TestOutcome>)]) -> Vec<String> {
    use verify::TestOutcome::*;
    let (mut ran, mut cached, mut modules_with_tests) = (0, 0, 0);
    let mut lines = vec!["tests by module:".to_string()];
    for (name, outcomes) in rows {
        let (r, c) = outcomes.iter().fold((0, 0), |(r, c), o| match o {
            Passed(n) => (r + n, c),
            Cached(n) => (r, c + n),
            NoTests => (r, c),
        });
        if r + c == 0 {
            continue;
        }
        modules_with_tests += 1;
        ran += r;
        cached += c;
        lines.push(format!(
            "  {name}: {} passed{}",
            r + c,
            if c > 0 {
                format!(" ({c} cached)")
            } else {
                String::new()
            }
        ));
    }
    let silent = rows.len() - modules_with_tests;
    lines.push(format!("  total: {} passed in {modules_with_tests} modules ({cached} cached); {silent} modules have no tests", ran + cached));
    lines
}

struct Services<'a> {
    repos: &'a repos::Repos,
    config: &'a settings::Config,
}

/// The modules `args` acts on, in build order, aggregators left out. Commands that only read dependencies
/// (`lock`, `fetch`, `resolve`, `classpath`) take just the module named with `-m`; the others also build what it needs.
fn pick(args: &Args, modules: &[reactor::Module]) -> Result<Vec<usize>> {
    let only_named = matches!(
        args.cmd.as_str(),
        "lock" | "fetch" | "resolve" | "classpath"
    );
    let chosen = match (&args.module, only_named) {
        (Some(m), true) => vec![reactor::find(modules, m)?],
        (m, _) => reactor::select(modules, m.as_deref())?,
    };
    let chosen: Vec<usize> = chosen
        .into_iter()
        .filter(|&i| !modules[i].is_aggregator())
        .collect();
    if chosen.is_empty() {
        bail!(
            "{} lists modules but none of them builds anything",
            args.pom_path.display()
        );
    }
    Ok(chosen)
}

fn run() -> Result<()> {
    // Downloads block on the network, so allow far more in flight than there are cores.
    rayon::ThreadPoolBuilder::new()
        .num_threads(32)
        .build_global()
        .ok();
    let raw: Vec<String> = std::env::args().skip(1).collect();
    if raw
        .first()
        .is_some_and(|a| matches!(a.as_str(), "--version" | "-V" | "version"))
    {
        println!("{}", version());
        return Ok(());
    }
    let Some(args) = Args::parse(&raw)? else {
        println!("{BANNER}");
        println!("{}", version());
        println!("{USAGE}");
        return Ok(());
    };
    print_banner();
    let project_dir = args.project_dir();
    let t0 = Instant::now();
    let (mut settings, _) = settings::load()?;
    let cold_dir = args.cold.then(cold_cache_dir);
    if let Some(d) = &cold_dir {
        settings.local_repo = Some(d.clone());
        eprintln!("rvn: cold run, cache {} (deleted afterwards)", d.display());
    }
    let (config, _) = settings::load_config(&project_dir)?;

    let scan = reactor::scan(&args.pom_path, &config.workspace.exclude)?;
    for g in &scan.unused {
        eprintln!("warning: [workspace] exclude '{g}' matches no module");
    }
    let found = scan.found;
    let declared: Vec<_> = found
        .iter()
        .flat_map(|f| f.poms.repos.iter().cloned())
        .collect();
    let repos = repos::Repos::new(&declared, &settings, &config, args.refresh, args.cold)?;
    let mut loader = model::Loader::new(&repos);
    let modules = reactor::load(found, &loader)?;
    for m in &modules {
        loader.register_local(m.effective.clone(), &m.dir);
    }
    let edges = reactor::dependency_edges(&modules);
    let selected: Vec<usize> = pick(&args, &modules)?;
    let many = selected.len() > 1;
    if many && args.cmd == "classpath" {
        bail!(
            "`classpath` prints one line; choose a module with -m ({})",
            selected
                .iter()
                .map(|&i| modules[i].name())
                .collect::<Vec<_>>()
                .join(", ")
        );
    }
    eprintln!(
        "rvn {}: {} ({} repositories, {} routes{})",
        args.cmd,
        args.pom_path.display(),
        repos.remote_count(),
        repos.route_count(),
        if many {
            format!(", {} modules", selected.len())
        } else {
            String::new()
        }
    );
    let test_providers = reactor::test_class_providers(&modules, &selected);
    let services = Services {
        repos: &repos,
        config: &config,
    };
    let mut test_rows = vec![];
    for &i in &selected {
        let module = &modules[i];
        if many {
            eprintln!("==> {}", module.name());
        }
        let lock_hash = lock::fingerprint(&reactor::lock_inputs(&modules, &edges, i))?;
        let resolved = dependencies(&args, module, &lock_hash, &repos, &loader)?;
        let direct = resolved.iter().filter(|r| r.depth == 1).count();
        eprintln!(
            "  {} artifacts ({direct} direct): {}",
            resolved.len(),
            repos.summary()
        );
        let outcomes = run_module(
            &args,
            module,
            &resolved,
            test_providers.contains(&i),
            &services,
        )?;
        test_rows.push((module.name().to_string(), outcomes));
    }
    if many && matches!(args.cmd.as_str(), "test" | "verify") {
        test_report(&test_rows)
            .iter()
            .for_each(|l| eprintln!("{l}"));
    }
    eprintln!("done in {:?}", t0.elapsed());
    if let Some(d) = cold_dir {
        let _ = std::fs::remove_dir_all(d);
    }
    Ok(())
}

/// The one-line version string: package version, git commit, build time and a name derived from the commit.
///
/// A `-dirty` suffix on the commit means the working tree had uncommitted changes when it was built.
pub fn version() -> String {
    format!(
        "rvn {} ({}, {}) \"{}\"",
        env!("CARGO_PKG_VERSION"),
        env!("RAYVN_COMMIT"),
        env!("RAYVN_BUILT"),
        env!("RAYVN_NAME")
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_version_line_names_the_binary_and_the_package_version() {
        let v = version();
        assert!(v.starts_with("rvn "));
        assert!(v.contains(env!("CARGO_PKG_VERSION")));
    }

    fn parse(args: &[&str]) -> Result<Option<Args>> {
        Args::parse(&args.iter().map(|a| a.to_string()).collect::<Vec<_>>())
    }

    #[test]
    fn no_or_unknown_command_means_usage() {
        assert_eq!(parse(&[]).unwrap(), None);
        assert_eq!(parse(&["frobnicate"]).unwrap(), None);
    }

    #[test]
    fn flags_and_pom_path_are_parsed() {
        let a = parse(&["build", "-f", "sub/pom.xml", "--locked", "--refresh"])
            .unwrap()
            .unwrap();
        assert_eq!(
            (a.cmd.as_str(), a.locked, a.refresh, a.cold, a.test),
            ("build", true, true, false, false)
        );
        assert_eq!(a.project_dir(), PathBuf::from("sub"));
        assert_eq!(
            parse(&["lock"]).unwrap().unwrap().project_dir(),
            PathBuf::from(".")
        );
    }

    #[test]
    fn cold_implies_refresh_and_is_refused_for_classpath() {
        let a = parse(&["lock", "--cold"]).unwrap().unwrap();
        assert!(a.cold && a.refresh);
        assert!(parse(&["classpath", "--cold"]).is_err());
    }

    #[test]
    fn rerun_flag_is_parsed() {
        assert!(parse(&["test", "--rerun"]).unwrap().unwrap().rerun);
        assert!(!parse(&["test"]).unwrap().unwrap().rerun);
    }

    #[test]
    fn test_cache_flag_takes_a_directory() {
        assert_eq!(
            parse(&["test", "--test-cache", "/ci"])
                .unwrap()
                .unwrap()
                .test_cache
                .as_deref(),
            Some("/ci")
        );
        assert_eq!(parse(&["test"]).unwrap().unwrap().test_cache, None);
        assert!(parse(&["test", "--test-cache"]).is_err());
    }

    #[test]
    fn module_flag_takes_a_name() {
        assert_eq!(
            parse(&["test", "-m", "core"])
                .unwrap()
                .unwrap()
                .module
                .as_deref(),
            Some("core")
        );
        assert_eq!(
            parse(&["test", "--module", "core"])
                .unwrap()
                .unwrap()
                .module
                .as_deref(),
            Some("core")
        );
        assert_eq!(parse(&["test"]).unwrap().unwrap().module, None);
        assert!(parse(&["test", "-m"]).is_err());
    }

    #[test]
    fn the_test_report_lists_modules_with_tests_and_counts_the_rest() {
        use verify::TestOutcome::*;
        let rows = vec![
            ("a".to_string(), vec![Passed(3)]),
            ("b".to_string(), vec![NoTests]),
            ("c".to_string(), vec![Cached(4), Passed(1)]),
        ];
        assert_eq!(
            test_report(&rows),
            [
                "tests by module:",
                "  a: 3 passed",
                "  c: 5 passed (4 cached)",
                "  total: 8 passed in 2 modules (4 cached); 1 modules have no tests"
            ]
        );
    }

    #[test]
    fn unrun_plugins_produce_one_warning_each_naming_the_module() {
        let mut e = model::Effective {
            gav: pom::Gav::new("g", "mod", "1"),
            packaging: "jar".into(),
            props: Default::default(),
            managed: Default::default(),
            deps: vec![],
            repos: vec![],
            shade: None,
            compiler: None,
            surefire: None,
            failsafe: None,
            relocation: None,
            unrun: vec!["exec-maven-plugin (exec at generate-sources)".into()],
        };
        let w = unrun_warnings(&e);
        assert_eq!(w.len(), 1);
        assert!(
            w[0].contains("mod")
                && w[0].contains("exec-maven-plugin (exec at generate-sources)")
                && w[0].contains("docs/plugins.md"),
            "{}",
            w[0]
        );
        e.unrun.clear();
        assert!(unrun_warnings(&e).is_empty());
    }

    #[test]
    fn dash_f_needs_a_path() {
        assert!(parse(&["lock", "-f"]).is_err());
    }
}
